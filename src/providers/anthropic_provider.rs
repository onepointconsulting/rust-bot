use std::collections::HashMap;

use crate::providers::{
    base::{GenerationSettings, LLMProvider, LLMResponse},
    registry::ProviderSpec,
};
use rand::seq::IndexedRandom;
use serde_json::json;

const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn gen_tool_id() -> String {
    let mut rng = rand::rng();
    let suffix: String = (0..22)
        .map(|_| *ALNUM.choose(&mut rng).unwrap() as char)
        .collect();
    format!("toolu_{suffix}")
}

pub struct AnthropicProvider {
    api_key: Option<String>,
    api_base: Option<String>,
    default_model: Option<String>,
    extra_headers: Option<HashMap<String, String>>,
    spec: Option<ProviderSpec>,
    generation: GenerationSettings,
}

impl AnthropicProvider {
    const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

    fn strip_prefix(model: &str) -> String {
        model
            .strip_prefix("anthropic/")
            .unwrap_or(model)
            .to_string()
    }

    fn convert_tool_choice(
        tool_choice: Option<serde_json::Value>,
        thinking_enabled: bool,
    ) -> Option<serde_json::Value> {
        if thinking_enabled {
            return Some(json!({ "type": "auto" }));
        }

        let Some(tc) = tool_choice else {
            return Some(json!({ "type": "auto" }));
        };

        if tc.as_str() == Some("auto") {
            return Some(json!({ "type": "auto" }));
        }

        if tc.as_str() == Some("required") {
            return Some(json!({ "type": "any" }));
        }

        if tc.as_str() == Some("none") {
            return None;
        }

        if let Some(name) = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            return Some(json!({ "type": "tool", "name": name }));
        }

        Some(json!({ "type": "auto" }))
    }
}

impl LLMProvider for AnthropicProvider {
    fn new(
        api_key: Option<String>,
        api_base: Option<String>,
        default_model: Option<String>,
        extra_headers: Option<HashMap<String, String>>,
        spec: Option<ProviderSpec>,
    ) -> Self {
        Self {
            api_key,
            api_base,
            default_model: Some(default_model.unwrap_or_else(|| Self::DEFAULT_MODEL.to_string())),
            extra_headers,
            spec,
            generation: GenerationSettings::new(),
        }
    }

    fn api_key(&self) -> Option<String> {
        self.api_key.clone()
    }

    fn api_base(&self) -> Option<String> {
        self.api_base.clone()
    }

    fn extra_headers(&self) -> Option<HashMap<String, String>> {
        self.extra_headers.clone()
    }

    fn generation_settings(&self) -> &GenerationSettings {
        &self.generation
    }

    fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
        &mut self.generation
    }

    fn spec(&self) -> Option<&ProviderSpec> {
        self.spec.as_ref()
    }

    async fn chat(
        &self,
        _messages: Vec<serde_json::Value>,
        _tools: Option<Vec<serde_json::Value>>,
        _model: Option<String>,
        _max_tokens: usize,
        _temperature: f32,
        _reasoning_effort: Option<String>,
        _tool_choice: Option<serde_json::Value>,
    ) -> LLMResponse {
        LLMResponse {
            content: Some("AnthropicProvider chat() not implemented yet".to_string()),
            finish_reason: "error".to_string(),
            tool_calls: Vec::new(),
            usage: HashMap::new(),
            reasoning_content: None,
            thinking_blocks: None,
        }
    }

    fn get_default_model(&self) -> String {
        self.default_model
            .clone()
            .unwrap_or_else(|| Self::DEFAULT_MODEL.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_anthropic_prefix_test() {
        let result = AnthropicProvider::strip_prefix("anthropic/claude");
        println!("result: {}", result);
        assert_eq!(result, "claude");
    }

    #[test]
    fn keeps_original_when_no_prefix_test() {
        let result = AnthropicProvider::strip_prefix("claude");

        println!("result is: {}", result);
        assert_eq!(result, "claude");
    }

    #[test]

    fn empty_string_test() {
        let result = AnthropicProvider::strip_prefix("");
        println!("result is: {}", result);
        assert_eq!(result, "");
    }

    #[test]
    fn only_prefix_test() {
        let result = AnthropicProvider::strip_prefix("anthropic/");
        println!("result is: {}", result);
        assert_eq!(result, "");
    }

    #[test]
    fn returns_auto_when_thinking_enabled() {
        let result = AnthropicProvider::convert_tool_choice(
            Some(serde_json::json!({ "type": "auto" })),
            true,
        );

        println!(
            "result is: {}",
            serde_json::to_string_pretty(&result).unwrap()
        );
        assert_eq!(result, Some(serde_json::json!({ "type": "auto" })));
    }

    #[test]
    fn returns_auto_when_tool_choice_is_none() {
        let result = AnthropicProvider::convert_tool_choice(None, false);

        assert_eq!(result, Some(serde_json::json!({ "type": "auto" })));
    }

    #[test]
    fn converts_auto_string() {
        let result = AnthropicProvider::convert_tool_choice(Some(serde_json::json!("auto")), false);

        assert_eq!(result, Some(serde_json::json!({ "type": "auto" })));
    }

    #[test]

    fn converts_required_to_any() {
        let result =
            AnthropicProvider::convert_tool_choice(Some(serde_json::json!("required")), false);
        assert_eq!(result, Some(serde_json::json!({ "type": "any" })));
    }

    #[test]

    fn converts_none_string_to_none() {
        let result = AnthropicProvider::convert_tool_choice(Some(serde_json::json!("none")), false);
        assert_eq!(result, None);
    }

    #[test]
    fn converts_function_name_to_tool_type() {
        let input = serde_json::json!({
            "function": {
                "name": "search_web"
            }
        });

        let result = AnthropicProvider::convert_tool_choice(Some(input), false);

        assert_eq!(
            result,
            Some(serde_json::json!({
                "type": "tool",
                "name": "search_web"
            }))
        );
    }

    #[test]
    fn falls_back_to_auto_for_unknown_input() {
        let result = AnthropicProvider::convert_tool_choice(
            Some(serde_json::json!({ "type": "unknown" })),
            false,
        );
        assert_eq!(result, Some(serde_json::json!({ "type": "auto" })));
    }
}
