//! Cursor-style sessions sidebar shared by `web-chat` and `websockets-chat`.
//!
//! Rows highlight the active session; clicking one invokes `on_select`.
//! When `on_pin`, `on_rename`, `on_workspace`, `on_summary`, `on_fork`, `on_clear`,
//! and/or `on_delete` is provided (websockets-chat), each row also has a
//! kebab that opens a menu with "Pin session"/"Unpin session" (pinned rows are
//! listed in a "Pinned" group on top and show a pin icon), "Rename", "Workspace" (localhost only),
//! "Summary" (only when that row has a persisted summary), "Fork session",
//! "Clear session", and/or a red "Delete" item.
//! Delete and clear each open their own confirmation dialog before actually
//! calling `on_delete`/`on_clear`; rename opens a small dialog to edit the
//! title; fork fires immediately (non-destructive — it only ever creates a
//! new chat).
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

use leptos::html::Input;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::components::brand::BrandLogo;
use crate::models::{SessionListItem, SessionSummaryPopup, WorkspaceDialogState};
use crate::session_groups::{
    display_title, filter_sessions_by_title, group_sessions, SessionGroup,
};

/// Rows beyond this count in a single group are collapsed behind a
/// "··· More" toggle, mirroring Cursor's own history list.
const COLLAPSE_THRESHOLD: usize = 8;

/// Width of the kebab menu (`w-40`), needed to right-align it to the kebab.
const MENU_WIDTH_PX: f64 = 160.0;

/// Height of one menu item (`py-2` + `text-sm` line) and of one divider
/// (`my-1` + 1px rule), used to estimate the menu height before it renders.
const MENU_ITEM_HEIGHT_PX: f64 = 36.0;
const MENU_DIVIDER_HEIGHT_PX: f64 = 9.0;

/// Minimum gap kept between the menu and the viewport edge.
const MENU_EDGE_GAP_PX: f64 = 8.0;

const MENU_ITEM: &str = "flex w-full items-center gap-2.5 px-3 py-2 text-left text-sm font-medium text-slate-700 hover:bg-slate-50";

fn icon_class() -> &'static str {
    "h-4 w-4"
}

/// Panel-with-left-rail glyph shared by the sidebar's collapse button, the
/// legacy header toggle and the collapsed-state icon strip.
#[component]
pub(crate) fn IconSidebarPanel() -> impl IntoView {
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

/// Speech bubble with a `+` (DeepSeek's "New Session" glyph), used by the
/// sidebar's "New chat" button and the icon strip's new-chat button.
#[component]
pub(crate) fn IconChatPlus() -> impl IntoView {
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
            <path d="M21 11.5a8.4 8.4 0 0 1-9 8.4 8.6 8.6 0 0 1-3.6-.8L3 21l1.9-5.4A8.4 8.4 0 0 1 3 11.5 8.5 8.5 0 0 1 12 3a8.5 8.5 0 0 1 9 8.5Z" />
            <line x1="12" x2="12" y1="8" y2="15" />
            <line x1="8.5" x2="15.5" y1="11.5" y2="11.5" />
        </svg>
    }
}

/// Narrowest width (px) the docked sidebar can be dragged to.
const SIDEBAR_MIN_WIDTH_PX: f64 = 160.0;
/// Widest width (px) the docked sidebar can be dragged to.
const SIDEBAR_MAX_WIDTH_PX: f64 = 480.0;

/// Width (px) the sidebar should have after a drag: the width it had when
/// the drag started, shifted by how far the pointer has moved horizontally,
/// and kept within [`SIDEBAR_MIN_WIDTH_PX`]..=[`SIDEBAR_MAX_WIDTH_PX`].
fn dragged_sidebar_width(start_width: f64, start_pointer_x: f64, current_pointer_x: f64) -> f64 {
    (start_width + current_pointer_x - start_pointer_x)
        .clamp(SIDEBAR_MIN_WIDTH_PX, SIDEBAR_MAX_WIDTH_PX)
}

/// Tailwind classes for a sidebar page button ("MCPs", "Sub-agents"), highlighted while
/// its page is showing.
fn nav_button_class(active: bool) -> &'static str {
    if active {
        "flex w-full items-center gap-2 rounded-xl bg-slate-200 px-3 py-2 text-sm font-medium text-slate-900"
    } else {
        "flex w-full items-center gap-2 rounded-xl px-3 py-2 text-sm font-medium text-slate-600 hover:bg-slate-100"
    }
}

/// A full-width sidebar button that opens a list page, highlighted while that page is
/// showing. The icon is passed as the children.
#[component]
fn SidebarNavButton(
    label: &'static str,
    #[prop(into)] active: Signal<bool>,
    on_click: Callback<()>,
    children: Children,
) -> impl IntoView {
    view! {
        <div class="px-2 pt-2">
            <button
                type="button"
                class=move || nav_button_class(active.get())
                aria-pressed=move || active.get().to_string()
                on:click=move |_| on_click.run(())
            >
                {children()}
                {label}
            </button>
        </div>
    }
}

/// Two linked nodes, used by the sidebar's "Sub-agents" button.
#[component]
fn IconAgents() -> impl IntoView {
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
            <circle cx="12" cy="5" r="2.5" />
            <circle cx="5" cy="19" r="2.5" />
            <circle cx="19" cy="19" r="2.5" />
            <path d="M12 7.5v4M12 11.5 6.2 16.8M12 11.5l5.8 5.3" />
        </svg>
    }
}

/// Gear, used by the sidebar's "Settings" button.
#[component]
fn IconSettings() -> impl IntoView {
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
            <circle cx="12" cy="12" r="3" />
            <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33h0a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51h0a1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82v0a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1Z" />
        </svg>
    }
}

/// Puzzle piece, used by the sidebar's "MCPs" button.
#[component]
fn IconPuzzle() -> impl IntoView {
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
            <path d="M19.4 12.6V8a1 1 0 0 0-1-1h-4.6a2.5 2.5 0 1 0-5 0H4.2a1 1 0 0 0-1 1v4.6a2.5 2.5 0 1 1 0 5V21a1 1 0 0 0 1 1h4.6v-.8a2.5 2.5 0 1 1 5 0v.8h4.6a1 1 0 0 0 1-1v-4.6a2.5 2.5 0 1 0 0-5Z" />
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

/// Pin glyph. `filled` renders the solid variant used as the "this row is
/// pinned" marker; the outline variant is the menu item's icon.
#[component]
fn IconPin(#[prop(default = false)] filled: bool) -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill=if filled { "currentColor" } else { "none" }
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class=if filled { "h-3.5 w-3.5" } else { icon_class() }
            aria-hidden="true"
        >
            <path d="M12 17v5" />
            <path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z" />
        </svg>
    }
}

/// Magnifying glass, used by the sidebar's search toggle and search field.
#[component]
fn IconSearch() -> impl IntoView {
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
            <circle cx="11" cy="11" r="8" />
            <path d="m21 21-4.3-4.3" />
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
fn IconFork() -> impl IntoView {
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
            <circle cx="12" cy="18" r="3" />
            <circle cx="6" cy="6" r="3" />
            <circle cx="18" cy="6" r="3" />
            <path d="M18 9v1a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V9" />
            <path d="M12 12v3" />
        </svg>
    }
}

#[component]
fn IconDocument() -> impl IntoView {
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
            <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" />
            <path d="M14 2v4a2 2 0 0 0 2 2h4" />
            <path d="M10 9H8" />
            <path d="M16 13H8" />
            <path d="M16 17H8" />
        </svg>
    }
}

#[component]
fn IconFolder() -> impl IntoView {
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
            <path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" />
        </svg>
    }
}

#[component]
fn IconEraser() -> impl IntoView {
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
            <path d="m7 21-4.3-4.3c-1-1-1-2.5 0-3.4l9.6-9.6c1-1 2.5-1 3.4 0l5.6 5.6c1 1 1 2.5 0 3.4L13 21" />
            <path d="M22 21H7" />
            <path d="m5 11 9 9" />
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

/// Tooltip for the sidebar's collapse button: `"Collapse"`, or
/// `"Close sidebar <hint>"` when the caller supplies a shortcut hint.
fn collapse_tooltip(hint: Option<&str>) -> String {
    match hint {
        Some(hint) => format!("Close sidebar {hint}"),
        None => "Collapse".to_string(),
    }
}

/// Estimated rendered height of a kebab menu with `item_count` items
/// separated by dividers (plus the `py-1` padding).
fn estimated_menu_height(item_count: usize) -> f64 {
    let count = item_count as f64;
    count * MENU_ITEM_HEIGHT_PX + (count - 1.0).max(0.0) * MENU_DIVIDER_HEIGHT_PX + 8.0
}

/// Inline `position: fixed` style for a kebab menu anchored to the button
/// spanning `anchor_top..anchor_bottom` and ending at `anchor_right`.
///
/// The menu opens below the button when it fits there, otherwise above it,
/// otherwise on whichever side has more room — in that last case its
/// `max-height` shrinks it so it scrolls internally rather than leaving the
/// viewport.
fn menu_placement_style(
    anchor_top: f64,
    anchor_bottom: f64,
    anchor_right: f64,
    viewport_height: f64,
    menu_height: f64,
) -> String {
    let space_below = viewport_height - anchor_bottom - MENU_EDGE_GAP_PX;
    let space_above = anchor_top - MENU_EDGE_GAP_PX;
    let left = (anchor_right + 4.0 - MENU_WIDTH_PX).max(MENU_EDGE_GAP_PX);
    let open_below = space_below >= menu_height || space_below >= space_above;
    if open_below {
        format!(
            "left:{left}px;top:{}px;max-height:{}px",
            anchor_bottom + 2.0,
            space_below.max(0.0)
        )
    } else {
        format!(
            "left:{left}px;bottom:{}px;max-height:{}px",
            viewport_height - anchor_top + 2.0,
            space_above.max(0.0)
        )
    }
}

/// Placement style for the menu opened from `kebab` (the clicked button), or
/// an empty style when the browser geometry is unavailable.
fn menu_style_for_button(kebab: &web_sys::Element, menu_height: f64) -> String {
    let viewport_height = web_sys::window()
        .and_then(|window| window.inner_height().ok())
        .and_then(|height| height.as_f64());
    let Some(viewport_height) = viewport_height else {
        return String::new();
    };
    let rect = kebab.get_bounding_client_rect();
    menu_placement_style(
        rect.top(),
        rect.bottom(),
        rect.right(),
        viewport_height,
        menu_height,
    )
}

/// Label of the kebab menu's pin item for a row that is currently `pinned`.
fn pin_menu_label(pinned: bool) -> &'static str {
    if pinned {
        "Unpin session"
    } else {
        "Pin session"
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
    has_summary: bool,
    pinned: bool,
    #[prop(into)] active_id: Signal<Option<String>>,
    on_select: impl Fn(String) + 'static + Send + Sync + Copy,
    open_menu_id: RwSignal<Option<String>>,
    on_open_pin: Option<Callback<(String, bool)>>,
    on_open_rename: Option<Callback<(String, String)>>,
    on_open_workspace: Option<Callback<String>>,
    on_open_summary: Option<Callback<String>>,
    on_open_fork: Option<Callback<String>>,
    on_open_clear: Option<Callback<String>>,
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
    let menu_style = RwSignal::new(String::new());
    let display_title_for_menu = display_title.clone();
    let show_pin = on_open_pin.is_some();
    let show_rename = on_open_rename.is_some();
    let show_workspace = on_open_workspace.is_some();
    let show_summary = on_open_summary.is_some() && has_summary;
    let show_fork = on_open_fork.is_some();
    let show_clear = on_open_clear.is_some();
    let show_delete = on_open_delete.is_some();
    let show_menu = show_pin
        || show_rename
        || show_workspace
        || show_summary
        || show_fork
        || show_clear
        || show_delete;
    let menu_item_count = [
        show_pin,
        show_rename,
        show_workspace,
        show_summary,
        show_fork,
        show_clear,
        show_delete,
    ]
    .into_iter()
    .filter(|shown| *shown)
    .count();

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
                {pinned.then(|| {
                    view! {
                        <span
                            class="mr-1 flex shrink-0 items-center text-slate-400"
                            title="Pinned"
                            role="img"
                            aria-label="Pinned"
                        >
                            <IconPin filled=true />
                        </span>
                    }
                })}
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
                                if let Some(kebab) = ev
                                    .current_target()
                                    .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                                {
                                    menu_style
                                        .set(
                                            menu_style_for_button(
                                                &kebab,
                                                estimated_menu_height(menu_item_count),
                                            ),
                                        );
                                }
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
                let id_for_pin = id.clone();
                let id_for_rename = id.clone();
                let id_for_workspace = id.clone();
                let id_for_summary = id.clone();
                let id_for_fork = id.clone();
                let id_for_clear = id.clone();
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
                            class="fixed z-30 w-40 overflow-y-auto rounded-xl bg-white py-1 shadow-lg ring-1 ring-slate-200"
                            style=move || menu_style.get()
                        >
                            {show_pin.then(|| {
                                view! {
                                    <button
                                        type="button"
                                        role="menuitem"
                                        class=MENU_ITEM
                                        on:click=move |_| {
                                            open_menu_id.set(None);
                                            if let Some(on_open_pin) = on_open_pin {
                                                on_open_pin.run((id_for_pin.clone(), !pinned));
                                            }
                                        }
                                    >
                                        <IconPin />
                                        {pin_menu_label(pinned)}
                                    </button>
                                }
                            })}
                            {(show_pin
                                && (show_rename
                                    || show_workspace
                                    || show_summary
                                    || show_fork
                                    || show_clear
                                    || show_delete))
                                .then(|| view! { <div class="my-1 h-px bg-slate-100"></div> })}
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
                            {(show_rename
                                && (show_workspace
                                    || show_summary
                                    || show_fork
                                    || show_clear
                                    || show_delete))
                                .then(|| view! { <div class="my-1 h-px bg-slate-100"></div> })}
                            {show_workspace.then(|| {
                                view! {
                                    <button
                                        type="button"
                                        role="menuitem"
                                        class=MENU_ITEM
                                        on:click=move |_| {
                                            open_menu_id.set(None);
                                            if let Some(on_open_workspace) = on_open_workspace {
                                                on_open_workspace.run(id_for_workspace.clone());
                                            }
                                        }
                                    >
                                        <IconFolder />
                                        "Workspace"
                                    </button>
                                }
                            })}
                            {(show_workspace
                                && (show_summary || show_fork || show_clear || show_delete))
                                .then(|| view! { <div class="my-1 h-px bg-slate-100"></div> })}
                            {show_summary.then(|| {
                                view! {
                                    <button
                                        type="button"
                                        role="menuitem"
                                        class=MENU_ITEM
                                        on:click=move |_| {
                                            open_menu_id.set(None);
                                            if let Some(on_open_summary) = on_open_summary {
                                                on_open_summary.run(id_for_summary.clone());
                                            }
                                        }
                                    >
                                        <IconDocument />
                                        "Summary"
                                    </button>
                                }
                            })}
                            {(show_summary && (show_fork || show_clear || show_delete))
                                .then(|| view! { <div class="my-1 h-px bg-slate-100"></div> })}
                            {show_fork.then(|| {
                                view! {
                                    <button
                                        type="button"
                                        role="menuitem"
                                        class=MENU_ITEM
                                        on:click=move |_| {
                                            open_menu_id.set(None);
                                            if let Some(on_open_fork) = on_open_fork {
                                                on_open_fork.run(id_for_fork.clone());
                                            }
                                        }
                                    >
                                        <IconFork />
                                        "Fork session"
                                    </button>
                                }
                            })}
                            {(show_fork && (show_clear || show_delete))
                                .then(|| view! { <div class="my-1 h-px bg-slate-100"></div> })}
                            {show_clear.then(|| {
                                view! {
                                    <button
                                        type="button"
                                        role="menuitem"
                                        class=MENU_ITEM
                                        on:click=move |_| {
                                            open_menu_id.set(None);
                                            if let Some(on_open_clear) = on_open_clear {
                                                on_open_clear.run(id_for_clear.clone());
                                            }
                                        }
                                    >
                                        <IconEraser />
                                        "Clear session"
                                    </button>
                                }
                            })}
                            {(show_clear && show_delete)
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
    on_open_pin: Option<Callback<(String, bool)>>,
    on_open_rename: Option<Callback<(String, String)>>,
    on_open_workspace: Option<Callback<String>>,
    on_open_summary: Option<Callback<String>>,
    on_open_fork: Option<Callback<String>>,
    on_open_clear: Option<Callback<String>>,
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
            let display_title = display_title(&item).to_string();
            view! {
                <SessionRow
                    id=item.id
                    display_title=display_title
                    raw_title=item.title
                    has_summary=item.has_summary
                    pinned=item.pinned
                    active_id=active_id
                    on_select=on_select
                    open_menu_id=open_menu_id
                    on_open_pin=on_open_pin
                    on_open_rename=on_open_rename
                    on_open_workspace=on_open_workspace
                    on_open_summary=on_open_summary
                    on_open_fork=on_open_fork
                    on_open_clear=on_open_clear
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
    on_close: impl Fn() + 'static + Send + Sync + Copy,
    on_select: impl Fn(String) + 'static + Send + Sync + Copy,
    /// When set, each row's kebab gets a "Pin session"/"Unpin session" item
    /// that fires immediately with `(chat_id, pinned)` — the *new* state.
    /// Omitted by `web-chat`, which has no pin API.
    #[prop(optional)]
    on_pin: Option<Callback<(String, bool)>>,
    /// When set, each row shows a kebab → Rename → dialog. Omitted by
    /// `web-chat`, which has no rename API yet.
    #[prop(optional)]
    on_rename: Option<Callback<(String, String)>>,
    /// When set, each row's kebab also gets a "Workspace" item that opens
    /// the folder-and-access-mode dialog. Omitted unless the parent is
    /// running on localhost.
    #[prop(default = None)]
    on_workspace: Option<Callback<String>>,
    /// Current contents of the Workspace dialog. `None` hides it; `Some`
    /// with `path: None` is the in-flight loading state.
    #[prop(optional)]
    workspace_popup: Option<Signal<Option<WorkspaceDialogState>>>,
    #[prop(optional)] on_close_workspace: Option<Callback<()>>,
    #[prop(optional)] on_browse_workspace: Option<Callback<String>>,
    #[prop(optional)] on_save_workspace: Option<Callback<(String, String)>>,
    #[prop(optional)] on_default_workspace: Option<Callback<()>>,
    /// When set, a row whose session has a persisted summary gets a
    /// "Summary" kebab item. Clicking it fires this callback and the parent
    /// fills [`summary_popup`]. Omitted by `web-chat`.
    #[prop(optional)]
    on_summary: Option<Callback<String>>,
    /// The Summary dialog's current contents. `None` hides it; `Some` with
    /// empty `text` is the in-flight loading state.
    #[prop(optional)]
    summary_popup: Option<Signal<Option<SessionSummaryPopup>>>,
    #[prop(optional)] on_close_summary: Option<Callback<()>>,
    /// When set, each row's kebab also gets a "Fork session" item that fires
    /// immediately (no confirmation dialog — forking is non-destructive, it
    /// only ever creates a new chat). Omitted by `web-chat`, which has no
    /// fork API yet.
    #[prop(optional)]
    on_fork: Option<Callback<String>>,
    /// When set, each row's kebab also gets a "Clear session" item → confirm
    /// dialog. Unlike delete, clearing the only remaining session is
    /// allowed — the chat stays in the sidebar, just with its messages
    /// wiped. Omitted by `web-chat`, which has no clear API yet.
    #[prop(optional)]
    on_clear: Option<Callback<String>>,
    /// When set, each row's kebab also gets a red "Delete" item → confirm
    /// dialog. Omitted by `web-chat`, which has no delete API yet.
    #[prop(optional)]
    on_delete: Option<Callback<String>>,
    /// When set, a full-width "New chat" button is shown at the top of the
    /// list column (below the header row). Omitted by `web-chat`, whose
    /// header carries its own "New chat" button.
    #[prop(optional)]
    on_new_chat: Option<Callback<()>>,
    /// When set, an "MCPs" button is shown under "New chat" that fires this
    /// callback (websockets-chat opens its MCP list). Omitted by `web-chat`
    /// and for anonymous visitors.
    #[prop(default = None)]
    on_mcps: Option<Callback<()>>,
    /// Whether the MCP list is currently showing, so the "MCPs" button can
    /// render as selected.
    #[prop(into, default = Signal::stored(false))]
    mcps_active: Signal<bool>,
    /// When set, a "Sub-agents" button is shown under "MCPs" that fires this callback
    /// (websockets-chat opens its child agents and spawn tasks list). Omitted by
    /// `web-chat` and for anonymous visitors.
    #[prop(default = None)]
    on_subagents: Option<Callback<()>>,
    /// Whether the sub-agents list is currently showing.
    #[prop(into, default = Signal::stored(false))]
    subagents_active: Signal<bool>,
    /// When set, a "Settings" button is pinned to the bottom of the sidebar and fires
    /// this callback (websockets-chat opens its Settings dialog). Omitted by
    /// `web-chat` and for anonymous visitors.
    #[prop(default = None)]
    on_settings: Option<Callback<()>>,
    /// When `true`, the header row shows the rust-bot logo + title instead of
    /// the "Chats" label.
    #[prop(default = false)]
    show_brand: bool,
    /// Appended to the collapse button's tooltip (e.g. the keyboard
    /// shortcut: `"Close sidebar Ctrl+B"`). `None` keeps the plain tooltip.
    #[prop(optional)]
    collapse_hint: Option<String>,
    /// Narrower docked panel (`sm:w-48` instead of `sm:w-64`) for the small
    /// floating-widget window.
    #[prop(into, default = Signal::stored(false))]
    compact: Signal<bool>,
) -> impl IntoView {
    // Which groups' "··· More" disclosure has been opened. Hoisted above
    // `body` (rather than living inside `SessionGroupSection`, as it used
    // to) so it survives `body`'s full re-renders on every `sessions`
    // change instead of resetting to collapsed on the very next refresh.
    let expanded_groups = RwSignal::new(HashSet::<SessionGroup>::new());
    let open_menu_id = RwSignal::new(None::<String>);
    // Title search: the field is hidden until the magnifier is clicked, and
    // closing it clears the query so the full list returns.
    let search_open = RwSignal::new(false);
    let search_query = RwSignal::new(String::new());
    let toggle_search = move || {
        if search_open.get_untracked() {
            search_open.set(false);
            search_query.set(String::new());
        } else {
            search_open.set(true);
        }
    };
    // Drag-to-resize: `None` keeps the default (compact/regular) width class
    // until the user first drags; `resize_drag` holds the pointer x and the
    // sidebar width at the moment the current drag started.
    let sidebar_width_px = RwSignal::new(None::<f64>);
    let resize_drag = RwSignal::new(None::<(f64, f64)>);
    let start_resize = move |ev: web_sys::PointerEvent| {
        let Some(handle) = ev
            .current_target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let Some(sidebar) = handle.parent_element() else {
            return;
        };
        ev.prevent_default();
        let _ = handle.set_pointer_capture(ev.pointer_id());
        let start_width = sidebar.get_bounding_client_rect().width();
        resize_drag.set(Some((f64::from(ev.client_x()), start_width)));
    };
    let continue_resize = move |ev: web_sys::PointerEvent| {
        if let Some((start_pointer_x, start_width)) = resize_drag.get_untracked() {
            sidebar_width_px.set(Some(dragged_sidebar_width(
                start_width,
                start_pointer_x,
                f64::from(ev.client_x()),
            )));
        }
    };
    let end_resize = move |_: web_sys::PointerEvent| resize_drag.set(None);
    let rename_id = RwSignal::new(None::<String>);
    let rename_draft = RwSignal::new(String::new());
    let rename_input = NodeRef::<Input>::new();
    let delete_id = RwSignal::new(None::<String>);
    let clear_id = RwSignal::new(None::<String>);

    let open_pin = Callback::new(move |(id, pinned): (String, bool)| {
        open_menu_id.set(None);
        if let Some(on_pin) = on_pin {
            on_pin.run((id, pinned));
        }
    });
    let on_open_pin = on_pin.map(|_| open_pin);

    let open_rename = Callback::new(move |(id, title): (String, String)| {
        open_menu_id.set(None);
        rename_id.set(Some(id));
        rename_draft.set(title);
    });
    let on_open_rename = on_rename.map(|_| open_rename);

    let workspace_access_mode = RwSignal::new("restricted".to_string());
    let open_workspace = Callback::new(move |id: String| {
        open_menu_id.set(None);
        workspace_access_mode.set("restricted".to_string());
        if let Some(on_workspace) = on_workspace {
            on_workspace.run(id);
        }
    });
    let on_open_workspace = on_workspace.map(|_| open_workspace);

    let close_workspace = move || {
        workspace_access_mode.set("restricted".to_string());
        if let Some(on_close_workspace) = on_close_workspace {
            on_close_workspace.run(());
        }
    };

    let open_summary = Callback::new(move |id: String| {
        open_menu_id.set(None);
        if let Some(on_summary) = on_summary {
            on_summary.run(id);
        }
    });
    let on_open_summary = on_summary.map(|_| open_summary);

    let close_summary = move || {
        if let Some(on_close_summary) = on_close_summary {
            on_close_summary.run(());
        }
    };

    // No dialog/confirm step for fork — unlike rename/delete, this just
    // fires the callback immediately.
    let open_fork = Callback::new(move |id: String| {
        open_menu_id.set(None);
        if let Some(on_fork) = on_fork {
            on_fork.run(id);
        }
    });
    let on_open_fork = on_fork.map(|_| open_fork);

    let open_clear = Callback::new(move |id: String| {
        open_menu_id.set(None);
        clear_id.set(Some(id));
    });
    let on_open_clear = on_clear.map(|_| open_clear);

    let close_clear = move || clear_id.set(None);

    let confirm_clear = move || {
        let Some(id) = clear_id.get() else {
            return;
        };
        if let Some(on_clear) = on_clear {
            on_clear.run(id);
        }
        close_clear();
    };

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

    // `StoredValue` is `Copy`, which keeps `body` callable from both the docked
    // aside and the mobile overlay.
    let collapse_title = StoredValue::new(collapse_tooltip(collapse_hint.as_deref()));

    let body = move || {
        let search_input = NodeRef::<Input>::new();
        Effect::new(move |_| {
            if !search_open.get() {
                return;
            }
            if let Some(el) = search_input.get() {
                let _ = el.focus();
            }
        });
        let group_sections = move || {
            let visible = filter_sessions_by_title(&sessions.get(), &search_query.get());
            group_sessions(&visible, js_sys::Date::now() as i64)
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
                            on_open_pin=on_open_pin
                            on_open_rename=on_open_rename
                            on_open_workspace=on_open_workspace
                            on_open_summary=on_open_summary
                            on_open_fork=on_open_fork
                            on_open_clear=on_open_clear
                            on_open_delete=on_open_delete
                        />
                    }
                })
                .collect_view()
        };

        view! {
            <div class="flex h-full flex-col">
                <div class="flex items-center justify-between border-b border-slate-200 px-3 py-3 h-14">
                    {if show_brand {
                        view! {
                            <span class="flex min-w-0 items-center gap-2">
                                <BrandLogo class="h-6 w-6 shrink-0" />
                                <span class="truncate text-base font-semibold text-slate-900">
                                    "rust-bot"
                                </span>
                            </span>
                        }
                        .into_any()
                    } else {
                        view! {
                            <span class="text-xs font-semibold uppercase tracking-wide text-slate-400">
                                "Chats"
                            </span>
                        }
                        .into_any()
                    }}
                    <div class="flex shrink-0 items-center gap-1">
                        <button
                            type="button"
                            aria-label="Collapse chats panel"
                            title=collapse_title.get_value()
                            class="flex h-7 w-7 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                            on:click=move |_| {
                                on_close()
                            }
                        >
                            <IconSidebarPanel />
                        </button>
                    </div>
                </div>
                {on_new_chat.map(|on_new_chat| {
                    view! {
                        <div class="px-2 pt-3">
                            <button
                                type="button"
                                class="flex w-full items-center justify-center gap-2 rounded-xl border border-slate-200 bg-white px-3 py-2.5 text-sm font-medium text-slate-700 shadow-sm hover:bg-slate-50"
                                on:click=move |_| on_new_chat.run(())
                            >
                                <IconChatPlus />
                                "New chat"
                            </button>
                        </div>
                    }
                })}
                {on_mcps.map(|on_mcps| {
                    view! {
                        <SidebarNavButton label="MCPs" active=mcps_active on_click=on_mcps>
                            <IconPuzzle />
                        </SidebarNavButton>
                    }
                })}
                {on_subagents.map(|on_subagents| {
                    view! {
                        <SidebarNavButton
                            label="Sub-agents"
                            active=subagents_active
                            on_click=on_subagents
                        >
                            <IconAgents />
                        </SidebarNavButton>
                    }
                })}
                <div class="flex justify-end px-2 pt-2">
                    <button
                        type="button"
                        aria-label="Search chats"
                        title="Search"
                        aria-pressed=move || search_open.get().to_string()
                        class=move || {
                            if search_open.get() {
                                "flex h-7 w-7 items-center justify-center rounded-full bg-slate-100 text-slate-700"
                            } else {
                                "flex h-7 w-7 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                            }
                        }
                        on:click=move |_| toggle_search()
                    >
                        <IconSearch />
                    </button>
                </div>
                {move || {
                    search_open
                        .get()
                        .then(|| {
                            view! {
                                <div class="px-2 pt-3">
                                    <div class="flex items-center gap-2 rounded-xl border border-slate-200 bg-white px-3 py-1.5 text-slate-400 focus-within:border-orange-500">
                                        <IconSearch />
                                        <input
                                            node_ref=search_input
                                            type="text"
                                            placeholder="Search session names"
                                            aria-label="Search session names"
                                            class="min-w-0 flex-1 bg-transparent text-sm text-slate-900 outline-none placeholder:text-slate-400"
                                            prop:value=move || search_query.get()
                                            on:input=move |ev| search_query.set(event_target_value(&ev))
                                            on:keydown=move |ev| {
                                                if ev.key() == "Escape" {
                                                    toggle_search();
                                                }
                                            }
                                        />
                                        <button
                                            type="button"
                                            aria-label="Close search"
                                            class="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-slate-500 hover:bg-slate-100 hover:text-slate-800"
                                            on:click=move |_| toggle_search()
                                        >
                                            <IconClose />
                                        </button>
                                    </div>
                                </div>
                            }
                        })
                }}
                <div class="flex-1 overflow-y-auto px-2 py-2">
                    {move || {
                        if sessions.get().is_empty() {
                            view! {
                                <p class="px-2 py-4 text-xs text-slate-400">"No chats yet"</p>
                            }
                            .into_any()
                        } else if filter_sessions_by_title(&sessions.get(), &search_query.get())
                            .is_empty()
                        {
                            view! {
                                <p class="px-2 py-4 text-xs text-slate-400">"No matching chats"</p>
                            }
                            .into_any()
                        } else {
                            group_sections().into_any()
                        }
                    }}
                </div>
                {on_settings.map(|on_settings| {
                    view! {
                        <div class="border-t border-slate-200 pb-2">
                            <SidebarNavButton
                                label="Settings"
                                active=Signal::stored(false)
                                on_click=on_settings
                            >
                                <IconSettings />
                            </SidebarNavButton>
                        </div>
                    }
                })}
            </div>
        }
    };

    view! {
        <>
            <Show when=move || open.get()>
                <aside
                    class=move || {
                        let base = "relative hidden shrink-0 border-r border-slate-200 bg-white sm:flex sm:flex-col";
                        match (sidebar_width_px.get(), compact.get()) {
                            (Some(_), _) => base.to_string(),
                            (None, true) => format!("{base} sm:w-48"),
                            (None, false) => format!("{base} sm:w-64"),
                        }
                    }
                    style=move || {
                        sidebar_width_px.get().map(|width| format!("width:{width}px"))
                    }
                >
                    <div
                        role="separator"
                        aria-orientation="vertical"
                        aria-label="Resize chats panel"
                        class=move || {
                            if resize_drag.get().is_some() {
                                "absolute inset-y-0 -right-1 z-20 w-2 cursor-col-resize touch-none select-none bg-orange-400/60"
                            } else {
                                "absolute inset-y-0 -right-1 z-20 w-2 cursor-col-resize touch-none select-none hover:bg-orange-400/40"
                            }
                        }
                        on:pointerdown=start_resize
                        on:pointermove=continue_resize
                        on:pointerup=end_resize
                        on:pointercancel=end_resize
                    ></div>
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

            <Show when=move || {
                summary_popup
                    .and_then(|popup| popup.get())
                    .is_some()
            }>
                <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                    <div
                        class="absolute inset-0 bg-slate-900/40"
                        aria-hidden="true"
                        on:click=move |_| close_summary()
                    ></div>
                    <div
                        role="dialog"
                        aria-modal="true"
                        aria-labelledby="session-summary-title"
                        class="relative flex w-full max-w-lg flex-col rounded-2xl bg-white p-5 shadow-2xl ring-1 ring-slate-200"
                        on:keydown=move |ev| {
                            if ev.key() == "Escape" {
                                close_summary();
                            }
                        }
                    >
                        <div class="mb-3 flex items-start justify-between gap-3">
                            <h2
                                id="session-summary-title"
                                class="text-base font-semibold text-slate-900"
                            >
                                "Summary"
                            </h2>
                            <button
                                type="button"
                                aria-label="Close"
                                class="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-slate-400 hover:bg-slate-100 hover:text-slate-700"
                                on:click=move |_| close_summary()
                            >
                                <IconClose />
                            </button>
                        </div>
                        {move || {
                            let popup = summary_popup.and_then(|signal| signal.get());
                            match popup {
                                Some(SessionSummaryPopup {
                                    text: Some(text),
                                    last_active,
                                    ..
                                }) => {
                                    let timestamp = last_active
                                        .filter(|value| !value.is_empty())
                                        .map(|value| format!("Last active: {value}"));
                                    view! {
                                        <div class="min-h-0 max-h-[min(24rem,70vh)] overflow-y-auto whitespace-pre-wrap text-sm text-slate-700">
                                            {text}
                                        </div>
                                        {timestamp
                                            .map(|label| {
                                                view! {
                                                    <p class="mt-3 text-xs text-slate-400">{label}</p>
                                                }
                                            })}
                                    }
                                    .into_any()
                                }
                                _ => view! {
                                    <p class="text-sm text-slate-400">"Loading…"</p>
                                }
                                .into_any(),
                            }
                        }}
                        <div class="mt-5 flex justify-end">
                            <button
                                type="button"
                                class="rounded-lg bg-slate-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-slate-800"
                                on:click=move |_| close_summary()
                            >
                                "Close"
                            </button>
                        </div>
                    </div>
                </div>
            </Show>

            <Show when=move || clear_id.get().is_some()>
                <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                    <div
                        class="absolute inset-0 bg-slate-900/40"
                        aria-hidden="true"
                        on:click=move |_| close_clear()
                    ></div>
                    <div
                        role="dialog"
                        aria-modal="true"
                        aria-labelledby="clear-session-title"
                        class="relative w-full max-w-sm rounded-2xl bg-white p-5 shadow-2xl ring-1 ring-slate-200"
                        on:keydown=move |ev| {
                            let key = ev.key();
                            if key == "Escape" {
                                close_clear();
                            }
                        }
                    >
                        <div class="mb-4 flex items-start justify-between gap-3">
                            <h2
                                id="clear-session-title"
                                class="text-base font-semibold text-slate-900"
                            >
                                "Clear session"
                            </h2>
                            <button
                                type="button"
                                aria-label="Close"
                                class="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-slate-400 hover:bg-slate-100 hover:text-slate-700"
                                on:click=move |_| close_clear()
                            >
                                <IconClose />
                            </button>
                        </div>
                        <p class="mb-5 text-sm text-slate-600">
                            "Messages will be removed, but this chat will stay in the list."
                        </p>
                        <div class="flex justify-end gap-2">
                            <button
                                type="button"
                                class="rounded-lg bg-slate-100 px-3 py-1.5 text-sm font-medium text-slate-700 hover:bg-slate-200"
                                on:click=move |_| close_clear()
                            >
                                "Cancel"
                            </button>
                            <button
                                type="button"
                                class="rounded-lg bg-slate-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-slate-800"
                                on:click=move |_| confirm_clear()
                            >
                                "Clear"
                            </button>
                        </div>
                    </div>
                </div>
            </Show>

            <Show when=move || {
                workspace_popup
                    .and_then(|popup| popup.get())
                    .is_some()
            }>
                <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                    <div
                        class="absolute inset-0 bg-slate-900/40"
                        aria-hidden="true"
                        on:click=move |_| close_workspace()
                    ></div>
                    <div
                        role="dialog"
                        aria-modal="true"
                        aria-labelledby="workspace-scope-title"
                        class="relative flex w-full max-w-lg flex-col rounded-2xl bg-white p-5 shadow-2xl ring-1 ring-slate-200"
                        on:keydown=move |ev| {
                            if ev.key() == "Escape" {
                                close_workspace();
                            }
                        }
                    >
                        <div class="mb-4 flex items-start justify-between gap-3">
                            <h2
                                id="workspace-scope-title"
                                class="text-base font-semibold text-slate-900"
                            >
                                "Workspace"
                            </h2>
                            <button
                                type="button"
                                aria-label="Close"
                                class="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-slate-400 hover:bg-slate-100 hover:text-slate-700"
                                on:click=move |_| close_workspace()
                            >
                                <IconClose />
                            </button>
                        </div>
                        <label class="mb-1 text-xs font-medium text-slate-500" for="workspace-access-mode">
                            "Access mode"
                        </label>
                        <select
                            id="workspace-access-mode"
                            class="mb-4 w-full rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm text-slate-900 outline-none focus:border-orange-500 focus:ring-1 focus:ring-orange-500"
                            prop:value=move || workspace_access_mode.get()
                            on:change=move |ev| workspace_access_mode.set(event_target_value(&ev))
                        >
                            <option value="restricted">"Restricted"</option>
                            <option value="full">"Full"</option>
                        </select>
                        <p class="mb-1 text-xs font-medium text-slate-500">"Folder"</p>
                        <p class="mb-2 truncate rounded-lg bg-slate-50 px-3 py-2 text-xs text-slate-600" title=move || {
                            workspace_popup
                                .and_then(|popup| popup.get())
                                .and_then(|state| state.path)
                                .unwrap_or_else(|| "Loading…".to_string())
                        }>
                            {move || {
                                workspace_popup
                                    .and_then(|popup| popup.get())
                                    .and_then(|state| state.path)
                                    .unwrap_or_else(|| "Loading…".to_string())
                            }}
                        </p>
                        <div class="mb-3 max-h-[min(16rem,50vh)] overflow-y-auto rounded-lg border border-slate-200">
                            {move || {
                                let Some(state) = workspace_popup.and_then(|popup| popup.get()) else {
                                    return view! { <p class="px-3 py-2 text-sm text-slate-400">"Loading…"</p> }.into_any();
                                };
                                let parent = state.parent.clone();
                                let path_pending = state.path.is_none();
                                let entries_empty = state.entries.is_empty();
                                let up_row = parent.clone().map(|parent_path| {
                                    view! {
                                        <button
                                            type="button"
                                            class="flex w-full items-center gap-2 border-b border-slate-100 px-3 py-2 text-left text-sm font-medium text-slate-700 hover:bg-slate-50"
                                            on:click=move |_| {
                                                if let Some(on_browse_workspace) = on_browse_workspace {
                                                    on_browse_workspace.run(parent_path.clone());
                                                }
                                            }
                                        >
                                            "↑ Up"
                                        </button>
                                    }
                                });
                                let rows = state
                                    .entries
                                    .into_iter()
                                    .map(|entry| {
                                        let path = entry.path.clone();
                                        view! {
                                            <button
                                                type="button"
                                                class="flex w-full items-center gap-2 px-3 py-2 text-left text-sm text-slate-700 hover:bg-slate-50"
                                                on:click=move |_| {
                                                    if let Some(on_browse_workspace) = on_browse_workspace {
                                                        on_browse_workspace.run(path.clone());
                                                    }
                                                }
                                            >
                                                <IconFolder />
                                                {entry.name}
                                            </button>
                                        }
                                    })
                                    .collect_view();
                                view! {
                                    <>
                                        {up_row}
                                        {if path_pending {
                                            view! {
                                                <p class="px-3 py-2 text-sm text-slate-400">"Loading…"</p>
                                            }.into_any()
                                        } else if entries_empty && parent.is_none() {
                                            view! {
                                                <p class="px-3 py-2 text-sm text-slate-400">"No folders here"</p>
                                            }.into_any()
                                        } else if entries_empty {
                                            view! {
                                                <p class="px-3 py-2 text-sm text-slate-400">"No subfolders"</p>
                                            }.into_any()
                                        } else {
                                            rows.into_any()
                                        }}
                                    </>
                                }
                                .into_any()
                            }}
                        </div>
                        {move || {
                            workspace_popup
                                .and_then(|popup| popup.get())
                                .and_then(|state| state.error)
                                .map(|detail| {
                                    view! {
                                        <p class="mb-3 text-sm text-red-600">{detail}</p>
                                    }
                                })
                        }}
                        <div class="flex flex-wrap justify-end gap-2">
                            <button
                                type="button"
                                class="rounded-lg bg-slate-100 px-3 py-1.5 text-sm font-medium text-slate-700 hover:bg-slate-200"
                                on:click=move |_| {
                                    if let Some(on_default_workspace) = on_default_workspace {
                                        on_default_workspace.run(());
                                    }
                                }
                            >
                                "Use default"
                            </button>
                            <button
                                type="button"
                                class="rounded-lg bg-slate-100 px-3 py-1.5 text-sm font-medium text-slate-700 hover:bg-slate-200"
                                on:click=move |_| close_workspace()
                            >
                                "Cancel"
                            </button>
                            <button
                                type="button"
                                class="rounded-lg bg-slate-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-40"
                                disabled=move || {
                                    workspace_popup
                                        .and_then(|popup| popup.get())
                                        .and_then(|state| state.path)
                                        .is_none()
                                }
                                on:click=move |_| {
                                    let Some(path) = workspace_popup
                                        .and_then(|popup| popup.get())
                                        .and_then(|state| state.path)
                                    else {
                                        return;
                                    };
                                    if let Some(on_save_workspace) = on_save_workspace {
                                        on_save_workspace.run((path, workspace_access_mode.get()));
                                    }
                                }
                            >
                                "Save"
                            </button>
                        </div>
                    </div>
                </div>
            </Show>
        </>
    }
}

#[cfg(test)]
mod collapse_tooltip_tests {
    use super::collapse_tooltip;

    #[test]
    fn plain_tooltip_without_hint() {
        assert_eq!(collapse_tooltip(None), "Collapse");
    }

    #[test]
    fn tooltip_includes_shortcut_hint() {
        assert_eq!(collapse_tooltip(Some("Ctrl+B")), "Close sidebar Ctrl+B");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dragged_sidebar_width, estimated_menu_height, menu_placement_style, nav_button_class,
        pin_menu_label,
    };

    #[test]
    fn dragging_right_widens_and_left_narrows_the_sidebar() {
        assert_eq!(dragged_sidebar_width(256.0, 256.0, 300.0), 300.0);
        assert_eq!(dragged_sidebar_width(256.0, 256.0, 200.0), 200.0);
    }

    #[test]
    fn dragged_width_is_clamped_to_min_and_max() {
        assert_eq!(dragged_sidebar_width(256.0, 256.0, 0.0), 160.0);
        assert_eq!(dragged_sidebar_width(256.0, 256.0, 2000.0), 480.0);
    }

    #[test]
    fn menu_opens_below_when_it_fits() {
        let style = menu_placement_style(100.0, 130.0, 250.0, 800.0, 200.0);
        assert!(style.contains("top:132px"), "{style}");
        assert!(!style.contains("bottom:"), "{style}");
    }

    #[test]
    fn menu_flips_above_near_the_bottom_of_the_viewport() {
        let style = menu_placement_style(700.0, 730.0, 250.0, 760.0, 200.0);
        assert!(style.contains("bottom:62px"), "{style}");
        assert!(!style.contains("top:"), "{style}");
    }

    #[test]
    fn menu_uses_the_larger_side_and_caps_height_when_neither_fits() {
        let style = menu_placement_style(300.0, 330.0, 250.0, 600.0, 500.0);
        // 262px below vs 292px above: above wins, height capped to what is there.
        assert!(style.contains("bottom:302px"), "{style}");
        assert!(style.contains("max-height:292px"), "{style}");
    }

    #[test]
    fn menu_stays_inside_the_left_edge() {
        let style = menu_placement_style(100.0, 130.0, 50.0, 800.0, 100.0);
        assert!(style.starts_with("left:8px"), "{style}");
    }

    #[test]
    fn estimated_menu_height_counts_items_and_dividers() {
        assert_eq!(estimated_menu_height(0), 8.0);
        assert_eq!(estimated_menu_height(1), 44.0);
        assert_eq!(estimated_menu_height(3), 3.0 * 36.0 + 2.0 * 9.0 + 8.0);
    }

    #[test]
    fn pin_menu_label_reflects_current_state() {
        assert_eq!(pin_menu_label(false), "Pin session");
        assert_eq!(pin_menu_label(true), "Unpin session");
    }

    #[test]
    fn nav_button_class_highlights_only_when_active() {
        assert!(nav_button_class(true).contains("bg-slate-200"));
        assert!(!nav_button_class(false).contains("bg-slate-200"));
    }
}
