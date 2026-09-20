//! Collapsible panel showing a turn's accumulated reasoning/thinking text.
//!
//! Text is a live `Signal` so streamed `reasoning_delta` chunks update the
//! open panel in place. Expand/collapse is stored on a parent `HashSet` of
//! entry ids rather than a local `RwSignal` — `message_list.rs` still
//! remounts this component when *other* entry fields change (`content`,
//! `tool_events`, `streaming`, …), and a local flag would reset to collapsed
//! on each remount.

use std::collections::HashSet;

use leptos::prelude::*;

/// Collapsible "Show reasoning" / "Hide reasoning" panel, closed by default.
#[component]
pub fn ReasoningPanel(
    entry_id: u64,
    #[prop(into)] text: Signal<String>,
    expanded_ids: RwSignal<HashSet<u64>>,
) -> impl IntoView {
    let is_expanded = move || expanded_ids.get().contains(&entry_id);
    let toggle = move |_| {
        expanded_ids.update(|ids| {
            if !ids.remove(&entry_id) {
                ids.insert(entry_id);
            }
        });
    };

    view! {
        <div class="reasoning-panel">
            <button
                type="button"
                class="reasoning-panel__toggle w-full text-left"
                aria-expanded=move || is_expanded().to_string()
                on:click=toggle
            >
            "💭 "
            {move || if is_expanded() { "Hide reasoning" } else { "Show reasoning" }}
            </button>
            <Show when=is_expanded>
                <p class="reasoning-panel__text">{move || text.get()}</p>
            </Show>
        </div>
    }
}
