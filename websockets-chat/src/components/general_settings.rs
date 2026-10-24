//! The "General" block at the top of the Tools page: the settings that apply to all
//! tools (restrict to workspace, confirm before execute, SSRF whitelist), laid out as
//! plain rows with the label and help text on the left and the control on the right.
//!
//! Unlike a tool's settings form there is no Save button: each control saves its setting
//! as soon as it is changed (switch flipped, option picked, text committed with Enter or
//! by leaving the field). The gateway marks the change as needing a restart.

use std::collections::HashMap;

use chat_ui::models::{ToolConfigSummary, ToolFieldKind, ToolFieldSummary};
use leptos::prelude::*;

use super::mcp_presets_page::{switch_hint, ToggleSwitch};
use super::tool_config_form::{draft_to_value, field_note, initial_draft, ToolConfigChange};

/// Tailwind classes shared by the text-like controls.
const CONTROL_CLASS: &str =
    "w-full sm:w-64 rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900";

/// The change to send when `field` is committed with `draft`, or `None` when the draft is
/// what is already stored. Fails with a message when the draft is not a valid value.
pub fn commit_change(
    tool: &str,
    field: &ToolFieldSummary,
    draft: &str,
) -> Result<Option<ToolConfigChange>, String> {
    if draft == initial_draft(field) {
        return Ok(None);
    }
    let value = draft_to_value(field, draft)?;
    Ok(Some((
        tool.to_string(),
        HashMap::from([(field.name.clone(), value)]),
    )))
}

/// Layout of a setting's row: switches stay beside their label on every screen; other
/// controls go below the label on phones and beside it from the `sm` breakpoint.
pub fn row_class(kind: &ToolFieldKind) -> &'static str {
    if matches!(kind, ToolFieldKind::Bool) {
        "flex items-start justify-between gap-4 py-4 sm:gap-6"
    } else {
        "flex flex-col gap-3 py-4 sm:flex-row sm:items-start sm:justify-between sm:gap-6"
    }
}

/// Classes of the box around a control: full width on phones for the wide controls.
pub fn control_wrapper_class(kind: &ToolFieldKind) -> &'static str {
    if matches!(kind, ToolFieldKind::Bool) {
        "shrink-0"
    } else {
        "w-full sm:w-auto sm:shrink-0"
    }
}

/// One setting: label and note on the left, its control on the right.
#[component]
fn GeneralFieldRow(
    tool: String,
    field: ToolFieldSummary,
    #[prop(into)] can_modify: Signal<bool>,
    #[prop(into)] busy: Signal<bool>,
    on_save: Callback<ToolConfigChange>,
) -> impl IntoView {
    let draft = RwSignal::new(initial_draft(&field));
    let parse_error = RwSignal::new(None::<String>);
    let disabled = Signal::derive(move || !can_modify.get() || busy.get());
    let field_for_commit = field.clone();
    let commit = move |text: String| {
        draft.set(text.clone());
        match commit_change(&tool, &field_for_commit, &text) {
            Ok(change) => {
                parse_error.set(None);
                if let Some(change) = change {
                    on_save.run(change);
                }
            }
            Err(message) => parse_error.set(Some(message)),
        }
    };
    let on_text = commit.clone();
    let on_toggle = commit.clone();
    let control = match field.kind.clone() {
        ToolFieldKind::Bool => {
            let on = draft.get_untracked() == "true";
            view! {
                <ToggleSwitch
                    on=on
                    label=field.label.clone()
                    disabled=disabled
                    hint=Signal::derive(move || switch_hint(can_modify.get()))
                    on_toggle=Callback::new(move |()| on_toggle((!on).to_string()))
                />
            }
            .into_any()
        }
        ToolFieldKind::Choice { choices } => view! {
            <select
                class=CONTROL_CLASS
                disabled=move || disabled.get()
                prop:value=move || draft.get()
                on:change=move |event| commit(event_target_value(&event))
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
                class=CONTROL_CLASS
                disabled=move || disabled.get()
                prop:value=move || draft.get()
                on:change=move |event| on_text(event_target_value(&event))
            ></textarea>
        }
        .into_any(),
        ToolFieldKind::Integer { min, max } => view! {
            <input
                type="number"
                class=CONTROL_CLASS
                min=min.map(|min| min.to_string())
                max=max.map(|max| max.to_string())
                disabled=move || disabled.get()
                prop:value=move || draft.get()
                on:change=move |event| on_text(event_target_value(&event))
            />
        }
        .into_any(),
        ToolFieldKind::Text => view! {
            <input
                type=if field.secret { "password" } else { "text" }
                autocomplete="off"
                spellcheck="false"
                class=CONTROL_CLASS
                disabled=move || disabled.get()
                prop:value=move || draft.get()
                on:change=move |event| on_text(event_target_value(&event))
            />
        }
        .into_any(),
    };
    let note = field_note(&field);
    let row_class = row_class(&field.kind);
    let control_class = control_wrapper_class(&field.kind);
    view! {
        <li class=row_class>
            <div class="min-w-0 flex-1">
                <p class="text-sm font-medium text-slate-900">{field.label.clone()}</p>
                <p class="text-sm text-slate-500">{note}</p>
                {move || {
                    parse_error
                        .get()
                        .map(|message| view! { <p class="mt-1 text-xs text-red-600">{message}</p> })
                }}
            </div>
            <div class=control_class>{control}</div>
        </li>
    }
}

/// The General block: one row per setting of the section, with the section's last
/// failure (if any) underneath.
#[component]
pub fn GeneralSettings(
    config: ToolConfigSummary,
    #[prop(into)] can_modify: Signal<bool>,
    #[prop(into)] busy: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    on_save: Callback<ToolConfigChange>,
) -> impl IntoView {
    let tool = config.name.clone();
    view! {
        <section class="mt-4">
            <h4 class="text-sm font-semibold text-slate-900">{config.title.clone()}</h4>
            <ul class="divide-y divide-slate-100">
                {config
                    .fields
                    .into_iter()
                    .map(|field| {
                        view! {
                            <GeneralFieldRow
                                tool=tool.clone()
                                field=field
                                can_modify=can_modify
                                busy=busy
                                on_save=on_save
                            />
                        }
                    })
                    .collect_view()}
            </ul>
            {move || {
                error.get().map(|message| view! { <p class="text-xs text-red-600">{message}</p> })
            }}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field(kind: ToolFieldKind, value: serde_json::Value) -> ToolFieldSummary {
        ToolFieldSummary {
            name: "restrictToWorkspace".to_string(),
            label: "Restrict to workspace".to_string(),
            help: String::new(),
            kind,
            secret: false,
            env_var: String::new(),
            configured: true,
            value: Some(value),
            default: None,
        }
    }

    #[test]
    fn switches_stay_beside_their_label_and_other_controls_stack_on_phones() {
        assert!(!row_class(&ToolFieldKind::Bool).contains("flex-col"));
        assert!(row_class(&ToolFieldKind::Text).contains("flex-col"));
        assert_eq!(control_wrapper_class(&ToolFieldKind::Bool), "shrink-0");
        assert!(control_wrapper_class(&ToolFieldKind::StringList).contains("w-full"));
    }

    #[test]
    fn an_unchanged_value_sends_nothing() {
        let setting = field(ToolFieldKind::Bool, json!(false));
        assert_eq!(commit_change("general", &setting, "false"), Ok(None));
    }

    #[test]
    fn a_changed_value_is_sent_alone_under_its_name() {
        let setting = field(ToolFieldKind::Bool, json!(false));
        let (tool, values) = commit_change("general", &setting, "true").unwrap().unwrap();
        assert_eq!(tool, "general");
        assert_eq!(
            values,
            HashMap::from([("restrictToWorkspace".to_string(), json!(true))])
        );
    }

    #[test]
    fn a_list_is_sent_as_trimmed_lines() {
        let setting = field(ToolFieldKind::StringList, json!([]));
        let (_, values) = commit_change("general", &setting, "10.0.0.0/8\n\n 100.64.0.0/10 ")
            .unwrap()
            .unwrap();
        assert_eq!(
            values["restrictToWorkspace"],
            json!(["10.0.0.0/8", "100.64.0.0/10"])
        );
    }

    #[test]
    fn an_invalid_number_is_reported_instead_of_sent() {
        let setting = field(
            ToolFieldKind::Integer {
                min: None,
                max: None,
            },
            json!(1),
        );
        assert!(commit_change("general", &setting, "many").is_err());
    }
}
