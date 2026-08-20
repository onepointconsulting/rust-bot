//! Cursor-style sessions sidebar shared by `web-chat` and `websockets-chat`.
//!
//! Rows highlight the active session; clicking one invokes `on_select`.
//! When `on_rename` and/or `on_delete` is provided (websockets-chat), each
//! row also has a kebab that opens a menu with "Rename" and/or a red
//! "Delete" item. Delete opens a confirmation dialog before actually
//! calling `on_delete`; rename opens a small dialog to edit the title.
//!
//! Open/collapsed is a single `open: Signal<bool>` shared across every
//! breakpoint (not "always docked on `sm+`, toggle-only on mobile" like an
//! earlier pass) — the same [`SessionsSidebarToggle`] button and the same
//! in-panel collapse icon drive it everywhere.
//!
//! The logged-in account chip (initial + email) lives at the bottom of this
//! panel and is not shown anywhere else, so collapsing the sidebar also
//! hides the email.
//!
//! Deliberately **not** a `<For>` keyed by group label for the outer
//! date-bucket list: `<For>`'s keyed diffing only re-renders a child when
//! its *key* disappears or reappears. A brand-new chat landing in an
//! already-rendered "Today" group, or a title that finishes generating,
//! changes a group's *contents* without changing which group labels exist —
//! `<For>` would keep showing whatever it rendered for "Today" the first
//! time, forever.
//!
//! Same class of freeze applies to `<Show when=|| !sessions.is_empty()>`:
//! Leptos memos the boolean, so once the list is non-empty the children are
//! not rebuilt when a later `list_chats` refresh only changes titles. The
//! grouped list is therefore a plain reactive closure that reads `sessions`
//! on every run (cheap: the list is small).

use std::collections::HashSet;

use leptos::html::Input;
use leptos::prelude::*;

use crate::models::SessionListItem;
use crate::session_groups::{group_sessions, SessionGroup};

use super::UserAccountChip;

/// Rows beyond this count in a single group are collapsed behind a
/// "··· More" toggle, mirroring Cursor's own history list.
const COLLAPSE_THRESHOLD: usize = 8;

const MENU_ITEM: &str = "flex w-full items-center gap-2.5 px-3 py-2 text-left text-sm font-medium text-slate-700 hover:bg-slate-50";

fn icon_class() -> &'static str {
    "h-4 w-4"
}

#[component]
fn IconSidebarPanel() -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class=icon_class()
            aria-hidden="true"
        >
            <rect width="18" height="18" x="3" y="3" rx="2" ry="2" />
            <line x1="9" x2="9" y1="3" y2="21" />
        </svg>
    }
}

#[component]
fn IconKebab() -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class="h-3.5 w-3.5"
            aria-hidden="true"
        >
            <circle cx="12" cy="5" r="1" fill="currentColor" />
            <circle cx="12" cy="12" r="1" fill="currentColor" />
            <circle cx="12" cy="19" r="1" fill="currentColor" />
        </svg>
    }
}

#[component]
fn IconPencil() -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class=icon_class()
            aria-hidden="true"
        >
            <path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z" />
            <path d="m15 5 4 4" />
        </svg>
    }
}

#[component]
fn IconTrash() -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class=icon_class()
            aria-hidden="true"
        >
            <path d="M3 6h18" />
            <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" />
            <path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
            <line x1="10" x2="10" y1="11" y2="17" />
            <line x1="14" x2="14" y1="11" y2="17" />
        </svg>
    }
}

#[component]
fn IconClose() -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class=icon_class()
            aria-hidden="true"
        >
            <path d="M18 6L6 18" />
            <path d="M6 6l12 12" />
        </svg>
    }
}

/// Header button (left of the title, same at every breakpoint) that opens
/// the sidebar once it's collapsed. Renders nothing while the sidebar is
/// already open — closing it is done from the collapse icon inside the
/// panel itself (see `SessionsSidebar`'s `body`).
#[component]
pub fn SessionsSidebarToggle(
    #[prop(into)] open: Signal<bool>,
    on_toggle: impl Fn() + 'static + Send + Sync + Copy,
) -> impl IntoView {
    view! {
        <Show when=move || !open.get()>
            <button
                type="button"
                class="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                aria-label="Open chats list"
                aria-haspopup="menu"
                aria-expanded="false"
                on:click=move |_| on_toggle()
            >
                <IconSidebarPanel />
            </button>
        </Show>
    }
}

/// One date-grouped section of the sidebar's list, with its own "··· More"
/// disclosure once a group grows past [`COLLAPSE_THRESHOLD`].
///
/// `is_expanded` is a plain `bool`, not a signal: this whole component is
/// recreated fresh every time [`SessionsSidebar`]'s outer reactive closure
/// reruns (see the module doc comment), so there's no need for row-level
/// reactivity here — plain values baked in at construction time are always
/// current as of the last `sessions`/expansion-state change.
#[component]
fn SessionRow(
    id: String,
    display_title: String,
    raw_title: String,
    #[prop(into)] active_id: Signal<Option<String>>,
    on_select: impl Fn(String) + 'static + Send + Sync + Copy,
    open_menu_id: RwSignal<Option<String>>,
    on_open_rename: Option<Callback<(String, String)>>,
    on_open_delete: Option<Callback<String>>,
) -> impl IntoView {
    let id_for_class = id.clone();
    let id_for_title_class = id.clone();
    let id_for_aria = id.clone();
    let id_for_click = id.clone();
    let id_for_kebab_open = id.clone();
    let id_for_kebab_active = id.clone();
    let id_for_expanded = id.clone();
    let id_for_toggle = id.clone();
    let display_title_for_menu = display_title.clone();
    let show_rename = on_open_rename.is_some();
    let show_delete = on_open_delete.is_some();
    let show_menu = show_rename || show_delete;

    view! {
        <li class="group relative">
            <div class=move || {
                if active_id.get().as_deref() == Some(id_for_class.as_str()) {
                    "flex w-full items-center rounded-lg bg-slate-100"
                } else {
                    "flex w-full items-center rounded-lg hover:bg-slate-50"
                }
            }>
                <button
                    type="button"
                    class=move || {
                        if active_id.get().as_deref() == Some(id_for_title_class.as_str()) {
                            "min-w-0 flex-1 truncate px-2 py-1.5 text-left text-sm font-medium text-slate-900"
                        } else {
                            "min-w-0 flex-1 truncate px-2 py-1.5 text-left text-sm text-slate-600"
                        }
                    }
                    aria-current=move || {
                        if active_id.get().as_deref() == Some(id_for_aria.as_str()) {
                            "true"
                        } else {
                            "false"
                        }
                    }
                    on:click=move |_| on_select(id_for_click.clone())
                    title=raw_title
                >
                    {display_title}
                </button>
                {show_menu.then(|| {
                    view! {
                        <button
                            type="button"
                            class=move || {
                                let open = open_menu_id.get().as_deref()
                                    == Some(id_for_kebab_open.as_str());
                                let active = active_id.get().as_deref()
                                    == Some(id_for_kebab_active.as_str());
                                if open || active {
                                    "mr-1 flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-slate-500 hover:bg-slate-200 hover:text-slate-800"
                                } else {
                                    "mr-1 flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-slate-400 opacity-0 hover:bg-slate-200 hover:text-slate-700 group-hover:opacity-100 group-focus-within:opacity-100"
                                }
                            }
                            aria-label="Chat actions"
                            aria-haspopup="menu"
                            aria-expanded=move || {
                                if open_menu_id.get().as_deref() == Some(id_for_expanded.as_str()) {
                                    "true"
                                } else {
                                    "false"
                                }
                            }
                            on:click=move |ev| {
                                ev.stop_propagation();
                                open_menu_id.update(|current| {
                                    if current.as_deref() == Some(id_for_toggle.as_str()) {
                                        *current = None;
                                    } else {
                                        *current = Some(id_for_toggle.clone());
                                    }
                                });
                            }
                        >
                            <IconKebab />
                        </button>
                    }
                })}
            </div>
            {show_menu.then(|| {
                let id_for_menu_visible = id.clone();
                let id_for_rename = id.clone();
                let id_for_delete = id.clone();
                let title_for_rename = display_title_for_menu;
                view! {
                    <div class=move || {
                        if open_menu_id.get().as_deref() == Some(id_for_menu_visible.as_str()) {
                            "contents"
                        } else {
                            "hidden"
                        }
                    }>
                        <div
                            class="fixed inset-0 z-20"
                            aria-hidden="true"
                            on:click=move |_| open_menu_id.set(None)
                        ></div>
                        <div
                            role="menu"
                            class="absolute right-1 z-30 mt-0.5 w-40 overflow-hidden rounded-xl bg-white py-1 shadow-lg ring-1 ring-slate-200"
                        >
                            {show_rename.then(|| {
                                view! {
                                    <button
                                        type="button"
                                        role="menuitem"
                                        class=MENU_ITEM
                                        on:click=move |_| {
                                            open_menu_id.set(None);
                                            if let Some(on_open_rename) = on_open_rename {
                                                on_open_rename
                                                    .run((
                                                        id_for_rename.clone(),
                                                        title_for_rename.clone(),
                                                    ));
                                            }
                                        }
                                    >
                                        <IconPencil />
                                        "Rename"
                                    </button>
                                }
                            })}
                            {(show_rename && show_delete)
                                .then(|| view! { <div class="my-1 h-px bg-slate-100"></div> })}
                            {show_delete.then(|| {
                                view! {
                                    <button
                                        type="button"
                                        role="menuitem"
                                        class="flex w-full items-center gap-2.5 px-3 py-2 text-left text-sm font-medium text-red-600 hover:bg-red-50"
                                        on:click=move |_| {
                                            open_menu_id.set(None);
                                            if let Some(on_open_delete) = on_open_delete {
                                                on_open_delete.run(id_for_delete.clone());
                                            }
                                        }
                                    >
                                        <IconTrash />
                                        "Delete"
                                    </button>
                                }
                            })}
                        </div>
                    </div>
                }
            })}
        </li>
    }
}

/// One date-grouped section of the sidebar's list, with its own "··· More"
/// disclosure once a group grows past [`COLLAPSE_THRESHOLD`].
///
/// `is_expanded` is a plain `bool`, not a signal: this whole component is
/// recreated fresh every time [`SessionsSidebar`]'s outer reactive closure
/// reruns (see the module doc comment), so there's no need for row-level
/// reactivity here — plain values baked in at construction time are always
/// current as of the last `sessions`/expansion-state change.
#[component]
fn SessionGroupSection(
    group: SessionGroup,
    items: Vec<SessionListItem>,
    is_expanded: bool,
    #[prop(into)] active_id: Signal<Option<String>>,
    on_expand: impl Fn() + 'static + Send + Sync + Copy,
    on_select: impl Fn(String) + 'static + Send + Sync + Copy,
    open_menu_id: RwSignal<Option<String>>,
    on_open_rename: Option<Callback<(String, String)>>,
    on_open_delete: Option<Callback<String>>,
) -> impl IntoView {
    let total = items.len();
    let visible_count = if is_expanded || total <= COLLAPSE_THRESHOLD {
        total
    } else {
        COLLAPSE_THRESHOLD
    };
    let has_more = total > COLLAPSE_THRESHOLD && !is_expanded;

    let rows = items
        .into_iter()
        .take(visible_count)
        .map(|item| {
            let display_title = if item.title.trim().is_empty() {
                "New chat".to_string()
            } else {
                item.title.clone()
            };
            view! {
                <SessionRow
                    id=item.id
                    display_title=display_title
                    raw_title=item.title
                    active_id=active_id
                    on_select=on_select
                    open_menu_id=open_menu_id
                    on_open_rename=on_open_rename
                    on_open_delete=on_open_delete
                />
            }
        })
        .collect_view();

    let more_button = has_more.then(|| {
        view! {
            <button
                type="button"
                class="mt-1 w-full px-2 py-1 text-left text-xs font-medium text-slate-400 hover:text-slate-600"
                on:click=move |_| on_expand()
            >
                "··· More"
            </button>
        }
    });

    view! {
        <div class="mb-3">
            <p class="mb-1 px-2 text-xs font-medium text-slate-400">{group.label()}</p>
            <ul class="space-y-0.5">{rows}</ul>
            {more_button}
        </div>
    }
}

/// The sessions list itself: date-grouped headings ("Today", "Yesterday",
/// "Last 7 Days", "Last 30 Days", "Older" — see `session_groups`) over
/// title rows, with the active session highlighted.
///
/// Renders as **two** separate DOM subtrees (matching the precedent set by
/// `ChatHeaderActions`'s inline-vs-hamburger split): a docked `<aside>`
/// (mounted only while `open`, and only visible on `sm+`) and a `sm:hidden`
/// floating overlay (backdrop + panel), also gated on `open`. Exactly one is
/// ever visible at a given viewport width; both share the same session
/// list/grouping and the same in-panel collapse icon (calls `on_close`).
#[component]
pub fn SessionsSidebar(
    #[prop(into)] sessions: Signal<Vec<SessionListItem>>,
    #[prop(into)] active_id: Signal<Option<String>>,
    #[prop(into)] open: Signal<bool>,
    #[prop(into)] user_email: Signal<Option<String>>,
    on_close: impl Fn() + 'static + Send + Sync + Copy,
    on_select: impl Fn(String) + 'static + Send + Sync + Copy,
    /// When set, each row shows a kebab → Rename → dialog. Omitted by
    /// `web-chat`, which has no rename API yet.
    #[prop(optional)]
    on_rename: Option<Callback<(String, String)>>,
    /// When set, each row's kebab also gets a red "Delete" item → confirm
    /// dialog. Omitted by `web-chat`, which has no delete API yet.
    #[prop(optional)]
    on_delete: Option<Callback<String>>,
) -> impl IntoView {
    // Which groups' "··· More" disclosure has been opened. Hoisted above
    // `body` (rather than living inside `SessionGroupSection`, as it used
    // to) so it survives `body`'s full re-renders on every `sessions`
    // change instead of resetting to collapsed on the very next refresh.
    let expanded_groups = RwSignal::new(HashSet::<SessionGroup>::new());
    let open_menu_id = RwSignal::new(None::<String>);
    let rename_id = RwSignal::new(None::<String>);
    let rename_draft = RwSignal::new(String::new());
    let rename_input = NodeRef::<Input>::new();
    let delete_id = RwSignal::new(None::<String>);

    let open_rename = Callback::new(move |(id, title): (String, String)| {
        open_menu_id.set(None);
        rename_id.set(Some(id));
        rename_draft.set(title);
    });
    let on_open_rename = on_rename.map(|_| open_rename);

    let close_rename = move || {
        rename_id.set(None);
        rename_draft.set(String::new());
    };

    let submit_rename = move || {
        let Some(id) = rename_id.get() else {
            return;
        };
        let trimmed = rename_draft.get();
        let trimmed = trimmed.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(on_rename) = on_rename {
            on_rename.run((id, trimmed.to_string()));
        }
        close_rename();
    };

    Effect::new(move |_| {
        if rename_id.get().is_none() {
            return;
        }
        if let Some(el) = rename_input.get() {
            let _ = el.focus();
            el.select();
        }
    });

    let open_delete = Callback::new(move |id: String| {
        open_menu_id.set(None);
        delete_id.set(Some(id));
    });
    let on_open_delete = on_delete.map(|_| open_delete);

    let close_delete = move || delete_id.set(None);
    let only_session = Signal::derive(move || sessions.get().len() <= 1);

    let confirm_delete = move || {
        let Some(id) = delete_id.get() else {
            return;
        };
        if only_session.get() {
            return;
        }
        if let Some(on_delete) = on_delete {
            on_delete.run(id);
        }
        close_delete();
    };

    let body = move || {
        let group_sections = move || {
            group_sessions(&sessions.get(), js_sys::Date::now() as i64)
                .into_iter()
                .map(|(group, items)| {
                    let is_expanded = expanded_groups.get().contains(&group);
                    view! {
                        <SessionGroupSection
                            group=group
                            items=items
                            is_expanded=is_expanded
                            active_id=active_id
                            on_expand=move || {
                                expanded_groups.update(|set| {
                                    set.insert(group);
                                });
                            }
                            on_select=on_select
                            open_menu_id=open_menu_id
                            on_open_rename=on_open_rename
                            on_open_delete=on_open_delete
                        />
                    }
                })
                .collect_view()
        };

        view! {
            <div class="flex h-full flex-col">
                <div class="flex items-center justify-between border-b border-slate-200 px-3 py-3 min-h-[4rem]">
                    <span class="text-xs font-semibold uppercase tracking-wide text-slate-400">
                        "Chats"
                    </span>
                    <button
                        type="button"
                        aria-label="Collapse chats panel"
                        title="Collapse"
                        class="flex h-7 w-7 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                        on:click=move |_| {
                            on_close()
                        }
                    >
                        <IconSidebarPanel />
                    </button>
                </div>
                <div class="flex-1 overflow-y-auto px-2 py-2">
                    {move || {
                        if sessions.get().is_empty() {
                            view! {
                                <p class="px-2 py-4 text-xs text-slate-400">"No chats yet"</p>
                            }
                            .into_any()
                        } else {
                            group_sections().into_any()
                        }
                    }}
                </div>
                <Show when=move || {
                    user_email.get().is_some_and(|value| !value.trim().is_empty())
                }>
                    <div class="shrink-0 border-t border-slate-200 bg-white">
                        <UserAccountChip email=user_email />
                    </div>
                </Show>
            </div>
        }
    };

    view! {
        <>
            <Show when=move || open.get()>
                <aside class="hidden shrink-0 border-r border-slate-200 bg-white sm:flex sm:w-64 sm:flex-col">
                    {body()}
                </aside>
            </Show>

            <div class=move || {
                if open.get() { "fixed inset-0 z-40 sm:hidden" } else { "hidden" }
            }>
                <div class="fixed inset-0 bg-slate-900/30" aria-hidden="true" on:click=move |_| on_close()></div>
                <div class="relative z-10 flex h-full w-64 max-w-[80vw] flex-col bg-white shadow-xl">
                    {body()}
                </div>
            </div>

            <Show when=move || rename_id.get().is_some()>
                <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                    <div
                        class="absolute inset-0 bg-slate-900/40"
                        aria-hidden="true"
                        on:click=move |_| close_rename()
                    ></div>
                    <div
                        role="dialog"
                        aria-modal="true"
                        aria-labelledby="rename-session-title"
                        class="relative w-full max-w-sm rounded-2xl bg-white p-5 shadow-2xl ring-1 ring-slate-200"
                        on:keydown=move |ev| {
                            let key = ev.key();
                            if key == "Escape" {
                                close_rename();
                            } else if key == "Enter" {
                                ev.prevent_default();
                                submit_rename();
                            }
                        }
                    >
                        <div class="mb-4 flex items-start justify-between gap-3">
                            <h2
                                id="rename-session-title"
                                class="text-base font-semibold text-slate-900"
                            >
                                "Rename session"
                            </h2>
                            <button
                                type="button"
                                aria-label="Close"
                                class="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-slate-400 hover:bg-slate-100 hover:text-slate-700"
                                on:click=move |_| close_rename()
                            >
                                <IconClose />
                            </button>
                        </div>
                        <input
                            node_ref=rename_input
                            type="text"
                            class="mb-5 w-full rounded-lg border border-slate-200 px-3 py-2 text-sm text-slate-900 outline-none focus:border-orange-500 focus:ring-1 focus:ring-orange-500"
                            prop:value=move || rename_draft.get()
                            on:input=move |ev| rename_draft.set(event_target_value(&ev))
                        />
                        <div class="flex justify-end gap-2">
                            <button
                                type="button"
                                class="rounded-lg bg-slate-100 px-3 py-1.5 text-sm font-medium text-slate-700 hover:bg-slate-200"
                                on:click=move |_| close_rename()
                            >
                                "Cancel"
                            </button>
                            <button
                                type="button"
                                class="rounded-lg bg-slate-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-40"
                                disabled=move || rename_draft.get().trim().is_empty()
                                on:click=move |_| submit_rename()
                            >
                                "Save"
                            </button>
                        </div>
                    </div>
                </div>
            </Show>

            <Show when=move || delete_id.get().is_some()>
                <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                    <div
                        class="absolute inset-0 bg-slate-900/40"
                        aria-hidden="true"
                        on:click=move |_| close_delete()
                    ></div>
                    <div
                        role="dialog"
                        aria-modal="true"
                        aria-labelledby="delete-session-title"
                        class="relative w-full max-w-sm rounded-2xl bg-white p-5 shadow-2xl ring-1 ring-slate-200"
                        on:keydown=move |ev| {
                            let key = ev.key();
                            if key == "Escape" {
                                close_delete();
                            }
                        }
                    >
                        <div class="mb-4 flex items-start justify-between gap-3">
                            <h2
                                id="delete-session-title"
                                class="text-base font-semibold text-slate-900"
                            >
                                {move || {
                                    if only_session.get() {
                                        "Cannot delete session"
                                    } else {
                                        "Delete session"
                                    }
                                }}
                            </h2>
                            <button
                                type="button"
                                aria-label="Close"
                                class="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-slate-400 hover:bg-slate-100 hover:text-slate-700"
                                on:click=move |_| close_delete()
                            >
                                <IconClose />
                            </button>
                        </div>
                        <p class="mb-5 text-sm text-slate-600">
                            {move || {
                                if only_session.get() {
                                    "It is not possible to delete the only session."
                                } else {
                                    "Are you sure you want to delete this session?"
                                }
                            }}
                        </p>
                        <div class="flex justify-end gap-2">
                            <Show
                                when=move || only_session.get()
                                fallback=move || {
                                    view! {
                                        <button
                                            type="button"
                                            class="rounded-lg bg-slate-100 px-3 py-1.5 text-sm font-medium text-slate-700 hover:bg-slate-200"
                                            on:click=move |_| close_delete()
                                        >
                                            "Cancel"
                                        </button>
                                        <button
                                            type="button"
                                            class="rounded-lg bg-red-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-red-700"
                                            on:click=move |_| confirm_delete()
                                        >
                                            "Delete"
                                        </button>
                                    }
                                }
                            >
                                <button
                                    type="button"
                                    class="rounded-lg bg-slate-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-slate-800"
                                    on:click=move |_| close_delete()
                                >
                                    "OK"
                                </button>
                            </Show>
                        </div>
                    </div>
                </div>
            </Show>
        </>
    }
}
