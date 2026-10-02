//! Test support shared by the ACP integration tests: a scripted LLM provider.
//!
//! Replies come from a fixed script (one per streamed agent-turn call), so no
//! network or API key is needed.

#![allow(dead_code)]

pub mod mock_llm;
pub mod parent;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_bot::cli::acp::{AcpRuntime, assemble_acp_runtime, assemble_acp_runtime_denying};
use rust_bot::config::schema::Config;
use rust_bot::providers::base::{
    BoxedProgressCallback, GenerationSettings, LLMProvider, LLMProviderDyn, LLMResponse, LLMUsage,
    ToolCallRequest,
};
use rust_bot::providers::registry::ProviderSpec;
use rust_bot::utils::helpers::sync_workspace_templates;
use serde_json::Value;

/// Replies from a fixed script (one per streamed agent-turn call); records every
/// message list a turn was given.
pub struct ScriptedProvider {
    script: Mutex<VecDeque<LLMResponse>>,
    pub seen_messages: Mutex<Vec<Vec<Value>>>,
    pub generation: GenerationSettings,
}

impl ScriptedProvider {
    pub fn new(script: Vec<LLMResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            seen_messages: Mutex::new(Vec::new()),
            generation: GenerationSettings::new(),
        })
    }

    fn next_response(&self, messages: Vec<Value>) -> LLMResponse {
        self.seen_messages.lock().unwrap().push(messages);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_reply("(script exhausted)"))
    }

    /// Text of every message the model was shown, across all calls.
    pub fn everything_seen(&self) -> String {
        serde_json::to_string(&*self.seen_messages.lock().unwrap()).unwrap()
    }

    /// Add replies for later turns.
    pub fn push(&self, replies: Vec<LLMResponse>) {
        self.script.lock().unwrap().extend(replies);
    }
}

pub fn text_reply(text: &str) -> LLMResponse {
    LLMResponse {
        content: Some(text.to_string()),
        tool_calls: Vec::new(),
        finish_reason: "stop".to_string(),
        usage: LLMUsage::new(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

pub fn tool_call_reply(id: &str, name: &str, arguments: Value) -> LLMResponse {
    LLMResponse {
        content: None,
        tool_calls: vec![ToolCallRequest {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments
                .as_object()
                .unwrap()
                .clone()
                .into_iter()
                .collect::<HashMap<_, _>>(),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }],
        finish_reason: "tool_calls".to_string(),
        usage: LLMUsage::new(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

/// `Arc<ScriptedProvider>` is what the test keeps; this newtype is what the agent owns.
pub struct SharedProvider(pub Arc<ScriptedProvider>);

impl LLMProvider for SharedProvider {
    fn new(
        _api_key: Option<String>,
        _api_base: Option<String>,
        _default_model: Option<String>,
        _extra_headers: Option<HashMap<String, String>>,
        _spec: Option<ProviderSpec>,
    ) -> Self {
        SharedProvider(ScriptedProvider::new(Vec::new()))
    }

    fn api_key(&self) -> Option<String> {
        None
    }

    fn api_base(&self) -> Option<String> {
        None
    }

    fn extra_headers(&self) -> Option<HashMap<String, String>> {
        None
    }

    fn generation_settings(&self) -> &GenerationSettings {
        &self.0.generation
    }

    fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
        // Only reachable before the provider is shared; the tests never call it.
        unimplemented!("generation settings are fixed for the scripted provider")
    }

    fn spec(&self) -> Option<&ProviderSpec> {
        None
    }

    fn get_default_model(&self) -> String {
        "scripted".to_string()
    }

    async fn chat(
        &self,
        messages: Vec<Value>,
        _tools: Option<Vec<Value>>,
        _model: Option<String>,
        _max_tokens: usize,
        _temperature: Option<f32>,
        _reasoning_effort: Option<String>,
        _tool_choice: Option<Value>,
    ) -> LLMResponse {
        // Non-streaming calls are background utilities (session title, memory
        // consolidation): agent turns always stream. They must not consume the
        // replies scripted for the turns.
        let _ = messages;
        text_reply("Scripted title")
    }

    async fn chat_stream<F, Fut>(
        &self,
        messages: Vec<Value>,
        _tools: Option<Vec<Value>>,
        _model: Option<String>,
        _max_tokens: usize,
        _temperature: Option<f32>,
        _reasoning_effort: Option<String>,
        _tool_choice: Option<Value>,
        on_content_delta: &Option<F>,
        _on_progress: &Option<BoxedProgressCallback>,
    ) -> LLMResponse
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let response = self.0.next_response(messages);
        if let (Some(callback), Some(text)) = (on_content_delta, &response.content) {
            callback(text.clone()).await;
        }
        response
    }
}

// ── a child agent to talk to ────────────────────────────────────────────────

pub struct Fixture {
    pub runtime: AcpRuntime,
    pub provider: Arc<ScriptedProvider>,
    pub project: tempfile::TempDir,
    pub workspace: tempfile::TempDir,
}

impl Fixture {
    /// A fresh runtime on the same workspace, like a restarted `rust-bot acp`
    /// process: new agent loop, new registry, new connection slot.
    pub fn restarted_runtime(
        &self,
        script: Vec<LLMResponse>,
    ) -> (AcpRuntime, Arc<ScriptedProvider>) {
        let mut config = Config::default();
        config.agents.workspace = self.workspace.path().to_string_lossy().into_owned();
        let provider = ScriptedProvider::new(script);
        let for_agent: Arc<dyn LLMProviderDyn> = Arc::new(SharedProvider(Arc::clone(&provider)));
        let runtime = assemble_acp_runtime(&config, self.workspace.path().to_path_buf(), for_agent);
        (runtime, provider)
    }

    /// Stored message lines (everything after the metadata line) of a session.
    pub fn stored_messages(&self, session_id: &str) -> Vec<String> {
        let path = self
            .workspace
            .path()
            .join("sessions")
            .join(format!("acp_{session_id}.jsonl"));
        std::fs::read_to_string(path)
            .expect("the session file exists")
            .lines()
            .skip(1)
            .map(str::to_string)
            .collect()
    }
}

pub fn fixture(script: Vec<LLMResponse>) -> Fixture {
    fixture_with(script, |_| {})
}

/// [`fixture`] with a chance to adjust the config before the agent is built.
pub fn fixture_with(script: Vec<LLMResponse>, adjust: impl FnOnce(&mut Config)) -> Fixture {
    fixture_denying(script, adjust, |_project| Vec::new())
}

/// [`fixture_with`] whose file tools also refuse the folders `denied_roots`
/// returns (given the project folder), as `--deny-path` makes a real child do.
pub fn fixture_denying(
    script: Vec<LLMResponse>,
    adjust: impl FnOnce(&mut Config),
    denied_roots: impl FnOnce(&Path) -> Vec<PathBuf>,
) -> Fixture {
    let workspace = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    sync_workspace_templates(workspace.path(), false);
    let denied_roots = denied_roots(project.path());

    let mut config = Config::default();
    config.agents.workspace = workspace.path().to_string_lossy().into_owned();
    adjust(&mut config);

    let provider = ScriptedProvider::new(script);
    let provider_for_agent: Arc<dyn LLMProviderDyn> =
        Arc::new(SharedProvider(Arc::clone(&provider)));
    let runtime = assemble_acp_runtime_denying(
        &config,
        workspace.path().to_path_buf(),
        provider_for_agent,
        denied_roots,
    );
    Fixture {
        runtime,
        provider,
        project,
        workspace,
    }
}

pub fn write_project_file(project: &Path, name: &str, contents: &str) -> PathBuf {
    let path = project.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

/// A shell command that runs for a long time and is easy to recognise.
pub fn long_running_command() -> &'static str {
    if cfg!(windows) {
        "ping -n 41 127.0.0.1 > nul"
    } else {
        "sleep 41"
    }
}

/// Number of running processes whose command line contains `needle`.
pub fn running_process_count(needle: &str) -> usize {
    if cfg!(windows) {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {needle}"), "/NH"])
            .output()
            .expect("run tasklist");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| {
                line.to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase())
            })
            .count()
    } else {
        let output = std::process::Command::new("pgrep")
            .args(["-c", "-f", needle])
            .output()
            .expect("run pgrep");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }
}

/// Name to look for in the process list while the long command runs.
pub fn long_running_process_name() -> &'static str {
    if cfg!(windows) {
        "PING.EXE"
    } else {
        "sleep 41"
    }
}

pub async fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}
