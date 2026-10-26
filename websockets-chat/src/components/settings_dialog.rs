//! The Settings dialog and the "configuration saved" notice.
//!
//! The dialog has a menu on the left ("Tools", "MCP Servers": see `settings_pages.rs`)
//! and, on the right, the open page. Its header has one more action, "Open configuration
//! file", which swaps the page for an editor holding the gateway's raw `config.json`.
//! Saving sends the text to the gateway, which validates it; on success
//! [`ConfigSavedNotice`] tells the user to restart the rust-bot process, because the
//! running bot does not reload its configuration.
//!
//! All data and actions come in as signals and callbacks (see `app.rs`), so the
//! component holds only the text being edited and the search text.

use leptos::prelude::*;

use super::json_editor::JsonEditor;
use super::settings_pages::{
    restart_button_label, SettingsActions, SettingsPage, SettingsPageView, SettingsPanel,
};
use crate::state::ConfigFileView;

/// Message in the notice shown after the configuration was saved.
const RESTART_NOTICE: &str =
    "The configuration was saved. Restart the rust-bot process for the new configuration to take effect.";

/// Whether the editor's Save button should do anything: not while a save is in flight.
fn can_save(saving: bool) -> bool {
    !saving
}

/// Label of the Save button.
fn save_label(saving: bool) -> &'static str {
    if saving {
        "Saving…"
    } else {
        "Save"
    }
}

/// Classes of a menu entry in the dialog's left column; `active` highlights the open page.
fn menu_button_class(active: bool) -> &'static str {
    if active {
        "shrink-0 whitespace-nowrap rounded-lg bg-slate-100 px-3 py-2 text-left text-sm font-medium text-slate-900 sm:w-full"
    } else {
        "shrink-0 whitespace-nowrap rounded-lg px-3 py-2 text-left text-sm text-slate-600 hover:bg-slate-50 sm:w-full"
    }
}

/// Classes of the panel: a card of at most `max_width_class` (a `sm:max-w-*` class when
/// `fullscreen_on_mobile`) on wide screens, and with `fullscreen_on_mobile` the whole
/// screen on phones.
pub fn dialog_panel_class(max_width_class: &str, fullscreen_on_mobile: bool) -> String {
    if fullscreen_on_mobile {
        format!(
            "relative flex h-[100dvh] w-full {max_width_class} flex-col bg-white p-4 sm:h-auto sm:max-h-[90vh] sm:rounded-2xl sm:p-5 sm:shadow-2xl sm:ring-1 sm:ring-slate-200"
        )
    } else {
        format!(
            "relative flex max-h-[90vh] w-full {max_width_class} flex-col rounded-2xl bg-white p-5 shadow-2xl ring-1 ring-slate-200"
        )
    }
}

/// Backdrop plus centred panel shared by the dialog and the notice. Clicking the
/// backdrop calls `on_close`; Escape inside the panel does too. With
/// `fullscreen_on_mobile` the panel fills the screen below the `sm` breakpoint.
#[component]
fn DialogFrame(
    title_id: &'static str,
    max_width_class: &'static str,
    on_close: Callback<()>,
    #[prop(optional)] fullscreen_on_mobile: bool,
    children: Children,
) -> impl IntoView {
    let wrapper_class = if fullscreen_on_mobile {
        "fixed inset-0 z-[60] flex items-center justify-center sm:p-4"
    } else {
        "fixed inset-0 z-[60] flex items-center justify-center p-4"
    };
    view! {
        <div class=wrapper_class>
            <div
                class="absolute inset-0 bg-slate-900/40"
                aria-hidden="true"
                on:click=move |_| on_close.run(())
            ></div>
            <div
                role="dialog"
                aria-modal="true"
                aria-labelledby=title_id
                class=dialog_panel_class(max_width_class, fullscreen_on_mobile)
                on:keydown=move |ev| {
                    if ev.key() == "Escape" {
                        on_close.run(());
                    }
                }
            >
                {children()}
            </div>
        </div>
    }
}

/// Close (×) icon button for dialog headers.
#[component]
fn CloseButton(on_close: Callback<()>) -> impl IntoView {
    view! {
        <button
            type="button"
            aria-label="Close"
            class="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-slate-400 hover:bg-slate-100 hover:text-slate-700"
            on:click=move |_| on_close.run(())
        >
            <svg
                xmlns="http://www.w3.org/2000/svg"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
                class="h-4 w-4"
                aria-hidden="true"
            >
                <path d="M18 6 6 18M6 6l12 12" />
            </svg>
        </button>
    }
}

/// The Settings dialog. Shown while `open`; `config_file` says whether the configuration
/// editor is showing (and holds the text to edit), `error` is the last failure to show
/// above the body, and `saving` is true while a save is in flight. `panel` and `actions`
/// drive the Tools and MCP Servers pages.
#[component]
pub fn SettingsDialog(
    #[prop(into)] open: Signal<bool>,
    panel: SettingsPanel,
    actions: SettingsActions,
    #[prop(into)] config_file: Signal<ConfigFileView>,
    #[prop(into)] saving: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    on_close: Callback<()>,
    on_open_config_file: Callback<()>,
    on_close_config_file: Callback<()>,
    on_save_config_file: Callback<String>,
) -> impl IntoView {
    // The text being edited. Reset to the file's text whenever a (re)load arrives.
    let draft = RwSignal::new(String::new());
    Effect::new(move |_| {
        if let ConfigFileView::Loaded { content, .. } = config_file.get() {
            draft.set(content);
        }
    });
    let editor_open = move || config_file.get() != ConfigFileView::Closed;
    // The search text belongs to the open page, so it starts empty on every page change.
    let search = RwSignal::new(String::new());
    let select_page = move |page: SettingsPage| {
        if editor_open() {
            on_close_config_file.run(());
        }
        search.set(String::new());
        actions.on_select_page.run(page);
    };

    view! {
        <Show when=move || open.get()>
            <DialogFrame
                title_id="settings-title"
                max_width_class="sm:max-w-4xl"
                on_close=on_close
                fullscreen_on_mobile=true
            >
                <div class="mb-4 flex items-center justify-between gap-3">
                    <h2 id="settings-title" class="text-lg font-semibold text-slate-900">
                        "Settings"
                    </h2>
                    <div class="flex items-center gap-2">
                        <button
                            type="button"
                            class="rounded-lg border border-slate-200 bg-white px-3 py-1.5 text-sm font-medium text-slate-700 shadow-sm hover:bg-slate-50 disabled:opacity-50"
                            disabled=editor_open
                            on:click=move |_| on_open_config_file.run(())
                        >
                            <span class="sm:hidden">"Config file"</span>
                            <span class="hidden sm:inline">"Open configuration file"</span>
                        </button>
                        <CloseButton on_close=on_close />
                    </div>
                </div>
                <Show when=move || error.get().is_some()>
                    <p
                        role="alert"
                        class="mb-3 whitespace-pre-wrap rounded-lg border border-red-100 bg-red-50 px-3 py-2 text-sm text-red-700"
                    >
                        {move || error.get().unwrap_or_default()}
                    </p>
                </Show>
                <div class="flex min-h-0 flex-1 flex-col gap-3 sm:h-[min(34rem,65vh)] sm:flex-none sm:flex-row sm:gap-4">
                    <nav
                        aria-label="Settings sections"
                        class="flex shrink-0 gap-2 overflow-x-auto sm:w-40 sm:flex-col sm:gap-0 sm:space-y-1 sm:overflow-visible"
                    >
                        {SettingsPage::ALL
                            .into_iter()
                            .map(|page| {
                                view! {
                                    <button
                                        type="button"
                                        class=move || {
                                            menu_button_class(
                                                !editor_open() && panel.page.get() == page,
                                            )
                                        }
                                        on:click=move |_| select_page(page)
                                    >
                                        {page.title()}
                                    </button>
                                }
                            })
                            .collect_view()}
                    </nav>
                    <div class="flex min-h-0 min-w-0 flex-1 flex-col">
                        {move || match config_file.get() {
                            ConfigFileView::Closed => {
                                view! {
                                    <SettingsPageView panel=panel actions=actions search=search />
                                }
                                    .into_any()
                            }
                            ConfigFileView::Loading => {
                                view! { <p class="py-6 text-sm text-slate-400">"Loading…"</p> }
                                    .into_any()
                            }
                            ConfigFileView::Loaded { path, .. } => {
                                view! {
                                    <div class="flex min-h-0 flex-1 flex-col">
                                        <p class="mb-2 break-all text-xs text-slate-500">{path}</p>
                                        <JsonEditor draft=draft aria_label="Configuration file" />
                                        <div class="mt-4 flex justify-end gap-2">
                                            <button
                                                type="button"
                                                class="rounded-lg bg-slate-100 px-3 py-1.5 text-sm font-medium text-slate-700 hover:bg-slate-200"
                                                on:click=move |_| on_close_config_file.run(())
                                            >
                                                "Cancel"
                                            </button>
                                            <button
                                                type="button"
                                                class="rounded-lg bg-slate-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-slate-800 disabled:opacity-50"
                                                disabled=move || !can_save(saving.get())
                                                on:click=move |_| on_save_config_file.run(draft.get_untracked())
                                            >
                                                {move || save_label(saving.get())}
                                            </button>
                                        </div>
                                    </div>
                                }
                                    .into_any()
                            }
                        }}
                    </div>
                </div>
            </DialogFrame>
        </Show>
    }
}

/// The popup shown after the configuration was saved, telling the user to restart the
/// rust-bot process. Shown while `open`; `on_close` dismisses it.
#[component]
pub fn ConfigSavedNotice(
    #[prop(into)] open: Signal<bool>,
    /// Whether a restart was requested and is under way.
    #[prop(into)]
    restarting: Signal<bool>,
    /// Why the last restart request was refused, if it was.
    #[prop(into)]
    error: Signal<Option<String>>,
    on_restart: Callback<()>,
    on_close: Callback<()>,
) -> impl IntoView {
    view! {
        <Show when=move || open.get()>
            <DialogFrame title_id="config-saved-title" max_width_class="max-w-sm" on_close=on_close>
                <div class="mb-3 flex items-start justify-between gap-3">
                    <h2 id="config-saved-title" class="text-base font-semibold text-slate-900">
                        "Restart required"
                    </h2>
                    <CloseButton on_close=on_close />
                </div>
                <p class="mb-5 text-sm text-slate-600">{RESTART_NOTICE}</p>
                {move || {
                    error
                        .get()
                        .map(|message| {
                            view! {
                                <p role="alert" class="mb-4 rounded-lg bg-red-50 px-3 py-2 text-sm text-red-700">
                                    {message}
                                </p>
                            }
                        })
                }}
                <div class="flex justify-end gap-2">
                    <button
                        type="button"
                        class="rounded-lg bg-amber-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-amber-700 disabled:opacity-60"
                        disabled=move || restarting.get()
                        on:click=move |_| on_restart.run(())
                    >
                        {move || restart_button_label(restarting.get())}
                    </button>
                    <button
                        type="button"
                        class="rounded-lg bg-slate-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-slate-800"
                        on:click=move |_| on_close.run(())
                    >
                        "OK"
                    </button>
                </div>
            </DialogFrame>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_is_disabled_only_while_saving() {
        assert!(can_save(false));
        assert!(!can_save(true));
    }

    #[test]
    fn the_panel_is_fullscreen_on_phones_only_when_asked() {
        let fullscreen = dialog_panel_class("sm:max-w-4xl", true);
        assert!(fullscreen.contains("h-[100dvh]") && fullscreen.contains("sm:rounded-2xl"));
        assert!(fullscreen.contains("sm:max-w-4xl"));
        let card = dialog_panel_class("max-w-sm", false);
        assert!(card.contains("rounded-2xl") && !card.contains("100dvh"));
    }

    #[test]
    fn menu_button_is_highlighted_only_for_the_open_page() {
        assert!(menu_button_class(true).contains("bg-slate-100"));
        assert!(!menu_button_class(false).contains("bg-slate-100"));
    }

    #[test]
    fn save_label_reflects_progress() {
        assert_eq!(save_label(false), "Save");
        assert_eq!(save_label(true), "Saving…");
    }

    #[test]
    fn restart_notice_tells_the_user_to_restart() {
        assert!(RESTART_NOTICE.contains("Restart the rust-bot process"));
    }
}
