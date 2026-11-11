//! The dashed "+ Add …" button and the panel it opens, shared by the Settings pages that
//! can add an entry to `config.json` (an MCP server, a model preset).
//!
//! The button sits next to the list. Clicking it swaps it for a panel: a name input and
//! the same settings form an existing entry has (the gateway sends the blank form), with
//! Cancel and Apply. The button comes back when the panel is cancelled or the entry was
//! added. The panel keeps its drafts to itself; only what the user changed is sent.
//!
//! The rule for when Apply is available is a plain function so it is unit-testable on
//! the host target without Leptos.

use std::collections::HashMap;

use chat_ui::models::{ToolConfigSummary, ToolFieldSummary};
use leptos::prelude::*;

use super::model_picker::ModelCatalog;
use super::tool_config_form::{changed_values, initial_drafts, FieldInput, ToolConfigChange};

/// Whether Apply can be pressed: changes are allowed, no request is in flight, the entry
/// has a name, and the filled-in settings are valid and not empty (the gateway checks
/// which ones are required).
pub fn apply_ready(
    name: &str,
    fields: &[ToolFieldSummary],
    drafts: &HashMap<String, String>,
    can_change: bool,
    busy: bool,
) -> bool {
    can_change
        && !busy
        && !name.trim().is_empty()
        && changed_values(fields, drafts).is_ok_and(|changes| !changes.is_empty())
}

/// The dashed button that opens the panel, labelled `label` (e.g. "+ Add MCP Server").
#[component]
pub fn AddItemButton(
    label: &'static str,
    #[prop(into)] disabled: Signal<bool>,
    on_click: Callback<()>,
) -> impl IntoView {
    view! {
        <button
            type="button"
            class="mt-4 w-full rounded-xl border-2 border-dashed border-slate-300 px-4 py-4 text-sm font-medium text-slate-600 hover:border-orange-500 hover:text-orange-600 disabled:cursor-not-allowed disabled:opacity-50"
            disabled=move || disabled.get()
            on:click=move |_| on_click.run(())
        >
            {label}
        </button>
    }
}

/// The panel that replaces the button: heading `title`, name (with `name_hint` under
/// it), an optional `note` (e.g. a warning), the settings, Cancel and Apply, and the last
/// failure of the add (if any) underneath.
#[component]
pub fn AddItemPanel(
    title: &'static str,
    name_hint: &'static str,
    /// The blank settings form the gateway sent.
    form: ToolConfigSummary,
    #[prop(into)] can_change: Signal<bool>,
    /// Whether the add request is in flight.
    #[prop(into)]
    busy: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    on_apply: Callback<ToolConfigChange>,
    on_cancel: Callback<()>,
    #[prop(optional, into)] note: Option<String>,
    /// Offers the provider's models under the `model` input (model preset forms).
    #[prop(optional_no_strip)]
    model_catalog: Option<ModelCatalog>,
) -> impl IntoView {
    let drafts = RwSignal::new(initial_drafts(&form.fields));
    let name = RwSignal::new(String::new());
    let fields_for_check = form.fields.clone();
    let fields_for_error = form.fields.clone();
    let fields_for_apply = form.fields.clone();
    let apply_enabled = move || {
        apply_ready(
            &name.get(),
            &fields_for_check,
            &drafts.get(),
            can_change.get(),
            busy.get(),
        )
    };
    let draft_error = move || changed_values(&fields_for_error, &drafts.get()).err();
    let apply = move |_| {
        if let Ok(changes) = changed_values(&fields_for_apply, &drafts.get_untracked()) {
            on_apply.run((name.get_untracked().trim().to_string(), changes));
        }
    };
    view! {
        <div class="mt-4 mb-3 rounded-xl border border-slate-200 bg-slate-50 p-4">
            <h4 class="mb-3 text-sm font-semibold text-slate-900">{title}</h4>
            <label class="mb-3 block text-sm text-slate-700">
                <span class="mb-1 block font-medium">"Name"</span>
                <input
                    type="text"
                    autocomplete="off"
                    spellcheck="false"
                    class="w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm"
                    prop:value=move || name.get()
                    on:input=move |event| name.set(event_target_value(&event))
                />
                <span class="mt-1 block text-xs text-slate-500">{name_hint}</span>
            </label>
            {note
                .filter(|note| !note.is_empty())
                .map(|note| view! { <p class="mb-3 text-xs text-amber-700">{note}</p> })}
            {form
                .fields
                .into_iter()
                .map(|field| {
                    view! { <FieldInput field=field drafts=drafts model_catalog=model_catalog /> }
                })
                .collect_view()}
            {move || {
                draft_error()
                    .or_else(|| error.get())
                    .map(|message| {
                        view! {
                            <p role="alert" class="mb-2 text-xs text-red-600">
                                {message}
                            </p>
                        }
                    })
            }}
            <div class="flex gap-2">
                <button
                    type="button"
                    class="rounded-full border border-slate-300 px-4 py-1.5 text-sm text-slate-700 hover:bg-slate-100"
                    on:click=move |_| on_cancel.run(())
                >
                    "Cancel"
                </button>
                <button
                    type="button"
                    class="rounded-full bg-slate-900 px-4 py-1.5 text-sm font-medium text-white disabled:cursor-not-allowed disabled:opacity-40"
                    disabled=move || !apply_enabled()
                    on:click=apply
                >
                    "Apply"
                </button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_ui::models::ToolFieldKind;
    use serde_json::json;

    fn text_field(name: &str, default: &str) -> ToolFieldSummary {
        ToolFieldSummary {
            name: name.to_string(),
            label: name.to_string(),
            help: String::new(),
            kind: ToolFieldKind::Text,
            secret: false,
            env_var: String::new(),
            configured: false,
            value: Some(json!(default)),
            default: Some(json!(default)),
        }
    }

    fn drafts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, text)| (name.to_string(), text.to_string()))
            .collect()
    }

    #[test]
    fn apply_needs_a_name_and_at_least_one_filled_in_setting() {
        let fields = vec![text_field("command", "")];
        let empty = drafts(&[("command", "")]);
        let filled = drafts(&[("command", "npx")]);

        assert!(apply_ready("cms", &fields, &filled, true, false));
        assert!(!apply_ready("", &fields, &filled, true, false), "no name");
        assert!(
            !apply_ready("  ", &fields, &filled, true, false),
            "blank name"
        );
        assert!(
            !apply_ready("cms", &fields, &empty, true, false),
            "nothing filled in"
        );
    }

    #[test]
    fn apply_waits_while_changes_are_not_allowed_or_a_request_is_in_flight() {
        let fields = vec![text_field("command", "")];
        let filled = drafts(&[("command", "npx")]);

        assert!(!apply_ready("cms", &fields, &filled, false, false));
        assert!(!apply_ready("cms", &fields, &filled, true, true));
    }

    #[test]
    fn apply_is_off_while_a_filled_in_value_is_invalid() {
        let mut number = text_field("toolTimeout", "30");
        number.kind = ToolFieldKind::Integer {
            min: Some(1),
            max: None,
        };
        let fields = vec![text_field("command", ""), number];

        let invalid = drafts(&[("command", "npx"), ("toolTimeout", "soon")]);

        assert!(!apply_ready("cms", &fields, &invalid, true, false));
    }
}
