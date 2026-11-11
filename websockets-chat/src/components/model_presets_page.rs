//! The "Models" page of the Settings dialog: every model preset in `config.json`, the
//! active one marked with a green dot and an "Active" badge, with Activate, Edit and
//! Delete per row and "+ Add model preset" below the list.
//!
//! Unlike the Tools and MCP Servers pages, nothing here waits for a restart: the gateway
//! applies every change to the running agent straight away (Activate makes the preset the
//! default for every chat that did not pick its own preset in the chat input). The first
//! row is the built-in "Default (agent settings)", which can only be activated.
//!
//! Which buttons a row offers, the row texts and the notices are plain functions so they
//! are unit-testable on the host target without Leptos.

use chat_ui::models::{ModelPresetChange, ModelPresetSummary, ToolConfigSummary};
use leptos::prelude::*;

use super::add_item_panel::{AddItemButton, AddItemPanel};
use super::list_page::Tile;
use super::mcp_presets_page::is_busy;
use super::model_picker::{provider_before_model, ModelCatalog};
use super::settings_pages::{SettingsActions, SettingsPage, SettingsPanel};
use super::tool_config_form::ToolConfigForm;

/// Label of the button that opens the add panel.
pub const ADD_BUTTON_LABEL: &str = "+ Add model preset";

/// Heading of the add panel.
const ADD_PANEL_TITLE: &str = "Add model preset";

/// Key the page's `busy` signal holds while an add request is in flight. Preset names
/// never contain '+', so it cannot be mistaken for a row.
pub const ADD_BUSY_KEY: &str = "+add-model-preset";

/// Hint under the name input of the add panel.
const NAME_HINT: &str =
    "Letters, digits, _ - . and spaces, e.g. openai-gpt-6.1-terra. \"default\" is reserved.";

/// The line above a preset's settings form: changes apply without a restart.
pub const LIVE_FORM_NOTE: &str =
    "Stored in config.json on this machine. Applies from the next message; no restart needed.";

/// Shown on a row whose provider has no API key configured.
const MISSING_KEY_HINT: &str = "No API key is configured for this provider in config.json.";

/// The buttons a row offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowActions {
    /// "Activate": every row that is not already the active one.
    pub activate: bool,
    /// "Edit": presets with a form (never the built-in row).
    pub edit: bool,
    /// "Delete" is shown: every preset except the built-in row.
    pub delete: bool,
    /// "Delete" can be pressed: not the active preset, not Dream's preset.
    pub delete_enabled: bool,
}

/// The buttons `row` offers to a connection that may change the configuration
/// (`can_modify`); none otherwise.
pub fn row_actions(row: &ModelPresetSummary, can_modify: bool) -> RowActions {
    if !can_modify {
        return RowActions {
            activate: false,
            edit: false,
            delete: false,
            delete_enabled: false,
        };
    }
    RowActions {
        activate: !row.active,
        edit: !row.built_in && row.form.is_some(),
        delete: !row.built_in,
        delete_enabled: !row.built_in && row.deletable,
    }
}

/// The bold heading of a row: its label, else its name.
pub fn row_title(row: &ModelPresetSummary) -> String {
    row.label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(&row.name)
        .to_string()
}

/// The muted line under a row's heading: `model · provider`.
pub fn row_subline(row: &ModelPresetSummary) -> String {
    format!("{} · {}", row.model, row.provider)
}

/// The name a notice uses for the preset `name` among `rows` (the built-in row by its
/// label).
fn display_name(rows: &[ModelPresetSummary], name: &str) -> String {
    rows.iter()
        .find(|row| row.name == name)
        .map(row_title)
        .unwrap_or_else(|| name.to_string())
}

/// The confirmation shown after a successful change, read against the fresh `rows`.
pub fn notice_for(change: &ModelPresetChange, rows: &[ModelPresetSummary]) -> String {
    let name = display_name(rows, &change.name);
    match change.action.as_str() {
        "added" => format!("Model preset '{name}' added."),
        "updated" => format!("Model preset '{name}' saved. It applies from the next message."),
        "deleted" => format!("Model preset '{name}' deleted."),
        "activated" => {
            format!("'{name}' is now the active model. Chats that picked their own preset keep it.")
        }
        _ => format!("Model preset '{name}' changed."),
    }
}

/// The names the chat input's preset picker offers: every row, the built-in `default`
/// first, as the gateway lists them.
pub fn picker_names(rows: &[ModelPresetSummary]) -> Vec<String> {
    rows.iter().map(|row| row.name.clone()).collect()
}

/// The warning in the add panel when some providers have no API key, or `None`.
pub fn missing_key_note(providers_without_key: &[String]) -> Option<String> {
    let named: Vec<&str> = providers_without_key
        .iter()
        .map(String::as_str)
        .filter(|provider| *provider != "auto")
        .collect();
    (!named.is_empty()).then(|| {
        format!(
            "No API key is configured for: {}. A preset using one of them will fail until its key is set under providers in config.json.",
            named.join(", ")
        )
    })
}

/// The question asked before deleting the preset `name`.
pub fn delete_question(name: &str) -> String {
    format!("Delete model preset '{name}'? This cannot be undone.")
}

/// Tailwind classes of the small outlined buttons of a row.
const ROW_BUTTON_CLASS: &str = "rounded-full border border-slate-200 px-3 py-1 text-sm text-slate-600 hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-40";

/// The model lists of the panel, asked for through `actions`, for the preset forms.
fn model_catalog(panel: SettingsPanel, actions: SettingsActions) -> ModelCatalog {
    ModelCatalog {
        models: panel.provider_models,
        request: actions.on_list_provider_models,
    }
}

/// `form` laid out for the Models page: the provider before the model, so the model
/// drop-down follows the chosen provider.
fn with_provider_first(form: ToolConfigSummary) -> ToolConfigSummary {
    ToolConfigSummary {
        fields: provider_before_model(form.fields),
        ..form
    }
}

/// The "Active" badge: a green dot and the word.
#[component]
fn ActiveBadge() -> impl IntoView {
    view! {
        <span class="inline-flex items-center gap-1.5 rounded-full bg-emerald-50 px-2 py-0.5 text-xs font-medium text-emerald-700">
            <span class="h-2 w-2 rounded-full bg-emerald-500" aria-hidden="true"></span>
            "Active"
        </span>
    }
}

/// The open settings form of the preset `config.name`, editing the drafts the panel
/// keeps for it.
#[component]
fn PresetSettingsForm(
    config: ToolConfigSummary,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let drafts =
        RwSignal::from(panel.row_drafts(SettingsPage::Models, &config.name, &config.fields));
    let name_for_busy = config.name.clone();
    view! {
        <div class="mt-3">
            <ToolConfigForm
                config=with_provider_first(config)
                drafts=drafts
                can_modify=Signal::derive(move || panel.can_change())
                busy=Signal::derive(move || is_busy(&panel.busy.get(), &name_for_busy))
                on_save=actions.on_save_model_preset
                note=LIVE_FORM_NOTE
                model_catalog=Some(model_catalog(panel, actions))
            />
        </div>
    }
}

/// The inline "Delete …?" question with its Delete and Cancel buttons.
#[component]
fn DeleteConfirmation(
    name: String,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let question = delete_question(&name);
    view! {
        <div class="mt-3 flex flex-wrap items-center gap-2 rounded-lg bg-red-50 px-3 py-2 text-sm text-red-800">
            <span class="flex-1">{question}</span>
            <button
                type="button"
                class="rounded-full border border-slate-300 bg-white px-3 py-1 text-sm text-slate-700 hover:bg-slate-100"
                on:click=move |_| panel.confirm_delete_preset.set(None)
            >
                "Cancel"
            </button>
            <button
                type="button"
                class="rounded-full bg-red-600 px-3 py-1 text-sm font-medium text-white hover:bg-red-700 disabled:opacity-40"
                disabled=move || !panel.can_change()
                on:click=move |_| {
                    panel.confirm_delete_preset.set(None);
                    actions.on_delete_model_preset.run(name.clone());
                }
            >
                "Delete"
            </button>
        </div>
    }
}

/// One preset row: tile, title with the Active badge, `model · provider`, warnings, the
/// buttons, and the open form, delete question and error underneath.
#[component]
fn ModelPresetRowView(
    row: ModelPresetSummary,
    /// Whether the connection may change the configuration (buttons are offered).
    can_modify: bool,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let actions_offered = row_actions(&row, can_modify);
    let title = row_title(&row);
    let subline = row_subline(&row);
    let name = row.name.clone();
    let name_for_activate = row.name.clone();
    let name_for_edit = row.name.clone();
    let name_for_form = row.name.clone();
    let name_for_delete = row.name.clone();
    let name_for_question = row.name.clone();
    let name_for_error = row.name.clone();
    // One change at a time: every button waits while any request is in flight.
    let disabled = Signal::derive(move || !panel.can_change() || panel.busy.get().is_some());
    let blocked_reason = row.delete_blocked_reason.clone();
    let card_class = if row.active {
        "rounded-xl border border-emerald-200 bg-white p-4"
    } else {
        "rounded-xl border border-slate-200 bg-white p-4"
    };
    view! {
        <li class=card_class>
            <div class="flex flex-wrap items-start gap-4">
                <Tile title=title.clone() />
                <div class="min-w-0 flex-1">
                    <div class="flex flex-wrap items-center gap-2">
                        <span class="break-words text-sm font-semibold text-slate-900">{title}</span>
                        {row.active.then(|| view! { <ActiveBadge /> })}
                    </div>
                    <p class="break-words text-sm text-slate-500">{subline}</p>
                    {(!row.built_in && row.label.is_some())
                        .then(|| view! { <p class="text-xs text-slate-400">{name.clone()}</p> })}
                    {row.missing_api_key
                        .then(|| view! { <p class="mt-1 text-xs font-medium text-amber-700">{MISSING_KEY_HINT}</p> })}
                </div>
                <div class="flex flex-wrap items-center gap-2">
                    {actions_offered
                        .activate
                        .then(|| {
                            view! {
                                <button
                                    type="button"
                                    class="rounded-full bg-slate-900 px-3 py-1 text-sm font-medium text-white hover:bg-slate-700 disabled:cursor-not-allowed disabled:opacity-40"
                                    disabled=move || disabled.get()
                                    on:click=move |_| actions.on_activate_model_preset.run(name_for_activate.clone())
                                >
                                    "Activate"
                                </button>
                            }
                        })}
                    {actions_offered
                        .edit
                        .then(|| {
                            view! {
                                <button
                                    type="button"
                                    class=ROW_BUTTON_CLASS
                                    on:click=move |_| {
                                        let target = name_for_edit.clone();
                                        panel.confirm_delete_preset.set(None);
                                        panel
                                            .editing_preset
                                            .update(|open| {
                                                *open = if open.as_deref() == Some(target.as_str()) {
                                                    None
                                                } else {
                                                    Some(target)
                                                };
                                            });
                                    }
                                >
                                    "Edit"
                                </button>
                            }
                        })}
                    {actions_offered
                        .delete
                        .then(|| {
                            let enabled = actions_offered.delete_enabled;
                            view! {
                                <button
                                    type="button"
                                    class=ROW_BUTTON_CLASS
                                    title=blocked_reason.clone()
                                    disabled=move || !enabled || disabled.get()
                                    on:click=move |_| {
                                        panel.editing_preset.set(None);
                                        panel.confirm_delete_preset.set(Some(name_for_delete.clone()));
                                    }
                                >
                                    "Delete"
                                </button>
                            }
                        })}
                </div>
            </div>
            {row
                .form
                .clone()
                .filter(|_| actions_offered.edit)
                .map(|config| {
                    view! {
                        <Show when=move || {
                            panel.editing_preset.get().as_deref() == Some(name_for_form.as_str())
                        }>
                            <PresetSettingsForm config=config.clone() panel=panel actions=actions />
                        </Show>
                    }
                })}
            {move || {
                (panel.confirm_delete_preset.get().as_deref() == Some(name_for_question.as_str()))
                    .then(|| {
                        view! {
                            <DeleteConfirmation
                                name=name_for_question.clone()
                                panel=panel
                                actions=actions
                            />
                        }
                    })
            }}
            {move || {
                panel
                    .error
                    .get()
                    .filter(|(failed_name, _)| *failed_name == name_for_error)
                    .map(|(_, message)| view! { <p class="mt-2 text-xs text-red-600">{message}</p> })
            }}
        </li>
    }
}

/// "+ Add model preset", or its panel while it is open. Nothing for a connection that
/// may not change the configuration (the gateway sends it no blank form).
#[component]
fn AddPresetSection(panel: SettingsPanel, actions: SettingsActions) -> impl IntoView {
    view! {
        {move || {
            let form = panel.new_preset_form.get().filter(|_| panel.can_modify.get())?;
            Some(
                if panel.adding.get() {
                    let note = missing_key_note(&panel.providers_without_key.get_untracked());
                    view! {
                        <AddItemPanel
                            title=ADD_PANEL_TITLE
                            name_hint=NAME_HINT
                            form=with_provider_first(form)
                            can_change=Signal::derive(move || panel.can_change())
                            busy=Signal::derive(move || is_busy(&panel.busy.get(), ADD_BUSY_KEY))
                            error=panel.add_error
                            on_apply=actions.on_add_model_preset
                            on_cancel=Callback::new(move |()| panel.close_add_panel())
                            note=note.unwrap_or_default()
                            model_catalog=Some(model_catalog(panel, actions))
                        />
                    }
                        .into_any()
                } else {
                    view! {
                        <AddItemButton
                            label=ADD_BUTTON_LABEL
                            disabled=Signal::derive(move || !panel.can_change())
                            on_click=Callback::new(move |()| {
                                panel.add_error.set(None);
                                panel.adding.set(true);
                            })
                        />
                    }
                        .into_any()
                },
            )
        }}
    }
}

/// The body of the Models page below the heading: the notice of the last change, the
/// rows and the add button.
#[component]
pub fn ModelPresetsSection(panel: SettingsPanel, actions: SettingsActions) -> impl IntoView {
    view! {
        <div class="min-h-0 flex-1 overflow-y-auto">
            {move || {
                panel
                    .model_notice
                    .get()
                    .map(|message| {
                        view! {
                            <p
                                role="status"
                                class="mt-3 rounded-lg bg-emerald-50 px-3 py-2 text-sm text-emerald-800"
                            >
                                {message}
                            </p>
                        }
                    })
            }}
            {move || {
                if !panel.model_presets_loaded.get() {
                    return view! { <p class="pt-6 text-sm text-slate-400">"Loading…"</p> }.into_any();
                }
                let rows = panel.model_presets.get();
                let can_modify = panel.can_modify.get();
                view! {
                    <ul class="mt-4 space-y-3">
                        {rows
                            .into_iter()
                            .map(|row| {
                                view! {
                                    <ModelPresetRowView
                                        row=row
                                        can_modify=can_modify
                                        panel=panel
                                        actions=actions
                                    />
                                }
                            })
                            .collect_view()}
                    </ul>
                }
                    .into_any()
            }}
            <AddPresetSection panel=panel actions=actions />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire name of the built-in row standing for the flat agent settings.
    const BUILT_IN_PRESET_NAME: &str = "default";

    fn preset(name: &str, active: bool) -> ModelPresetSummary {
        ModelPresetSummary {
            name: name.to_string(),
            label: None,
            model: format!("{name}-model"),
            provider: "openai".to_string(),
            active,
            built_in: false,
            deletable: !active,
            delete_blocked_reason: if active {
                "it is the active preset.".to_string()
            } else {
                String::new()
            },
            missing_api_key: false,
            form: Some(ToolConfigSummary {
                name: name.to_string(),
                title: name.to_string(),
                description: String::new(),
                enabled: None,
                tool_names: Vec::new(),
                fields: Vec::new(),
            }),
        }
    }

    fn built_in(active: bool) -> ModelPresetSummary {
        ModelPresetSummary {
            label: Some("Default (agent settings)".to_string()),
            built_in: true,
            deletable: false,
            form: None,
            ..preset(BUILT_IN_PRESET_NAME, active)
        }
    }

    fn change(action: &str, name: &str) -> ModelPresetChange {
        ModelPresetChange {
            action: action.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn an_inactive_preset_offers_activate_edit_and_delete() {
        assert_eq!(
            row_actions(&preset("terra", false), true),
            RowActions {
                activate: true,
                edit: true,
                delete: true,
                delete_enabled: true
            }
        );
    }

    #[test]
    fn the_active_preset_cannot_be_activated_again_or_deleted() {
        let actions = row_actions(&preset("terra", true), true);
        assert!(!actions.activate && actions.edit);
        assert!(actions.delete && !actions.delete_enabled);
    }

    #[test]
    fn a_preset_dream_uses_shows_delete_disabled() {
        let mut dreamy = preset("dreamy", false);
        dreamy.deletable = false;
        dreamy.delete_blocked_reason = "Dream uses it".to_string();

        let actions = row_actions(&dreamy, true);

        assert!(actions.activate && actions.delete && !actions.delete_enabled);
    }

    #[test]
    fn the_built_in_row_can_only_be_activated() {
        assert_eq!(
            row_actions(&built_in(false), true),
            RowActions {
                activate: true,
                edit: false,
                delete: false,
                delete_enabled: false
            }
        );
        assert!(!row_actions(&built_in(true), true).activate);
    }

    #[test]
    fn a_read_only_connection_gets_no_buttons() {
        let none = RowActions {
            activate: false,
            edit: false,
            delete: false,
            delete_enabled: false,
        };
        assert_eq!(row_actions(&preset("terra", false), false), none);
        assert_eq!(row_actions(&built_in(false), false), none);
    }

    #[test]
    fn rows_are_titled_by_label_else_name_with_model_and_provider_below() {
        let mut labelled = preset("terra", false);
        labelled.label = Some("Terra".to_string());
        assert_eq!(row_title(&labelled), "Terra");
        assert_eq!(row_title(&preset("terra", false)), "terra");
        labelled.label = Some("  ".to_string());
        assert_eq!(row_title(&labelled), "terra");
        assert_eq!(row_subline(&preset("terra", false)), "terra-model · openai");
    }

    #[test]
    fn notices_say_what_happened_to_which_preset() {
        let rows = vec![built_in(true), preset("terra", false)];

        assert_eq!(
            notice_for(&change("added", "terra"), &rows),
            "Model preset 'terra' added."
        );
        assert!(notice_for(&change("updated", "terra"), &rows).contains("next message"));
        assert_eq!(
            notice_for(&change("deleted", "gone"), &rows),
            "Model preset 'gone' deleted."
        );
        let activated = notice_for(&change("activated", "default"), &rows);
        assert!(activated.starts_with("'Default (agent settings)' is now the active model"));
        assert!(activated.contains("keep it"));
    }

    #[test]
    fn the_picker_offers_every_row_with_default_first() {
        let rows = vec![
            built_in(false),
            preset("fast", true),
            preset("terra", false),
        ];
        assert_eq!(picker_names(&rows), ["default", "fast", "terra"]);
    }

    #[test]
    fn the_add_panel_warns_about_providers_without_a_key_but_not_about_auto() {
        assert_eq!(missing_key_note(&[]), None);
        assert_eq!(missing_key_note(&["auto".to_string()]), None);
        let note = missing_key_note(&["openai".to_string(), "together".to_string()]).unwrap();
        assert!(note.contains("openai, together"));
    }

    #[test]
    fn deleting_asks_first_naming_the_preset() {
        assert!(delete_question("terra").contains("'terra'"));
    }

    #[test]
    fn the_busy_key_cannot_be_a_preset_name() {
        assert!(ADD_BUSY_KEY.contains('+'));
    }
}
