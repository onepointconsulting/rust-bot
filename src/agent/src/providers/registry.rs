#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpec {
    // identity
    pub name: String,          // config field name, e.g. "dashscope"
    pub keywords: Vec<String>, // model-name keywords for matching (lowercase)
    pub env_key: String,       // env var for API key, e.g. "DASHSCOPE_API_KEY"
    pub display_name: String,  // shown in `nanobot status`

    // which provider implementation to use
    // "openai_compat" | "anthropic" | "azure_openai" | "openai_codex"
    pub backend: String,

    // extra env vars, e.g. vec![("ZHIPUAI_API_KEY", "{api_key}")]
    pub env_extras: Vec<(String, String)>,

    // gateway / local detection
    pub is_gateway: bool,             // routes any model (OpenRouter, AiHubMix)
    pub is_local: bool,               // local deployment (vLLM, Ollama)
    pub detect_by_key_prefix: String, // match api_key prefix, e.g. "sk-or-"
    pub detect_by_base_keyword: String, // match substring in api_base URL
    pub default_api_base: Option<String>, // OpenAI-compatible base URL for this provider

    // gateway behavior
    pub strip_model_prefix: bool, // strip "provider/" before sending to gateway

    // per-model param overrides, e.g. vec![("kimi-k2.5", HashMap)]
    // Using serde_json::Value for generic map
    pub model_overrides: Vec<(String, serde_json::Value)>,

    // OAuth-based providers (e.g., OpenAI Codex) don't use API keys
    pub is_oauth: bool,

    // Direct providers skip API-key validation (user supplies everything)
    pub is_direct: bool,

    // Provider supports cache_control on content blocks (e.g. Anthropic prompt caching)
    pub supports_prompt_caching: bool,

    pub supports_max_completion_tokens: bool,
}

impl ProviderSpec {
    pub fn label(&self) -> String {
        if !self.display_name.is_empty() {
            self.display_name.clone()
        } else {
            let mut s = self.name.clone();
            if let Some(c) = s.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            s
        }
    }
}

impl Default for ProviderSpec {
    fn default() -> Self {
        ProviderSpec {
            name: String::new(),
            keywords: Vec::new(),
            env_key: String::new(),
            display_name: String::new(),
            backend: "openai_compat".to_string(),
            env_extras: Vec::new(),
            is_gateway: false,
            is_local: false,
            detect_by_key_prefix: String::new(),
            detect_by_base_keyword: String::new(),
            default_api_base: None,
            strip_model_prefix: false,
            model_overrides: Vec::new(),
            is_oauth: false,
            is_direct: false,
            supports_prompt_caching: false,
            supports_max_completion_tokens: false,
        }
    }
}

/// Find a provider spec by config field name (e.g. `"dashscope"` or `"azure-openai"`).
///
/// The name is normalised before matching: hyphens are replaced with underscores and
/// the result is lowercased, mirroring the Python `to_snake(name.replace("-", "_"))`.
pub fn find_by_name(name: &str) -> Option<ProviderSpec> {
    let normalized = name.replace('-', "_").to_lowercase();
    providers().into_iter().find(|spec| spec.name == normalized)
}

pub fn providers() -> Vec<ProviderSpec> {
    vec![
        ProviderSpec {
            name: "custom".to_string(),
            display_name: "Custom".to_string(),
            backend: "openai_compat".to_string(),
            is_direct: true,
            ..ProviderSpec::default()
        },
        ProviderSpec {
            name: "azure_openai".to_string(),
            keywords: vec!["azure".to_string(), "azure-openai".to_string()],
            display_name: "Azure OpenAI".to_string(),
            backend: "azure_openai".to_string(),
            is_direct: true,
            ..ProviderSpec::default()
        },
        ProviderSpec {
            name: "anthropic".to_string(),
            keywords: vec!["anthropic".to_string(), "claude".to_string()],
            env_key: "ANTHROPIC_API_KEY".to_string(),
            display_name: "Anthropic".to_string(),
            backend: "anthropic".to_string(),
            supports_prompt_caching: true,
            ..ProviderSpec::default()
        },
        ProviderSpec {
            name: "openai".to_string(),
            keywords: vec!["openai".to_string(), "gpt".to_string()],
            env_key: "OPENAI_API_KEY".to_string(),
            display_name: "OpenAI".to_string(),
            backend: "openai_compat".to_string(),
            supports_max_completion_tokens: true,
            ..ProviderSpec::default()
        },
        ProviderSpec {
            name: "openrouter".to_string(),
            keywords: vec!["openrouter".to_string(), "gpt".to_string()],
            env_key: "OPENROUTER_API_KEY".to_string(),
            display_name: "OpenRouter".to_string(),
            backend: "openai_compat".to_string(),
            supports_max_completion_tokens: true,
            is_gateway: true,
            detect_by_key_prefix: "sk-or-".to_string(),
            detect_by_base_keyword: "openrouter".to_string(),
            default_api_base: Some("https://openrouter.ai/api/v1".to_string()),
            supports_prompt_caching: true,
            ..ProviderSpec::default()
        },
        ProviderSpec {
            name: "gemini".to_string(),
            keywords: vec!["gemini".to_string()],
            env_key: "GEMINI_API_KEY".to_string(),
            display_name: "Gemini".to_string(),
            backend: "openai_compat".to_string(),
            default_api_base: Some(
                "https://generativelanguage.googleapis.com/v1beta/openai/".to_string(),
            ),
            ..ProviderSpec::default()
        },
    ]
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_find_by_name() {
        let spec = find_by_name("openai").unwrap();
        assert_eq!(spec.name, "openai");
        let spec1 = find_by_name("azure-openai").unwrap();
        assert_eq!(spec1.name, "azure_openai");
    }
}
