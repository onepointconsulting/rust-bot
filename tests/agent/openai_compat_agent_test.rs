use std::sync::Arc;
use serde_json::Value;
use rust_bot::agent::runner::{AgentRunSpec, AgentRunner};

use crate::config::helpers::{read_env, create_openrouter_provider, create_openrouter_provider_with_spec};

fn create_agent_run_spec(messages: Vec<Value>) -> AgentRunSpec {
    let (_openai_api_key, _openai_api_base, openai_api_model) = read_env();
    AgentRunSpec {
        model: openai_api_model,
        max_iterations: 30,
        initial_messages: messages,
        ..AgentRunSpec::default()   // everything else gets its default
    }
}

#[tokio::test]
async fn test_simple_run_no_tools() {
    let (_openai_api_key, _openai_api_base, _openai_api_model) = read_env();
    let provider = create_openrouter_provider();
    let runner = AgentRunner::new(Arc::new(provider));
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Hello, how are you?"
    })];
    let spec = create_agent_run_spec(messages);
    let result = runner.run(spec).await;
    assert!(result.final_content.is_some());
    let final_message = result.final_content.unwrap();
    assert!(final_message.len() > 0);
    assert!(result.stop_reason == "completed");
    println!("final_message: {:?}", final_message);
}