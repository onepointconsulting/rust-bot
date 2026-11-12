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

use crate::config::schema::ProviderConfig;
use crate::config::schema::{Config, ProviderBackend};
use crate::providers::anthropic_provider::AnthropicProvider;
use crate::providers::base::{LLMProvider, LLMProviderDyn};
use crate::providers::openai_compat_provider::OpenAICompatProvider;
use crate::providers::openai_responses_provider::OpenAIResponsesProvider;
use crate::providers::registry::{ProviderSpec, find_by_name};

/// Provider names [`create_provider_for`] accepts besides `"auto"`.
const BUILDABLE_PROVIDERS: [&str; 5] = ["openai", "custom", "openrouter", "together", "anthropic"];

/// The configured API key when not blank, else the provider's environment variable
/// (`spec.env_key`) when set and not blank. `spec` is `None` for a named custom
/// provider, which has no environment variable.
pub(crate) fn configured_or_env_api_key(
    provider_config: &ProviderConfig,
    spec: Option<&ProviderSpec>,
) -> Option<String> {
    let configured = provider_config.api_key.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    let env_key = &spec?.env_key;
    if env_key.is_empty() {
        return None;
    }
    std::env::var(env_key)
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// The names of the named custom providers (`providers.customProviders`), sorted.
pub fn custom_provider_names(config: &Config) -> Vec<String> {
    config.providers.custom_providers.keys().cloned().collect()
}

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

/// A provider name resolved to the settings its provider instance is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProvider {
    /// The concrete provider name (`"auto"` already resolved).
    pub name: String,
    pub api_key: Option<String>,
    /// The configured base URL; `None` when unset or blank.
    pub api_base: Option<String>,
    pub extra_headers: Option<HashMap<String, String>>,
    /// `providers.<name>.providerBackEnd`: the implementation the configuration asks
    /// for. `None` lets [`select_backend`] choose.
    pub configured_backend: Option<ProviderBackend>,
}

/// The concrete provider behind `provider_name` and its settings: `"auto"` resolved
/// via [`try_auto_provider_selection`], any other buildable name read from its own
/// `providers.*` slot. A blank `api_base` counts as unset.
pub fn resolve_provider_credentials(
    config: &Config,
    provider_name: &str,
) -> Result<ResolvedProvider, String> {
    let (name, api_key, api_base, extra_headers) = if provider_name == "auto" {
        try_auto_provider_selection(config)?
    } else {
        let is_buildable = BUILDABLE_PROVIDERS.contains(&provider_name)
            || config
                .providers
                .custom_providers
                .contains_key(provider_name);
        let provider_config = is_buildable
            .then(|| config.providers.get_by_name(provider_name))
            .flatten()
            .ok_or_else(|| format!("Invalid provider: {provider_name}"))?;
        let spec = find_by_name(provider_name);
        (
            provider_name.to_string(),
            Some(
                configured_or_env_api_key(provider_config, spec.as_ref())
                    .unwrap_or_else(|| provider_config.api_key.clone()),
            ),
            provider_config.api_base.clone(),
            provider_config.extra_headers.clone(),
        )
    };
    let configured_backend = config
        .providers
        .get_by_name(&name)
        .and_then(|provider_config| provider_config.provider_back_end);
    Ok(ResolvedProvider {
        name,
        api_key,
        api_base: api_base.filter(|base| !base.trim().is_empty()),
        extra_headers,
        configured_backend,
    })
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

/// The implementation that serves a resolved provider.
///
/// The configured `providerBackEnd` wins when set, e.g. OpenRouter through the
/// Responses API. Otherwise it follows from the provider name and base URL:
/// `anthropic` uses the Messages API, api.openai.com the Responses API (Chat
/// Completions there cannot combine tools with reasoning on the GPT-6 family), and
/// everything else Chat Completions.
pub(crate) fn select_backend(
    resolved_name: &str,
    api_base: Option<&str>,
    configured_backend: Option<ProviderBackend>,
) -> ProviderBackend {
    if let Some(backend) = configured_backend {
        backend
    } else if resolved_name == "anthropic" {
        ProviderBackend::Anthropic
    } else if targets_direct_openai(resolved_name, api_base) {
        ProviderBackend::OpenAiResponses
    } else {
        ProviderBackend::OpenAiCompat
    }
}

/// Construct the provider for a resolved provider (see [`select_backend`]).
fn build_provider(model: &str, resolved: ResolvedProvider) -> Arc<dyn LLMProviderDyn> {
    let backend = select_backend(
        &resolved.name,
        resolved.api_base.as_deref(),
        resolved.configured_backend,
    );
    let ResolvedProvider {
        name,
        api_key,
        api_base,
        extra_headers,
        ..
    } = resolved;
    // A named custom provider has no registry entry of its own: it is built like the
    // legacy `custom` slot (no prompt caching, no default base).
    let spec = find_by_name(&name).or_else(|| find_by_name("custom"));
    let model = Some(model.to_string());
    match backend {
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
        let resolved = resolve_provider_credentials(config, provider_name).unwrap();
        select_backend(
            &resolved.name,
            resolved.api_base.as_deref(),
            resolved.configured_backend,
        )
    }

    #[test]
    fn a_configured_backend_overrides_the_routing() {
        let mut config = Config::default();
        config.providers.openrouter.provider_back_end = Some(ProviderBackend::OpenAiResponses);
        assert_eq!(
            backend_for(&config, "openrouter"),
            ProviderBackend::OpenAiResponses
        );

        config.providers.custom.api_base = Some("https://api.z.ai/api/anthropic".to_string());
        config.providers.custom.provider_back_end = Some(ProviderBackend::Anthropic);
        assert_eq!(backend_for(&config, "custom"), ProviderBackend::Anthropic);

        config.providers.openai.provider_back_end = Some(ProviderBackend::OpenAiCompat);
        assert_eq!(
            backend_for(&config, "openai"),
            ProviderBackend::OpenAiCompat
        );
    }

    #[test]
    fn auto_uses_the_backend_configured_for_the_provider_it_picks() {
        let mut config = Config::default();
        config.providers.openrouter.api_key = "or-key".to_string();
        config.providers.openrouter.provider_back_end = Some(ProviderBackend::OpenAiResponses);
        assert_eq!(
            backend_for(&config, "auto"),
            ProviderBackend::OpenAiResponses
        );
    }

    #[test]
    fn providers_with_a_configured_backend_can_be_built() {
        let mut config = Config::default();
        config.providers.openrouter.api_key = "or-key".to_string();
        config.providers.custom.api_key = "c-key".to_string();
        for backend in [
            ProviderBackend::Anthropic,
            ProviderBackend::OpenAiCompat,
            ProviderBackend::OpenAiResponses,
        ] {
            config.providers.openrouter.provider_back_end = Some(backend);
            config.providers.custom.provider_back_end = Some(backend);
            for provider in ["openrouter", "custom"] {
                let built = create_provider_for(&config, "some/model", provider).unwrap();
                assert_eq!(
                    built.get_default_model(),
                    "some/model",
                    "{provider} {backend:?}"
                );
            }
        }
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
        config.providers.custom.provider_back_end = Some(ProviderBackend::OpenAiResponses);
        let resolved = resolve_provider_credentials(&config, "custom").unwrap();
        assert_eq!(resolved.name, "custom");
        assert_eq!(resolved.api_key.as_deref(), Some("custom-key"));
        assert_eq!(resolved.api_base.as_deref(), Some("https://gw.example/v1"));
        assert_eq!(
            resolved.configured_backend,
            Some(ProviderBackend::OpenAiResponses)
        );
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

    // ── named custom providers ────────────────────────────────────────────────

    fn config_with_relay(backend: Option<ProviderBackend>) -> Config {
        let mut config = Config::default();
        config.providers.custom_providers.insert(
            "my-relay".to_string(),
            ProviderConfig {
                api_key: "relay-key".to_string(),
                api_base: Some("https://gateway.example/v1".to_string()),
                provider_back_end: backend,
                ..ProviderConfig::default()
            },
        );
        config
    }

    #[test]
    fn a_custom_name_builds_with_each_backend() {
        for backend in ProviderBackend::ALL {
            let config = config_with_relay(Some(backend));
            assert_eq!(backend_for(&config, "my-relay"), backend);
            let provider = create_provider_for(&config, "some-model", "my-relay").unwrap();
            assert_eq!(provider.api_key(), Some("relay-key".to_string()));
            assert_eq!(
                provider.api_base(),
                Some("https://gateway.example/v1".to_string())
            );
            assert_eq!(provider.spec().map(|s| s.name.as_str()), Some("custom"));
        }
    }

    #[test]
    fn a_custom_name_without_a_backend_uses_chat_completions() {
        let config = config_with_relay(None);
        assert_eq!(
            backend_for(&config, "my-relay"),
            ProviderBackend::OpenAiCompat
        );
    }

    #[test]
    fn an_unknown_custom_name_is_refused() {
        let config = config_with_relay(None);
        assert!(create_provider_for(&config, "m", "other-relay").is_err());
        assert_eq!(custom_provider_names(&config), vec!["my-relay".to_string()]);
    }

    #[test]
    fn a_blank_key_falls_back_to_the_providers_environment_variable() {
        let mut spec = find_by_name("openrouter").unwrap();
        spec.env_key = "RUST_BOT_TEST_FACTORY_KEY_VAR".to_string();
        let blank = ProviderConfig::default();
        // SAFETY: the variable is unique to this test.
        unsafe { std::env::set_var("RUST_BOT_TEST_FACTORY_KEY_VAR", " env-key ") };
        assert_eq!(
            configured_or_env_api_key(&blank, Some(&spec)),
            Some("env-key".to_string())
        );
        let stored = ProviderConfig {
            api_key: "stored".to_string(),
            ..ProviderConfig::default()
        };
        assert_eq!(
            configured_or_env_api_key(&stored, Some(&spec)),
            Some("stored".to_string())
        );
        unsafe { std::env::remove_var("RUST_BOT_TEST_FACTORY_KEY_VAR") };
        assert_eq!(configured_or_env_api_key(&blank, Some(&spec)), None);
        assert_eq!(configured_or_env_api_key(&blank, None), None);
    }

    #[test]
    fn auto_ignores_custom_providers() {
        let config = config_with_relay(None);
        assert!(try_auto_provider_selection(&config).is_err());
    }
}
