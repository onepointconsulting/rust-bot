use rust_bot::providers::base::{LLMProvider, LLMResponse};
use rust_bot::providers::openai_compat_provider::OpenAICompatProvider;

use crate::config::helpers::{
    create_openrouter_provider, create_openrouter_provider_with_spec, read_env,
};

#[test]
fn creates_provider_via_trait_constructor() {
    let (_openai_api_key, _openai_api_base, openai_api_model) = read_env();
    let provider = create_openrouter_provider();

    assert!(provider.api_key().is_some());
    assert!(provider.api_key().unwrap().len() > 0);
    assert!(provider.api_base().is_some());
    assert!(provider.get_default_model().len() > 0);
    assert!(provider.get_default_model() == openai_api_model);
    assert!(provider.spec().is_none());
    assert!(provider.extra_headers().is_some());
    assert_eq!(provider.extra_headers().unwrap().len(), 1);
}

#[test]
fn test_create_openrouter_provider_with_spec() {
    let (_openai_api_key, _openai_api_base, openai_api_model) = read_env();
    let provider = create_openrouter_provider_with_spec();
    assert!(provider.api_key().is_some());
    assert!(provider.api_base().is_some());
    assert!(provider.get_default_model().len() > 0);
    assert_eq!(provider.get_default_model(), openai_api_model);
    assert!(provider.spec().is_some());
    assert_eq!(provider.spec().unwrap().name, "openrouter");
}

async fn simple_test_chat_system(system_message: &str, user_message: &str) -> LLMResponse {
    let provider = create_openrouter_provider();
    let messages = vec![
        serde_json::json!({
            "role": "system",
            "content": system_message
        }),
        serde_json::json!({
            "role": "user",
            "content": user_message
        }),
    ];
    let response = provider
        .chat(messages, None, None, 100, Some(0.5), None, None)
        .await;
    response
}

async fn simple_test_chat(message: &str) -> LLMResponse {
    let provider = create_openrouter_provider();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": message
    })];
    let response = provider
        .chat(messages, None, None, 1000, Some(0.5), None, None)
        .await;
    response
}

fn simple_weather_tool() -> serde_json::Value {
    serde_json::json!({
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Get the current weather for a given location",
        "parameters": {
          "type": "object",
          "properties": {
            "location": {
              "type": "string",
              "description": "The city and country, e.g. 'London, UK'"
            },
            "unit": {
              "type": "string",
              "enum": ["celsius", "fahrenheit"],
              "description": "The temperature unit to use"
            }
          },
          "required": ["location"]
        }
      }
    })
}

async fn simple_test_safe_chat(message: &str) -> LLMResponse {
    let provider = create_openrouter_provider();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": message
    })];
    let response = provider
        .safe_chat(
            messages,
            Some(vec![simple_weather_tool()]),
            None,
            1000,
            Some(0.5),
            None,
            None,
        )
        .await;
    response
}

#[tokio::test]
async fn test_chat_success() {
    let response = simple_test_chat("Hello, how are you?").await;
    assert!(response.content.is_some());
    println!("response: {}", response.content.unwrap());
    assert!(response.finish_reason == "stop");
    assert!(response.tool_calls.is_empty());
}

#[tokio::test]
async fn test_who_are_you() {
    let response = simple_test_chat("Who are you?").await;
    assert!(response.content.is_some());
    println!("response: {}", response.content.unwrap());
    assert!(response.finish_reason == "stop");
}

#[tokio::test]
async fn test_who_are_you_safe() {
    let response =
        simple_test_safe_chat("Who are you? And what is your knowledge cut-off date?").await;
    println!("finish reason: {}", response.finish_reason);
    assert!(response.finish_reason == "stop");
    assert!(response.content.is_some());
    println!("response: {}", response.content.unwrap());
}

#[tokio::test]
async fn test_who_are_you_system() {
    let response = simple_test_chat_system(
        "You are a helpful assistant who loves jokes. You always respond with a joke.",
        "Who are you? And what is your knowledge cut-off date?",
    )
    .await;
    assert!(response.content.is_some());
    println!("response: {}", response.content.unwrap());
    println!("finish reason: {}", response.finish_reason);
    assert!(response.tool_calls.is_empty());
}

#[tokio::test]
async fn test_who_are_you_system_safe() {
    let response = simple_test_safe_chat("How is the weather in London?").await;
    println!("finish reason: {}", response.finish_reason);
    assert!(!response.tool_calls.is_empty());
}

fn create_openrouter_provider_with_long_message() -> (OpenAICompatProvider, Vec<serde_json::Value>)
{
    let provider = create_openrouter_provider();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": "Can you tell me a really long joke?"
    })];
    return (provider, messages);
}

fn create_openrouter_provider_with_long_message_2(
    message: &str,
) -> (OpenAICompatProvider, Vec<serde_json::Value>) {
    let provider = create_openrouter_provider();
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": message
    })];
    return (provider, messages);
}

#[tokio::test]
async fn test_chat_stream_success() {
    let (provider, messages) = create_openrouter_provider_with_long_message();
    let response = provider
        .chat_stream(
            messages,
            None,
            None,
            1000,
            Some(0.5),
            None,
            None,
            &Some(|content| async move {
                println!("==>: {}", content);
            }),
            &None,
        )
        .await;
    assert!(response.content.is_some());
    assert!(response.content.unwrap().len() > 0);
    // println!("response: {}", response.content.unwrap());
}

#[tokio::test]
async fn test_safe_chat_stream_success() {
    let (provider, messages) = create_openrouter_provider_with_long_message();
    let response = provider
        .safe_chat_stream(
            messages,
            None,
            None,
            1000,
            Some(0.5),
            None,
            None,
            &Some(|content| async move {
                println!("==>: {}", content);
            }),
            &None,
        )
        .await;
    assert!(response.content.is_some());
    assert!(response.content.unwrap().len() > 0);
    // println!("response: {}", response.content.unwrap());
}

#[tokio::test]
async fn test_safe_chat_stream_with_retry_success() {
    let (provider, messages) = create_openrouter_provider_with_long_message();
    let response = provider
        .safe_chat_stream_with_retry(
            messages,
            None,
            None,
            Some(3000),
            Some(0.5f32),
            None,
            None,
            &Some(|content| async move {
                println!("==>: {}", content);
            }),
            &None,
        )
        .await;
    assert!(response.content.is_some());
    assert!(response.content.unwrap().len() > 0);
    // println!("response: {}", response.content.unwrap());
}

#[tokio::test]
async fn test_safe_chat_stream_with_retry_success_2() {
    let (provider, messages) =
        create_openrouter_provider_with_long_message_2("Can you please tell me  a bed time story?");
    let response = provider
        .safe_chat_stream_with_retry(
            messages,
            None,
            None,
            Some(3000),
            Some(0.5f32),
            None,
            None,
            &Some(|content| async move {
                println!("==>: {}", content);
            }),
            &None,
        )
        .await;
    assert!(response.content.is_some());
    assert!(response.content.unwrap().len() > 0);
    // println!("response: {}", response.content.unwrap());
}
