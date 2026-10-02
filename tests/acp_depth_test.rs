//! A chain of three: the root, a child, and a grandchild created by the child.
//!
//! Two guarantees are checked on real processes:
//!
//! * **Depth.** With `maxDepth` 2 the child (depth 1) may create and run agents,
//!   and the grandchild (depth 2) is at the limit and gets no `acp_*` tools.
//! * **Inheritance.** The grandchild inherits the child's restrictions. The child
//!   has no shell and cannot write files; a grandchild whose own overlay says
//!   nothing about that must not get them back from the root config.
//!
//! In its own file because the config path is process-global: only one test per
//! file may run real children.

use rust_bot::config::loader::set_config_path;
use serde_json::{Value, json};

mod support;

use support::mock_llm::MockReply;
use support::parent::{parent_loop, say, world};
use support::{text_reply, tool_call_reply};

/// Tool names of every streamed (agent-turn) request the mock LLM received.
fn tool_lists(requests: &[Value]) -> Vec<Vec<String>> {
    requests
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
        .collect()
}

#[tokio::test]
async fn a_grandchild_is_at_the_depth_limit_and_inherits_the_childs_restrictions() {
    // The middle agent's acp calls are not read-only, so the policy must allow them.
    let world = world(json!({"maxDepth": 2, "permissionPolicy": "allow-all"}));
    set_config_path(world.config_path.clone());
    let project = world.project.to_string_lossy().into_owned();

    // The root creates a read-only child and runs it.
    let (root, _) = parent_loop(
        &world,
        vec![
            tool_call_reply(
                "r1",
                "acp_create_agent",
                json!({
                    "name": "mid",
                    "purpose": "a read-only middle agent",
                    "overlay": {"tools": {"exec": {"enable": false}, "disabledTools": ["write_file", "edit_file"]}}
                }),
            ),
            tool_call_reply(
                "r2",
                "acp_run_agent",
                json!({"name": "mid", "prompt": "make a helper and ask it", "cwd": project}),
            ),
            text_reply("the root is done"),
        ],
    );
    // The middle agent creates a grandchild whose overlay says nothing about
    // restrictions, runs it, and answers. Replies are taken in the order the
    // processes ask: the child blocks while its grandchild works.
    world.children_llm.push(vec![
        MockReply::tool_call(
            "m1",
            "acp_create_agent",
            json!({"name": "helper", "purpose": "a helper"}),
        ),
        MockReply::tool_call(
            "m2",
            "acp_run_agent",
            json!({"name": "helper", "prompt": "say hello", "cwd": project}),
        ),
        // ← the grandchild asks here
        MockReply::text("hello from the grandchild"),
        // ← then the middle agent asks again, with the grandchild's answer
        MockReply::text("the middle agent heard the helper"),
    ]);

    let answer = say(&root, "ask the middle agent to use a helper", None).await;

    assert!(answer.contains("the root is done"), "{answer}");
    let lists = tool_lists(&world.children_llm.requests());
    assert!(
        lists.len() >= 4,
        "child, grandchild, child again: {lists:?}"
    );

    // Nobody below the root got back what the child gave up.
    for tools in &lists {
        for forbidden in ["shell", "write_file", "edit_file"] {
            assert!(
                !tools.contains(&forbidden.to_string()),
                "{forbidden} must stay disabled all the way down: {tools:?}"
            );
        }
        assert!(tools.contains(&"read_file".to_string()), "{tools:?}");
    }
    // The child (depth 1) has the acp tools; the grandchild (depth 2) is at the limit.
    let with_acp = lists
        .iter()
        .filter(|tools| tools.contains(&"acp_run_agent".to_string()))
        .count();
    let without_acp = lists.len() - with_acp;
    assert!(
        with_acp >= 2,
        "the child asks the model at least twice: {lists:?}"
    );
    assert!(
        without_acp >= 1,
        "the grandchild has no acp tools: {lists:?}"
    );
    // The grandchild really ran under the child's home.
    assert!(
        world
            .workspace
            .join("acp")
            .join("agents")
            .join("mid")
            .join("acp")
            .join("agents")
            .join("helper")
            .join("agent.json")
            .is_file(),
        "the helper lives in the middle agent's home, not the root's"
    );
    assert!(
        !world
            .workspace
            .join("acp")
            .join("agents")
            .join("helper")
            .exists(),
        "and nothing was created in the root's own agent list"
    );
}
