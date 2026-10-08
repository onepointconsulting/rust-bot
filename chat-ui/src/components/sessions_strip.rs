//! Narrow icon rail shown on the left edge while the sessions pane is
//! collapsed (DeepSeek-style). Owns the "open sidebar" and "new chat" icon
//! buttons; the parent decides when to mount it (strip ⇔ pane collapsed).

use leptos::prelude::*;

use super::sessions_sidebar::{IconChatPlus, IconSidebarPanel};

const STRIP_BUTTON: &str = "flex h-9 w-9 items-center justify-center rounded-lg text-slate-500 hover:bg-slate-100 hover:text-slate-700 active:bg-slate-200";

/// Tooltip for the open button: `"Open sidebar"`, or `"Open sidebar <hint>"`
/// when the caller supplies a keyboard-shortcut hint.
fn open_tooltip(hint: Option<&str>) -> String {
    match hint {
        Some(hint) => format!("Open sidebar {hint}"),
        None => "Open sidebar".to_string(),
    }
}

/// Vertical icon rail: [open sidebar] [new chat] plus optional extra
/// `children` (future plugin/workspace/search icons).
///
/// `compact` narrows the rail for the small floating-widget window.
#[component]
pub fn SessionsStrip(
    on_open: impl Fn() + 'static + Send + Sync + Copy,
    /// Hidden when `None`.
    #[prop(optional)]
    on_new_chat: Option<Callback<()>>,
    /// Keyboard-shortcut hint appended to the open button's tooltip.
    #[prop(optional)]
    open_hint: Option<String>,
    #[prop(into, default = Signal::stored(false))] compact: Signal<bool>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let open_title = open_tooltip(open_hint.as_deref());
    let strip_class = move || {
        format!(
            "flex h-full shrink-0 flex-col items-center gap-1 border-r border-slate-200 bg-white py-3 {}",
            if compact.get() { "w-10" } else { "w-12" }
        )
    };

    view! {
        <nav class=strip_class aria-label="Chats">
            <button
                type="button"
                class=STRIP_BUTTON
                aria-label="Open sidebar"
                title=open_title
                on:click=move |_| on_open()
            >
                <IconSidebarPanel />
            </button>
            {on_new_chat.map(|on_new_chat| {
                view! {
                    <button
                        type="button"
                        class=STRIP_BUTTON
                        aria-label="New chat"
                        title="New chat"
                        on:click=move |_| on_new_chat.run(())
                    >
                        <IconChatPlus />
                    </button>
                }
            })}
            {children.map(|children| children())}
        </nav>
    }
}

#[cfg(test)]
mod tests {
    use super::open_tooltip;

    #[test]
    fn open_tooltip_without_hint_is_plain() {
        assert_eq!(open_tooltip(None), "Open sidebar");
    }

    #[test]
    fn open_tooltip_appends_hint() {
        assert_eq!(open_tooltip(Some("Ctrl+B")), "Open sidebar Ctrl+B");
    }
}
