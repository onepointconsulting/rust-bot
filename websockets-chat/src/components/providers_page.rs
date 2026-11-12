//! The "Providers" page of the Settings dialog: every LLM provider in `config.json`
//! (dedicated slots such as Anthropic and named custom endpoints), with Edit and Delete
//! per row and "+ Add provider" below the list.
//!
//! "+ Add provider" opens a panel with a two-way toggle:
//!
//! * **Dedicated model provider**: pick OpenAI, Anthropic, OpenRouter or Together AI and
//!   enter the API key (blank means "use the environment variable", with a warning when
//!   it is not set). A collapsible "Customized settings" section holds the Base URL, the
//!   API protocol and extra headers.
//! * **Custom model provider**: a name, Base URL, API protocol, API key and headers. Model
//!   presets select it by its name.
//!
//! API keys never reach the browser: a stored key shows as "configured", a blank key
//! field keeps it, and "Forget stored key" clears it. Like the Models page, nothing here
//! waits for a restart: the gateway applies every change to the running agent.
//!
//! The texts, the readiness rules and the notices are plain functions so they are
//! unit-testable on the host target without Leptos.

use std::collections::HashMap;

use chat_ui::models::{
    BackendChoice, DedicatedProviderChoice, ProviderChange, ProviderSummary, ToolConfigSummary,
    ToolFieldSummary,
};
use leptos::prelude::*;
use serde_json::Value;

use super::add_item_panel::AddItemButton;
use super::key_value_rows::KeyValueRows;
use super::list_page::Tile;
use super::mcp_presets_page::is_busy;
use super::segmented_toggle::SegmentedToggle;
use super::settings_pages::{FormDrafts, SettingsActions, SettingsPage, SettingsPanel};
use super::tool_config_form::{changed_values, initial_drafts, ToolConfigChange};

/// Label of the button that opens the add panel.
pub const ADD_BUTTON_LABEL: &str = "+ Add provider";

/// Key the page's `busy` signal holds while an add request is in flight. Provider names
/// never contain '+', so it cannot be mistaken for a row.
pub const ADD_BUSY_KEY: &str = "+add-provider";

/// Labels of the add panel's two tabs, in toggle order (see [`ProviderFormKind`]).
const TAB_LABELS: [&str; 2] = ["Dedicated model provider", "Custom model provider"];

/// The line above a provider's settings form: changes apply without a restart.
pub const LIVE_FORM_NOTE: &str =
    "Stored in config.json on this machine. Applies from the next message; no restart needed.";

/// Hint under the name input of the custom tab.
const NAME_HINT: &str =
    "Letters, digits, _ - . and spaces, e.g. my-relay. Model presets select the provider by this name.";

/// Placeholder of the custom provider's Base URL.
const CUSTOM_BASE_URL_PLACEHOLDER: &str = "https://gateway.example/v1";

/// Name of the setting form fields the page lays out by hand.
const API_KEY: &str = "apiKey";
const API_BASE: &str = "apiBase";
const EXTRA_HEADERS: &str = "extraHeaders";
const PROVIDER_BACK_END: &str = "providerBackEnd";

/// What the gateway sent besides the rows: the add panel's choices and the blank form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderExtras {
    /// The providers of the Dedicated tab.
    pub dedicated_choices: Vec<DedicatedProviderChoice>,
    /// The API protocol drop-down, automatic first.
    pub backend_choices: Vec<BackendChoice>,
    /// The blank form both tabs pick their fields from; only for connections that may
    /// change the configuration.
    pub new_form: Option<ToolConfigSummary>,
}

/// What the add button sends: `(kind wire name, provider name, changed settings)`.
pub type ProviderAddRequest = (&'static str, String, HashMap<String, Value>);

/// Which kind of provider a form edits; also the add panel's tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFormKind {
    /// A built-in slot (Anthropic, OpenAI, ...). The first tab.
    Dedicated,
    /// A named endpoint. The second tab.
    Custom,
}

impl ProviderFormKind {
    /// The kind of a row, from its wire `kind` (anything but `dedicated` is custom).
    pub fn from_wire(kind: &str) -> Self {
        if kind == "dedicated" {
            Self::Dedicated
        } else {
            Self::Custom
        }
    }

    /// The kind of the add panel's tab `index`.
    pub fn from_tab(index: usize) -> Self {
        if index == 0 {
            Self::Dedicated
        } else {
            Self::Custom
        }
    }

    /// The name the gateway's `add_provider` envelope uses.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Dedicated => "dedicated",
            Self::Custom => "custom",
        }
    }
}

/// Whether a form adds a provider or edits an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMode {
    Add,
    Edit,
}

/// What a form needs to know about the provider it configures, for its hints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderHints {
    /// The environment variable that supplies the key, when the provider has one.
    pub env_var: Option<String>,
    /// Whether that variable is set.
    pub env_var_set: bool,
    /// The Base URL used when none is entered.
    pub default_api_base: Option<String>,
}

/// The hints of a dedicated choice.
pub fn hints_of_choice(choice: &DedicatedProviderChoice) -> ProviderHints {
    ProviderHints {
        env_var: Some(choice.env_var.clone()).filter(|name| !name.is_empty()),
        env_var_set: choice.env_var_set,
        default_api_base: choice.default_api_base.clone(),
    }
}

/// The hints of a listed row.
pub fn hints_of_row(row: &ProviderSummary) -> ProviderHints {
    ProviderHints {
        env_var: row.env_var.clone(),
        env_var_set: row.env_var_set,
        default_api_base: row.default_api_base.clone(),
    }
}

/// The name of an API protocol as the page words it.
pub fn backend_label(backend: &str) -> &str {
    match backend {
        "openai_compat" => "OpenAI Chat Completions API",
        "openai_responses" => "OpenAI Responses API",
        "anthropic" => "Anthropic Messages API",
        other => other,
    }
}

/// The bold heading of a row: its label, else its name.
pub fn row_title(row: &ProviderSummary) -> String {
    Some(row.label.trim())
        .filter(|label| !label.is_empty())
        .unwrap_or(&row.name)
        .to_string()
}

/// The muted line under a row's heading: `Dedicated · anthropic · https://…` or
/// `Custom · OpenAI Responses API · https://gateway…`. A dedicated provider names its
/// protocol only when it was changed from the default.
pub fn row_subline(row: &ProviderSummary) -> String {
    let endpoint = row
        .api_base
        .as_deref()
        .or(row.default_api_base.as_deref())
        .unwrap_or("default endpoint");
    match ProviderFormKind::from_wire(&row.kind) {
        ProviderFormKind::Dedicated => match &row.backend {
            Some(backend) => format!(
                "Dedicated · {} · {endpoint} · {}",
                row.name,
                backend_label(backend)
            ),
            None => format!("Dedicated · {} · {endpoint}", row.name),
        },
        ProviderFormKind::Custom => format!(
            "Custom · {} · {endpoint}",
            backend_label(row.backend.as_deref().unwrap_or(&row.backend_default))
        ),
    }
}

/// The amber line under a row with no API key from either source, or `None`. A custom
/// provider has no environment variable and may not need a key (a local server), so it
/// gets no warning.
pub fn missing_key_note(row: &ProviderSummary) -> Option<String> {
    if row.api_key_configured || row.env_var_set {
        return None;
    }
    row.env_var
        .as_deref()
        .map(|variable| format!("No API key: set one or {variable}"))
}

/// The question asked before deleting the provider `name`.
pub fn delete_question(name: &str) -> String {
    format!("Delete provider '{name}'? This cannot be undone.")
}

/// The confirmation shown after a successful change, read against the fresh `rows`.
pub fn notice_for(change: &ProviderChange, rows: &[ProviderSummary]) -> String {
    let name = rows
        .iter()
        .find(|row| row.name == change.name)
        .map(row_title)
        .unwrap_or_else(|| change.name.clone());
    match change.action.as_str() {
        "added" => format!("Provider '{name}' added. It applies from the next message."),
        "updated" => format!("Provider '{name}' saved. It applies from the next message."),
        "deleted" => format!("Provider '{name}' deleted."),
        "key_forgotten" => format!("The stored API key of '{name}' was removed."),
        _ => format!("Provider '{name}' changed."),
    }
}

/// The dedicated providers that can still be added (the others already have a row).
pub fn dedicated_choices_left(choices: &[DedicatedProviderChoice]) -> Vec<DedicatedProviderChoice> {
    choices
        .iter()
        .filter(|choice| !choice.configured)
        .cloned()
        .collect()
}

/// The placeholder of the API key input: what a blank key does.
pub fn key_placeholder(env_var: Option<&str>) -> String {
    match env_var {
        Some(variable) if !variable.is_empty() => {
            format!("Leave blank to use env ({variable})")
        }
        _ => String::new(),
    }
}

/// The orange warning under the key input: no key is stored or typed and the environment
/// variable is not set either, so requests would fail. `None` otherwise.
pub fn env_warning(hints: &ProviderHints, key_stored: bool, key_draft: &str) -> Option<String> {
    let variable = hints.env_var.as_deref()?;
    (!hints.env_var_set && !key_stored && key_draft.trim().is_empty()).then(|| {
        format!(
            "{variable} is not set on the gateway machine. Enter a key, or requests will fail until it is set."
        )
    })
}

/// Whether the form's primary button can be pressed: changes are allowed, no request is
/// in flight, the filled-in settings are valid, and
///
/// * adding: the provider has a name, and a custom one a Base URL; a dedicated provider
///   may be added with nothing else (its environment variable supplies the key);
/// * editing: something changed, and a custom provider keeps its Base URL.
pub fn form_ready(
    kind: ProviderFormKind,
    mode: FormMode,
    name: &str,
    fields: &[ToolFieldSummary],
    drafts: &FormDrafts,
    can_change: bool,
    busy: bool,
) -> bool {
    if !can_change || busy {
        return false;
    }
    let Ok(changes) = changed_values(fields, drafts) else {
        return false;
    };
    let needs_base_url = kind == ProviderFormKind::Custom
        && drafts
            .get(API_BASE)
            .is_none_or(|base| base.trim().is_empty());
    if needs_base_url {
        return false;
    }
    match mode {
        FormMode::Add => !name.trim().is_empty(),
        FormMode::Edit => !changes.is_empty(),
    }
}

/// The label of the form's primary button.
pub fn submit_label(kind: ProviderFormKind, mode: FormMode) -> &'static str {
    match (mode, kind) {
        (FormMode::Edit, _) => "Save",
        (FormMode::Add, ProviderFormKind::Dedicated) => "Apply",
        (FormMode::Add, ProviderFormKind::Custom) => "Create provider",
    }
}

/// Tailwind classes of the small outlined buttons of a row.
const ROW_BUTTON_CLASS: &str = "rounded-full border border-slate-200 px-3 py-1 text-sm text-slate-600 hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-40";

/// Tailwind classes of a text input.
const INPUT_CLASS: &str = "w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm";

/// A text input bound to the draft `name`, with its label and an optional hint.
#[component]
fn DraftTextInput(
    drafts: RwSignal<FormDrafts>,
    name: &'static str,
    #[prop(into)] label: String,
    #[prop(into)] placeholder: Signal<String>,
    #[prop(optional)] secret: bool,
    #[prop(optional, into)] hint: Option<String>,
    #[prop(into)] enabled: Signal<bool>,
) -> impl IntoView {
    view! {
        <label class="mb-3 block text-sm text-slate-700">
            <span class="mb-1 block font-medium">{label}</span>
            <input
                type=if secret { "password" } else { "text" }
                autocomplete="off"
                spellcheck="false"
                class=INPUT_CLASS
                placeholder=move || placeholder.get()
                disabled=move || !enabled.get()
                prop:value=move || drafts.with(|map| map.get(name).cloned().unwrap_or_default())
                on:input=move |event| {
                    let text = event_target_value(&event);
                    drafts
                        .update(|map| {
                            map.insert(name.to_string(), text);
                        });
                }
            />
            {hint
                .filter(|hint| !hint.is_empty())
                .map(|hint| view! { <span class="mt-1 block text-xs text-slate-500">{hint}</span> })}
        </label>
    }
}

/// The API protocol drop-down bound to the draft `providerBackEnd`.
#[component]
fn ProtocolSelect(
    drafts: RwSignal<FormDrafts>,
    choices: Vec<BackendChoice>,
    #[prop(into)] enabled: Signal<bool>,
) -> impl IntoView {
    view! {
        <label class="mb-3 block text-sm text-slate-700">
            <span class="mb-1 block font-medium">"API protocol"</span>
            <select
                class=INPUT_CLASS
                disabled=move || !enabled.get()
                prop:value=move || {
                    drafts.with(|map| map.get(PROVIDER_BACK_END).cloned().unwrap_or_default())
                }
                on:change=move |event| {
                    let text = event_target_value(&event);
                    drafts
                        .update(|map| {
                            map.insert(PROVIDER_BACK_END.to_string(), text);
                        });
                }
            >
                {choices
                    .into_iter()
                    .map(|choice| view! { <option value=choice.value>{choice.label}</option> })
                    .collect_view()}
            </select>
        </label>
    }
}

/// The extra headers as add/delete rows, writing the `extraHeaders` draft.
#[component]
fn HeaderRows(drafts: RwSignal<FormDrafts>, #[prop(into)] enabled: Signal<bool>) -> impl IntoView {
    let lines = RwSignal::new(
        drafts.with_untracked(|map| map.get(EXTRA_HEADERS).cloned().unwrap_or_default()),
    );
    Effect::new(move |_| {
        let text = lines.get();
        drafts.update(|map| {
            map.insert(EXTRA_HEADERS.to_string(), text);
        });
    });
    view! {
        <div class="mb-3 text-sm text-slate-700">
            <span class="mb-1 block font-medium">"Extra headers"</span>
            <KeyValueRows
                draft=lines
                add_label="+ Add header"
                name_placeholder="Header name"
                value_placeholder="Value"
                enabled=enabled
            />
            <span class="mt-1 block text-xs text-slate-500">
                "Sent with every request. Stored as written in config.json."
            </span>
        </div>
    }
}

/// The settings of one provider form: the key, the Base URL, the protocol and the
/// headers, laid out for `kind`. The dedicated layout puts the Base URL, protocol and
/// headers in a collapsible "Customized settings" section.
#[component]
fn ProviderFields(
    kind: ProviderFormKind,
    drafts: RwSignal<FormDrafts>,
    backend_choices: Vec<BackendChoice>,
    #[prop(into)] hints: Signal<ProviderHints>,
    /// Whether an API key is stored (it is never sent to the browser).
    #[prop(into)]
    key_stored: Signal<bool>,
    #[prop(into)] enabled: Signal<bool>,
    /// Forget the stored key; offered while one is stored.
    #[prop(optional_no_strip)]
    on_forget_key: Option<Callback<()>>,
) -> impl IntoView {
    let key_placeholder_text = Signal::derive(move || {
        let hints = hints.get();
        if key_stored.get() {
            "A key is stored; type to replace it".to_string()
        } else {
            key_placeholder(hints.env_var.as_deref())
        }
    });
    let base_placeholder = Signal::derive(move || match kind {
        ProviderFormKind::Dedicated => hints.get().default_api_base.unwrap_or_default(),
        ProviderFormKind::Custom => CUSTOM_BASE_URL_PLACEHOLDER.to_string(),
    });
    let warning = move || {
        let key_draft = drafts.with(|map| map.get(API_KEY).cloned().unwrap_or_default());
        env_warning(&hints.get(), key_stored.get(), &key_draft)
    };
    let key_input = view! {
        <DraftTextInput
            drafts=drafts
            name=API_KEY
            label="API key"
            placeholder=key_placeholder_text
            secret=true
            enabled=enabled
        />
        {move || {
            warning()
                .map(|message| view! { <p class="-mt-2 mb-3 text-xs text-orange-600">{message}</p> })
        }}
        {on_forget_key
            .map(|forget| {
                view! {
                    <Show when=move || key_stored.get()>
                        <button
                            type="button"
                            class="-mt-1 mb-3 text-xs text-slate-500 underline hover:text-red-600 disabled:opacity-40"
                            disabled=move || !enabled.get()
                            on:click=move |_| forget.run(())
                        >
                            "Forget stored key"
                        </button>
                    </Show>
                }
            })}
    };
    let base_input = view! {
        <DraftTextInput
            drafts=drafts
            name=API_BASE
            label=match kind {
                ProviderFormKind::Dedicated => "Base URL",
                ProviderFormKind::Custom => "Base URL (required)",
            }
            placeholder=base_placeholder
            enabled=enabled
        />
    };
    match kind {
        ProviderFormKind::Dedicated => {
            view! {
                {key_input}
                <details class="mb-3 rounded-lg border border-slate-200 bg-white px-3 py-2">
                    <summary class="cursor-pointer text-sm font-medium text-slate-700">
                        "Customized settings"
                    </summary>
                    <div class="mt-3">
                        {base_input} <ProtocolSelect drafts=drafts choices=backend_choices enabled=enabled />
                        <HeaderRows drafts=drafts enabled=enabled />
                    </div>
                </details>
            }
            .into_any()
        }
        ProviderFormKind::Custom => {
            view! {
                {base_input}
                <ProtocolSelect drafts=drafts choices=backend_choices enabled=enabled />
                {key_input}
                <details class="mb-3 rounded-lg border border-slate-200 bg-white px-3 py-2">
                    <summary class="cursor-pointer text-sm font-medium text-slate-700">
                        "Extra headers"
                    </summary>
                    <div class="mt-3">
                        <HeaderRows drafts=drafts enabled=enabled />
                    </div>
                </details>
            }
            .into_any()
        }
    }
}

/// The shell around [`ProviderFields`] shared by the add panel's tabs and the edit form:
/// the fields, the draft's error or the request's error, and Cancel and the primary
/// button (see [`submit_label`]), which stays off until [`form_ready`].
#[component]
fn ProviderFormShell(
    kind: ProviderFormKind,
    mode: FormMode,
    form: ToolConfigSummary,
    drafts: RwSignal<FormDrafts>,
    /// The provider's name (typed or picked when adding, fixed when editing).
    #[prop(into)]
    name: Signal<String>,
    backend_choices: Vec<BackendChoice>,
    #[prop(into)] hints: Signal<ProviderHints>,
    #[prop(into)] key_stored: Signal<bool>,
    #[prop(into)] can_change: Signal<bool>,
    #[prop(into)] busy: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    on_submit: Callback<ToolConfigChange>,
    on_cancel: Callback<()>,
    #[prop(optional_no_strip)] on_forget_key: Option<Callback<()>>,
    /// Content above the fields (the name input or the provider picker).
    #[prop(optional)]
    children: Option<Children>,
) -> impl IntoView {
    let fields_for_ready = form.fields.clone();
    let fields_for_error = form.fields.clone();
    let fields_for_submit = form.fields.clone();
    let ready = move || {
        form_ready(
            kind,
            mode,
            &name.get(),
            &fields_for_ready,
            &drafts.get(),
            can_change.get(),
            busy.get(),
        )
    };
    let draft_error = move || changed_values(&fields_for_error, &drafts.get()).err();
    let submit = move |_| {
        if let Ok(changes) = changed_values(&fields_for_submit, &drafts.get_untracked()) {
            on_submit.run((name.get_untracked(), changes));
        }
    };
    view! {
        {children.map(|children| children())}
        <ProviderFields
            kind=kind
            drafts=drafts
            backend_choices=backend_choices
            hints=hints
            key_stored=key_stored
            enabled=can_change
            on_forget_key=on_forget_key
        />
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
                disabled=move || !ready()
                on:click=submit
            >
                {submit_label(kind, mode)}
            </button>
        </div>
    }
}

/// The open settings form of the provider `row`, editing the drafts the panel keeps for
/// it. The name is fixed.
#[component]
fn ProviderEditForm(
    row: ProviderSummary,
    form: ToolConfigSummary,
    panel: SettingsPanel,
    actions: SettingsActions,
    backend_choices: Vec<BackendChoice>,
) -> impl IntoView {
    let kind = ProviderFormKind::from_wire(&row.kind);
    let drafts = RwSignal::from(panel.row_drafts(SettingsPage::Providers, &row.name, &form.fields));
    let name = row.name.clone();
    let name_for_busy = row.name.clone();
    let name_for_forget = row.name.clone();
    let hints = hints_of_row(&row);
    let key_stored = row.api_key_configured;
    let name_signal = Signal::derive({
        let name = name.clone();
        move || name.clone()
    });
    view! {
        <div class="mt-3 rounded-xl border border-slate-200 bg-slate-50 p-4 sm:ml-16">
            <p class="mb-3 text-xs text-slate-500">{LIVE_FORM_NOTE}</p>
            <ProviderFormShell
                kind=kind
                mode=FormMode::Edit
                form=form
                drafts=drafts
                name=name_signal
                backend_choices=backend_choices
                hints=Signal::derive(move || hints.clone())
                key_stored=Signal::derive(move || key_stored)
                can_change=Signal::derive(move || panel.can_change())
                busy=Signal::derive(move || is_busy(&panel.busy.get(), &name_for_busy))
                error=Signal::derive(|| None::<String>)
                on_submit=actions.on_save_provider
                on_cancel=Callback::new(move |()| panel.editing_provider.set(None))
                on_forget_key=Some(Callback::new(move |()| {
                    actions.on_forget_provider_key.run(name_for_forget.clone())
                }))
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
                on:click=move |_| panel.confirm_delete_provider.set(None)
            >
                "Cancel"
            </button>
            <button
                type="button"
                class="rounded-full bg-red-600 px-3 py-1 text-sm font-medium text-white hover:bg-red-700 disabled:opacity-40"
                disabled=move || !panel.can_change()
                on:click=move |_| {
                    panel.confirm_delete_provider.set(None);
                    actions.on_delete_provider.run(name.clone());
                }
            >
                "Delete"
            </button>
        </div>
    }
}

/// One provider row: tile, title, `Dedicated · name · endpoint`, the missing-key warning,
/// the buttons, and the open form, delete question and error underneath.
#[component]
fn ProviderRowView(
    row: ProviderSummary,
    /// Whether the connection may change the configuration (buttons are offered).
    can_modify: bool,
    backend_choices: Vec<BackendChoice>,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let title = row_title(&row);
    let subline = row_subline(&row);
    let key_note = missing_key_note(&row);
    let can_edit = can_modify && row.form.is_some();
    let name_for_edit = row.name.clone();
    let name_for_form = row.name.clone();
    let name_for_delete = row.name.clone();
    let name_for_question = row.name.clone();
    let name_for_error = row.name.clone();
    // One change at a time: every button waits while any request is in flight.
    let disabled = Signal::derive(move || !panel.can_change() || panel.busy.get().is_some());
    let blocked_reason = row.delete_blocked_reason.clone();
    let delete_enabled = row.deletable;
    let form = row.form.clone();
    let row_for_form = row.clone();
    view! {
        <li class="rounded-xl border border-slate-200 bg-white p-4">
            <div class="flex flex-wrap items-start gap-4">
                <Tile title=title.clone() />
                <div class="min-w-0 flex-1">
                    <span class="break-words text-sm font-semibold text-slate-900">{title}</span>
                    <p class="break-words text-sm text-slate-500">{subline}</p>
                    {key_note
                        .map(|note| view! { <p class="mt-1 text-xs font-medium text-amber-700">{note}</p> })}
                </div>
                {can_modify
                    .then(|| {
                        view! {
                            <div class="flex flex-wrap items-center gap-2">
                                {can_edit
                                    .then(|| {
                                        view! {
                                            <button
                                                type="button"
                                                class=ROW_BUTTON_CLASS
                                                on:click=move |_| {
                                                    let target = name_for_edit.clone();
                                                    panel.confirm_delete_provider.set(None);
                                                    panel
                                                        .editing_provider
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
                                <button
                                    type="button"
                                    class=ROW_BUTTON_CLASS
                                    title=blocked_reason.clone()
                                    disabled=move || !delete_enabled || disabled.get()
                                    on:click=move |_| {
                                        panel.editing_provider.set(None);
                                        panel.confirm_delete_provider.set(Some(name_for_delete.clone()));
                                    }
                                >
                                    "Delete"
                                </button>
                            </div>
                        }
                    })}
            </div>
            {form
                .filter(|_| can_edit)
                .map(|form| {
                    view! {
                        <Show when=move || {
                            panel.editing_provider.get().as_deref() == Some(name_for_form.as_str())
                        }>
                            <ProviderEditForm
                                row=row_for_form.clone()
                                form=form.clone()
                                panel=panel
                                actions=actions
                                backend_choices=backend_choices.clone()
                            />
                        </Show>
                    }
                })}
            {move || {
                (panel.confirm_delete_provider.get().as_deref() == Some(name_for_question.as_str()))
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

/// The Dedicated tab of the add panel: the provider picker and the dedicated fields.
#[component]
fn DedicatedTab(
    extras: ProviderExtras,
    form: ToolConfigSummary,
    drafts: RwSignal<FormDrafts>,
    panel: SettingsPanel,
    on_submit: Callback<ProviderAddRequest>,
) -> impl IntoView {
    let left = dedicated_choices_left(&extras.dedicated_choices);
    if left.is_empty() {
        return view! {
            <p class="mb-3 text-sm text-slate-500">
                "Every dedicated provider is already added. Edit it from the list."
            </p>
        }
        .into_any();
    }
    let selected = RwSignal::new(left[0].name.clone());
    let left_for_hints = left.clone();
    let hints = Signal::derive(move || {
        let current = selected.get();
        left_for_hints
            .iter()
            .find(|choice| choice.name == current)
            .map(hints_of_choice)
            .unwrap_or_default()
    });
    view! {
        <ProviderFormShell
            kind=ProviderFormKind::Dedicated
            mode=FormMode::Add
            form=form
            drafts=drafts
            name=selected
            backend_choices=extras.backend_choices
            hints=hints
            key_stored=Signal::derive(|| false)
            can_change=Signal::derive(move || panel.can_change())
            busy=Signal::derive(move || is_busy(&panel.busy.get(), ADD_BUSY_KEY))
            error=panel.add_error
            on_submit=Callback::new(move |(name, values): ToolConfigChange| {
                on_submit.run((ProviderFormKind::Dedicated.wire_name(), name, values))
            })
            on_cancel=Callback::new(move |()| panel.close_add_panel())
        >
            <label class="mb-3 block text-sm text-slate-700">
                <span class="mb-1 block font-medium">"Provider"</span>
                <select
                    class=INPUT_CLASS
                    prop:value=move || selected.get()
                    on:change=move |event| selected.set(event_target_value(&event))
                >
                    {left
                        .into_iter()
                        .map(|choice| view! { <option value=choice.name>{choice.label}</option> })
                        .collect_view()}
                </select>
            </label>
        </ProviderFormShell>
    }
    .into_any()
}

/// The Custom tab of the add panel: the name and the custom fields.
#[component]
fn CustomTab(
    extras: ProviderExtras,
    form: ToolConfigSummary,
    drafts: RwSignal<FormDrafts>,
    name: RwSignal<String>,
    panel: SettingsPanel,
    on_submit: Callback<ProviderAddRequest>,
) -> impl IntoView {
    view! {
        <ProviderFormShell
            kind=ProviderFormKind::Custom
            mode=FormMode::Add
            form=form
            drafts=drafts
            name=name
            backend_choices=extras.backend_choices
            hints=Signal::derive(ProviderHints::default)
            key_stored=Signal::derive(|| false)
            can_change=Signal::derive(move || panel.can_change())
            busy=Signal::derive(move || is_busy(&panel.busy.get(), ADD_BUSY_KEY))
            error=panel.add_error
            on_submit=Callback::new(move |(name, values): ToolConfigChange| {
                on_submit.run((ProviderFormKind::Custom.wire_name(), name, values))
            })
            on_cancel=Callback::new(move |()| panel.close_add_panel())
        >
            <label class="mb-3 block text-sm text-slate-700">
                <span class="mb-1 block font-medium">"Name"</span>
                <input
                    type="text"
                    autocomplete="off"
                    spellcheck="false"
                    class=INPUT_CLASS
                    prop:value=move || name.get()
                    on:input=move |event| name.set(event_target_value(&event))
                />
                <span class="mt-1 block text-xs text-slate-500">{NAME_HINT}</span>
            </label>
        </ProviderFormShell>
    }
}

/// The panel that replaces "+ Add provider": the toggle between the two kinds and the
/// open tab's form. Each tab keeps its own drafts, so switching back and forth loses
/// nothing.
#[component]
fn AddProviderPanel(
    extras: ProviderExtras,
    form: ToolConfigSummary,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let dedicated_drafts = RwSignal::new(initial_drafts(&form.fields));
    let custom_drafts = RwSignal::new(initial_drafts(&form.fields));
    let custom_name = RwSignal::new(String::new());
    let tab = panel.add_provider_kind;
    let on_submit = actions.on_add_provider;
    view! {
        <div class="mt-4 mb-3 rounded-xl border border-slate-200 bg-slate-50 p-4">
            <h4 class="mb-3 text-sm font-semibold text-slate-900">"Add provider"</h4>
            <SegmentedToggle options=TAB_LABELS.to_vec() selected=tab label="Kind of provider" />
            {move || {
                if ProviderFormKind::from_tab(tab.get()) == ProviderFormKind::Dedicated {
                    view! {
                        <DedicatedTab
                            extras=extras.clone()
                            form=form.clone()
                            drafts=dedicated_drafts
                            panel=panel
                            on_submit=on_submit
                        />
                    }
                        .into_any()
                } else {
                    view! {
                        <CustomTab
                            extras=extras.clone()
                            form=form.clone()
                            drafts=custom_drafts
                            name=custom_name
                            panel=panel
                            on_submit=on_submit
                        />
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// "+ Add provider", or its panel while it is open. Nothing for a connection that may not
/// change the configuration (the gateway sends it no blank form).
#[component]
fn AddProviderSection(panel: SettingsPanel, actions: SettingsActions) -> impl IntoView {
    view! {
        {move || {
            let extras = panel.provider_overview_extras.get();
            let form = extras.new_form.clone().filter(|_| panel.can_modify.get())?;
            Some(
                if panel.adding.get() {
                    view! { <AddProviderPanel extras=extras form=form panel=panel actions=actions /> }
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

/// The body of the Providers page below the heading: the notice of the last change, the
/// rows and the add button.
#[component]
pub fn ProvidersSection(panel: SettingsPanel, actions: SettingsActions) -> impl IntoView {
    view! {
        <div class="min-h-0 flex-1 overflow-y-auto">
            {move || {
                panel
                    .provider_notice
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
                if !panel.providers_loaded.get() {
                    return view! { <p class="pt-6 text-sm text-slate-400">"Loading…"</p> }.into_any();
                }
                let rows = panel.providers.get();
                let can_modify = panel.can_modify.get();
                let backend_choices = panel.provider_overview_extras.get().backend_choices;
                if rows.is_empty() {
                    return view! {
                        <p class="py-4 text-sm text-slate-500">"No providers configured."</p>
                    }
                        .into_any();
                }
                view! {
                    <ul class="mt-4 space-y-3">
                        {rows
                            .into_iter()
                            .map(|row| {
                                view! {
                                    <ProviderRowView
                                        row=row
                                        can_modify=can_modify
                                        backend_choices=backend_choices.clone()
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
            <AddProviderSection panel=panel actions=actions />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_ui::models::ToolFieldKind;
    use serde_json::json;

    fn row(name: &str, kind: &str) -> ProviderSummary {
        ProviderSummary {
            name: name.to_string(),
            kind: kind.to_string(),
            label: if kind == "dedicated" {
                "Anthropic".to_string()
            } else {
                name.to_string()
            },
            api_base: None,
            default_api_base: Some("https://api.anthropic.com".to_string()),
            backend: None,
            backend_default: "anthropic".to_string(),
            api_key_configured: true,
            env_var: Some("ANTHROPIC_API_KEY".to_string()),
            env_var_set: false,
            used_by: Vec::new(),
            deletable: true,
            delete_blocked_reason: String::new(),
            form: None,
        }
    }

    fn custom_row() -> ProviderSummary {
        ProviderSummary {
            api_base: Some("https://gateway.example/v1".to_string()),
            backend: Some("openai_responses".to_string()),
            backend_default: "openai_compat".to_string(),
            default_api_base: None,
            env_var: None,
            ..row("my-relay", "custom")
        }
    }

    fn text_field(name: &str, value: &str) -> ToolFieldSummary {
        ToolFieldSummary {
            name: name.to_string(),
            label: name.to_string(),
            help: String::new(),
            kind: ToolFieldKind::Text,
            secret: name == "apiKey",
            env_var: String::new(),
            configured: false,
            value: (name != "apiKey").then(|| json!(value)),
            default: None,
        }
    }

    fn fields() -> Vec<ToolFieldSummary> {
        vec![
            text_field("apiKey", ""),
            text_field("apiBase", ""),
            text_field("providerBackEnd", ""),
        ]
    }

    fn drafts(pairs: &[(&str, &str)]) -> FormDrafts {
        pairs
            .iter()
            .map(|(name, text)| (name.to_string(), text.to_string()))
            .collect()
    }

    fn choice(name: &str, configured: bool) -> DedicatedProviderChoice {
        DedicatedProviderChoice {
            name: name.to_string(),
            label: name.to_string(),
            env_var: format!("{}_API_KEY", name.to_uppercase()),
            env_var_set: false,
            default_api_base: None,
            configured,
        }
    }

    #[test]
    fn a_dedicated_row_reads_dedicated_name_and_endpoint() {
        assert_eq!(
            row_subline(&row("anthropic", "dedicated")),
            "Dedicated · anthropic · https://api.anthropic.com"
        );
        let mut based = row("anthropic", "dedicated");
        based.api_base = Some("https://proxy.example".to_string());
        based.backend = Some("openai_compat".to_string());
        assert_eq!(
            row_subline(&based),
            "Dedicated · anthropic · https://proxy.example · OpenAI Chat Completions API"
        );
        let mut unknown = row("together", "dedicated");
        unknown.default_api_base = None;
        assert!(row_subline(&unknown).ends_with("default endpoint"));
    }

    #[test]
    fn a_custom_row_reads_custom_protocol_and_endpoint() {
        assert_eq!(
            row_subline(&custom_row()),
            "Custom · OpenAI Responses API · https://gateway.example/v1"
        );
        let mut automatic = custom_row();
        automatic.backend = None;
        assert!(row_subline(&automatic).starts_with("Custom · OpenAI Chat Completions API"));
    }

    #[test]
    fn rows_are_titled_by_label_else_name() {
        assert_eq!(row_title(&row("anthropic", "dedicated")), "Anthropic");
        let mut unlabelled = custom_row();
        unlabelled.label = "  ".to_string();
        assert_eq!(row_title(&unlabelled), "my-relay");
    }

    #[test]
    fn a_row_without_any_key_names_the_variable_to_set() {
        let mut dedicated = row("anthropic", "dedicated");
        assert_eq!(missing_key_note(&dedicated), None, "a key is stored");
        dedicated.api_key_configured = false;
        assert_eq!(
            missing_key_note(&dedicated).as_deref(),
            Some("No API key: set one or ANTHROPIC_API_KEY")
        );
        dedicated.env_var_set = true;
        assert_eq!(missing_key_note(&dedicated), None, "the variable is set");
    }

    #[test]
    fn a_custom_row_without_a_key_is_not_warned_about() {
        let mut custom = custom_row();
        custom.api_key_configured = false;
        assert_eq!(missing_key_note(&custom), None);
    }

    #[test]
    fn deleting_asks_first_naming_the_provider() {
        assert!(delete_question("my-relay").contains("'my-relay'"));
    }

    #[test]
    fn notices_say_what_happened_to_which_provider() {
        let rows = vec![row("anthropic", "dedicated")];
        let change = |action: &str, name: &str| ProviderChange {
            action: action.to_string(),
            name: name.to_string(),
        };
        assert_eq!(
            notice_for(&change("added", "anthropic"), &rows),
            "Provider 'Anthropic' added. It applies from the next message."
        );
        assert!(notice_for(&change("updated", "anthropic"), &rows).contains("saved"));
        assert_eq!(
            notice_for(&change("deleted", "gone"), &rows),
            "Provider 'gone' deleted."
        );
        assert!(notice_for(&change("key_forgotten", "anthropic"), &rows).contains("removed"));
    }

    #[test]
    fn only_dedicated_providers_without_a_row_are_offered() {
        let left = dedicated_choices_left(&[
            choice("openai", false),
            choice("anthropic", true),
            choice("together", false),
        ]);
        let names: Vec<&str> = left.iter().map(|choice| choice.name.as_str()).collect();
        assert_eq!(names, ["openai", "together"]);
        assert!(dedicated_choices_left(&[choice("openai", true)]).is_empty());
    }

    #[test]
    fn the_key_input_says_what_blank_does() {
        assert_eq!(
            key_placeholder(Some("ANTHROPIC_API_KEY")),
            "Leave blank to use env (ANTHROPIC_API_KEY)"
        );
        assert_eq!(key_placeholder(None), "");
    }

    #[test]
    fn the_environment_warning_needs_no_key_and_no_variable() {
        let unset = ProviderHints {
            env_var: Some("ANTHROPIC_API_KEY".to_string()),
            env_var_set: false,
            default_api_base: None,
        };
        assert!(env_warning(&unset, false, "")
            .unwrap()
            .contains("ANTHROPIC_API_KEY"));
        assert_eq!(env_warning(&unset, true, ""), None, "a key is stored");
        assert_eq!(env_warning(&unset, false, "sk-x"), None, "a key is typed");
        let set = ProviderHints {
            env_var_set: true,
            ..unset.clone()
        };
        assert_eq!(env_warning(&set, false, ""), None, "the variable is set");
        assert_eq!(env_warning(&ProviderHints::default(), false, ""), None);
    }

    #[test]
    fn hints_come_from_the_choice_or_the_row() {
        let mut openai = choice("openai", false);
        openai.env_var_set = true;
        openai.default_api_base = Some("https://api.openai.com/v1".to_string());
        let hints = hints_of_choice(&openai);
        assert_eq!(hints.env_var.as_deref(), Some("OPENAI_API_KEY"));
        assert!(hints.env_var_set);
        assert_eq!(hints_of_row(&custom_row()).env_var, None);
        let mut no_variable = choice("x", false);
        no_variable.env_var.clear();
        assert_eq!(hints_of_choice(&no_variable).env_var, None);
    }

    #[test]
    fn a_dedicated_provider_can_be_added_with_nothing_but_its_name() {
        let ready = |name: &str, draft: &FormDrafts, can_change: bool, busy: bool| {
            form_ready(
                ProviderFormKind::Dedicated,
                FormMode::Add,
                name,
                &fields(),
                draft,
                can_change,
                busy,
            )
        };
        let blank = drafts(&[("apiKey", ""), ("apiBase", ""), ("providerBackEnd", "")]);
        assert!(ready("anthropic", &blank, true, false));
        assert!(!ready("", &blank, true, false), "no provider picked");
        assert!(
            !ready("anthropic", &blank, false, false),
            "changes not allowed"
        );
        assert!(!ready("anthropic", &blank, true, true), "request in flight");
    }

    #[test]
    fn a_custom_provider_needs_a_name_and_a_base_url() {
        let ready = |name: &str, base: &str| {
            form_ready(
                ProviderFormKind::Custom,
                FormMode::Add,
                name,
                &fields(),
                &drafts(&[("apiKey", ""), ("apiBase", base), ("providerBackEnd", "")]),
                true,
                false,
            )
        };
        assert!(ready("my-relay", "https://g/v1"));
        assert!(!ready("", "https://g/v1"));
        assert!(!ready("my-relay", ""));
        assert!(!ready("my-relay", "   "));
    }

    #[test]
    fn editing_needs_a_change_and_a_custom_provider_keeps_its_base_url() {
        let stored = fields()
            .into_iter()
            .map(|mut field| {
                if field.name == "apiBase" {
                    field.value = Some(json!("https://g/v1"));
                }
                field
            })
            .collect::<Vec<_>>();
        let ready = |kind: ProviderFormKind, draft: &FormDrafts| {
            form_ready(
                kind,
                FormMode::Edit,
                "my-relay",
                &stored,
                draft,
                true,
                false,
            )
        };
        let unchanged = drafts(&[
            ("apiKey", ""),
            ("apiBase", "https://g/v1"),
            ("providerBackEnd", ""),
        ]);
        assert!(
            !ready(ProviderFormKind::Custom, &unchanged),
            "nothing changed"
        );
        let new_key = drafts(&[
            ("apiKey", "sk-x"),
            ("apiBase", "https://g/v1"),
            ("providerBackEnd", ""),
        ]);
        assert!(ready(ProviderFormKind::Custom, &new_key));
        let no_base = drafts(&[("apiKey", "sk-x"), ("apiBase", ""), ("providerBackEnd", "")]);
        assert!(
            !ready(ProviderFormKind::Custom, &no_base),
            "base URL cleared"
        );
    }

    #[test]
    fn kinds_map_between_tabs_and_the_wire() {
        assert_eq!(ProviderFormKind::from_tab(0), ProviderFormKind::Dedicated);
        assert_eq!(ProviderFormKind::from_tab(1), ProviderFormKind::Custom);
        assert_eq!(
            ProviderFormKind::from_wire("dedicated"),
            ProviderFormKind::Dedicated
        );
        assert_eq!(
            ProviderFormKind::from_wire("custom"),
            ProviderFormKind::Custom
        );
        assert_eq!(ProviderFormKind::Dedicated.wire_name(), "dedicated");
        assert_eq!(ProviderFormKind::Custom.wire_name(), "custom");
    }

    #[test]
    fn the_primary_button_says_what_it_does() {
        assert_eq!(
            submit_label(ProviderFormKind::Custom, FormMode::Add),
            "Create provider"
        );
        assert_eq!(
            submit_label(ProviderFormKind::Dedicated, FormMode::Add),
            "Apply"
        );
        assert_eq!(
            submit_label(ProviderFormKind::Custom, FormMode::Edit),
            "Save"
        );
        assert_eq!(
            submit_label(ProviderFormKind::Dedicated, FormMode::Edit),
            "Save"
        );
    }

    #[test]
    fn the_protocol_names_are_the_ones_the_wizard_shows() {
        assert_eq!(
            backend_label("openai_compat"),
            "OpenAI Chat Completions API"
        );
        assert_eq!(backend_label("openai_responses"), "OpenAI Responses API");
        assert_eq!(backend_label("anthropic"), "Anthropic Messages API");
    }

    #[test]
    fn the_busy_key_cannot_be_a_provider_name() {
        assert!(ADD_BUSY_KEY.contains('+'));
    }
}
