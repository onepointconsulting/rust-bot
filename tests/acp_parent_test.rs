//! A parent rust-bot, end to end: its own agent loop calls the `acp_*` tools,
//! which launch real child `rust-bot acp` processes.
//!
//! The parent's LLM is scripted in-process; the children's LLM is a mock HTTP
//! server (the children are separate processes). The `rustbot` launch preset in
//! the config points at the real binary, because inside a test `current_exe` is
//! the test runner.
//!
//! The config path is process-global, so exactly one test in this file runs
//! children (the depth-limit test lives in `acp_depth_test.rs` for that reason).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_bot::config::loader::set_config_path;
use serde_json::{Value, json};

mod support;

use support::mock_llm::MockReply;
use support::parent::{acp_names, parent_loop, progress_recorder, say, tool_names, world};
use support::{ScriptedProvider, text_reply, tool_call_reply};

fn read_only_overlay() -> Value {
    json!({"tools": {"exec": {"enable": false}, "disabledTools": ["write_file", "edit_file"]}})
}

fn create_call(project: &Path) -> Value {
    json!({
        "name": "code-review",
        "purpose": "reviews the project's code",
        "agents": "You are a code review specialist.",
        "overlay": read_only_overlay(),
        "cwd": project.to_string_lossy()
    })
}

// ── which tools exist ───────────────────────────────────────────────────────

#[test]
fn the_acp_tools_exist_only_when_enabled() {
    let off = world(json!({"enabled": false}));
    let (agent_loop, _) = parent_loop(&off, Vec::new());
    assert!(acp_names(&tool_names(&agent_loop)).is_empty());
    assert!(agent_loop.acp_tools().is_none());

    let on = world(json!({}));
    let (agent_loop, _) = parent_loop(&on, Vec::new());
    assert_eq!(
        acp_names(&tool_names(&agent_loop)),
        vec![
            "acp_create_agent",
            "acp_list_agents",
            "acp_run_agent",
            "acp_update_agent"
        ]
    );
}

#[test]
fn dynamic_agents_off_leaves_only_run_and_list() {
    let world = world(json!({"allowDynamicAgents": false}));
    let (agent_loop, _) = parent_loop(&world, Vec::new());
    assert_eq!(
        acp_names(&tool_names(&agent_loop)),
        vec!["acp_list_agents", "acp_run_agent"]
    );
}

#[test]
fn a_depth_limit_of_zero_means_no_children_and_no_tools() {
    let world = world(json!({"maxDepth": 0}));
    let (agent_loop, _) = parent_loop(&world, Vec::new());
    assert!(acp_names(&tool_names(&agent_loop)).is_empty());
}

#[test]
fn disabled_tools_can_remove_an_acp_tool() {
    let mut world = world(json!({}));
    world.config.tools.disabled_tools = vec!["acp_run_agent".to_string()];
    let (agent_loop, _) = parent_loop(&world, Vec::new());
    assert!(!acp_names(&tool_names(&agent_loop)).contains(&"acp_run_agent".to_string()));
}

// ── the nine steps ──────────────────────────────────────────────────────────

#[tokio::test]
async fn create_run_follow_up_relaunch_and_a_parent_restart_keep_one_conversation() {
    let world = world(json!({}));
    // The children read the config the parent was started with.
    set_config_path(world.config_path.clone());

    // Steps 1-4: the operator asks; the parent creates the agent, runs it, and relays.
    let (parent, parent_model) = parent_loop(
        &world,
        vec![
            tool_call_reply("p1", "acp_create_agent", create_call(&world.project)),
            tool_call_reply(
                "p2",
                "acp_run_agent",
                json!({"name": "code-review", "prompt": "what is in notes.txt?"}),
            ),
            text_reply("The reviewer says: it is a greeting."),
        ],
    );
    world.children_llm.push(vec![
        MockReply::tool_call("c1", "read_file", json!({"path": "notes.txt"})),
        MockReply::text("child answer one: notes.txt greets the project"),
    ]);
    let (progress, shown) = progress_recorder();

    let first = say(&parent, "review the project's notes please", Some(progress)).await;

    assert!(first.contains("The reviewer says"), "{first}");
    // The agent exists, with its own home and its overlay.
    let agent_home = world
        .workspace
        .join("acp")
        .join("agents")
        .join("code-review");
    assert!(agent_home.join("overlay.json").is_file());
    assert!(
        std::fs::read_to_string(agent_home.join("AGENTS.md"))
            .unwrap()
            .contains("You are a code review specialist.")
    );
    // The child's answer reached the parent as a tool result.
    assert!(
        parent_model.everything_seen().contains("child answer one"),
        "the parent's model must have been shown the child's reply"
    );
    // The operator saw the child's tool use as progress while the parent waited.
    let lines = shown.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("code-review ›") && line.contains("notes.txt")),
        "progress lines: {lines:?}"
    );
    // The child kept its memory in its own home.
    assert!(
        std::fs::read_dir(agent_home.join("sessions"))
            .unwrap()
            .next()
            .is_some()
    );
    // maxDepth defaults to 2 and the child is at depth 1, so it may run agents of
    // its own (and a grandchild would be at the limit).
    let child_tool_lists: Vec<Vec<String>> = world
        .children_llm
        .requests()
        .iter()
        .filter(|body| body["stream"] == json!(true))
        .map(|body| {
            body["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();
    assert!(!child_tool_lists.is_empty());
    for tools in &child_tool_lists {
        assert!(tools.contains(&"acp_run_agent".to_string()), "{tools:?}");
    }

    // Steps 5-8: a follow-up. The agent is now in the system prompt, so the model routes to it.
    let (parent, parent_model) = (parent, parent_model);
    parent_model_push(
        &parent_model,
        vec![
            tool_call_reply(
                "p3",
                "acp_run_agent",
                json!({"name": "code-review", "prompt": "and what did I ask you before?"}),
            ),
            text_reply("The reviewer remembers: you asked about notes.txt."),
        ],
    );
    world.children_llm.push(vec![MockReply::text(
        "child answer two: you asked what is in notes.txt",
    )]);

    let second = say(&parent, "ask the reviewer what I asked before", None).await;

    assert!(second.contains("The reviewer remembers"), "{second}");
    assert!(
        parent_model
            .everything_seen()
            .contains("## Available ACP agents"),
        "the parent's system prompt must list the existing agent"
    );
    assert!(
        parent_model
            .everything_seen()
            .contains("code-review: reviews the project's code")
    );
    // The relaunched child continued the conversation: its model saw the first prompt.
    assert!(
        world
            .children_llm
            .everything_seen()
            .contains("what is in notes.txt?"),
        "the second run must have loaded the first run's session"
    );

    // Step 9: the parent restarts (a new loop, a new manager, nothing in memory)
    // and the very next question still reaches the same conversation.
    wait_until_child_is_gone(&agent_home).await;
    let (restarted, restarted_model) = parent_loop(
        &world,
        vec![
            tool_call_reply(
                "p4",
                "acp_run_agent",
                json!({"name": "code-review", "prompt": "one more question"}),
            ),
            text_reply("The reviewer, after a restart: still here."),
        ],
    );
    world
        .children_llm
        .push(vec![MockReply::text("child answer three: still here")]);

    let third = say(&restarted, "one more question for the reviewer", None).await;

    assert!(third.contains("still here"), "{third}");
    let child_saw = world.children_llm.everything_seen();
    assert!(
        child_saw.contains("what is in notes.txt?")
            && child_saw.contains("and what did I ask you before?"),
        "after a parent restart the child still has both earlier turns"
    );
    assert!(restarted_model.everything_seen().contains("code-review"));
}

/// Wait until the child of `agent_home` has exited and released its lock.
async fn wait_until_child_is_gone(agent_home: &Path) {
    drop(
        rust_bot::agent::acp::workspace_lock::acquire(
            agent_home,
            Duration::from_secs(90),
            Duration::from_millis(200),
        )
        .await
        .expect("the child exits and frees its workspace"),
    );
}

fn parent_model_push(
    provider: &Arc<ScriptedProvider>,
    replies: Vec<rust_bot::providers::base::LLMResponse>,
) {
    provider.push(replies);
}

trait Lines {
    fn lines(&self) -> Vec<String>;
}

impl Lines for Arc<Mutex<Vec<String>>> {
    fn lines(&self) -> Vec<String> {
        self.lock().unwrap().clone()
    }
}
