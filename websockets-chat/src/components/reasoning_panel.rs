//! Collapsible panel showing a turn's accumulated reasoning/thinking text.
//!
//! Fed a plain `String` snapshot rather than a `Signal` — see the note on
//! `ToolActivity` in `tool_activity.rs`: `message_list.rs` already
//! recreates this component whenever the entry's `reasoning` text changes,
//! so the accumulated text is always up to date without needing its own
//! internal reactivity.

use leptos::prelude::*;

/// Collapsible "Show reasoning" / "Hide reasoning" panel, closed by default.
#[component]
pub fn ReasoningPanel(text: String) -> impl IntoView {
    let expanded = RwSignal::new(false);

    view! {
        <div class="reasoning-panel">
            <button
                type="button"
                class="reasoning-panel__toggle"
                aria-expanded=move || expanded.get().to_string()
                on:click=move |_| expanded.update(|value| *value = !*value)
            >
            "💭 "
            {move || if expanded.get() { "Hide reasoning" } else { "Show reasoning" }}
            </button>
            <Show when=move || expanded.get()>
                <p class="reasoning-panel__text">{text.clone()}</p>
            </Show>
        </div>
    }
}
