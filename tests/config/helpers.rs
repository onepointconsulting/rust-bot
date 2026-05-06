use std::{collections::HashMap, env};
use dotenv::dotenv;
use rust_bot::providers::{base::LLMProvider, openai_compat_provider::OpenAICompatProvider, registry::ProviderSpec};
use uuid::Uuid;

pub fn read_env() -> (String, String, String) {
    dotenv().expect("Failed to read .env file");
    let openai_api_key = env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY is not set");
    let openai_api_base = env::var("OPENAI_API_BASE").expect("OPENAI_API_BASE is not set");
    let openai_api_model = env::var("OPENAI_API_MODEL").expect("OPENAI_API_MODEL is not set");
    (openai_api_key, openai_api_base, openai_api_model)
}

pub fn read_mcp_env() -> (String, String) {
    dotenv().expect("Failed to read .env file");
    let mcp_server_url = env::var("MCP_SERVER_URL").expect("MCP_SERVER_URL is not set");
    let mcp_headers_jwt = env::var("MCP_HEADERS_JWT").expect("MCP_HEADERS_JWT is not set");
    (mcp_server_url, mcp_headers_jwt)
}

pub fn create_openrouter_provider() -> OpenAICompatProvider {
    let (openai_api_key, openai_api_base, openai_api_model) = read_env();
    let mut extra_headers = HashMap::new();
    extra_headers.insert(
        "x-session-affinity".to_string(),
        Uuid::new_v4().simple().to_string(),
    );
    <OpenAICompatProvider as LLMProvider>::new(
        Some(openai_api_key),
        Some(openai_api_base),
        Some(openai_api_model),
        Some(extra_headers),
        None,
    )
}

pub fn create_openrouter_provider_with_spec() -> OpenAICompatProvider {
    let (openai_api_key, openai_api_base, openai_api_model) = read_env();
    let mut extra_headers = HashMap::new();
    extra_headers.insert(
        "x-session-affinity".to_string(),
        Uuid::new_v4().simple().to_string(),
    );
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
            supports_max_completion_tokens: false,
        }),
    )
}