//! The "Channels" page of the Settings dialog: one collapsible section per channel type
//! (today only Email). A section's header shows the type and a badge (Enabled, Disabled
//! or Not set up) and expands or collapses the section; every section starts expanded.
//!
//! * **Set up:** a row with the channel's summary (`bot@example.com · imap.example.com`),
//!   its warnings (e.g. no consent, so it will not start) and Edit and Delete. Edit opens
//!   the settings form, grouped under headings; Delete asks first.
//! * **Not set up:** "+ Create email channel", which opens the blank form with Cancel and
//!   "Create channel". There is no name to enter: the channel type is the key.
//!
//! Passwords never reach the browser: a stored one shows as configured and a blank
//! password input keeps it. Every change is written to `config.json` and applies after
//! a restart (the page shows the restart banner).
//!
//! The texts and the collapse state are plain functions so they are unit-testable on the
//! host target without Leptos.

use std::collections::HashSet;

use chat_ui::models::{ChannelSummary, ToolConfigSummary};
use leptos::prelude::*;

use super::add_item_panel::AddItemButton;
use super::delete_confirmation::DeleteConfirmation;
use super::list_page::Tile;
use super::mcp_presets_page::is_busy;
use super::settings_pages::{SettingsActions, SettingsPage, SettingsPanel};
use super::tool_config_form::ToolConfigForm;

/// Badge of a channel type that has no channel yet.
pub const BADGE_NOT_SET_UP: &str = "Not set up";
/// Badge of a channel that is switched on.
pub const BADGE_ENABLED: &str = "Enabled";
/// Badge of a channel that is set up but switched off.
pub const BADGE_DISABLED: &str = "Disabled";

/// Summary line of a set-up channel the gateway could not summarise.
const NO_SUMMARY: &str = "No mail server set";

/// The line above the create form.
const CREATE_FORM_NOTE: &str =
    "Stored in config.json on this machine. The channel starts after a restart, once it is enabled and consent is granted.";

/// Label of the create form's submit button.
const CREATE_SUBMIT_LABEL: &str = "Create channel";

/// Classes of the Edit and Delete buttons of a row.
const ROW_BUTTON_CLASS: &str = "rounded-full border border-slate-200 px-3 py-1 text-sm text-slate-600 hover:bg-slate-100 disabled:cursor-not-allowed disabled:opacity-40";

/// The badge in a section's header.
pub fn section_badge(row: &ChannelSummary) -> &'static str {
    match (row.configured, row.enabled) {
        (false, _) => BADGE_NOT_SET_UP,
        (true, true) => BADGE_ENABLED,
        (true, false) => BADGE_DISABLED,
    }
}

/// The colours of the badge `label` (see [`section_badge`]).
fn badge_class(label: &str) -> &'static str {
    match label {
        BADGE_ENABLED => {
            "rounded-full bg-emerald-100 px-2 py-0.5 text-xs font-medium text-emerald-800"
        }
        BADGE_DISABLED => {
            "rounded-full bg-slate-200 px-2 py-0.5 text-xs font-medium text-slate-700"
        }
        _ => "rounded-full bg-amber-100 px-2 py-0.5 text-xs font-medium text-amber-800",
    }
}

/// The line under a set-up channel's title: the gateway's summary, or a stand-in when
/// it has none.
pub fn row_summary_line(row: &ChannelSummary) -> String {
    if row.summary.trim().is_empty() {
        NO_SUMMARY.to_string()
    } else {
        row.summary.clone()
    }
}

/// The question asked before deleting the channel titled `title` (e.g. `Email`).
pub fn delete_question(title: &str) -> String {
    format!(
        "Delete the {} channel? All its settings are removed from config.json. This cannot be undone.",
        title.to_lowercase()
    )
}

/// Label of the button that opens the create form of the type titled `title`.
pub fn create_button_label(title: &str) -> String {
    format!("+ Create {} channel", title.to_lowercase())
}

/// Collapse the section of `channel_type` when it is expanded, expand it otherwise.
pub fn toggle_section(collapsed: &mut HashSet<String>, channel_type: &str) {
    if !collapsed.remove(channel_type) {
        collapsed.insert(channel_type.to_string());
    }
}

/// The key the panel keeps the create form's drafts under, apart from the edit form's
/// (both forms are named after the channel type). Channel types never contain '+'.
pub fn new_form_draft_key(channel_type: &str) -> String {
    format!("+new:{channel_type}")
}

/// The open edit form of the channel `channel_type`, editing the drafts the panel keeps
/// for it.
#[component]
fn ChannelEditForm(
    channel_type: String,
    form: ToolConfigSummary,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let drafts =
        RwSignal::from(panel.row_drafts(SettingsPage::Channels, &channel_type, &form.fields));
    view! {
        <div class="mt-3">
            <ToolConfigForm
                config=form
                drafts=drafts
                can_modify=Signal::derive(move || panel.can_change())
                busy=Signal::derive(move || is_busy(&panel.busy.get(), &channel_type))
                on_save=actions.on_save_channel
                on_cancel=Some(Callback::new(move |()| panel.editing_channel.set(None)))
            />
        </div>
    }
}

/// A set-up channel: tile, title, summary, warnings, Edit and Delete, and the open form,
/// delete question and error underneath.
#[component]
fn ConfiguredChannel(
    row: ChannelSummary,
    /// Whether the connection may change the configuration (buttons are offered).
    can_modify: bool,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let can_edit = can_modify && row.form.is_some();
    let summary = row_summary_line(&row);
    let channel_type = row.channel_type.clone();
    let type_for_edit = channel_type.clone();
    let type_for_form = channel_type.clone();
    let type_for_delete = channel_type.clone();
    let type_for_question = channel_type.clone();
    let title = row.title.clone();
    // One change at a time: every button waits while any request is in flight.
    let disabled = Signal::derive(move || !panel.can_change() || panel.busy.get().is_some());
    view! {
        <div class="flex flex-wrap items-start gap-4">
            <Tile title=title.clone() />
            <div class="min-w-0 flex-1">
                <span class="break-words text-sm font-semibold text-slate-900">{title.clone()}</span>
                <p class="break-words text-sm text-slate-500">{summary}</p>
                {row
                    .notes
                    .iter()
                    .map(|note| view! { <p class="mt-1 text-xs font-medium text-amber-700">{note.clone()}</p> })
                    .collect_view()}
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
                                            disabled=move || disabled.get()
                                            on:click=move |_| {
                                                let target = type_for_edit.clone();
                                                panel.confirm_delete_channel.set(None);
                                                panel
                                                    .editing_channel
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
                                disabled=move || disabled.get()
                                on:click=move |_| {
                                    panel.editing_channel.set(None);
                                    panel.confirm_delete_channel.set(Some(type_for_delete.clone()));
                                }
                            >
                                "Delete"
                            </button>
                        </div>
                    }
                })}
        </div>
        {row
            .form
            .filter(|_| can_edit)
            .map(|form| {
                view! {
                    <Show when=move || {
                        panel.editing_channel.get().as_deref() == Some(type_for_form.as_str())
                    }>
                        <ChannelEditForm
                            channel_type=channel_type.clone()
                            form=form.clone()
                            panel=panel
                            actions=actions
                        />
                    </Show>
                }
            })}
        {move || {
            (panel.confirm_delete_channel.get().as_deref() == Some(type_for_question.as_str()))
                .then(|| {
                    let channel_type = type_for_question.clone();
                    view! {
                        <DeleteConfirmation
                            question=delete_question(&title)
                            disabled=Signal::derive(move || !panel.can_change())
                            on_cancel=Callback::new(move |()| panel.confirm_delete_channel.set(None))
                            on_confirm=Callback::new(move |()| {
                                panel.confirm_delete_channel.set(None);
                                actions.on_delete_channel.run(channel_type.clone());
                            })
                        />
                    }
                })
        }}
    }
}

/// A channel type without a channel: "+ Create … channel", or the create form while it
/// is open (with the last refusal underneath). Nothing for a connection that may not
/// change the configuration (the gateway sends it no blank form).
#[component]
fn NewChannel(
    row: ChannelSummary,
    /// Whether the connection may change the configuration (the button is offered).
    can_modify: bool,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let Some(form) = row.new_form.clone().filter(|_| can_modify) else {
        return view! { <p class="text-sm text-slate-500">"No channel set up."</p> }.into_any();
    };
    let channel_type = row.channel_type.clone();
    let type_for_open = channel_type.clone();
    let type_for_button = channel_type.clone();
    let label = create_button_label(&row.title);
    view! {
        <Show
            when=move || panel.adding_channel.get().as_deref() == Some(type_for_open.as_str())
            fallback={
                let label = label.clone();
                let channel_type = type_for_button.clone();
                move || {
                    let channel_type = channel_type.clone();
                    view! {
                        <AddItemButton
                            label=label.clone()
                            disabled=Signal::derive(move || !panel.can_change())
                            on_click=Callback::new(move |()| {
                                panel.add_error.set(None);
                                panel.confirm_delete_channel.set(None);
                                panel.adding_channel.set(Some(channel_type.clone()));
                            })
                        />
                    }
                }
            }
        >
            {
                let draft_key = new_form_draft_key(&channel_type);
                let drafts = RwSignal::from(
                    panel.row_drafts(SettingsPage::Channels, &draft_key, &form.fields),
                );
                let busy_type = channel_type.clone();
                view! {
                    <div class="mt-3">
                        <ToolConfigForm
                            config=form.clone()
                            drafts=drafts
                            note=CREATE_FORM_NOTE
                            submit_label=CREATE_SUBMIT_LABEL
                            can_modify=Signal::derive(move || panel.can_change())
                            busy=Signal::derive(move || is_busy(&panel.busy.get(), &busy_type))
                            on_save=actions.on_add_channel
                            on_cancel=Some(Callback::new(move |()| panel.close_add_panel()))
                        />
                        {move || {
                            panel
                                .add_error
                                .get()
                                .map(|message| {
                                    view! { <p role="alert" class="mt-2 text-xs text-red-600 sm:ml-16">{message}</p> }
                                })
                        }}
                    </div>
                }
            }
        </Show>
    }
    .into_any()
}

/// One channel type: the header that expands and collapses it, and its body.
#[component]
fn ChannelSection(
    row: ChannelSummary,
    /// Whether the connection may change the configuration.
    can_modify: bool,
    panel: SettingsPanel,
    actions: SettingsActions,
) -> impl IntoView {
    let channel_type = row.channel_type.clone();
    let type_for_toggle = channel_type.clone();
    let type_for_error = channel_type.clone();
    let expanded = Signal::derive(move || {
        !panel
            .collapsed_channel_sections
            .with(|collapsed| collapsed.contains(&channel_type))
    });
    let badge = section_badge(&row);
    let title = row.title.clone();
    let body = if row.configured {
        view! { <ConfiguredChannel row=row can_modify=can_modify panel=panel actions=actions /> }
            .into_any()
    } else {
        view! { <NewChannel row=row can_modify=can_modify panel=panel actions=actions /> }
            .into_any()
    };
    view! {
        <section class="mt-4 rounded-xl border border-slate-200 bg-white">
            <button
                type="button"
                class="flex w-full items-center gap-3 rounded-xl px-4 py-3 text-left hover:bg-slate-50"
                aria-expanded=move || expanded.get().to_string()
                on:click=move |_| {
                    panel
                        .collapsed_channel_sections
                        .update(|collapsed| toggle_section(collapsed, &type_for_toggle));
                }
            >
                <svg
                    viewBox="0 0 20 20"
                    fill="currentColor"
                    aria-hidden="true"
                    class=move || {
                        if expanded.get() {
                            "h-4 w-4 rotate-90 text-slate-500 transition-transform"
                        } else {
                            "h-4 w-4 text-slate-500 transition-transform"
                        }
                    }
                >
                    <path
                        fill-rule="evenodd"
                        d="M7.21 14.77a.75.75 0 0 1 .02-1.06L11.168 10 7.23 6.29a.75.75 0 1 1 1.04-1.08l4.5 4.25a.75.75 0 0 1 0 1.08l-4.5 4.25a.75.75 0 0 1-1.06-.02Z"
                        clip-rule="evenodd"
                    />
                </svg>
                <span class="flex-1 text-sm font-semibold text-slate-900">{title}</span>
                <span class=badge_class(badge)>{badge}</span>
            </button>
            <div class="border-t border-slate-200 px-4 py-4" class:hidden=move || !expanded.get()>
                {body}
                {move || {
                    panel
                        .error
                        .get()
                        .filter(|(failed_type, _)| *failed_type == type_for_error)
                        .map(|(_, message)| view! { <p class="mt-2 text-xs text-red-600">{message}</p> })
                }}
            </div>
        </section>
    }
}

/// The body of the Channels page below the heading: one section per channel type.
#[component]
pub fn ChannelsSection(panel: SettingsPanel, actions: SettingsActions) -> impl IntoView {
    view! {
        <div class="min-h-0 flex-1 overflow-y-auto">
            {move || {
                if !panel.channels_loaded.get() {
                    return view! { <p class="pt-6 text-sm text-slate-400">"Loading…"</p> }.into_any();
                }
                let rows = panel.channels.get();
                let can_modify = panel.can_modify.get();
                if rows.is_empty() {
                    return view! {
                        <p class="py-4 text-sm text-slate-500">"No channel types available."</p>
                    }
                        .into_any();
                }
                rows.into_iter()
                    .map(|row| {
                        view! {
                            <ChannelSection row=row can_modify=can_modify panel=panel actions=actions />
                        }
                    })
                    .collect_view()
                    .into_any()
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(configured: bool, enabled: bool, summary: &str) -> ChannelSummary {
        ChannelSummary {
            channel_type: "email".to_string(),
            title: "Email".to_string(),
            configured,
            enabled,
            summary: summary.to_string(),
            notes: Vec::new(),
            form: None,
            new_form: None,
        }
    }

    #[test]
    fn the_badge_says_whether_the_channel_is_set_up_and_on() {
        assert_eq!(section_badge(&row(false, false, "")), BADGE_NOT_SET_UP);
        assert_eq!(section_badge(&row(true, true, "")), BADGE_ENABLED);
        assert_eq!(section_badge(&row(true, false, "")), BADGE_DISABLED);
        assert_ne!(badge_class(BADGE_ENABLED), badge_class(BADGE_DISABLED));
    }

    #[test]
    fn the_summary_line_falls_back_when_the_gateway_sent_none() {
        assert_eq!(
            row_summary_line(&row(true, true, "bot@example.com · imap.example.com")),
            "bot@example.com · imap.example.com"
        );
        assert_eq!(row_summary_line(&row(true, true, "  ")), NO_SUMMARY);
    }

    #[test]
    fn deleting_asks_first_naming_the_channel_type() {
        let question = delete_question("Email");
        assert!(question.starts_with("Delete the email channel?"));
        assert!(question.contains("cannot be undone"));
    }

    #[test]
    fn the_create_button_names_the_channel_type() {
        assert_eq!(create_button_label("Email"), "+ Create email channel");
    }

    #[test]
    fn toggling_a_section_collapses_then_expands_it() {
        let mut collapsed = HashSet::new();

        toggle_section(&mut collapsed, "email");
        assert!(
            collapsed.contains("email"),
            "collapsed after the first click"
        );

        toggle_section(&mut collapsed, "email");
        assert!(
            collapsed.is_empty(),
            "expanded again after the second click"
        );
    }

    #[test]
    fn the_create_form_keeps_its_drafts_apart_from_the_edit_form() {
        assert_ne!(new_form_draft_key("email"), "email");
        assert!(new_form_draft_key("email").ends_with("email"));
    }
}
