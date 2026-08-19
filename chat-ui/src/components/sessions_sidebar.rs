//! Cursor-style sessions sidebar shared by `web-chat` and `websockets-chat`.
//!
//! Display-only for now: rows highlight the active session but clicking one
//! only invokes `on_select` (currently a no-op in both frontends) — there is
//! no session-switch/history-replay API yet. See [`SessionsSidebar`]'s own
//! doc comment for the docked-vs-overlay layout split.
//!
//! Open/collapsed is a single `open: Signal<bool>` shared across every
//! breakpoint (not "always docked on `sm+`, toggle-only on mobile" like an
//! earlier pass) — the same [`SessionsSidebarToggle`] button and the same
//! in-panel collapse icon drive it everywhere.
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

use leptos::prelude::*;

use crate::models::SessionListItem;
use crate::session_groups::{group_sessions, SessionGroup};

/// Rows beyond this count in a single group are collapsed behind a
/// "··· More" toggle, mirroring Cursor's own history list.
const COLLAPSE_THRESHOLD: usize = 8;

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
fn SessionGroupSection(
    group: SessionGroup,
    items: Vec<SessionListItem>,
    is_expanded: bool,
    #[prop(into)] active_id: Signal<Option<String>>,
    on_expand: impl Fn() + 'static + Send + Sync + Copy,
    on_select: impl Fn(String) + 'static + Send + Sync + Copy,
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
            let id_for_class = item.id.clone();
            let id_for_aria = item.id.clone();
            let id_for_click = item.id.clone();
            let title = if item.title.trim().is_empty() {
                "New chat".to_string()
            } else {
                item.title.clone()
            };
            view! {
                <li>
                    <button
                        type="button"
                        class=move || {
                            if active_id.get().as_deref() == Some(id_for_class.as_str()) {
                                "block w-full truncate rounded-lg bg-slate-100 px-2 py-1.5 text-left text-sm font-medium text-slate-900"
                            } else {
                                "block w-full truncate rounded-lg px-2 py-1.5 text-left text-sm text-slate-600 hover:bg-slate-50"
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
                        title={item.title.clone()}
                    >
                        {title}
                    </button>
                </li>
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
    on_close: impl Fn() + 'static + Send + Sync + Copy,
    on_select: impl Fn(String) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    // Which groups' "··· More" disclosure has been opened. Hoisted above
    // `body` (rather than living inside `SessionGroupSection`, as it used
    // to) so it survives `body`'s full re-renders on every `sessions`
    // change instead of resetting to collapsed on the very next refresh.
    let expanded_groups = RwSignal::new(HashSet::<SessionGroup>::new());

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
                        on:click=move |_| on_close()
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
        </>
    }
}
