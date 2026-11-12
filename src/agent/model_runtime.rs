//! Resolves named model presets (and the implicit flat-field "default") to a
//! concrete, ready-to-use [`ModelRuntime`] — the provider instance, model
//! name, and generation settings a turn should actually use.
//!
//! There is deliberately no "fixed startup provider" anywhere once this
//! resolver is in place: the main turn loop, subagents, and Dream/Consolidator
//! all resolve their runtime through the same [`ModelRuntimeResolver`],
//! keyed by session (or, for Dream, by its own configured preset), instead of
//! holding a provider/model captured once at process startup.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::config::schema::{
    Config, ModelPresetConfig, ProvidersConfig, RESERVED_MODEL_PRESET_NAME,
};
use crate::providers::base::LLMProviderDyn;
use crate::providers::factory::{create_provider_for, resolve_concrete_provider_name};
use crate::session::manager::{Session, SessionManager};

pub use crate::session::keys::SESSION_MODEL_PRESET_METADATA_KEY;

/// One resolved, immutable model/provider/generation snapshot for a turn.
///
/// Cheap to clone: `provider` is an `Arc`, everything else is a small value.
#[derive(Clone)]
pub struct ModelRuntime {
    /// `"default"` or a `model_presets` key.
    pub preset_name: String,
    pub provider: Arc<dyn LLMProviderDyn>,
    /// The concrete provider name this runtime resolved to (never `"auto"`).
    pub provider_name: String,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub reasoning_effort: Option<String>,
    pub context_window_tokens: u64,
}

/// Owns model-preset selection and resolves names to immutable [`ModelRuntime`]
/// values, independent of any particular session.
///
/// Provider instances are cached by their *resolved* concrete provider name
/// (never by preset name), so two presets that share a provider — even via
/// `"auto"` — reuse one provider instance; only `model`/generation settings
/// differ between them. This is safe because those fields are already
/// per-call overrides on [`LLMProviderDyn::chat_with_retry`], not baked into
/// the provider at construction.
///
/// The preset catalog can be replaced while the process runs
/// ([`Self::reload_presets`], used by the WebUI's Models page), and so can the
/// provider settings ([`Self::reload_providers`], used by its Providers page,
/// which also drops the cached providers). The flat `agents.*` fields stay as
/// they were at startup.
pub struct ModelRuntimeResolver {
    /// Startup configuration with the live `providers.*`: provider credentials
    /// and the flat `agents.*` fields. Its `model_presets` is not read after
    /// construction; the live catalog is `presets`.
    config: RwLock<Config>,
    /// The live preset catalog (`model_presets`), replaced by [`Self::reload_presets`].
    presets: RwLock<HashMap<String, ModelPresetConfig>>,
    provider_cache: Mutex<HashMap<String, Arc<dyn LLMProviderDyn>>>,
    /// The implicit `"default"` runtime built at startup from the flat
    /// `agents.*` fields and the startup provider; restored exactly when
    /// `"default"` is made the process-wide default again.
    flat_default: RwLock<ModelRuntime>,
    default_runtime: Mutex<ModelRuntime>,
}

impl ModelRuntimeResolver {
    /// Build the resolver, seeding the provider cache with the
    /// already-constructed startup `initial_provider` (under the concrete
    /// name `config.agents.provider` resolves to) so the common case — no
    /// `model_preset` set, or one that resolves to the same provider — does
    /// not rebuild a provider at startup.
    pub fn new(config: Config, initial_provider: Arc<dyn LLMProviderDyn>) -> Self {
        let flat_provider_name = resolve_concrete_provider_name(&config, &config.agents.provider)
            .unwrap_or_else(|_| config.agents.provider.clone());
        let flat_default = ModelRuntime {
            preset_name: RESERVED_MODEL_PRESET_NAME.to_string(),
            provider: initial_provider,
            provider_name: flat_provider_name.clone(),
            model: config.agents.model.clone(),
            max_tokens: config.agents.max_tokens,
            temperature: config.agents.temperature,
            reasoning_effort: config.agents.reasoning_effort.clone(),
            context_window_tokens: config.agents.context_window_tokens,
        };

        let cache: Mutex<HashMap<String, Arc<dyn LLMProviderDyn>>> = Mutex::new(HashMap::new());
        cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(flat_provider_name, flat_default.provider.clone());

        // The implicit "default" preset always reuses the already-constructed
        // `initial_provider` directly rather than re-deriving one via
        // `create_provider_for` — callers may have built it through means the
        // config alone can't reproduce (e.g. a test double, or an OAuth/login
        // flow), so re-deriving it here must never be a hard requirement.
        let presets = config.model_presets.clone();
        let default_name = config.agents.model_preset.clone();
        let default_runtime = match default_name {
            Some(name) if name != RESERVED_MODEL_PRESET_NAME => {
                Self::resolve_preset_with(&config, &presets, &cache, &name).unwrap_or_else(|e| {
                    log::warn!(
                        "agents.modelPreset '{name}' failed to resolve at startup ({e}); \
                         falling back to agents.* fields"
                    );
                    flat_default.clone()
                })
            }
            _ => flat_default.clone(),
        };

        Self {
            config: RwLock::new(config),
            presets: RwLock::new(presets),
            provider_cache: cache,
            flat_default: RwLock::new(flat_default),
            default_runtime: Mutex::new(default_runtime),
        }
    }

    /// Resolve a preset name to a runtime, without changing the process-wide default.
    pub fn resolve_preset(&self, name: &str) -> Result<ModelRuntime, String> {
        let config = self.config.read().unwrap_or_else(|e| e.into_inner());
        let presets = self.presets.read().unwrap_or_else(|e| e.into_inner());
        Self::resolve_preset_with(&config, &presets, &self.provider_cache, name)
    }

    /// Replace the preset catalog with `presets` and make `default_name` the
    /// process-wide default (`None` or `"default"`: the flat `agents.*`
    /// fields with the startup provider, exactly as at startup).
    ///
    /// Sessions with their own preset override keep it and resolve it
    /// against the new catalog on their next turn; an override naming a
    /// preset that no longer exists falls back to the default, as always.
    ///
    /// The catalog is replaced even when the new default fails to resolve
    /// (e.g. an unknown provider); the previous default is then kept and the
    /// error returned, so the caller can report that a restart is needed.
    pub fn reload_presets(
        &self,
        presets: HashMap<String, ModelPresetConfig>,
        default_name: Option<&str>,
    ) -> Result<ModelRuntime, String> {
        *self.presets.write().unwrap_or_else(|e| e.into_inner()) = presets;
        let runtime = match default_name {
            Some(name) if name != RESERVED_MODEL_PRESET_NAME => self.resolve_preset(name)?,
            _ => self
                .flat_default
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        };
        *self
            .default_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = runtime.clone();
        Ok(runtime)
    }

    /// Replace the provider settings (`providers.*`) with `providers` and apply
    /// them to the running process.
    ///
    /// 1. The new settings are swapped in and every cached provider dropped, so
    ///    the next resolution of a preset builds its provider from them.
    /// 2. The flat `agents.*` provider is rebuilt. When that fails the previous
    ///    one is kept and the error is returned (after steps 3 and 4).
    /// 3. The current process-wide default is resolved again.
    ///
    /// Turns in progress keep the provider they started with; their next turn
    /// resolves a new one. A default that cannot be resolved with the new
    /// settings is kept as it was and its error returned.
    pub fn reload_providers(&self, providers: ProvidersConfig) -> Result<ModelRuntime, String> {
        let (flat_provider_name, flat_rebuild) = {
            let mut config = self.config.write().unwrap_or_else(|e| e.into_inner());
            config.providers = providers;
            let flat_model = self
                .flat_default
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model
                .clone();
            let flat_provider_name =
                resolve_concrete_provider_name(&config, &config.agents.provider)
                    .unwrap_or_else(|_| config.agents.provider.clone());
            let flat_rebuild = create_provider_for(&config, &flat_model, &config.agents.provider);
            (flat_provider_name, flat_rebuild)
        };
        let mut cache = self
            .provider_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.clear();
        let flat_error = {
            let mut flat_default = self.flat_default.write().unwrap_or_else(|e| e.into_inner());
            match flat_rebuild {
                Ok(provider) => {
                    flat_default.provider = provider;
                    flat_default.provider_name = flat_provider_name;
                    None
                }
                Err(error) => Some(error),
            }
        };
        let flat_default = self
            .flat_default
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        cache.insert(
            flat_default.provider_name.clone(),
            flat_default.provider.clone(),
        );
        drop(cache);

        let current = self.current_default();
        let runtime = if current.preset_name == RESERVED_MODEL_PRESET_NAME {
            // Keep a model chosen with `select_model` and the like; only the provider is new.
            ModelRuntime {
                provider: flat_default.provider,
                provider_name: flat_default.provider_name,
                ..current
            }
        } else {
            self.resolve_preset(&current.preset_name)?
        };
        *self
            .default_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = runtime.clone();
        match flat_error {
            Some(error) => Err(error),
            None => Ok(runtime),
        }
    }

    fn resolve_preset_with(
        config: &Config,
        presets: &HashMap<String, ModelPresetConfig>,
        provider_cache: &Mutex<HashMap<String, Arc<dyn LLMProviderDyn>>>,
        name: &str,
    ) -> Result<ModelRuntime, String> {
        let (
            model,
            provider_cfg_name,
            max_tokens,
            context_window_tokens,
            temperature,
            reasoning_effort,
        ) = if name == RESERVED_MODEL_PRESET_NAME {
            let a = &config.agents;
            (
                a.model.clone(),
                a.provider.clone(),
                a.max_tokens,
                a.context_window_tokens,
                a.temperature,
                a.reasoning_effort.clone(),
            )
        } else {
            let preset = presets
                .get(name)
                .ok_or_else(|| format!("Unknown model preset '{name}'"))?;
            (
                preset.model.clone(),
                preset.provider.clone(),
                preset.max_tokens,
                preset.context_window_tokens,
                preset.temperature,
                preset.reasoning_effort.clone(),
            )
        };

        let resolved_provider_name = resolve_concrete_provider_name(config, &provider_cfg_name)?;

        let provider = {
            let mut cache = provider_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(p) = cache.get(&resolved_provider_name) {
                p.clone()
            } else {
                let p = create_provider_for(config, &model, &provider_cfg_name)?;
                cache.insert(resolved_provider_name.clone(), p.clone());
                p
            }
        };

        Ok(ModelRuntime {
            preset_name: name.to_string(),
            provider,
            provider_name: resolved_provider_name,
            model,
            max_tokens,
            temperature,
            reasoning_effort,
            context_window_tokens,
        })
    }

    /// Resolve a preset name and make it the process-wide default for future
    /// turns/sessions that have no session-scoped override. No caller in the
    /// current `/model` command uses this — session switches never touch the
    /// process default — but it's kept for a future admin-level setter.
    pub fn select_preset(&self, name: &str) -> Result<ModelRuntime, String> {
        let runtime = self.resolve_preset(name)?;
        *self
            .default_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = runtime.clone();
        Ok(runtime)
    }

    /// Overwrite the process-wide default's model, without touching the
    /// preset catalog or reconstructing the provider. `preset_name` is reset
    /// to [`RESERVED_MODEL_PRESET_NAME`] since the runtime is no longer tied
    /// to any configured preset. Mirrors nanobot's
    /// `RuntimeResolver.select_model`.
    pub fn select_model(&self, model: &str) -> Result<ModelRuntime, String> {
        let model = model.trim();
        if model.is_empty() {
            return Err("select_model: model must not be empty".to_string());
        }
        let mut guard = self
            .default_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.model = model.to_string();
        guard.preset_name = RESERVED_MODEL_PRESET_NAME.to_string();
        Ok(guard.clone())
    }

    /// The process-wide default model name.
    ///
    /// Does not reflect any session-scoped preset override (after
    /// `/model <preset>`). For that, use [`Self::runtime_for_session`] or
    /// [`Self::resolve_for_session_key`] and read `.model`.
    pub fn get_model(&self) -> String {
        self.current_default().model
    }

    /// Overwrite the process-wide default's context-window budget, without
    /// touching any other field. Mirrors nanobot's
    /// `RuntimeResolver.select_context_window`.
    pub fn select_context_window(&self, tokens: u64) -> ModelRuntime {
        let mut guard = self
            .default_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.context_window_tokens = tokens;
        guard.clone()
    }

    /// The current process-wide default runtime.
    pub fn current_default(&self) -> ModelRuntime {
        self.default_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Names available for `/model <name>`: `"default"` plus every configured preset, sorted.
    pub fn available_preset_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .presets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        names.sort();
        names.insert(0, RESERVED_MODEL_PRESET_NAME.to_string());
        names
    }

    /// Resolve the runtime for one turn: the session's stored preset override
    /// if present, else the process-wide default. Never fails — an
    /// unknown/removed preset name logs a warning and falls back to the
    /// default rather than failing the turn.
    pub fn runtime_for_session(&self, session: Option<&Session>) -> ModelRuntime {
        let Some(session) = session else {
            return self.current_default();
        };
        let Some(name) = session
            .metadata
            .get(SESSION_MODEL_PRESET_METADATA_KEY)
            .and_then(|v| v.as_str())
        else {
            return self.current_default();
        };
        self.resolve_or_default(&session.key, name)
    }

    /// Same as [`Self::runtime_for_session`], but for callers that only have
    /// a session key (e.g. `SubagentManager::spawn`), not an already-loaded
    /// `Session` — loads it from the shared `SessionManager` internally.
    pub fn resolve_for_session_key(
        &self,
        sessions: &Mutex<SessionManager>,
        key: &str,
    ) -> ModelRuntime {
        let name = {
            let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions
                .get_or_create_session(key)
                .metadata
                .get(SESSION_MODEL_PRESET_METADATA_KEY)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        match name {
            Some(name) => self.resolve_or_default(key, &name),
            None => self.current_default(),
        }
    }

    fn resolve_or_default(&self, session_key: &str, name: &str) -> ModelRuntime {
        match self.resolve_preset(name) {
            Ok(runtime) => runtime,
            Err(e) => {
                log::warn!(
                    "Session '{session_key}' references model preset '{name}' ({e}); \
                     falling back to the process default"
                );
                self.current_default()
            }
        }
    }

    /// Resolver for tests that never issue an LLM call. Uses a dummy Anthropic
    /// provider so construction does not depend on real credentials.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Arc<Self> {
        let mut config = Config::default();
        config.agents.provider = "anthropic".to_string();
        config.providers.anthropic.api_key = "test-key".to_string();
        let provider = crate::providers::factory::create_provider_for(
            &config,
            &config.agents.model,
            &config.agents.provider,
        )
        .expect("test provider");
        Arc::new(Self::new(config, provider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_anthropic_preset(name: &str, model: &str) -> Config {
        let mut config = Config::default();
        config.providers.anthropic.api_key = "test-key".to_string();
        config.model_presets.insert(
            name.to_string(),
            crate::config::schema::ModelPresetConfig {
                model: model.to_string(),
                provider: "anthropic".to_string(),
                ..Default::default()
            },
        );
        config
    }

    fn build_resolver(config: Config) -> ModelRuntimeResolver {
        let provider = create_provider_for(&config, &config.agents.model, &config.agents.provider)
            .expect("default provider should build");
        ModelRuntimeResolver::new(config, provider)
    }

    #[test]
    fn resolve_default_preset_uses_flat_agents_fields() {
        let mut config = Config::default();
        config.agents.model = "anthropic/claude-opus-4-6".to_string();
        config.providers.anthropic.api_key = "test-key".to_string();
        config.agents.provider = "anthropic".to_string();
        let resolver = build_resolver(config);

        let runtime = resolver.resolve_preset(RESERVED_MODEL_PRESET_NAME).unwrap();
        assert_eq!(runtime.model, "anthropic/claude-opus-4-6");
        assert_eq!(runtime.preset_name, "default");
    }

    #[test]
    fn resolve_named_preset_returns_its_model_and_settings() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);

        let runtime = resolver.resolve_preset("fast").unwrap();
        assert_eq!(runtime.model, "claude-haiku");
        assert_eq!(runtime.preset_name, "fast");
    }

    #[test]
    fn resolve_unknown_preset_name_errs() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        assert!(resolver.resolve_preset("does-not-exist").is_err());
    }

    #[test]
    fn resolve_preset_caches_provider_by_resolved_provider_name() {
        let mut config = config_with_anthropic_preset("fast", "claude-haiku");
        config.model_presets.insert(
            "deep".to_string(),
            crate::config::schema::ModelPresetConfig {
                model: "claude-opus".to_string(),
                provider: "anthropic".to_string(),
                ..Default::default()
            },
        );
        let resolver = build_resolver(config);

        let fast = resolver.resolve_preset("fast").unwrap();
        let deep = resolver.resolve_preset("deep").unwrap();
        assert!(Arc::ptr_eq(&fast.provider, &deep.provider));
        assert_ne!(fast.model, deep.model);
    }

    #[test]
    fn select_preset_mutates_default_but_resolve_preset_does_not() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);

        let before = resolver.current_default();
        let _ = resolver.resolve_preset("fast").unwrap();
        assert_eq!(resolver.current_default().model, before.model);

        resolver.select_preset("fast").unwrap();
        assert_eq!(resolver.current_default().model, "claude-haiku");
    }

    #[test]
    fn select_model_overwrites_default_model_and_resets_preset_name() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        resolver.select_preset("fast").unwrap();
        assert_eq!(resolver.current_default().preset_name, "fast");

        let runtime = resolver.select_model("claude-opus-4-6").unwrap();
        assert_eq!(runtime.model, "claude-opus-4-6");
        assert_eq!(runtime.preset_name, RESERVED_MODEL_PRESET_NAME);
        assert_eq!(resolver.current_default().model, "claude-opus-4-6");
        assert_eq!(
            resolver.current_default().preset_name,
            RESERVED_MODEL_PRESET_NAME
        );
    }

    #[test]
    fn select_model_rejects_blank_input() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        let before = resolver.current_default();

        assert!(resolver.select_model("").is_err());
        assert!(resolver.select_model("   ").is_err());
        assert_eq!(resolver.current_default().model, before.model);
    }

    #[test]
    fn get_model_returns_process_default_model() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);

        assert_eq!(resolver.get_model(), resolver.current_default().model);

        resolver.select_model("claude-opus-4-6").unwrap();
        assert_eq!(resolver.get_model(), "claude-opus-4-6");
    }

    #[test]
    fn select_model_trims_input_and_leaves_other_fields_untouched() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        let before = resolver.current_default();

        let runtime = resolver.select_model("  claude-opus-4-6  ").unwrap();
        assert_eq!(runtime.model, "claude-opus-4-6");
        assert_eq!(runtime.max_tokens, before.max_tokens);
        assert_eq!(runtime.temperature, before.temperature);
        assert_eq!(runtime.context_window_tokens, before.context_window_tokens);
        assert!(Arc::ptr_eq(&runtime.provider, &before.provider));
    }

    #[test]
    fn select_context_window_overwrites_only_that_field() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        let before = resolver.current_default();

        let runtime = resolver.select_context_window(123_456);
        assert_eq!(runtime.context_window_tokens, 123_456);
        assert_eq!(runtime.model, before.model);
        assert_eq!(runtime.preset_name, before.preset_name);
        assert_eq!(resolver.current_default().context_window_tokens, 123_456);
    }

    #[test]
    fn select_model_and_select_context_window_are_independent() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);

        resolver.select_model("claude-opus-4-6").unwrap();
        resolver.select_context_window(50_000);

        let runtime = resolver.current_default();
        assert_eq!(runtime.model, "claude-opus-4-6");
        assert_eq!(runtime.context_window_tokens, 50_000);
    }

    #[test]
    fn runtime_for_session_without_override_returns_process_default() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        let session = Session::new("s1".to_string());

        let runtime = resolver.runtime_for_session(Some(&session));
        assert_eq!(runtime.preset_name, "default");
    }

    #[test]
    fn runtime_for_session_with_override_returns_named_preset() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        let mut session = Session::new("s1".to_string());
        session.metadata.insert(
            SESSION_MODEL_PRESET_METADATA_KEY.to_string(),
            serde_json::Value::String("fast".to_string()),
        );

        let runtime = resolver.runtime_for_session(Some(&session));
        assert_eq!(runtime.preset_name, "fast");
        assert_eq!(runtime.model, "claude-haiku");
    }

    #[test]
    fn runtime_for_session_none_returns_process_default() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        let runtime = resolver.runtime_for_session(None);
        assert_eq!(runtime.preset_name, "default");
    }

    #[test]
    fn runtime_for_session_unknown_override_falls_back_to_default() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        let mut session = Session::new("s1".to_string());
        session.metadata.insert(
            SESSION_MODEL_PRESET_METADATA_KEY.to_string(),
            serde_json::Value::String("removed-preset".to_string()),
        );

        let runtime = resolver.runtime_for_session(Some(&session));
        assert_eq!(runtime.preset_name, "default");
    }

    fn anthropic_preset(model: &str) -> crate::config::schema::ModelPresetConfig {
        crate::config::schema::ModelPresetConfig {
            model: model.to_string(),
            provider: "anthropic".to_string(),
            ..Default::default()
        }
    }

    fn session_with_override(name: &str) -> Session {
        let mut session = Session::new("s1".to_string());
        session.metadata.insert(
            SESSION_MODEL_PRESET_METADATA_KEY.to_string(),
            serde_json::Value::String(name.to_string()),
        );
        session
    }

    #[test]
    fn reload_adds_presets_that_can_then_be_resolved_and_listed() {
        let resolver = build_resolver(config_with_anthropic_preset("fast", "claude-haiku"));
        assert!(resolver.resolve_preset("terra").is_err());

        let presets = HashMap::from([
            ("fast".to_string(), anthropic_preset("claude-haiku")),
            ("terra".to_string(), anthropic_preset("gpt-6.1-terra")),
        ]);
        resolver.reload_presets(presets, None).unwrap();

        assert_eq!(
            resolver.resolve_preset("terra").unwrap().model,
            "gpt-6.1-terra"
        );
        assert_eq!(
            resolver.available_preset_names(),
            ["default", "fast", "terra"]
        );
    }

    #[test]
    fn reload_with_a_new_default_changes_the_process_default() {
        let resolver = build_resolver(config_with_anthropic_preset("fast", "claude-haiku"));

        let presets = HashMap::from([("terra".to_string(), anthropic_preset("gpt-6.1-terra"))]);
        let runtime = resolver.reload_presets(presets, Some("terra")).unwrap();

        assert_eq!(runtime.preset_name, "terra");
        assert_eq!(resolver.current_default().model, "gpt-6.1-terra");
        assert_eq!(resolver.runtime_for_session(None).model, "gpt-6.1-terra");
    }

    #[test]
    fn reload_picks_up_edits_to_the_active_preset() {
        let mut config = config_with_anthropic_preset("fast", "claude-haiku");
        config.agents.model_preset = Some("fast".to_string());
        let resolver = build_resolver(config);
        assert_eq!(resolver.current_default().model, "claude-haiku");

        let mut edited = anthropic_preset("claude-haiku-2");
        edited.temperature = 0.7;
        let presets = HashMap::from([("fast".to_string(), edited)]);
        resolver.reload_presets(presets, Some("fast")).unwrap();

        assert_eq!(resolver.current_default().model, "claude-haiku-2");
        assert_eq!(resolver.current_default().temperature, 0.7);
    }

    #[test]
    fn a_session_override_survives_a_reload_of_the_default() {
        let resolver = build_resolver(config_with_anthropic_preset("fast", "claude-haiku"));
        let session = session_with_override("fast");

        let presets = HashMap::from([
            ("fast".to_string(), anthropic_preset("claude-haiku")),
            ("terra".to_string(), anthropic_preset("gpt-6.1-terra")),
        ]);
        resolver.reload_presets(presets, Some("terra")).unwrap();

        assert_eq!(
            resolver.runtime_for_session(Some(&session)).model,
            "claude-haiku"
        );
        assert_eq!(resolver.runtime_for_session(None).model, "gpt-6.1-terra");
    }

    #[test]
    fn an_override_naming_a_deleted_preset_falls_back_to_the_new_default() {
        let resolver = build_resolver(config_with_anthropic_preset("fast", "claude-haiku"));
        let session = session_with_override("fast");

        let presets = HashMap::from([("terra".to_string(), anthropic_preset("gpt-6.1-terra"))]);
        resolver.reload_presets(presets, Some("terra")).unwrap();

        let runtime = resolver.runtime_for_session(Some(&session));
        assert_eq!(runtime.preset_name, "terra");
        assert_eq!(runtime.model, "gpt-6.1-terra");
    }

    #[test]
    fn reloading_with_default_restores_the_startup_flat_runtime() {
        let resolver = build_resolver(config_with_anthropic_preset("fast", "claude-haiku"));
        let startup = resolver.current_default();
        resolver.select_preset("fast").unwrap();

        let presets = HashMap::from([("fast".to_string(), anthropic_preset("claude-haiku"))]);
        let runtime = resolver
            .reload_presets(presets, Some(RESERVED_MODEL_PRESET_NAME))
            .unwrap();

        assert_eq!(runtime.preset_name, RESERVED_MODEL_PRESET_NAME);
        assert_eq!(runtime.model, startup.model);
        assert!(Arc::ptr_eq(&runtime.provider, &startup.provider));
        assert_eq!(resolver.current_default().model, startup.model);
    }

    #[test]
    fn a_failed_reload_keeps_the_previous_default_but_installs_the_catalog() {
        let resolver = build_resolver(config_with_anthropic_preset("fast", "claude-haiku"));
        let before = resolver.current_default();
        let mut broken = anthropic_preset("x");
        broken.provider = "not-a-provider".to_string();
        let presets = HashMap::from([("broken".to_string(), broken)]);

        assert!(resolver.reload_presets(presets, Some("broken")).is_err());

        assert_eq!(resolver.current_default().model, before.model);
        assert_eq!(resolver.available_preset_names(), ["default", "broken"]);
    }

    #[test]
    fn available_preset_names_includes_default_first() {
        let config = config_with_anthropic_preset("fast", "claude-haiku");
        let resolver = build_resolver(config);
        let names = resolver.available_preset_names();
        assert_eq!(names[0], "default");
        assert!(names.contains(&"fast".to_string()));
    }

    // ── reload_providers ──────────────────────────────────────────────────────

    fn providers_with_anthropic(api_key: &str, api_base: Option<&str>) -> ProvidersConfig {
        let mut providers = ProvidersConfig::default();
        providers.anthropic.api_key = api_key.to_string();
        providers.anthropic.api_base = api_base.map(str::to_string);
        providers
    }

    #[test]
    fn reload_providers_resolves_presets_with_the_new_key_and_base() {
        let mut config = config_with_anthropic_preset("fast", "claude-haiku");
        config.agents.provider = "anthropic".to_string();
        let resolver = build_resolver(config);
        assert_eq!(
            resolver.resolve_preset("fast").unwrap().provider.api_key(),
            Some("test-key".to_string())
        );

        resolver
            .reload_providers(providers_with_anthropic(
                "new-key",
                Some("https://relay.example"),
            ))
            .unwrap();

        let provider = resolver.resolve_preset("fast").unwrap().provider;
        assert_eq!(provider.api_key(), Some("new-key".to_string()));
        assert_eq!(
            provider.api_base(),
            Some("https://relay.example".to_string())
        );
        let default = resolver.current_default();
        assert_eq!(default.provider.api_key(), Some("new-key".to_string()));
    }

    #[test]
    fn reload_providers_drops_the_cached_providers() {
        let mut config = config_with_anthropic_preset("fast", "claude-haiku");
        config.agents.provider = "anthropic".to_string();
        let resolver = build_resolver(config);
        let before = resolver.resolve_preset("fast").unwrap().provider;

        resolver
            .reload_providers(providers_with_anthropic("test-key", None))
            .unwrap();

        let after = resolver.resolve_preset("fast").unwrap().provider;
        assert!(!Arc::ptr_eq(&before, &after));
        // Resolved again, the provider is cached again.
        let again = resolver.resolve_preset("fast").unwrap().provider;
        assert!(Arc::ptr_eq(&after, &again));
    }

    #[test]
    fn a_failing_flat_default_rebuild_keeps_the_old_provider_and_reports_the_error() {
        let mut config = config_with_anthropic_preset("fast", "claude-haiku");
        config.agents.provider = "anthropic".to_string();
        let resolver = build_resolver(config);
        let old_provider = resolver.current_default().provider;

        // The flat provider names an unknown provider: it cannot be rebuilt.
        resolver.config.write().unwrap().agents.provider = "nonesuch".to_string();
        let failing = ProvidersConfig::default();
        let outcome = resolver.reload_providers(failing);

        assert!(outcome.is_err());
        assert!(Arc::ptr_eq(
            &old_provider,
            &resolver.current_default().provider
        ));
    }
}
