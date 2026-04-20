use std::{path::{Path, PathBuf}, sync::Arc};
use serde_json::Value;
use rust_bot::agent::{registry::ToolRegistry, runner::{AgentRunResult, AgentRunSpec, AgentRunner}, tools::{base::Tool, filesystem::{ListDirTool, WriteFileTool}}};

use crate::config::helpers::{read_env, create_openrouter_provider};

const WORKSPACE: &str = "workspace";

fn create_agent_run_spec(messages: Vec<Value>) -> AgentRunSpec {
    let (_openai_api_key, _openai_api_base, openai_api_model) = read_env();
    AgentRunSpec {
        model: openai_api_model,
        max_iterations: 30,
        initial_messages: messages,
        ..AgentRunSpec::default()   // everything else gets its default
    }
}

fn prepare_workspace() -> PathBuf {
    let workspace = Path::new(WORKSPACE);
    let workspace_path = workspace.to_path_buf();
    if !workspace_path.exists() {
        std::fs::create_dir_all(&workspace_path).unwrap();
    }
    workspace_path
}

fn create_agent_run_spec_with_write_tool(messages: Vec<Value>) -> AgentRunSpec {
    // Create workspace directory relative to the project root
    let workspace_path = prepare_workspace();
    create_agent_run_spec_with_tools(messages, vec![Box::new(WriteFileTool::new(
        Some(workspace_path), 
        None, 
        None
    ))])
}

fn create_agent_run_spec_with_write_and_list_dir_tool(messages: Vec<Value>) -> AgentRunSpec {
    // Create workspace directory relative to the project root
    let workspace_path = prepare_workspace();
    let read_tool = Box::new(WriteFileTool::new(
        Some(workspace_path.clone()), 
        None, 
        None
    ));
    let list_dir_tool = Box::new(ListDirTool::new(
        Some(workspace_path), 
        None, 
        None
    ));
    create_agent_run_spec_with_tools(messages, vec![read_tool, list_dir_tool])
}

fn create_agent_run_spec_with_tools(messages: Vec<Value>, tools: Vec<Box<dyn Tool>>) -> AgentRunSpec {
    let (_openai_api_key, _openai_api_base, openai_api_model) = read_env();
    let mut tool_registry = ToolRegistry::new();
    for tool in tools {
        tool_registry.register(tool);
    }
    AgentRunSpec {
        model: openai_api_model,
        max_iterations: 30,
        initial_messages: messages,
        tools: tool_registry,
        ..AgentRunSpec::default()   // everything else gets its default
    }
}

#[tokio::test]
async fn test_simple_run_no_tools() {
    let provider = create_openrouter_provider();
    let runner = AgentRunner::new(Arc::new(provider));
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Hello, how are you?"
    })];
    let spec = create_agent_run_spec(messages);
    let result = runner.run(spec).await;
    completion_message_check(&result);
}

fn completion_message_check(result: &AgentRunResult) {
    if let Some(final_message) = result.final_content.as_ref() {
        let final_message = final_message.as_str();
        assert!(final_message.len() > 0);
        assert!(result.stop_reason == "completed");
        println!("final_message: {:?}", final_message);
    } else {
        assert!(false, "No final message found");
    }
}

#[tokio::test]
async fn test_simple_run_with_write_tool() {
    let provider = create_openrouter_provider();
    let runner = AgentRunner::new(Arc::new(provider));
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please write a joke to a file called joke.txt in the workspace directory?"
    })];
    let spec = create_agent_run_spec_with_write_tool(messages);
    let result = runner.run(spec).await;
    completion_message_check(&result);
}

#[tokio::test]
async fn test_simple_run_with_write_and_list_dir_tool() {
    let provider = create_openrouter_provider();
    let runner = AgentRunner::new(Arc::new(provider));
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please write a joke to a file called joke1.txt in the workspace directory and then list the contents of the workspace directory?"
    })];
    let spec = create_agent_run_spec_with_write_and_list_dir_tool(messages);
    let result = runner.run(spec).await;
    completion_message_check(&result);
    assert!(result.final_content.unwrap().contains("joke1.txt"));
}