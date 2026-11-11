//! Listing the models a configured provider offers ("model catalog").
//!
//! Every provider `create_provider_for` can build speaks one of two listing protocols,
//! both answering with a `data[].id` envelope:
//!
//! * **OpenAI-compatible** (`openai`, `openrouter`, `together`, `gemini`, `custom`):
//!   `GET {base}/models` with `Authorization: Bearer`. Together answers with a bare JSON
//!   array; Gemini's ids carry a `models/` prefix.
//! * **Anthropic**: `GET {base}/v1/models` with `x-api-key` + `anthropic-version`,
//!   paginated with `limit` / `after_id` and `has_more` / `last_id`.
//!
//! Azure OpenAI presets name *deployments*, which only the Azure management plane can
//! list, so it is [`ModelListingProtocol::Unsupported`] and keeps free-text entry.
//!
//! Listing is a free function over the provider *configuration* rather than an
//! `LLMProvider` method: it needs only credentials and a base URL, and must work before
//! any preset (and so any model) exists. Like `providers::factory`, nothing here prints,
//! exits or panics — every failure is returned as a message for the user.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use adk_anthropic::Anthropic;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::loader::resolve_env_vars_in;
use crate::config::schema::{Config, ProviderConfig};
use crate::providers::anthropic_provider::AnthropicProvider;
use crate::providers::factory::resolve_concrete_provider_name;
use crate::providers::openai_compat_provider::{OpenAICompatProvider, body_preview};
use crate::providers::registry::{ProviderSpec, find_by_name};

/// How long one listing request may take before it is abandoned.
const MODEL_LISTING_TIMEOUT: Duration = Duration::from_secs(15);

/// Page size asked of the Anthropic API (its maximum).
const ANTHROPIC_PAGE_LIMIT: u32 = 1000;

/// Upper bound on Anthropic pages followed, guarding against a server that keeps
/// answering `has_more: true`.
const MAX_ANTHROPIC_PAGES: usize = 20;

/// The `type` Anthropic gives every entry; carries no information about the model.
const GENERIC_MODEL_TYPE: &str = "model";

/// Prefix Gemini's OpenAI-compatible endpoint puts on model ids; chat requests take
/// the bare id.
const GEMINI_MODEL_ID_PREFIX: &str = "models/";

/// One model a provider offers, as shown when picking a preset's `model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelInfo {
    /// The id to put in a preset's `model` field.
    pub id: String,
    /// Human-readable name, when the provider reports one (`display_name` or `name`).
    pub display_name: Option<String>,
    /// Context window in tokens, when the provider reports one.
    pub context_length: Option<u64>,
    /// What kind of model this is (`chat`, `embedding`, `image`, ...), when the provider
    /// says. Never filtered on here; callers may.
    pub model_type: Option<String>,
}

/// How a provider's models are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelListingProtocol {
    /// `GET {base}/models` with a Bearer token.
    OpenAiCompatible,
    /// `GET {base}/v1/models` with `x-api-key`, paginated.
    Anthropic,
    /// No usable listing endpoint (Azure deployments, OAuth backends).
    Unsupported,
}

/// One parsed page of a listing response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelListingPage {
    /// The models on this page, in response order.
    pub models: Vec<ProviderModelInfo>,
    /// Whether the provider has more pages (Anthropic only).
    pub has_more: bool,
    /// The id to continue after (Anthropic's `last_id`).
    pub last_id: Option<String>,
}

/// Everything needed to send the listing request(s) for one provider.
#[derive(Debug, Clone)]
pub struct ModelListingRequest {
    /// The registry name of the provider being listed (`"auto"` already resolved).
    pub provider_name: String,
    /// Which listing protocol the provider speaks; never [`ModelListingProtocol::Unsupported`].
    pub protocol: ModelListingProtocol,
    /// The models endpoint, without query string.
    pub endpoint_url: String,
    /// Authentication and extra headers sent with every page request.
    pub headers: HeaderMap,
}

/// The listing protocol of a registry provider, keyed on its `backend`.
pub fn listing_protocol_for(spec: &ProviderSpec) -> ModelListingProtocol {
    match spec.backend.as_str() {
        // The Responses API shares `GET /models` with Chat Completions.
        "openai_compat" | "openai_responses" => ModelListingProtocol::OpenAiCompatible,
        "anthropic" => ModelListingProtocol::Anthropic,
        _ => ModelListingProtocol::Unsupported,
    }
}

/// The base URL a provider's own client falls back to when neither the configuration
/// nor the registry names one.
fn protocol_default_api_base(protocol: ModelListingProtocol) -> Option<&'static str> {
    match protocol {
        ModelListingProtocol::OpenAiCompatible => Some(OpenAICompatProvider::DEFAULT_API_BASE),
        ModelListingProtocol::Anthropic => Some(AnthropicProvider::DEFAULT_API_BASE),
        ModelListingProtocol::Unsupported => None,
    }
}

/// The base URL requests go to: the configured `api_base` (when not blank), else the
/// registry's `default_api_base`, else the protocol's default.
fn effective_api_base(
    provider_config: &ProviderConfig,
    spec: &ProviderSpec,
    protocol: ModelListingProtocol,
) -> Option<String> {
    let non_blank = |base: &Option<String>| base.clone().filter(|b| !b.trim().is_empty());
    non_blank(&provider_config.api_base)
        .or_else(|| non_blank(&spec.default_api_base))
        .or_else(|| protocol_default_api_base(protocol).map(str::to_string))
}

/// The models endpoint for `api_base`: `{base}/models` for OpenAI-compatible APIs and
/// `{base}/v1/models` for Anthropic (whose configured base has no `/v1`), trailing
/// slashes trimmed. `None` for [`ModelListingProtocol::Unsupported`].
pub fn models_endpoint_url(protocol: ModelListingProtocol, api_base: &str) -> Option<String> {
    let base = api_base.trim().trim_end_matches('/');
    match protocol {
        ModelListingProtocol::OpenAiCompatible => Some(format!("{base}/models")),
        ModelListingProtocol::Anthropic => Some(format!("{base}/v1/models")),
        ModelListingProtocol::Unsupported => None,
    }
}

/// The first non-empty string among `keys` of `entry`.
fn first_string(entry: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| entry.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

/// The first non-negative integer among `keys` of `entry`.
fn first_unsigned(entry: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| entry.get(*key).and_then(Value::as_u64))
}

/// One listing entry as a [`ProviderModelInfo`]; `None` when it has no usable id. A bare
/// string entry is taken as the id.
fn parse_model_entry(entry: &Value) -> Option<ProviderModelInfo> {
    if let Some(id) = entry.as_str().map(str::trim).filter(|id| !id.is_empty()) {
        return Some(ProviderModelInfo {
            id: id.to_string(),
            display_name: None,
            context_length: None,
            model_type: None,
        });
    }
    let fields = entry.as_object()?;
    let id = first_string(fields, &["id"])?;
    Some(ProviderModelInfo {
        display_name: first_string(fields, &["display_name", "name"]).filter(|name| *name != id),
        id,
        context_length: first_unsigned(fields, &["context_length", "context_window"]),
        model_type: first_string(fields, &["type"]).filter(|kind| kind != GENERIC_MODEL_TYPE),
    })
}

/// The error message an API put in an error body (`{error: {message}}`,
/// `{error: "..."}` or `{message}`), if any.
fn api_error_message(body: &Value) -> Option<String> {
    let error = body.get("error");
    error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .or_else(|| error.and_then(Value::as_str))
        .or_else(|| body.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .filter(|message| !message.is_empty())
}

/// Parse one listing response body. Accepts `{data: [...]}` (OpenAI, OpenRouter, Gemini,
/// Anthropic) and a bare array (Together); entries without an id are skipped. Reads
/// Anthropic's `has_more` / `last_id`. Errs when the body holds no model list.
pub fn parse_model_listing(body: &Value) -> Result<ModelListingPage, String> {
    let entries = match body {
        Value::Array(entries) => entries,
        Value::Object(fields) => match fields.get("data") {
            Some(Value::Array(entries)) => entries,
            _ => {
                return Err(match api_error_message(body) {
                    Some(message) => format!("the provider answered with an error: {message}"),
                    None => "unexpected response: no `data` array of models".to_string(),
                });
            }
        },
        _ => return Err("unexpected response: expected a JSON object or array".to_string()),
    };
    Ok(ModelListingPage {
        models: entries.iter().filter_map(parse_model_entry).collect(),
        has_more: body
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        last_id: body
            .get("last_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
    })
}

/// The configured API key when not blank, else the provider's environment variable
/// (`spec.env_key`) when set and not blank.
fn configured_or_env_api_key(
    provider_config: &ProviderConfig,
    spec: &ProviderSpec,
) -> Option<String> {
    let configured = provider_config.api_key.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    if spec.env_key.is_empty() {
        return None;
    }
    std::env::var(&spec.env_key)
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// The headers for listing `spec`'s models — the same ones its chat client sends, minus
/// per-request routing hints. Anthropic requires a key (resolved like its chat client
/// does, `file://` indirection included); OpenAI-compatible servers are asked without
/// `Authorization` when there is no key (local Ollama, LM Studio, ...).
fn listing_headers(
    protocol: ModelListingProtocol,
    spec: &ProviderSpec,
    api_key: Option<String>,
    api_base: &str,
    extra_headers: &HashMap<String, String>,
) -> Result<HeaderMap, String> {
    match protocol {
        ModelListingProtocol::OpenAiCompatible => Ok(OpenAICompatProvider::build_request_headers(
            api_key.as_deref(),
            Some(spec),
            Some(api_base),
            extra_headers,
        )),
        ModelListingProtocol::Anthropic => {
            let resolved_key = Anthropic::new(api_key)
                .map_err(|e| format!("No usable API key for provider '{}': {e}", spec.name))?
                .api_key()
                .to_string();
            AnthropicProvider::try_build_request_headers(&resolved_key, extra_headers)
        }
        ModelListingProtocol::Unsupported => Err(unsupported_message(spec)),
    }
}

/// Why `spec`'s models cannot be listed, for the user.
fn unsupported_message(spec: &ProviderSpec) -> String {
    format!(
        "Provider '{}' does not offer a model list; enter the model (or deployment) name by hand.",
        spec.name
    )
}

/// Work out where and how to list the models of `provider_name` (`"auto"` resolved to the
/// provider it would pick) from `config`, without sending anything. `${VAR}` placeholders
/// in that provider's settings are resolved, so `config` may come straight from the file.
pub fn build_model_listing_request(
    config: &Config,
    provider_name: &str,
) -> Result<ModelListingRequest, String> {
    let concrete_name = resolve_concrete_provider_name(config, provider_name)?;
    let spec = find_by_name(&concrete_name)
        .ok_or_else(|| format!("Unknown provider '{concrete_name}'."))?;
    let configured_slot = config
        .providers
        .get_by_name(&spec.name)
        .ok_or_else(|| format!("Provider '{}' has no configuration slot.", spec.name))?;
    let provider_config = &resolve_env_vars_in(configured_slot)
        .map_err(|e| format!("Settings of provider '{}': {e}", spec.name))?;
    let protocol = listing_protocol_for(&spec);
    let api_base = effective_api_base(provider_config, &spec, protocol)
        .ok_or_else(|| unsupported_message(&spec))?;
    let endpoint_url =
        models_endpoint_url(protocol, &api_base).ok_or_else(|| unsupported_message(&spec))?;
    let headers = listing_headers(
        protocol,
        &spec,
        configured_or_env_api_key(provider_config, &spec),
        &api_base,
        &provider_config.extra_headers.clone().unwrap_or_default(),
    )?;
    Ok(ModelListingRequest {
        provider_name: spec.name,
        protocol,
        endpoint_url,
        headers,
    })
}

/// GET `url` and parse the body as one listing page. Errs with the URL and cause on a
/// transport failure, with the HTTP status and a body excerpt on a non-2xx answer.
async fn fetch_listing_page(
    client: &reqwest::Client,
    url: &str,
) -> Result<ModelListingPage, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request to {url} failed: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("failed to read the response body (HTTP {status}) from {url}: {e}"))?;
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|json| api_error_message(&json))
            .unwrap_or_else(|| body_preview(&body));
        return Err(format!("HTTP {status} from {url}: {detail}"));
    }
    let json: Value = serde_json::from_str(&body).map_err(|e| {
        format!(
            "invalid JSON response (HTTP {status}) from {url}: {e}; body starts: {}",
            body_preview(&body)
        )
    })?;
    parse_model_listing(&json).map_err(|message| format!("{message} (from {url})"))
}

/// The URL of the Anthropic page after `after_id` (the first page when `None`).
fn anthropic_page_url(endpoint_url: &str, after_id: Option<&str>) -> String {
    match after_id {
        Some(after_id) => format!(
            "{endpoint_url}?limit={ANTHROPIC_PAGE_LIMIT}&after_id={}",
            url_query_escape(after_id)
        ),
        None => format!("{endpoint_url}?limit={ANTHROPIC_PAGE_LIMIT}"),
    }
}

/// Percent-encode everything outside the URL-safe unreserved set.
fn url_query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Send `request` and collect every model it lists (all Anthropic pages followed).
pub async fn fetch_model_listing(
    request: &ModelListingRequest,
) -> Result<Vec<ProviderModelInfo>, String> {
    let client = reqwest::Client::builder()
        .timeout(MODEL_LISTING_TIMEOUT)
        .default_headers(request.headers.clone())
        .build()
        .map_err(|e| format!("could not build the HTTP client: {e}"))?;

    if request.protocol != ModelListingProtocol::Anthropic {
        return Ok(fetch_listing_page(&client, &request.endpoint_url)
            .await?
            .models);
    }

    let mut models = Vec::new();
    let mut after_id: Option<String> = None;
    for _ in 0..MAX_ANTHROPIC_PAGES {
        let page_url = anthropic_page_url(&request.endpoint_url, after_id.as_deref());
        let page = fetch_listing_page(&client, &page_url).await?;
        models.extend(page.models);
        match (page.has_more, page.last_id) {
            (true, Some(last_id)) => after_id = Some(last_id),
            _ => return Ok(models),
        }
    }
    log::warn!(
        "Stopped listing models of '{}' after {MAX_ANTHROPIC_PAGES} pages",
        request.provider_name
    );
    Ok(models)
}

/// The id a preset should use for a listed model of `provider_name`: Gemini's
/// `models/` prefix dropped, everything else unchanged.
fn normalize_model_id(provider_name: &str, id: &str) -> String {
    if provider_name == "gemini" {
        id.strip_prefix(GEMINI_MODEL_ID_PREFIX)
            .unwrap_or(id)
            .to_string()
    } else {
        id.to_string()
    }
}

/// Normalize ids, drop duplicates (the first entry of an id wins) and sort by id.
fn normalize_model_list(
    provider_name: &str,
    models: Vec<ProviderModelInfo>,
) -> Vec<ProviderModelInfo> {
    let mut by_id: BTreeMap<String, ProviderModelInfo> = BTreeMap::new();
    for mut model in models {
        model.id = normalize_model_id(provider_name, &model.id);
        by_id.entry(model.id.clone()).or_insert(model);
    }
    by_id.into_values().collect()
}

/// List the models `provider_name` offers with the credentials and base URL of `config`
/// (`"auto"` lists the provider it would pick), sorted by id without duplicates.
///
/// Errs — never panics — for an unknown or unlistable provider, a missing Anthropic key,
/// a transport failure, a non-2xx answer (status and body excerpt included) or a body
/// without a model list.
pub async fn list_provider_models(
    config: &Config,
    provider_name: &str,
) -> Result<Vec<ProviderModelInfo>, String> {
    let request = build_model_listing_request(config, provider_name)?;
    let models = fetch_model_listing(&request).await?;
    Ok(normalize_model_list(&request.provider_name, models))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn spec(name: &str) -> ProviderSpec {
        find_by_name(name).unwrap_or_else(|| panic!("provider '{name}' is registered"))
    }

    fn ids(models: &[ProviderModelInfo]) -> Vec<&str> {
        models.iter().map(|model| model.id.as_str()).collect()
    }

    // ── listing_protocol_for ─────────────────────────────────────────────────

    #[test]
    fn listing_protocol_follows_the_registry_backend() {
        for name in ["openai", "openrouter", "together", "gemini", "custom"] {
            assert_eq!(
                listing_protocol_for(&spec(name)),
                ModelListingProtocol::OpenAiCompatible,
                "{name}"
            );
        }
        assert_eq!(
            listing_protocol_for(&spec("anthropic")),
            ModelListingProtocol::Anthropic
        );
        assert_eq!(
            listing_protocol_for(&spec("azure_openai")),
            ModelListingProtocol::Unsupported
        );
    }

    #[test]
    fn every_registry_provider_has_a_listing_protocol() {
        for provider in crate::providers::registry::providers() {
            let expected_unsupported = provider.backend == "azure_openai";
            assert_eq!(
                listing_protocol_for(&provider) == ModelListingProtocol::Unsupported,
                expected_unsupported,
                "{}",
                provider.name
            );
        }
    }

    #[test]
    fn unknown_backends_are_unsupported() {
        let oauth = ProviderSpec {
            backend: "openai_codex".to_string(),
            ..ProviderSpec::default()
        };
        assert_eq!(
            listing_protocol_for(&oauth),
            ModelListingProtocol::Unsupported
        );
    }

    // ── models_endpoint_url ──────────────────────────────────────────────────

    #[test]
    fn openai_compatible_endpoint_appends_models() {
        assert_eq!(
            models_endpoint_url(
                ModelListingProtocol::OpenAiCompatible,
                "https://api.openai.com/v1"
            ),
            Some("https://api.openai.com/v1/models".to_string())
        );
    }

    #[test]
    fn endpoint_trims_trailing_slashes() {
        assert_eq!(
            models_endpoint_url(
                ModelListingProtocol::OpenAiCompatible,
                "https://generativelanguage.googleapis.com/v1beta/openai/"
            ),
            Some("https://generativelanguage.googleapis.com/v1beta/openai/models".to_string())
        );
        assert_eq!(
            models_endpoint_url(
                ModelListingProtocol::Anthropic,
                "https://api.anthropic.com//"
            ),
            Some("https://api.anthropic.com/v1/models".to_string())
        );
    }

    #[test]
    fn anthropic_endpoint_appends_v1_models() {
        assert_eq!(
            models_endpoint_url(ModelListingProtocol::Anthropic, "https://api.anthropic.com"),
            Some("https://api.anthropic.com/v1/models".to_string())
        );
    }

    #[test]
    fn unsupported_protocol_has_no_endpoint() {
        assert_eq!(
            models_endpoint_url(ModelListingProtocol::Unsupported, "https://x.example"),
            None
        );
    }

    #[test]
    fn default_bases_come_from_the_registry_then_the_provider() {
        let blank = ProviderConfig::default();
        let together = spec("together");
        assert_eq!(
            effective_api_base(&blank, &together, listing_protocol_for(&together)).as_deref(),
            Some(crate::providers::registry::TOGETHER_API_BASE)
        );
        let openai = spec("openai");
        assert_eq!(
            effective_api_base(&blank, &openai, listing_protocol_for(&openai)).as_deref(),
            Some(OpenAICompatProvider::DEFAULT_API_BASE)
        );
        let anthropic = spec("anthropic");
        assert_eq!(
            effective_api_base(&blank, &anthropic, listing_protocol_for(&anthropic)).as_deref(),
            Some(AnthropicProvider::DEFAULT_API_BASE)
        );
    }

    #[test]
    fn configured_base_wins_unless_blank() {
        let openai = spec("openai");
        let configured = ProviderConfig {
            api_base: Some("http://localhost:11434/v1".to_string()),
            ..ProviderConfig::default()
        };
        assert_eq!(
            effective_api_base(&configured, &openai, ModelListingProtocol::OpenAiCompatible)
                .as_deref(),
            Some("http://localhost:11434/v1")
        );
        let blank_base = ProviderConfig {
            api_base: Some("  ".to_string()),
            ..ProviderConfig::default()
        };
        assert_eq!(
            effective_api_base(&blank_base, &openai, ModelListingProtocol::OpenAiCompatible)
                .as_deref(),
            Some(OpenAICompatProvider::DEFAULT_API_BASE)
        );
    }

    // ── parse_model_listing ──────────────────────────────────────────────────

    #[test]
    fn parses_an_openai_listing() {
        let page = parse_model_listing(&json!({
            "object": "list",
            "data": [
                {"id": "gpt-6-luna", "object": "model", "created": 1, "owned_by": "openai"},
                {"id": "text-embedding-3-small", "object": "model", "created": 2, "owned_by": "system"}
            ]
        }))
        .unwrap();
        assert_eq!(ids(&page.models), ["gpt-6-luna", "text-embedding-3-small"]);
        assert_eq!(page.models[0].display_name, None);
        assert_eq!(page.models[0].context_length, None);
        assert_eq!(page.models[0].model_type, None);
        assert!(!page.has_more);
        assert_eq!(page.last_id, None);
    }

    #[test]
    fn parses_an_openrouter_listing_with_names_and_context() {
        let page = parse_model_listing(&json!({
            "data": [{
                "id": "anthropic/claude-sonnet-5-5",
                "name": "Anthropic: Claude Sonnet 5.5",
                "context_length": 1000000,
                "pricing": {"prompt": "0.000003"}
            }]
        }))
        .unwrap();
        assert_eq!(
            page.models,
            vec![ProviderModelInfo {
                id: "anthropic/claude-sonnet-5-5".to_string(),
                display_name: Some("Anthropic: Claude Sonnet 5.5".to_string()),
                context_length: Some(1_000_000),
                model_type: None,
            }]
        );
    }

    #[test]
    fn parses_a_together_bare_array_with_types() {
        let page = parse_model_listing(&json!([
            {"id": "meta-llama/Llama-4-70B", "display_name": "Llama 4 70B", "type": "chat", "context_length": 131072},
            {"id": "BAAI/bge-large-en-v1.5", "display_name": "BGE large", "type": "embedding"}
        ]))
        .unwrap();
        assert_eq!(
            ids(&page.models),
            ["meta-llama/Llama-4-70B", "BAAI/bge-large-en-v1.5"]
        );
        assert_eq!(page.models[0].model_type.as_deref(), Some("chat"));
        assert_eq!(page.models[0].context_length, Some(131_072));
        assert_eq!(page.models[1].model_type.as_deref(), Some("embedding"));
        assert_eq!(page.models[1].display_name.as_deref(), Some("BGE large"));
    }

    #[test]
    fn parses_a_gemini_listing_keeping_the_raw_id() {
        let page = parse_model_listing(&json!({
            "object": "list",
            "data": [{"id": "models/gemini-3-pro", "object": "model", "owned_by": "google"}]
        }))
        .unwrap();
        // The prefix is dropped later, per provider (see `normalize_model_id`).
        assert_eq!(ids(&page.models), ["models/gemini-3-pro"]);
    }

    #[test]
    fn parses_an_anthropic_page_with_pagination() {
        let page = parse_model_listing(&json!({
            "data": [
                {"type": "model", "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "created_at": "2026-01-01T00:00:00Z"}
            ],
            "has_more": true,
            "first_id": "claude-opus-5-5",
            "last_id": "claude-opus-5-5"
        }))
        .unwrap();
        assert_eq!(
            page.models[0].display_name.as_deref(),
            Some("Claude Opus 5.5")
        );
        // Anthropic's generic `type: model` carries no information.
        assert_eq!(page.models[0].model_type, None);
        assert!(page.has_more);
        assert_eq!(page.last_id.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn skips_entries_without_an_id_and_accepts_bare_strings() {
        let page = parse_model_listing(&json!({
            "data": [{"name": "no id"}, {"id": ""}, "llama3:8b", 42, {"id": "qwen3"}]
        }))
        .unwrap();
        assert_eq!(ids(&page.models), ["llama3:8b", "qwen3"]);
    }

    #[test]
    fn display_name_equal_to_the_id_is_dropped() {
        let page = parse_model_listing(&json!({"data": [{"id": "m", "name": "m"}]})).unwrap();
        assert_eq!(page.models[0].display_name, None);
    }

    #[test]
    fn malformed_bodies_are_errors() {
        assert!(parse_model_listing(&json!("text")).is_err());
        assert!(parse_model_listing(&json!({"models": []})).is_err());
        assert!(parse_model_listing(&json!({"data": {"id": "x"}})).is_err());
    }

    #[test]
    fn error_bodies_report_the_provider_message() {
        let error =
            parse_model_listing(&json!({"error": {"message": "Invalid API key", "type": "auth"}}))
                .unwrap_err();
        assert!(error.contains("Invalid API key"), "{error}");
    }

    // ── normalisation ────────────────────────────────────────────────────────

    #[test]
    fn gemini_ids_lose_the_models_prefix_only_for_gemini() {
        assert_eq!(
            normalize_model_id("gemini", "models/gemini-3-pro"),
            "gemini-3-pro"
        );
        assert_eq!(normalize_model_id("gemini", "gemini-3-pro"), "gemini-3-pro");
        assert_eq!(normalize_model_id("custom", "models/x"), "models/x");
    }

    #[test]
    fn model_lists_are_sorted_and_deduplicated() {
        let model = |id: &str, name: Option<&str>| ProviderModelInfo {
            id: id.to_string(),
            display_name: name.map(str::to_string),
            context_length: None,
            model_type: None,
        };
        let normalized = normalize_model_list(
            "openai",
            vec![
                model("b", Some("first b")),
                model("a", None),
                model("b", Some("second b")),
            ],
        );
        assert_eq!(ids(&normalized), ["a", "b"]);
        assert_eq!(normalized[1].display_name.as_deref(), Some("first b"));
    }

    #[test]
    fn anthropic_page_urls_carry_limit_and_escaped_cursor() {
        assert_eq!(
            anthropic_page_url("http://h/v1/models", None),
            "http://h/v1/models?limit=1000"
        );
        assert_eq!(
            anthropic_page_url("http://h/v1/models", Some("a b&c")),
            "http://h/v1/models?limit=1000&after_id=a%20b%26c"
        );
    }

    // ── build_model_listing_request ──────────────────────────────────────────

    #[test]
    fn unknown_provider_is_an_error() {
        let error = build_model_listing_request(&Config::default(), "nonesuch").unwrap_err();
        assert!(error.contains("nonesuch"), "{error}");
    }

    #[test]
    fn azure_is_reported_as_unsupported() {
        let error = build_model_listing_request(&Config::default(), "azure_openai").unwrap_err();
        assert!(error.contains("by hand"), "{error}");
    }

    #[test]
    fn auto_lists_the_provider_it_would_pick() {
        let mut config = Config::default();
        config.providers.together.api_key = "tk".to_string();
        let request = build_model_listing_request(&config, "auto").unwrap();
        assert_eq!(request.provider_name, "together");
        assert_eq!(
            request.endpoint_url,
            format!("{}/models", crate::providers::registry::TOGETHER_API_BASE)
        );
    }

    #[test]
    fn unset_environment_placeholder_in_the_listed_provider_is_an_error() {
        let mut config = Config::default();
        config.providers.openai.api_key = "${RUSTBOT_MODEL_CATALOG_TEST_UNSET_KEY}".to_string();
        let error = build_model_listing_request(&config, "openai").unwrap_err();
        assert!(
            error.contains("RUSTBOT_MODEL_CATALOG_TEST_UNSET_KEY"),
            "{error}"
        );
        // Other providers' placeholders do not matter.
        config.providers.custom.api_base = Some("http://localhost:1234/v1".to_string());
        assert!(build_model_listing_request(&config, "custom").is_ok());
    }

    #[test]
    fn openai_compatible_listing_without_a_key_sends_no_authorization() {
        let mut config = Config::default();
        config.providers.custom.api_base = Some("http://localhost:11434/v1".to_string());
        let request = build_model_listing_request(&config, "custom").unwrap();
        assert!(request.headers.get("authorization").is_none());
        assert_eq!(request.endpoint_url, "http://localhost:11434/v1/models");
    }

    #[test]
    fn openrouter_listing_keeps_the_attribution_headers() {
        let mut config = Config::default();
        config.providers.openrouter.api_key = "sk-or-test".to_string();
        let request = build_model_listing_request(&config, "openrouter").unwrap();
        assert!(request.headers.contains_key("http-referer"));
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            "Bearer sk-or-test"
        );
        assert!(!request.headers.contains_key("x-session-affinity"));
    }

    // ── list_provider_models against a local mock server ─────────────────────

    /// A canned HTTP response the mock server sends for one request.
    struct CannedResponse {
        status_line: &'static str,
        body: String,
    }

    impl CannedResponse {
        fn ok(body: Value) -> Self {
            Self {
                status_line: "200 OK",
                body: body.to_string(),
            }
        }
    }

    /// Serve `responses` in order, one per connection, on a local port; record each raw
    /// request (request line and headers, lowercased). Returns `http://{addr}` and the
    /// recorded requests.
    async fn serve_in_order(responses: Vec<CannedResponse>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let recorded_requests = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&recorded_requests);
        tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 16384];
                let read = socket.read(&mut request).await.unwrap_or(0);
                recorder
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request[..read]).to_lowercase());
                let raw_response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status_line,
                    response.body.len(),
                    response.body
                );
                socket.write_all(raw_response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}"), recorded_requests)
    }

    #[tokio::test]
    async fn lists_openai_compatible_models_with_a_bearer_token() {
        let (base, requests) = serve_in_order(vec![CannedResponse::ok(json!({
            "data": [{"id": "zeta"}, {"id": "alpha"}, {"id": "zeta"}]
        }))])
        .await;
        let mut config = Config::default();
        config.providers.openai.api_key = "sk-test".to_string();
        config.providers.openai.api_base = Some(format!("{base}/v1/"));

        let models = list_provider_models(&config, "openai").await.unwrap();

        assert_eq!(ids(&models), ["alpha", "zeta"]);
        let request = requests.lock().unwrap()[0].clone();
        assert!(request.starts_with("get /v1/models "), "{request}");
        assert!(
            request.contains("authorization: bearer sk-test"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn lists_anthropic_models_over_two_pages_with_its_headers() {
        let (base, requests) = serve_in_order(vec![
            CannedResponse::ok(json!({
                "data": [{"type": "model", "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5"}],
                "has_more": true,
                "last_id": "claude-opus-5-5"
            })),
            CannedResponse::ok(json!({
                "data": [{"type": "model", "id": "claude-haiku-5-5", "display_name": "Claude Haiku 5.5"}],
                "has_more": false,
                "last_id": "claude-haiku-5-5"
            })),
        ])
        .await;
        let mut config = Config::default();
        config.providers.anthropic.api_key = "sk-ant-test".to_string();
        config.providers.anthropic.api_base = Some(base);

        let models = list_provider_models(&config, "anthropic").await.unwrap();

        assert_eq!(ids(&models), ["claude-haiku-5-5", "claude-opus-5-5"]);
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].starts_with("get /v1/models?limit=1000 "),
            "{}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("get /v1/models?limit=1000&after_id=claude-opus-5-5 "),
            "{}",
            requests[1]
        );
        for request in &requests {
            assert!(request.contains("x-api-key: sk-ant-test"), "{request}");
            assert!(
                request.contains("anthropic-version: 2023-06-01"),
                "{request}"
            );
            assert!(!request.contains("authorization:"), "{request}");
        }
    }

    #[tokio::test]
    async fn non_success_status_is_an_error_with_status_and_message() {
        let (base, _requests) = serve_in_order(vec![CannedResponse {
            status_line: "401 Unauthorized",
            body: json!({"error": {"message": "Incorrect API key provided"}}).to_string(),
        }])
        .await;
        let mut config = Config::default();
        config.providers.openai.api_key = "bad".to_string();
        config.providers.openai.api_base = Some(format!("{base}/v1"));

        let error = list_provider_models(&config, "openai").await.unwrap_err();

        assert!(error.starts_with("HTTP 401 Unauthorized from "), "{error}");
        assert!(error.contains("Incorrect API key provided"), "{error}");
    }

    #[tokio::test]
    async fn non_json_success_body_is_an_error_with_a_snippet() {
        let (base, _requests) = serve_in_order(vec![CannedResponse {
            status_line: "200 OK",
            body: "<html>login</html>".to_string(),
        }])
        .await;
        let mut config = Config::default();
        config.providers.custom.api_base = Some(format!("{base}/v1"));

        let error = list_provider_models(&config, "custom").await.unwrap_err();

        assert!(
            error.starts_with("invalid JSON response (HTTP 200 OK)"),
            "{error}"
        );
        assert!(error.contains("<html>login</html>"), "{error}");
    }

    #[tokio::test]
    async fn unreachable_server_is_an_error_naming_the_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut config = Config::default();
        config.providers.custom.api_base = Some(format!("http://{address}/v1"));

        let error = list_provider_models(&config, "custom").await.unwrap_err();

        assert!(
            error.starts_with(&format!("request to http://{address}/v1/models failed")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn unknown_provider_fails_without_a_request() {
        let error = list_provider_models(&Config::default(), "nonesuch")
            .await
            .unwrap_err();
        assert_eq!(error, "Unknown provider 'nonesuch'.");
    }
}
