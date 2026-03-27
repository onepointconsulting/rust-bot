

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpec {
    // identity
    pub name: String,                   // config field name, e.g. "dashscope"
    pub keywords: Vec<String>,          // model-name keywords for matching (lowercase)
    pub env_key: String,                // env var for API key, e.g. "DASHSCOPE_API_KEY"
    pub display_name: String,           // shown in `nanobot status`

    // which provider implementation to use
    // "openai_compat" | "anthropic" | "azure_openai" | "openai_codex"
    pub backend: String,

    // extra env vars, e.g. vec![("ZHIPUAI_API_KEY", "{api_key}")]
    pub env_extras: Vec<(String, String)>,

    // gateway / local detection
    pub is_gateway: bool,                    // routes any model (OpenRouter, AiHubMix)
    pub is_local: bool,                      // local deployment (vLLM, Ollama)
    pub detect_by_key_prefix: String,        // match api_key prefix, e.g. "sk-or-"
    pub detect_by_base_keyword: String,      // match substring in api_base URL
    pub default_api_base: String,            // OpenAI-compatible base URL for this provider

    // gateway behavior
    pub strip_model_prefix: bool,            // strip "provider/" before sending to gateway

    // per-model param overrides, e.g. vec![("kimi-k2.5", HashMap)]
    // Using serde_json::Value for generic map
    pub model_overrides: Vec<(String, serde_json::Value)>,

    // OAuth-based providers (e.g., OpenAI Codex) don't use API keys
    pub is_oauth: bool,

    // Direct providers skip API-key validation (user supplies everything)
    pub is_direct: bool,

    // Provider supports cache_control on content blocks (e.g. Anthropic prompt caching)
    pub supports_prompt_caching: bool,
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
            default_api_base: String::new(),
            strip_model_prefix: false,
            model_overrides: Vec::new(),
            is_oauth: false,
            is_direct: false,
            supports_prompt_caching: false,
        }
    }
}