use std::collections::HashMap;

use rust_bot::providers::base::{LLMProvider, LLMResponse};
use rust_bot::providers::openai_compat_provider::OpenAICompatProvider;
use rust_bot::providers::registry::ProviderSpec;
use uuid::Uuid;

fn read_env() -> (String, String, String) {
    dotenv::dotenv().expect("Failed to read .env file");
    let openai_api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY is not set");
    let openai_api_base = std::env::var("OPENAI_API_BASE").expect("OPENAI_API_BASE is not set");
    let openai_api_model = std::env::var("OPENAI_API_MODEL").expect("OPENAI_API_MODEL is not set");
    (openai_api_key, openai_api_base, openai_api_model)
}

fn create_openrouter_provider() -> OpenAICompatProvider {
    let (openai_api_key, openai_api_base, openai_api_model) = read_env();
    let mut extra_headers = HashMap::new();
    extra_headers.insert("x-session-affinity".to_string(), Uuid::new_v4().simple().to_string());
    <OpenAICompatProvider as LLMProvider>::new(
        Some(openai_api_key), 
        Some(openai_api_base),
        Some(openai_api_model), 
        Some(extra_headers),
        None
    )
}

fn create_openrouter_provider_with_spec() -> OpenAICompatProvider {
    let (openai_api_key, openai_api_base, openai_api_model) = read_env();
    let mut extra_headers = HashMap::new();
    extra_headers.insert("x-session-affinity".to_string(), Uuid::new_v4().simple().to_string());
    <OpenAICompatProvider as LLMProvider>::new(
        Some(openai_api_key), 
        None,
        Some(openai_api_model), 
        None,
        Some(ProviderSpec {
            name: "openrouter".to_string(),
            keywords: vec!["openai_compat".to_string()],
            env_key: "OPENAI_API_KEY".to_string(),
            display_name: "openrouter".to_string(),
            backend: "openai_compat".to_string(),
            env_extras: vec![],
            is_gateway: true,
            is_local: false,
            detect_by_key_prefix: "".to_string(),
            detect_by_base_keyword: "".to_string(),
            default_api_base: Some(openai_api_base),
            strip_model_prefix: false,
            model_overrides: vec![],
            is_oauth: false,
            is_direct: false,
            supports_prompt_caching: false,
        })
    )
}

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

async fn simple_test_chat(message: &str) -> LLMResponse {
    let provider = create_openrouter_provider();
    let messages = vec![
        serde_json::json!({
            "role": "user",
            "content": message
        })
    ];
    let response = provider.chat(messages, None, None, 100, 0.5, None, None).await;
    response
}

async fn simple_test_safe_chat(message: &str) -> LLMResponse {
    let provider = create_openrouter_provider();
    let messages = vec![
        serde_json::json!({
            "role": "user",
            "content": message
        })
    ];
    let response = provider.safe_chat(messages, None, None, 100, 0.5, None, None).await;
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
    let response = simple_test_safe_chat("Who are you? And what is your knownledge cut-off date?").await;
    assert!(response.content.is_some());
    println!("response: {}", response.content.unwrap());
    assert!(response.finish_reason == "stop");
}