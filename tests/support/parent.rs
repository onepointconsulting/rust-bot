//! A parent rust-bot in a temp folder, for tests that run real children.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_bot::agent::agent_loop::{AgentLoop, ProgressCallback, StreamCallback, StreamEndCallback};
use rust_bot::bus::outbound_events::ProgressKind;
use rust_bot::bus::queue::MessageBus;
use rust_bot::config::schema::Config;
use rust_bot::providers::base::LLMProviderDyn;
use rust_bot::utils::helpers::sync_workspace_templates;
use serde_json::{Value, json};

use super::mock_llm::MockLlm;
use super::{ScriptedProvider, SharedProvider};

pub struct World {
    pub _root: tempfile::TempDir,
    pub workspace: PathBuf,
    pub project: PathBuf,
    pub config_path: PathBuf,
    pub config: Config,
    pub children_llm: MockLlm,
}

/// A parent workspace, a project folder and a config whose `rustbot` preset is
/// the real binary and whose provider is the children's mock LLM.
pub fn world(extra_acp: Value) -> World {
    let root = tempfile::tempdir().unwrap();
    let base = std::path::absolute(root.path()).unwrap();
    let workspace = base.join("parent-home");
    let project = base.join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("notes.txt"), "hello from the project").unwrap();
    sync_workspace_templates(&workspace, true);

    let children_llm = MockLlm::start(Vec::new());
    let mut acp = json!({
        "enabled": true,
        "shutdownGraceSecs": 60,
        "defaultTimeoutSecs": 120,
        "launchPresets": {"rustbot": {"command": [env!("CARGO_BIN_EXE_rust-bot"), "acp"]}}
    });
    if let (Some(target), Some(extra)) = (acp.as_object_mut(), extra_acp.as_object()) {
        target.extend(extra.clone());
    }
    let raw = json!({
        "agents": {"provider": "openai", "model": "gpt-4o-mini", "workspace": workspace},
        "providers": {"openai": {"apiKey": "sk-test", "apiBase": children_llm.api_base()}},
        "tools": {"acp": acp}
    });
    let config_path = base.join("config.json");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
    let config: Config = serde_json::from_value(raw).unwrap();
    World {
        _root: root,
        workspace,
        project,
        config_path,
        config,
        children_llm,
    }
}

/// A parent agent loop on `world.workspace` whose model replies from `script`.
pub fn parent_loop(
    world: &World,
    script: Vec<rust_bot::providers::base::LLMResponse>,
) -> (Arc<AgentLoop>, Arc<ScriptedProvider>) {
    let provider = ScriptedProvider::new(script);
    let for_loop: Arc<dyn LLMProviderDyn> = Arc::new(SharedProvider(Arc::clone(&provider)));
    let agent_loop = AgentLoop::new(
        Arc::new(MessageBus::new()),
        for_loop,
        world.workspace.clone(),
        world.config.clone(),
        None,
        None,
        None,
    );
    (Arc::new(agent_loop), provider)
}

pub fn tool_names(agent_loop: &AgentLoop) -> Vec<String> {
    let mut names = agent_loop.tools_for_session(None).tool_names();
    names.sort();
    names
}

pub fn acp_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|name| name.starts_with("acp_"))
        .cloned()
        .collect()
}

/// Collects the progress lines a turn showed.
pub fn progress_recorder() -> (ProgressCallback, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let callback: ProgressCallback = Arc::new(move |text, kind| {
        let sink = Arc::clone(&sink);
        Box::pin(async move {
            if kind == ProgressKind::ToolHint {
                sink.lock().unwrap().push(text);
            }
        })
    });
    (callback, seen)
}

pub async fn say(
    agent_loop: &Arc<AgentLoop>,
    text: &str,
    on_progress: Option<ProgressCallback>,
) -> String {
    // Like the CLI and the gateway, stream: the scripted model answers agent
    // turns only when streamed (its plain `chat` is for background work).
    let on_stream: StreamCallback = Arc::new(|_delta| Box::pin(async {}));
    let on_stream_end: StreamEndCallback = Arc::new(|_resuming| Box::pin(async {}));
    let reply = tokio::time::timeout(
        Duration::from_secs(180),
        Arc::clone(agent_loop).process_direct(
            text,
            None,
            None,
            None,
            None,
            on_progress,
            Some(on_stream),
            Some(on_stream_end),
        ),
    )
    .await
    .expect("the turn finished in time")
    .expect("the turn produced a reply");
    reply.content
}
