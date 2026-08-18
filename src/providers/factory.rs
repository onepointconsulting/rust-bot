//! Shared provider-construction logic, parameterized by an explicit
//! `model`/`provider_name` pair instead of reading `config.agents.*`
//! directly, so it can be reused both for the process-wide startup provider
//! (`cli::commands::create_provider`) and for arbitrary named model presets
//! (`agent::model_runtime::ModelRuntimeResolver`).
//!
//! Unlike the CLI's own error handling, functions here never print to
//! stderr or exit the process — building a provider for a bad preset must
//! be recoverable (reject the `/model` switch), not fatal to the whole run.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::schema::Config;
use crate::providers::anthropic_provider::AnthropicProvider;
use crate::providers::base::{LLMProvider, LLMProviderDyn};
use crate::providers::openai_compat_provider::OpenAICompatProvider;
use crate::providers::registry::find_by_name;

/// Resolve `"auto"` to a concrete provider name + credentials by picking the
/// first configured provider (by API key presence) in this fixed order:
/// openai, openrouter, custom, anthropic. Mirrors the CLI's own
/// `get_auto_provider` selection order exactly — do not reorder without
/// updating both.
pub fn try_auto_provider_selection(
    config: &Config,
) -> Result<
    (
        String,
        Option<String>,
        Option<String>,
        Option<HashMap<String, String>>,
    ),
    String,
> {
    if !config.providers.openai.api_key.is_empty() {
        return Ok((
            "openai".to_string(),
            Some(config.providers.openai.api_key.clone()),
            config.providers.openai.api_base.clone(),
            config.providers.openai.extra_headers.clone(),
        ));
    }
    if !config.providers.openrouter.api_key.is_empty() {
        return Ok((
            "openrouter".to_string(),
            Some(config.providers.openrouter.api_key.clone()),
            config.providers.openrouter.api_base.clone(),
            config.providers.openrouter.extra_headers.clone(),
        ));
    }
    if !config.providers.custom.api_key.is_empty() {
        return Ok((
            "custom".to_string(),
            Some(config.providers.custom.api_key.clone()),
            config.providers.custom.api_base.clone(),
            config.providers.custom.extra_headers.clone(),
        ));
    }
    if !config.providers.anthropic.api_key.is_empty() {
        return Ok((
            "anthropic".to_string(),
            Some(config.providers.anthropic.api_key.clone()),
            config.providers.anthropic.api_base.clone(),
            config.providers.anthropic.extra_headers.clone(),
        ));
    }
    Err(
        "Could not resolve auto provider, please set a provider (custom, openai, openrouter, anthropic) in the config"
            .to_string(),
    )
}

/// Resolve `provider_name` to the concrete provider name that would actually
/// back requests — i.e. `"auto"` resolved via [`try_auto_provider_selection`],
/// anything else returned unchanged. Used as a cache key by
/// `ModelRuntimeResolver`; does not construct a provider instance.
pub fn resolve_concrete_provider_name(
    config: &Config,
    provider_name: &str,
) -> Result<String, String> {
    if provider_name == "auto" {
        Ok(try_auto_provider_selection(config)?.0)
    } else {
        Ok(provider_name.to_string())
    }
}

/// Build a provider instance for an explicit `model`/`provider_name` pair.
///
/// This is the parameterized core of the CLI's `create_provider` — the exact
/// same branch logic, just not reading `config.agents.model`/`.provider`
/// directly, so a `ModelPresetConfig` can be resolved the same way.
pub fn create_provider_for(
    config: &Config,
    model: &str,
    provider_name: &str,
) -> Result<Arc<dyn LLMProviderDyn>, String> {
    match provider_name {
        "openai" | "custom" | "openrouter" | "auto" => {
            let (api_key, api_base, extra_headers) = if provider_name == "openai" {
                (
                    Some(config.providers.openai.api_key.clone()),
                    config.providers.openai.api_base.clone(),
                    config.providers.openai.extra_headers.clone(),
                )
            } else if provider_name == "custom" {
                (
                    Some(config.providers.custom.api_key.clone()),
                    config.providers.custom.api_base.clone(),
                    config.providers.custom.extra_headers.clone(),
                )
            } else if provider_name == "openrouter" {
                (
                    Some(config.providers.openrouter.api_key.clone()),
                    config.providers.openrouter.api_base.clone(),
                    config.providers.openrouter.extra_headers.clone(),
                )
            } else {
                // "auto"
                let (resolved_provider_name, api_key, api_base, extra_headers) =
                    try_auto_provider_selection(config)?;
                if resolved_provider_name == "anthropic" {
                    return Ok(Arc::new(AnthropicProvider::new(
                        api_key,
                        api_base,
                        Some(model.to_string()),
                        extra_headers,
                        find_by_name(&resolved_provider_name),
                    )));
                }
                return Ok(Arc::new(OpenAICompatProvider::new(
                    api_key,
                    api_base,
                    Some(model.to_string()),
                    extra_headers,
                    find_by_name(&resolved_provider_name),
                )));
            };
            Ok(Arc::new(OpenAICompatProvider::new(
                api_key,
                api_base,
                Some(model.to_string()),
                extra_headers,
                find_by_name(provider_name),
            )))
        }
        "anthropic" => Ok(Arc::new(AnthropicProvider::new(
            Some(config.providers.anthropic.api_key.clone()),
            config.providers.anthropic.api_base.clone(),
            Some(model.to_string()),
            config.providers.anthropic.extra_headers.clone(),
            find_by_name("anthropic"),
        ))),
        other => Err(format!("Invalid provider: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_provider_selection_prefers_openai() {
        let mut config = Config::default();
        config.providers.openai.api_key = "k1".to_string();
        config.providers.anthropic.api_key = "k2".to_string();
        let (name, key, _, _) = try_auto_provider_selection(&config).unwrap();
        assert_eq!(name, "openai");
        assert_eq!(key, Some("k1".to_string()));
    }

    #[test]
    fn test_auto_provider_selection_falls_back_to_anthropic() {
        let mut config = Config::default();
        config.providers.anthropic.api_key = "k2".to_string();
        let (name, key, _, _) = try_auto_provider_selection(&config).unwrap();
        assert_eq!(name, "anthropic");
        assert_eq!(key, Some("k2".to_string()));
    }

    #[test]
    fn test_auto_provider_selection_none_configured_errs() {
        let config = Config::default();
        assert!(try_auto_provider_selection(&config).is_err());
    }

    #[test]
    fn test_resolve_concrete_provider_name_passthrough() {
        let config = Config::default();
        assert_eq!(
            resolve_concrete_provider_name(&config, "anthropic").unwrap(),
            "anthropic"
        );
    }

    #[test]
    fn test_resolve_concrete_provider_name_auto() {
        let mut config = Config::default();
        config.providers.anthropic.api_key = "k2".to_string();
        assert_eq!(
            resolve_concrete_provider_name(&config, "auto").unwrap(),
            "anthropic"
        );
    }

    #[test]
    fn test_create_provider_for_invalid_provider_errs() {
        let config = Config::default();
        assert!(create_provider_for(&config, "some-model", "not-a-real-provider").is_err());
    }

    #[test]
    fn test_create_provider_for_anthropic() {
        let mut config = Config::default();
        config.providers.anthropic.api_key = "k2".to_string();
        assert!(create_provider_for(&config, "claude-opus-4-6", "anthropic").is_ok());
    }

    #[test]
    fn test_create_provider_for_auto_with_no_credentials_errs() {
        let config = Config::default();
        assert!(create_provider_for(&config, "some-model", "auto").is_err());
    }
}
