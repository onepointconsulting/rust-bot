use crate::config::helpers::create_anthropic_provider;
use rust_bot::providers::base::LLMProvider;

#[tokio::test]
async fn test_simple_chat() {
    let provider = create_anthropic_provider();
    let response = provider
        .chat(
            vec![serde_json::json!({
                "role": "user",
                "content": "Hello, how are you? Who are you as an AI model?"
            })],
            None,
            None,
            1024,
            Some(0.5),
            None,
            None,
        )
        .await;
    println!("response: {:?}", response);
    assert!(response.content.is_some());
    assert!(response.finish_reason == "stop");
    assert!(response.tool_calls.is_empty());
    assert!(response.thinking_blocks.is_none());
}

#[tokio::test]
async fn test_simple_chat_with_thinking() {
    let provider = create_anthropic_provider();
    let response = provider
        .chat(
            vec![serde_json::json!({
                "role": "user",
                "content": "What is 17 + 25? Think step by step, then give the final answer on its own line."
            })],
            None,
            None,
            4096,
            Some(0.5),
            Some("high".to_string()),
            None,
        )
        .await;

    println!("response: {:?}", response);
    assert_ne!(
        response.finish_reason, "error",
        "expected a successful response, got: {:?}",
        response.content
    );
    assert_eq!(response.finish_reason, "stop");
    assert!(response.tool_calls.is_empty());

    let thinking_blocks = response
        .thinking_blocks
        .as_ref()
        .expect("expected thinking blocks when reasoning_effort is enabled");
    assert!(!thinking_blocks.is_empty());
    assert_eq!(thinking_blocks[0]["type"], serde_json::json!("thinking"));

    assert!(
        response.content.is_some_and(|c| !c.is_empty()),
        "expected a text reply alongside thinking blocks"
    );
}

#[tokio::test]
async fn test_simple_chat_with_thinking_stream() {
    let provider = create_anthropic_provider();
    let response = provider.chat_stream(vec![
        serde_json::json!({
            "role": "user",
            "content": "What is 129 / 17? Think step by step, then give the final answer on its own line."
        })],
        None,
        None,
        4096,
        Some(0.5),
        Some("high".to_string()),
        None,
        &None::<fn(String) -> std::future::Ready<()>>,
        &None,
    ).await;
    println!("response: {:?}", response);
    assert_ne!(
        response.finish_reason, "error",
        "expected a successful response, got: {:?}",
        response.content
    );
    assert_eq!(response.finish_reason, "stop");
    assert!(response.tool_calls.is_empty());
}
