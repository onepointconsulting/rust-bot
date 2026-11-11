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
use crate::providers::openai_responses_provider::OpenAIResponsesProvider;
use crate::providers::registry::find_by_name;

/// Provider names [`create_provider_for`] accepts besides `"auto"`.
const BUILDABLE_PROVIDERS: [&str; 5] = ["openai", "custom", "openrouter", "together", "anthropic"];

/// Resolved provider selection: provider name, api key, api base, extra headers.
pub type AutoProviderSelection = (
    String,
    Option<String>,
    Option<String>,
    Option<HashMap<String, String>>,
);

/// Resolve `"auto"` to a concrete provider name + credentials by picking the
/// first configured provider (by API key presence) in this fixed order:
/// openai, openrouter, custom, anthropic, together. Mirrors the CLI's own
/// `get_auto_provider` selection order exactly — do not reorder without
/// updating both.
pub fn try_auto_provider_selection(config: &Config) -> Result<AutoProviderSelection, String> {
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
    if !config.providers.together.api_key.is_empty() {
        return Ok((
            "together".to_string(),
            Some(config.providers.together.api_key.clone()),
            config.providers.together.api_base.clone(),
            config.providers.together.extra_headers.clone(),
        ));
    }
    Err(
        "Could not resolve auto provider, please set a provider (custom, openai, openrouter, anthropic, together) in the config"
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

/// The concrete provider name and credentials behind `provider_name`: `"auto"`
/// resolved via [`try_auto_provider_selection`], any other buildable name read from
/// its own `providers.*` slot. A blank `api_base` counts as unset.
pub fn resolve_provider_credentials(
    config: &Config,
    provider_name: &str,
) -> Result<AutoProviderSelection, String> {
    let (resolved_name, api_key, api_base, extra_headers) = if provider_name == "auto" {
        try_auto_provider_selection(config)?
    } else {
        let provider_config = BUILDABLE_PROVIDERS
            .contains(&provider_name)
            .then(|| config.providers.get_by_name(provider_name))
            .flatten()
            .ok_or_else(|| format!("Invalid provider: {provider_name}"))?;
        (
            provider_name.to_string(),
            Some(provider_config.api_key.clone()),
            provider_config.api_base.clone(),
            provider_config.extra_headers.clone(),
        )
    };
    let api_base = api_base.filter(|base| !base.trim().is_empty());
    Ok((resolved_name, api_key, api_base, extra_headers))
}

/// Which provider implementation serves a resolved provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderBackend {
    /// [`AnthropicProvider`]: the Messages API.
    Anthropic,
    /// [`OpenAIResponsesProvider`]: api.openai.com's Responses API.
    OpenAiResponses,
    /// [`OpenAICompatProvider`]: any other Chat Completions endpoint.
    OpenAiCompat,
}

/// Whether requests for `provider_name` with `api_base` end up at api.openai.com:
/// the base (else the registry default for the provider, else the Chat Completions
/// client's own fallback, which is api.openai.com) is `https://api.openai.com/v1`.
pub(crate) fn targets_direct_openai(provider_name: &str, api_base: Option<&str>) -> bool {
    let registry_default = find_by_name(provider_name).and_then(|spec| spec.default_api_base);
    let effective_base = api_base
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .or(registry_default.as_deref())
        .unwrap_or(OpenAICompatProvider::DEFAULT_API_BASE);
    effective_base
        .trim_end_matches('/')
        .eq_ignore_ascii_case(OpenAICompatProvider::DEFAULT_API_BASE)
}

/// The implementation for a resolved provider name and base URL. api.openai.com uses
/// the Responses API, because Chat Completions there cannot combine tools with
/// reasoning on the GPT-6 family.
pub(crate) fn select_backend(resolved_name: &str, api_base: Option<&str>) -> ProviderBackend {
    if resolved_name == "anthropic" {
        ProviderBackend::Anthropic
    } else if targets_direct_openai(resolved_name, api_base) {
        ProviderBackend::OpenAiResponses
    } else {
        ProviderBackend::OpenAiCompat
    }
}

/// Construct the provider for resolved credentials (see [`select_backend`]).
fn build_provider(model: &str, credentials: AutoProviderSelection) -> Arc<dyn LLMProviderDyn> {
    let (resolved_name, api_key, api_base, extra_headers) = credentials;
    let spec = find_by_name(&resolved_name);
    let model = Some(model.to_string());
    match select_backend(&resolved_name, api_base.as_deref()) {
        ProviderBackend::Anthropic => Arc::new(AnthropicProvider::new(
            api_key,
            api_base,
            model,
            extra_headers,
            spec,
        )),
        ProviderBackend::OpenAiResponses => Arc::new(OpenAIResponsesProvider::new(
            api_key,
            api_base,
            model,
            extra_headers,
            spec,
        )),
        ProviderBackend::OpenAiCompat => Arc::new(OpenAICompatProvider::new(
            api_key,
            api_base,
            model,
            extra_headers,
            spec,
        )),
    }
}

/// Build a provider instance for an explicit `model`/`provider_name` pair.
///
/// The parameterized core of the CLI's `create_provider`: it does not read
/// `config.agents.model`/`.provider` directly, so a `ModelPresetConfig` can be
/// resolved the same way.
pub fn create_provider_for(
    config: &Config,
    model: &str,
    provider_name: &str,
) -> Result<Arc<dyn LLMProviderDyn>, String> {
    let credentials = resolve_provider_credentials(config, provider_name)?;
    Ok(build_provider(model, credentials))
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
    fn test_auto_provider_selection_falls_back_to_together() {
        let mut config = Config::default();
        config.providers.together.api_key = "tk".to_string();
        let (name, key, _, _) = try_auto_provider_selection(&config).unwrap();
        assert_eq!(name, "together");
        assert_eq!(key, Some("tk".to_string()));
    }

    #[test]
    fn test_explicit_together_uses_together_credentials_even_when_others_are_set() {
        let mut config = Config::default();
        config.providers.openai.api_key = "openai-key".to_string();
        config.providers.openrouter.api_key = "openrouter-key".to_string();
        config.providers.anthropic.api_key = "anthropic-key".to_string();
        config.providers.together.api_key = "together-key".to_string();
        config.providers.together.api_base = Some("https://together.example/v1".to_string());

        let provider = create_provider_for(
            &config,
            "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            "together",
        )
        .unwrap();
        assert_eq!(provider.api_key(), Some("together-key".to_string()));
        assert_eq!(
            provider.api_base(),
            Some("https://together.example/v1".to_string())
        );
        assert_eq!(provider.spec().map(|s| s.name.as_str()), Some("together"));
    }

    #[test]
    fn test_explicit_together_defaults_to_the_together_api_base() {
        let mut config = Config::default();
        config.providers.together.api_key = "together-key".to_string();
        let provider = create_provider_for(&config, "any/model", "together").unwrap();
        assert_eq!(
            provider.api_base(),
            Some(crate::providers::registry::TOGETHER_API_BASE.to_string())
        );
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

    /// The backend `provider_name` gets under `config`, as `create_provider_for` picks it.
    fn backend_for(config: &Config, provider_name: &str) -> ProviderBackend {
        let (name, _, api_base, _) = resolve_provider_credentials(config, provider_name).unwrap();
        select_backend(&name, api_base.as_deref())
    }

    #[test]
    fn openai_with_its_default_base_uses_the_responses_api() {
        let mut config = Config::default();
        config.providers.openai.api_key = "k".to_string();
        assert_eq!(
            backend_for(&config, "openai"),
            ProviderBackend::OpenAiResponses
        );
        for base in [
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            "  ",
        ] {
            config.providers.openai.api_base = Some(base.to_string());
            assert_eq!(
                backend_for(&config, "openai"),
                ProviderBackend::OpenAiResponses,
                "{base:?}"
            );
        }
    }

    #[test]
    fn openai_pointed_elsewhere_stays_on_chat_completions() {
        let mut config = Config::default();
        config.providers.openai.api_base = Some("https://proxy.example/v1".to_string());
        assert_eq!(
            backend_for(&config, "openai"),
            ProviderBackend::OpenAiCompat
        );
    }

    #[test]
    fn custom_pointed_at_api_openai_com_uses_the_responses_api() {
        let mut config = Config::default();
        config.providers.custom.api_base = Some("https://api.openai.com/v1".to_string());
        assert_eq!(
            backend_for(&config, "custom"),
            ProviderBackend::OpenAiResponses
        );
        config.providers.custom.api_base = Some("http://localhost:11434/v1".to_string());
        assert_eq!(
            backend_for(&config, "custom"),
            ProviderBackend::OpenAiCompat
        );
    }

    #[test]
    fn gateways_stay_on_chat_completions() {
        let config = Config::default();
        assert_eq!(
            backend_for(&config, "openrouter"),
            ProviderBackend::OpenAiCompat
        );
        assert_eq!(
            backend_for(&config, "together"),
            ProviderBackend::OpenAiCompat
        );
        assert_eq!(
            backend_for(&config, "anthropic"),
            ProviderBackend::Anthropic
        );
    }

    #[test]
    fn auto_resolving_to_openai_uses_the_responses_api() {
        let mut config = Config::default();
        config.providers.openai.api_key = "k".to_string();
        config.providers.anthropic.api_key = "k2".to_string();
        assert_eq!(
            backend_for(&config, "auto"),
            ProviderBackend::OpenAiResponses
        );
        config.providers.openai.api_key.clear();
        assert_eq!(backend_for(&config, "auto"), ProviderBackend::Anthropic);
    }

    #[test]
    fn credentials_come_from_the_named_slot() {
        let mut config = Config::default();
        config.providers.custom.api_key = "custom-key".to_string();
        config.providers.custom.api_base = Some("https://gw.example/v1".to_string());
        let (name, key, base, _) = resolve_provider_credentials(&config, "custom").unwrap();
        assert_eq!(name, "custom");
        assert_eq!(key.as_deref(), Some("custom-key"));
        assert_eq!(base.as_deref(), Some("https://gw.example/v1"));
        // Registry providers the factory does not build are rejected.
        assert!(resolve_provider_credentials(&config, "gemini").is_err());
    }

    #[test]
    fn the_built_openai_provider_keeps_model_spec_and_key() {
        let mut config = Config::default();
        config.providers.openai.api_key = "k".to_string();
        let provider = create_provider_for(&config, "gpt-6-astra", "openai").unwrap();
        assert_eq!(provider.get_default_model(), "gpt-6-astra");
        assert_eq!(provider.spec().map(|s| s.name.as_str()), Some("openai"));
        assert_eq!(provider.api_key().as_deref(), Some("k"));
    }
}
