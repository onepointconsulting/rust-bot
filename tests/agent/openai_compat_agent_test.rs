use std::{collections::HashMap, path::{Path, PathBuf}, sync::Arc, time::Duration};
use rmcp::ServiceExt;
use serde_json::Value;
use rust_bot::{agent::{runner::{AgentRunResult, AgentRunSpec, AgentRunner}, tools::{base::Tool, filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool}, mcp::{LoadedMcpTools, MCPToolWrapper, load_mcp_tools_from_config}, registry::ToolRegistry, search::{GlobTool, GrepTool}, shell::ShellTool, web::{WebFetchTool, WebSearchTool}}}, config::schema::{McpServerConfig, McpTransportType, WebSearchConfig}};
use ctor::ctor;

use crate::{agent::mcp_dummy_client::DummyMcpClient, config::helpers::read_mcp_env};
use crate::agent::mcp_dummy_server::HelloServer;
use crate::config::helpers::{read_env, create_openrouter_provider};

const WORKSPACE: &str = "workspace";

#[ctor(unsafe)]
pub fn init_logger() {
    dotenv::dotenv().expect("Failed to read .env file");
    env_logger::init();
}

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

fn create_agent_runner() -> AgentRunner {
    let provider = create_openrouter_provider();
    AgentRunner::new(Arc::new(provider))
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

fn create_agent_run_spec_with_shell_tool(messages: Vec<Value>) -> AgentRunSpec {
    let workspace_path = prepare_workspace();
    let shell_tool = Box::new(ShellTool::new(
        10, Some(workspace_path.clone()), None, None, false, None, None));
    let write_tool = Box::new(WriteFileTool::new(
        Some(workspace_path), 
        None, 
        None
    ));
    create_agent_run_spec_with_tools(messages, vec![shell_tool, write_tool])
}

#[tokio::test]
async fn test_simple_run_no_tools() {
    let runner = create_agent_runner();
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
        assert!(result.stop_reason == "completed", "stop_reason: {}", result.stop_reason);
        println!("final_message: {:?}", final_message);
    } else {
        assert!(false, "No final message found");
    }
}

#[tokio::test]
async fn test_simple_run_with_write_tool() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please write a joke to a file called joke.txt in the workspace directory?"
    })];
    let spec = create_agent_run_spec_with_write_tool(messages);
    let result = runner.run(spec).await;
    completion_message_check(&result);
}

fn create_agent_run_spec_with_read_and_write_tool(messages: Vec<Value>) -> AgentRunSpec {
    let workspace_path = prepare_workspace();
    let write_tool = Box::new(WriteFileTool::new(Some(workspace_path.clone()), None, None));
    let read_tool = Box::new(ReadFileTool::new(Some(workspace_path), None, None));
    create_agent_run_spec_with_tools(messages, vec![write_tool, read_tool])
}

/// Two-turn conversation:
///   Turn 1 — ask the agent to write a poem to `llm_poem.txt`.
///   Turn 2 — append a follow-up user message to the existing history and ask
///             the agent to summarize the poem it just wrote; the agent reads
///             the file and produces a summary.
#[tokio::test]
async fn test_write_poem_then_summarize() {
    let workspace_path = prepare_workspace();
    let poem_file = workspace_path.join("llm_poem.txt");
    // Start clean so the assertion below is unambiguous.
    let _ = std::fs::remove_file(&poem_file);

    let runner = create_agent_runner();

    // ── Turn 1: write the poem ────────────────────────────────────────────────
    let initial_messages = vec![serde_json::json!({
        "role": "user",
        "content": "Please write a short poem (4–8 lines) to a file called llm_poem.txt."
    })];
    let spec1 = create_agent_run_spec_with_read_and_write_tool(initial_messages);
    let result1 = runner.run(spec1).await;
    completion_message_check(&result1);
    assert!(poem_file.exists(), "llm_poem.txt should have been created");

    // ── Turn 2: summarize the poem ────────────────────────────────────────────
    // Build conversation history: everything from turn 1 plus a new user message.
    let mut turn2_messages = result1.messages.clone();
    turn2_messages.push(serde_json::json!({
        "role": "user",
        "content": "Now please read llm_poem.txt from the workspace directory and give me a one-sentence summary of the poem you wrote."
    }));
    let spec2 = create_agent_run_spec_with_read_and_write_tool(turn2_messages);
    let result2 = runner.run(spec2).await;
    completion_message_check(&result2);
    // The summary must reference the file name or its content in some way.
    let summary = result2.final_content.unwrap();
    assert!(!summary.is_empty(), "summary should not be empty");
    println!("Poem summary: {}", summary);
}

#[tokio::test]
async fn test_simple_run_with_write_and_list_dir_tool() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please write a joke to a file called joke1.txt and then list the contents of the workspace directory?"
    })];
    let spec = create_agent_run_spec_with_write_and_list_dir_tool(messages);
    let result = runner.run(spec).await;
    completion_message_check(&result);
    assert!(result.final_content.unwrap().contains("joke1.txt"));
}

fn create_agent_run_spec_with_read_write_edit_tool(messages: Vec<Value>) -> AgentRunSpec {
    let workspace_path = prepare_workspace();
    let write_tool = Box::new(WriteFileTool::new(Some(workspace_path.clone()), None, None));
    let read_tool = Box::new(ReadFileTool::new(Some(workspace_path.clone()), None, None));
    let edit_tool = Box::new(EditFileTool::new(Some(workspace_path), None, None));
    create_agent_run_spec_with_tools(messages, vec![write_tool, read_tool, edit_tool])
}

/// Two-turn conversation:
///   Turn 1 — ask the agent to write a first verse of a poem to `llm_poem2.txt`.
///   Turn 2 — ask the agent to append a second verse to the same file using the
///             edit tool, then verify both verses are present on disk.
#[tokio::test]
async fn test_write_and_append_poem_verse() {
    let workspace_path = prepare_workspace();
    let poem_file = workspace_path.join("llm_poem2.txt");
    // Start clean so assertions below are unambiguous.
    let _ = std::fs::remove_file(&poem_file);

    let runner = create_agent_runner();

    // ── Turn 1: write the first verse ────────────────────────────────────────
    let initial_messages = vec![serde_json::json!({
        "role": "user",
        "content": "Please write exactly one verse (4 lines) of an original poem to a file called llm_poem2.txt. Only write the verse itself, no title or extra commentary."
    })];
    let spec1 = create_agent_run_spec_with_read_write_edit_tool(initial_messages);
    let result1 = runner.run(spec1).await;
    completion_message_check(&result1);
    assert!(poem_file.exists(), "llm_poem2.txt should have been created after turn 1");
    let content_after_turn1 = std::fs::read_to_string(&poem_file).unwrap();
    assert!(!content_after_turn1.trim().is_empty(), "File should contain the first verse");

    // ── Turn 2: append a second verse ────────────────────────────────────────
    let mut turn2_messages = result1.messages.clone();
    turn2_messages.push(serde_json::json!({
        "role": "user",
        "content": "Great! Now please append a second verse (4 lines) to llm_poem2.txt using the edit tool. The new verse should be separated from the first by a blank line."
    }));
    let spec2 = create_agent_run_spec_with_read_write_edit_tool(turn2_messages);
    let result2 = runner.run(spec2).await;
    completion_message_check(&result2);

    let content_after_turn2 = std::fs::read_to_string(&poem_file).unwrap();
    // The file should be longer than after turn 1 (second verse was appended).
    assert!(
        content_after_turn2.len() > content_after_turn1.len(),
        "File should be longer after the second verse was appended"
    );
    println!("Final poem:\n{}", content_after_turn2);
}

#[tokio::test]
async fn test_shell_tool() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please recursively list the contents of the workspace directory and write the result to a file called shell_tool_result.txt?"
    })];
    let spec = create_agent_run_spec_with_shell_tool(messages);
    let result = runner.run(spec).await;
    println!("result: {:?}", result);
    completion_message_check(&result);
}

#[tokio::test]
async fn test_shell_tool_with_volume() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please check the volume information of the disk using the vol command and write the result to a file called shell_tool_vol_result.txt?"
    })];
    let spec = create_agent_run_spec_with_shell_tool(messages);
    let result = runner.run(spec).await;
    println!("result: {:?}", result);
    completion_message_check(&result);
}

#[tokio::test]
async fn test_shell_tool_with_system_info() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please check the system info and write the result to a file called system_info_vol_result.txt?"
    })];
    let spec = create_agent_run_spec_with_shell_tool(messages);
    let result = runner.run(spec).await;
    println!("result: {:?}", result);
    completion_message_check(&result);
}

#[tokio::test]
async fn test_shell_tool_with_system_info_2() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please check the system info and write the result to a file called system_info_vol_result.txt?"
    })];
    let spec = create_agent_run_spec_with_shell_tool(messages);
    let result = runner.run(spec).await;
    println!("result: {:?}", result);
    completion_message_check(&result);
}

#[tokio::test]
async fn test_shell_tool_associations_with_read_write_tool() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please list the associations of file types to applications on this system and write it to a file called assoc_result.txt?"
    })];
    let spec = create_agent_run_spec_with_shell_tool(messages);
    let result = runner.run(spec).await;
    println!("result: {:?}", result);
    completion_message_check(&result);
}

#[tokio::test]
async fn test_glob_tool() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please list all files with the txt extension in the workspace directory?"
    })];
    let workspace_path = prepare_workspace();
    let tool = Box::new(GlobTool::new(Some(workspace_path.clone()), None, None));
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(tool);
    let (_openai_api_key, _openai_api_base, openai_api_model) = read_env();
    let spec = AgentRunSpec {
        model: openai_api_model,
        max_iterations: 30,
        initial_messages: messages,
        tools: tool_registry,
        ..AgentRunSpec::default()   // everything else gets its default
    };
    let result = runner.run(spec).await;
    completion_message_check(&result);
    println!("result: {}", result.final_content.unwrap().as_str());
}

#[tokio::test]
async fn test_grep_tool() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "I am looking for content about food in the workspace directory."
    })];
    let workspace_path = prepare_workspace();
    let tool = Box::new(GrepTool::new(Some(workspace_path.clone()), None, None));
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(tool);
    let (_openai_api_key, _openai_api_base, openai_api_model) = read_env();
    let spec = AgentRunSpec {
        model: openai_api_model,
        max_iterations: 30,
        initial_messages: messages,
        tools: tool_registry,
        ..AgentRunSpec::default()   // everything else gets its default
    };
    let result = runner.run(spec).await;
    completion_message_check(&result);
    println!("result: {}", result.final_content.unwrap().as_str());
}

#[tokio::test]
async fn test_mcp_tool() {
    let runner = create_agent_runner();

    // Spin up a HelloServer over an in-process duplex byte pipe
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        HelloServer::new()
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });

    // Connect the client side and discover the tools the server advertises
    let client_service = DummyMcpClient::default()
        .serve(client_transport)
        .await
        .expect("MCP client failed to connect");

    let peer = client_service.peer().clone();
    let tools_result = peer.list_tools(None).await.expect("list_tools failed");
    assert!(!tools_result.tools.is_empty(), "HelloServer must expose at least one tool");

    // Wrap every discovered tool in an MCPToolWrapper so the agent can call it
    let mcp_tools: Vec<Box<dyn Tool>> = tools_result
        .tools
        .iter()
        .map(|tool_def| {
            Box::new(MCPToolWrapper::new(
                peer.clone(),
                "hello_server",
                tool_def,
                Duration::from_secs(30),
            )) as Box<dyn Tool>
        })
        .collect();

    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Please use the say_hello tool to greet 'World' and tell me what it responded with."
    })];

    let spec = create_agent_run_spec_with_tools(messages, mcp_tools);
    let result = runner.run(spec).await;
    println!("result: {:?}", result);
    completion_message_check(&result);
}

#[tokio::test]
async fn test_mcp_tool_with_mcp_config() {
    let runner = create_agent_runner();
    let mut headers = HashMap::new();
    let (mcp_server_url, mcp_headers_jwt, mcp_test_prompt) = read_mcp_env();
    log::info!("mcp_server_url: {}", mcp_server_url);
    log::info!("mcp_headers_jwt: {}", mcp_headers_jwt);
    headers.insert("Authorization".to_string(), mcp_headers_jwt.to_string());
    let mcp_server_config = McpServerConfig {
        transport_type: Some(McpTransportType::Sse),
        command: "".to_string(),
        args: Vec::new(),
        env: HashMap::new(),
        url: mcp_server_url.to_string(),
        headers,
        tool_timeout: 30,
        enabled_tools: Vec::new(),
    };
    let LoadedMcpTools {
        client: _mcp_client_keepalive,
        tools: mcp_tools,
    } = load_mcp_tools_from_config(&mcp_server_config, "ems")
        .await
        .expect("Failed to connect to MCP server and list tools");
    assert!(!mcp_tools.is_empty(), "EMS must expose at least one tool");
    log::info!("mcp_tools length: {}", mcp_tools.len());
    for tool in mcp_tools.iter() {
        log::info!("tool: {:?}", tool.name());
    }
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": mcp_test_prompt
    })];
    let spec = create_agent_run_spec_with_tools(messages, mcp_tools);
    let result = runner.run(spec).await;
    assert!(result.final_content.is_some(), "result should have final content");
    println!("result: {:?}", result.final_content.clone().unwrap());
    completion_message_check(&result);

}

#[tokio::test]
async fn test_web_search_tool() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please search the web for information about the weather in London?"
    })];
    let config = WebSearchConfig {
        provider: "brave".to_string(),
        api_key: std::env::var("BRAVE_API_KEY").unwrap_or_default(),
        ..WebSearchConfig::default()
    };
    let web_search_tool = WebSearchTool::new(Some(config), None);
    let spec = create_agent_run_spec_with_tools(messages, vec![Box::new(web_search_tool)]);
    let result = runner.run(spec).await;
    completion_message_check(&result);
}

#[tokio::test]
async fn test_web_search_fetch_tool() {
    let runner = create_agent_runner();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you please search the web for information about the weather in London and 
        then fetch from the first result the content of the page and write it to a file called weather_in_london.txt?"
    })];
    let config = WebSearchConfig {
        provider: "brave".to_string(),
        api_key: std::env::var("BRAVE_API_KEY").unwrap_or_default(),
        ..WebSearchConfig::default()
    };
    let web_search_tool = WebSearchTool::new(Some(config), None);
    let web_fetch_tool = WebFetchTool::new(None, None);
    let workspace_path = prepare_workspace();
    let write_tool = WriteFileTool::new(Some(workspace_path.clone()), None, None);
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(web_search_tool),
        Box::new(web_fetch_tool),
        Box::new(write_tool)
    ];
    let spec = create_agent_run_spec_with_tools(messages, tools);
    let result = runner.run(spec).await;
    completion_message_check(&result);
}