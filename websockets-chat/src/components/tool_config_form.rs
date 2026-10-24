//! The settings form of a tool section on the Tools page (e.g. Gmail's paths, the image
//! generation API key), generated from the field descriptions the gateway sends.
//!
//! Every input is edited as text in a draft map keyed by setting name; only settings the
//! user actually changed are sent. Secrets are never shown: an empty secret input means
//! "keep what is stored", and a field backed by an exported environment variable says so.
//!
//! The draft handling is plain functions so it is unit-testable on the host target
//! without Leptos.

use std::collections::HashMap;

use chat_ui::models::{ToolConfigSummary, ToolFieldKind, ToolFieldSummary};
use leptos::prelude::*;
use serde_json::Value;

/// What the form reports when saved: the tool section's name and the changed settings.
pub type ToolConfigChange = (String, HashMap<String, Value>);

/// Text shown in the input before the user touches it: the stored value, or nothing for
/// secrets (which the gateway never sends).
pub fn initial_draft(field: &ToolFieldSummary) -> String {
    let Some(value) = field.value.as_ref().filter(|_| !field.secret) else {
        return String::new();
    };
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The draft of every field, as shown before any edit.
pub fn initial_drafts(fields: &[ToolFieldSummary]) -> HashMap<String, String> {
    fields
        .iter()
        .map(|field| (field.name.clone(), initial_draft(field)))
        .collect()
}

/// Convert a draft into the JSON value the gateway expects, or say why it cannot be.
pub fn draft_to_value(field: &ToolFieldSummary, draft: &str) -> Result<Value, String> {
    match &field.kind {
        ToolFieldKind::Bool => Ok(Value::Bool(draft == "true")),
        ToolFieldKind::Integer { .. } => draft
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("{} must be a whole number.", field.label)),
        ToolFieldKind::Text | ToolFieldKind::Choice { .. } => {
            Ok(Value::String(draft.trim().to_string()))
        }
        ToolFieldKind::StringList => Ok(Value::Array(
            draft
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| Value::String(line.to_string()))
                .collect(),
        )),
    }
}

/// The settings to send: only fields whose draft differs from what was shown, and for a
/// secret only when something was typed. Fails with a message for the first draft that
/// is not a valid value (e.g. text in a number field).
pub fn changed_values(
    fields: &[ToolFieldSummary],
    drafts: &HashMap<String, String>,
) -> Result<HashMap<String, Value>, String> {
    let mut changes = HashMap::new();
    for field in fields {
        let Some(draft) = drafts.get(&field.name) else {
            continue;
        };
        if *draft == initial_draft(field) || (field.secret && draft.trim().is_empty()) {
            continue;
        }
        changes.insert(field.name.clone(), draft_to_value(field, draft)?);
    }
    Ok(changes)
}

/// Whether Save has anything valid to send.
pub fn can_save(fields: &[ToolFieldSummary], drafts: &HashMap<String, String>) -> bool {
    changed_values(fields, drafts).is_ok_and(|changes| !changes.is_empty())
}

/// The note under a field: its help text, plus where else its value can come from.
pub fn field_note(field: &ToolFieldSummary) -> String {
    let mut note = field.help.clone();
    if field.secret && field.configured {
        note.push_str(" A value is configured; leave blank to keep it.");
    }
    if !field.env_var.is_empty() {
        note.push_str(&format!(
            " The environment variable {} also works.",
            field.env_var
        ));
    }
    note.trim().to_string()
}

/// Placeholder of a text-like input: the default for plain fields, nothing for secrets.
pub fn field_placeholder(field: &ToolFieldSummary) -> String {
    if field.secret {
        return String::new();
    }
    match field.default.as_ref() {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// One input for `field`, bound to its entry in `drafts`.
#[component]
fn FieldInput(field: ToolFieldSummary, drafts: RwSignal<HashMap<String, String>>) -> impl IntoView {
    let name = field.name.clone();
    let name_for_read = name.clone();
    let draft = move || drafts.with(|map| map.get(&name_for_read).cloned().unwrap_or_default());
    let set_draft = move |text: String| {
        let key = name.clone();
        drafts.update(|map| {
            map.insert(key, text);
        });
    };
    let input_class = "w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm";
    let placeholder = field_placeholder(&field);
    let input = match field.kind.clone() {
        ToolFieldKind::Bool => view! {
            <input
                type="checkbox"
                class="h-4 w-4"
                prop:checked=move || draft() == "true"
                on:change=move |event| set_draft(event_target_checked(&event).to_string())
            />
        }
        .into_any(),
        ToolFieldKind::Choice { choices } => view! {
            <select
                class=input_class
                prop:value=draft
                on:change=move |event| set_draft(event_target_value(&event))
            >
                {choices
                    .into_iter()
                    .map(|choice| {
                        let label = if choice.is_empty() {
                            "(default)".to_string()
                        } else {
                            choice.clone()
                        };
                        view! { <option value=choice>{label}</option> }
                    })
                    .collect_view()}
            </select>
        }
        .into_any(),
        ToolFieldKind::StringList => view! {
            <textarea
                rows="3"
                class=input_class
                prop:value=draft
                on:input=move |event| set_draft(event_target_value(&event))
            ></textarea>
        }
        .into_any(),
        ToolFieldKind::Integer { min, max } => view! {
            <input
                type="number"
                class=input_class
                min=min.map(|min| min.to_string())
                max=max.map(|max| max.to_string())
                placeholder=placeholder.clone()
                prop:value=draft
                on:input=move |event| set_draft(event_target_value(&event))
            />
        }
        .into_any(),
        ToolFieldKind::Text => view! {
            <input
                type=if field.secret { "password" } else { "text" }
                autocomplete="off"
                spellcheck="false"
                class=input_class
                placeholder=placeholder.clone()
                prop:value=draft
                on:input=move |event| set_draft(event_target_value(&event))
            />
        }
        .into_any(),
    };
    let note = field_note(&field);
    view! {
        <label class="mb-3 block text-sm text-slate-700">
            <span class="mb-1 block font-medium">{field.label.clone()}</span>
            {input}
            {(!note.is_empty()).then(|| view! { <span class="mt-1 block text-xs text-slate-500">{note}</span> })}
        </label>
    }
}

/// The inline settings form of one tool section: an input per setting and a Save button
/// that stays off until something valid changed. Drafts live only in this component and
/// are rebuilt from the gateway's reply after a save.
#[component]
pub fn ToolConfigForm(
    config: ToolConfigSummary,
    #[prop(into)] can_modify: Signal<bool>,
    #[prop(into)] busy: Signal<bool>,
    on_save: Callback<ToolConfigChange>,
) -> impl IntoView {
    let drafts = RwSignal::new(initial_drafts(&config.fields));
    let fields_for_check = config.fields.clone();
    let fields_for_error = config.fields.clone();
    let fields_for_save = config.fields.clone();
    let tool = config.name.clone();
    let save_ready =
        move || can_modify.get() && !busy.get() && can_save(&fields_for_check, &drafts.get());
    let draft_error = move || changed_values(&fields_for_error, &drafts.get()).err();
    let save = move |_| {
        if let Ok(changes) = changed_values(&fields_for_save, &drafts.get_untracked()) {
            on_save.run((tool.clone(), changes));
        }
    };
    view! {
        <div class="mb-3 rounded-xl border border-slate-200 bg-slate-50 p-4 sm:ml-16">
            <p class="mb-3 text-xs text-slate-500">
                "Stored in config.json on this machine. Changes apply after a restart."
            </p>
            {config
                .fields
                .into_iter()
                .map(|field| view! { <FieldInput field=field drafts=drafts /> })
                .collect_view()}
            {move || draft_error().map(|message| view! { <p class="mb-2 text-xs text-red-600">{message}</p> })}
            <button
                type="button"
                class="rounded-full bg-slate-900 px-4 py-1.5 text-sm font-medium text-white disabled:cursor-not-allowed disabled:opacity-40"
                disabled=move || !save_ready()
                on:click=save
            >
                "Save"
            </button>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field(name: &str, kind: ToolFieldKind, value: Option<Value>) -> ToolFieldSummary {
        ToolFieldSummary {
            name: name.to_string(),
            label: name.to_string(),
            help: String::new(),
            kind,
            secret: false,
            env_var: String::new(),
            configured: value.is_some(),
            value,
            default: None,
        }
    }

    fn secret(name: &str) -> ToolFieldSummary {
        ToolFieldSummary {
            secret: true,
            configured: true,
            ..field(name, ToolFieldKind::Text, None)
        }
    }

    fn drafts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, text)| (name.to_string(), text.to_string()))
            .collect()
    }

    #[test]
    fn initial_drafts_show_stored_values_as_text() {
        let fields = vec![
            field("model", ToolFieldKind::Text, Some(json!("m"))),
            field("on", ToolFieldKind::Bool, Some(json!(true))),
            field(
                "n",
                ToolFieldKind::Integer {
                    min: None,
                    max: None,
                },
                Some(json!(20)),
            ),
            field("dirs", ToolFieldKind::StringList, Some(json!(["a", "b"]))),
            secret("apiKey"),
        ];
        let initial = initial_drafts(&fields);
        assert_eq!(initial["model"], "m");
        assert_eq!(initial["on"], "true");
        assert_eq!(initial["n"], "20");
        assert_eq!(initial["dirs"], "a\nb");
        assert_eq!(initial["apiKey"], "");
    }

    #[test]
    fn untouched_drafts_send_nothing() {
        let fields = vec![
            field("model", ToolFieldKind::Text, Some(json!("m"))),
            secret("apiKey"),
        ];
        let changes = changed_values(&fields, &initial_drafts(&fields)).unwrap();
        assert!(changes.is_empty());
        assert!(!can_save(&fields, &initial_drafts(&fields)));
    }

    #[test]
    fn only_edited_fields_are_sent_with_typed_values() {
        let fields = vec![
            field("model", ToolFieldKind::Text, Some(json!("m"))),
            field(
                "n",
                ToolFieldKind::Integer {
                    min: Some(1),
                    max: None,
                },
                Some(json!(20)),
            ),
            field("dirs", ToolFieldKind::StringList, Some(json!([]))),
            field("on", ToolFieldKind::Bool, Some(json!(false))),
        ];
        let edited = drafts(&[
            ("model", " new "),
            ("n", "5"),
            ("dirs", "x\n\n y "),
            ("on", "true"),
        ]);

        let changes = changed_values(&fields, &edited).unwrap();

        assert_eq!(changes["model"], json!("new"));
        assert_eq!(changes["n"], json!(5));
        assert_eq!(changes["dirs"], json!(["x", "y"]));
        assert_eq!(changes["on"], json!(true));
    }

    #[test]
    fn a_secret_is_sent_only_when_something_was_typed() {
        let fields = vec![secret("apiKey")];
        assert!(changed_values(&fields, &drafts(&[("apiKey", "  ")]))
            .unwrap()
            .is_empty());
        let changes = changed_values(&fields, &drafts(&[("apiKey", "sk-1")])).unwrap();
        assert_eq!(changes["apiKey"], json!("sk-1"));
    }

    #[test]
    fn text_in_a_number_field_is_reported_and_blocks_saving() {
        let fields = vec![field(
            "n",
            ToolFieldKind::Integer {
                min: None,
                max: None,
            },
            Some(json!(1)),
        )];
        let edited = drafts(&[("n", "many")]);
        assert_eq!(
            changed_values(&fields, &edited),
            Err("n must be a whole number.".to_string())
        );
        assert!(!can_save(&fields, &edited));
    }

    #[test]
    fn notes_mention_kept_secrets_and_environment_variables() {
        let mut api_key = secret("apiKey");
        api_key.help = "API key.".to_string();
        api_key.env_var = "OPENROUTER_API_KEY".to_string();
        let note = field_note(&api_key);
        assert!(note.starts_with("API key."));
        assert!(note.contains("leave blank to keep it"));
        assert!(note.contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn placeholders_show_defaults_but_never_for_secrets() {
        let mut model = field("model", ToolFieldKind::Text, None);
        model.default = Some(json!("openai/gpt-image-2"));
        assert_eq!(field_placeholder(&model), "openai/gpt-image-2");
        let mut api_key = secret("apiKey");
        api_key.default = Some(json!("x"));
        assert_eq!(field_placeholder(&api_key), "");
    }
}
