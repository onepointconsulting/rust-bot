//! The model drop-down of the model preset form: once a provider is chosen, the gateway
//! is asked which models that provider offers (`list_provider_models`) and a drop-down
//! under the Model input lists them. Picking one fills the Model input, which stays
//! editable for names the provider does not list (Azure deployments, local servers).
//!
//! The lists are cached per provider on the Settings panel for as long as the dialog is
//! open, so switching providers back and forth asks each one only once. A failed listing
//! is shown with a Retry button and never blocks typing a name.
//!
//! Grouping, labels and the field order are plain functions so they are unit-testable on
//! the host target without Leptos.

use std::collections::HashMap;

use chat_ui::models::{ProviderModelSummary, ToolFieldSummary};
use leptos::prelude::*;

/// Setting name of the model in the preset form.
pub const MODEL_FIELD: &str = "model";

/// Setting name of the provider in the preset form.
pub const PROVIDER_FIELD: &str = "provider";

/// Model types listed first, in this order; they are the ones a preset can chat with.
const CHAT_MODEL_TYPES: [&str; 3] = ["chat", "language", "code"];

/// Group heading of models whose provider did not say what they are, when others did.
const UNTYPED_GROUP_LABEL: &str = "Other";

/// Where a provider's model list stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderModelsState {
    /// Asked; no answer yet.
    Loading,
    /// The provider's models, sorted by id.
    Loaded(Vec<ProviderModelSummary>),
    /// Refused or failed, with the message for the user.
    Failed(String),
}

impl ProviderModelsState {
    /// The state of a `provider_models` reply: its models, or — when it carries a
    /// `detail` or an `error` — the failure, worded by `error` (else the `detail` code).
    pub fn from_reply(
        models: Vec<ProviderModelSummary>,
        detail: Option<String>,
        error: Option<String>,
    ) -> Self {
        if detail.is_none() && error.is_none() {
            return Self::Loaded(models);
        }
        Self::Failed(
            error
                .filter(|message| !message.trim().is_empty())
                .or(detail)
                .unwrap_or_default(),
        )
    }
}

/// The model lists of every provider asked so far, and how to ask for another.
#[derive(Clone, Copy)]
pub struct ModelCatalog {
    /// Provider name -> its list, as cached on the Settings panel.
    pub models: RwSignal<HashMap<String, ProviderModelsState>>,
    /// Ask the gateway for one provider's models (marks it [`ProviderModelsState::Loading`]).
    pub request: Callback<String>,
}

/// Models of one type, under a heading (empty when the provider types none of them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGroup {
    pub label: String,
    pub models: Vec<ProviderModelSummary>,
}

/// Whether `provider`'s models still have to be asked for: a provider is chosen and it
/// was never asked (a failure is retried only on request, not on every redraw).
pub fn needs_request(provider: &str, cache: &HashMap<String, ProviderModelsState>) -> bool {
    !provider.is_empty() && !cache.contains_key(provider)
}

/// `fields` with the provider moved right before the model, so the provider is chosen
/// first and the model drop-down under the Model input follows it. Other fields keep
/// their order; without both fields nothing moves.
pub fn provider_before_model(mut fields: Vec<ToolFieldSummary>) -> Vec<ToolFieldSummary> {
    let position = |fields: &[ToolFieldSummary], name: &str| {
        fields.iter().position(|field| field.name == name)
    };
    let (Some(model_index), Some(provider_index)) = (
        position(&fields, MODEL_FIELD),
        position(&fields, PROVIDER_FIELD),
    ) else {
        return fields;
    };
    if provider_index > model_index {
        let provider = fields.remove(provider_index);
        fields.insert(model_index, provider);
    }
    fields
}

/// A context window in tokens, short: `131k`, `1M`, `1.5M`.
pub fn format_context_length(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let tenths_of_millions = (tokens + 50_000) / 100_000;
        if tenths_of_millions % 10 == 0 {
            format!("{}M", tenths_of_millions / 10)
        } else {
            format!("{}.{}M", tenths_of_millions / 10, tenths_of_millions % 10)
        }
    } else if tokens >= 1_000 {
        format!("{}k", (tokens + 500) / 1_000)
    } else {
        tokens.to_string()
    }
}

/// The text of one model in the drop-down: its id, then its name and context window in
/// brackets when the provider reports them.
pub fn model_option_label(model: &ProviderModelSummary) -> String {
    let mut details: Vec<String> = Vec::new();
    if let Some(name) = model
        .display_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        details.push(name.to_string());
    }
    if let Some(tokens) = model.context_length {
        details.push(format!("{} context", format_context_length(tokens)));
    }
    if details.is_empty() {
        model.id.clone()
    } else {
        format!("{} ({})", model.id, details.join(" · "))
    }
}

/// A model type as a group heading: `chat` -> `Chat`.
fn type_heading(model_type: &str) -> String {
    let mut characters = model_type.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

/// Where a model type sorts among the groups: chat types first (in
/// [`CHAT_MODEL_TYPES`] order), then the others A–Z, untyped models last.
fn group_rank(model_type: Option<&str>) -> (u8, String) {
    match model_type {
        Some(kind) => match CHAT_MODEL_TYPES.iter().position(|chat| *chat == kind) {
            Some(index) => (0, index.to_string()),
            None => (1, kind.to_string()),
        },
        None => (2, String::new()),
    }
}

/// `models` grouped by type for the drop-down, chat models first. When the provider
/// types none of them (most do not), one group without a heading, in the given order.
pub fn model_groups(models: &[ProviderModelSummary]) -> Vec<ModelGroup> {
    if models.iter().all(|model| model.model_type.is_none()) {
        return vec![ModelGroup {
            label: String::new(),
            models: models.to_vec(),
        }];
    }
    let mut by_type: HashMap<Option<&str>, Vec<ProviderModelSummary>> = HashMap::new();
    for model in models {
        by_type
            .entry(model.model_type.as_deref())
            .or_default()
            .push(model.clone());
    }
    let mut groups: Vec<(Option<&str>, Vec<ProviderModelSummary>)> = by_type.into_iter().collect();
    groups.sort_by_key(|(model_type, _)| group_rank(*model_type));
    groups
        .into_iter()
        .map(|(model_type, models)| ModelGroup {
            label: model_type
                .map(type_heading)
                .unwrap_or_else(|| UNTYPED_GROUP_LABEL.to_string()),
            models,
        })
        .collect()
}

/// The first, empty entry of the drop-down.
pub fn picker_placeholder(provider: &str, count: usize) -> String {
    match count {
        1 => format!("Pick the one {provider} model…"),
        _ => format!("Pick one of {count} {provider} models…"),
    }
}

/// The value the drop-down shows for the typed `model`: the model itself when the list
/// has it, else the placeholder (empty value).
pub fn selected_option(model: &str, models: &[ProviderModelSummary]) -> String {
    let model = model.trim();
    if models.iter().any(|listed| listed.id == model) {
        model.to_string()
    } else {
        String::new()
    }
}

/// The line shown when `provider`'s models could not be listed.
pub fn failure_hint(provider: &str, message: &str) -> String {
    let message = message.trim();
    let ending = if message.ends_with(['.', '!', '?']) {
        ""
    } else {
        "."
    };
    format!(
        "Could not list the models of {provider}: {message}{ending} Type the model name instead."
    )
}

/// Tailwind classes of the hint lines under the Model input.
const HINT_CLASS: &str = "mt-1 block text-xs text-slate-500";

/// The drop-down of `provider`'s models under the Model input, or what stands in for it
/// (loading, failure with Retry, empty list). Picking a model writes it into the
/// `model` draft.
#[component]
pub fn ModelPicker(
    catalog: ModelCatalog,
    /// The drafts of the form the Model input belongs to (provider and model are read
    /// from, and the picked model written to, here).
    drafts: RwSignal<HashMap<String, String>>,
) -> impl IntoView {
    let provider = Memo::new(move |_| {
        drafts.with(|map| {
            map.get(PROVIDER_FIELD)
                .map(|provider| provider.trim().to_string())
                .unwrap_or_default()
        })
    });
    // Ask once per provider, as soon as it is chosen.
    Effect::new(move |_| {
        let provider = provider.get();
        if catalog.models.with(|cache| needs_request(&provider, cache)) {
            catalog.request.run(provider);
        }
    });
    let typed_model = move || drafts.with(|map| map.get(MODEL_FIELD).cloned().unwrap_or_default());
    let pick = move |model: String| {
        if !model.is_empty() {
            drafts.update(|map| {
                map.insert(MODEL_FIELD.to_string(), model);
            });
        }
    };

    move || {
        let provider = provider.get();
        if provider.is_empty() {
            return ().into_any();
        }
        let state = catalog.models.with(|cache| cache.get(&provider).cloned());
        match state {
            None | Some(ProviderModelsState::Loading) => view! {
                <span class=HINT_CLASS>{format!("Loading the models {provider} offers…")}</span>
            }
            .into_any(),
            Some(ProviderModelsState::Failed(message)) => {
                let provider_for_retry = provider.clone();
                view! {
                    <span class="mt-1 flex flex-wrap items-center gap-2 text-xs text-amber-700">
                        <span class="min-w-0 flex-1 break-words">
                            {failure_hint(&provider, &message)}
                        </span>
                        <button
                            type="button"
                            class="rounded-full border border-slate-300 bg-white px-2 py-0.5 text-xs text-slate-700 hover:bg-slate-100"
                            on:click=move |_| catalog.request.run(provider_for_retry.clone())
                        >
                            "Retry"
                        </button>
                    </span>
                }
                .into_any()
            }
            Some(ProviderModelsState::Loaded(models)) if models.is_empty() => view! {
                <span class=HINT_CLASS>{format!("{provider} lists no models; type the model name.")}</span>
            }
            .into_any(),
            Some(ProviderModelsState::Loaded(models)) => {
                let placeholder = picker_placeholder(&provider, models.len());
                let models_for_selection = models.clone();
                view! {
                    <select
                        aria-label=format!("Models {provider} offers")
                        class="mt-2 w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm"
                        prop:value=move || selected_option(&typed_model(), &models_for_selection)
                        on:change=move |event| pick(event_target_value(&event))
                    >
                        <option value="">{placeholder}</option>
                        {model_groups(&models)
                            .into_iter()
                            .map(|group| {
                                let options = group
                                    .models
                                    .iter()
                                    .map(|model| {
                                        let label = model_option_label(model);
                                        view! { <option value=model.id.clone()>{label}</option> }
                                    })
                                    .collect_view();
                                if group.label.is_empty() {
                                    options.into_any()
                                } else {
                                    view! { <optgroup label=group.label>{options}</optgroup> }.into_any()
                                }
                            })
                            .collect_view()}
                    </select>
                }
                .into_any()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_ui::models::ToolFieldKind;

    fn model(id: &str, model_type: Option<&str>) -> ProviderModelSummary {
        ProviderModelSummary {
            id: id.to_string(),
            display_name: None,
            context_length: None,
            model_type: model_type.map(str::to_string),
        }
    }

    fn field(name: &str) -> ToolFieldSummary {
        ToolFieldSummary {
            name: name.to_string(),
            label: name.to_string(),
            help: String::new(),
            kind: ToolFieldKind::Text,
            secret: false,
            env_var: String::new(),
            configured: false,
            value: None,
            default: None,
        }
    }

    fn names(fields: &[ToolFieldSummary]) -> Vec<&str> {
        fields.iter().map(|field| field.name.as_str()).collect()
    }

    #[test]
    fn replies_become_lists_or_failures() {
        let models = vec![model("gpt-x", None)];
        assert_eq!(
            ProviderModelsState::from_reply(models.clone(), None, None),
            ProviderModelsState::Loaded(models)
        );
        assert_eq!(
            ProviderModelsState::from_reply(Vec::new(), None, None),
            ProviderModelsState::Loaded(Vec::new())
        );
        assert_eq!(
            ProviderModelsState::from_reply(
                Vec::new(),
                Some("listing_failed".to_string()),
                Some("HTTP 401".to_string())
            ),
            ProviderModelsState::Failed("HTTP 401".to_string())
        );
        assert_eq!(
            ProviderModelsState::from_reply(Vec::new(), Some("admin_required".to_string()), None),
            ProviderModelsState::Failed("admin_required".to_string())
        );
    }

    #[test]
    fn a_chosen_provider_is_asked_once() {
        let mut cache = HashMap::new();
        assert!(needs_request("openai", &cache));
        assert!(!needs_request("", &cache), "no provider chosen");
        cache.insert("openai".to_string(), ProviderModelsState::Loading);
        assert!(!needs_request("openai", &cache));
        cache.insert(
            "openai".to_string(),
            ProviderModelsState::Failed("401".to_string()),
        );
        assert!(!needs_request("openai", &cache), "failures wait for Retry");
        assert!(needs_request("anthropic", &cache));
    }

    #[test]
    fn provider_moves_before_model() {
        let fields = vec![
            field("label"),
            field(MODEL_FIELD),
            field(PROVIDER_FIELD),
            field("maxTokens"),
        ];
        assert_eq!(
            names(&provider_before_model(fields)),
            ["label", PROVIDER_FIELD, MODEL_FIELD, "maxTokens"]
        );
    }

    #[test]
    fn field_order_is_kept_when_provider_already_comes_first_or_is_missing() {
        let ordered = vec![field(PROVIDER_FIELD), field(MODEL_FIELD)];
        assert_eq!(
            names(&provider_before_model(ordered)),
            [PROVIDER_FIELD, MODEL_FIELD]
        );
        let without_provider = vec![field("command"), field(MODEL_FIELD)];
        assert_eq!(
            names(&provider_before_model(without_provider)),
            ["command", MODEL_FIELD]
        );
    }

    #[test]
    fn context_lengths_are_short() {
        assert_eq!(format_context_length(512), "512");
        assert_eq!(format_context_length(131_072), "131k");
        assert_eq!(format_context_length(200_000), "200k");
        assert_eq!(format_context_length(1_000_000), "1M");
        assert_eq!(format_context_length(1_048_576), "1M");
        assert_eq!(format_context_length(1_500_000), "1.5M");
    }

    #[test]
    fn option_labels_add_name_and_context_when_known() {
        assert_eq!(model_option_label(&model("gpt-x", None)), "gpt-x");
        let described = ProviderModelSummary {
            display_name: Some("Claude Opus 5.5".to_string()),
            context_length: Some(200_000),
            ..model("claude-opus-5-5", None)
        };
        assert_eq!(
            model_option_label(&described),
            "claude-opus-5-5 (Claude Opus 5.5 · 200k context)"
        );
        let context_only = ProviderModelSummary {
            context_length: Some(131_072),
            ..model("llama", None)
        };
        assert_eq!(model_option_label(&context_only), "llama (131k context)");
    }

    #[test]
    fn untyped_models_form_one_group_without_heading() {
        let models = vec![model("b", None), model("a", None)];
        let groups = model_groups(&models);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "");
        assert_eq!(groups[0].models, models, "order kept");
    }

    #[test]
    fn typed_models_are_grouped_chat_first_untyped_last() {
        let models = vec![
            model("embed", Some("embedding")),
            model("img", Some("image")),
            model("code", Some("code")),
            model("plain", None),
            model("chat-1", Some("chat")),
            model("chat-2", Some("chat")),
        ];
        let groups = model_groups(&models);
        let labels: Vec<&str> = groups.iter().map(|group| group.label.as_str()).collect();
        assert_eq!(labels, ["Chat", "Code", "Embedding", "Image", "Other"]);
        let chat_ids: Vec<&str> = groups[0].models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(chat_ids, ["chat-1", "chat-2"]);
    }

    #[test]
    fn failure_hint_ends_sentences_once() {
        assert_eq!(
            failure_hint("openai", "HTTP 401 Unauthorized"),
            "Could not list the models of openai: HTTP 401 Unauthorized. Type the model name instead."
        );
        assert_eq!(
            failure_hint("azure_openai", "Enter it by hand."),
            "Could not list the models of azure_openai: Enter it by hand. Type the model name instead."
        );
    }

    #[test]
    fn placeholder_counts_the_models() {
        assert_eq!(
            picker_placeholder("openrouter", 466),
            "Pick one of 466 openrouter models…"
        );
        assert_eq!(
            picker_placeholder("custom", 1),
            "Pick the one custom model…"
        );
    }

    #[test]
    fn the_drop_down_shows_the_typed_model_only_when_listed() {
        let models = vec![model("gpt-x", None)];
        assert_eq!(selected_option(" gpt-x ", &models), "gpt-x");
        assert_eq!(selected_option("my-deployment", &models), "");
    }
}
