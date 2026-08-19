//! The chat transcript: auto-scrolling list of message bubbles plus the
//! empty-state suggestion prompts.
//!
//! Unlike `web-chat`'s otherwise-identical component, entries here can
//! mutate in place after they're first pushed (delta text growing in,
//! `streaming` flipping to `false`, tool/reasoning panels attaching) — see
//! `entry_render_key`'s doc comment for why that requires a different
//! `<For>` keying strategy than a plain `entry.id`.

use chat_ui::components::MessageBubble;
use chat_ui::models::ChatEntry;
use leptos::html::Div;
use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::components::{ReasoningPanel, ToolActivity};

fn scroll_list_to_bottom(list_ref: NodeRef<Div>) {
    // Wait one frame so newly rendered/updated bubbles are in the layout.
    if let Some(window) = web_sys::window() {
        let cb = Closure::once(move || {
            if let Some(el) = list_ref.get() {
                el.set_scroll_top(el.scroll_height());
            }
        });
        let _ = window.request_animation_frame(cb.as_ref().unchecked_ref());
        cb.forget();
    }
}

/// `<For>`'s diffing (`tachys::view::keyed`) only adds/removes/reorders by
/// key — for a key that persists across two snapshots of the source list, it
/// reuses the previously built view **unchanged**, it does not re-invoke the
/// per-item render function with the new data. `web-chat` never notices this
/// because its entries are write-once (pushed once, never mutated), so
/// keying by `entry.id` alone is correct there.
///
/// This app's entries mutate in place as gateway events arrive (delta text
/// appended, `streaming` flipped off, tool/reasoning panels attached), and
/// `chat_ui::components::MessageBubble` takes its `entry`/`extra` content as
/// plain (non-reactive) values captured once at render time — so the only
/// way to make those in-place mutations actually show up is to make the
/// `<For>` key itself change whenever anything user-visibly relevant about
/// the entry changes, forcing that entry's bubble to be torn down and
/// rebuilt with the current data. `content`/`tool_events`/`reasoning` are
/// folded through `{:?}` into one string component rather than requiring
/// `Hash` on `chat_ui::models::ToolEvent` (which doesn't derive it).
fn entry_render_key(entry: &ChatEntry) -> (u64, String, bool, String) {
    (
        entry.id,
        entry.content.clone(),
        entry.streaming,
        format!("{:?}|{:?}", entry.tool_events, entry.reasoning),
    )
}

/// One transcript entry, with a `ToolActivity`/`ReasoningPanel` slot injected
/// under the bubble when the entry carries live tool events and/or
/// accumulated reasoning text.
///
/// `chat_ui::components::MessageBubble`'s `extra` prop is `#[prop(optional)]
/// Option<Children>`, but its Leptos-generated builder setter expects a bare
/// `Children` value, not an `Option<Children>` — passing `extra=None`
/// doesn't compile. Callers that want no extra content must omit the prop
/// entirely, hence the two separate `view!` branches below rather than one
/// with a conditional prop value.
#[component]
fn ChatEntryBubble(entry: ChatEntry, #[prop(into)] token_streaming: Signal<bool>) -> impl IntoView {
    let streaming = entry.streaming;
    let tool_events = entry.tool_events.clone().unwrap_or_default();
    let reasoning = entry.reasoning.clone().unwrap_or_default();
    let has_extra = !tool_events.is_empty() || !reasoning.is_empty();

    if has_extra {
        let has_tool_events = !tool_events.is_empty();
        let has_reasoning = !reasoning.is_empty();
        // `MessageBubble`'s `extra` field is `Option<Children>`, i.e.
        // `Option<Box<dyn FnOnce() -> AnyView + Send>>` — a bare closure
        // doesn't coerce into that automatically, so it must be boxed (and
        // its body's return type collapsed to `AnyView` via `.into_any()`)
        // explicitly.
        let extra: Children = Box::new(move || {
            let tool_view = if has_tool_events {
                view! { <ToolActivity events=tool_events.clone() /> }.into_any()
            } else {
                ().into_any()
            };
            let reasoning_view = if has_reasoning {
                view! { <ReasoningPanel text=reasoning.clone() /> }.into_any()
            } else {
                ().into_any()
            };
            view! {
                <div class="flex flex-col gap-2">
                    {tool_view}
                    {reasoning_view}
                </div>
            }
            .into_any()
        });
        view! {
            <MessageBubble
                entry=entry
                streaming=Signal::derive(move || streaming)
                token_streaming=token_streaming
                extra=extra
            />
        }
        .into_any()
    } else {
        view! {
            <MessageBubble
                entry=entry
                streaming=Signal::derive(move || streaming)
                token_streaming=token_streaming
            />
        }
        .into_any()
    }
}

#[component]
pub fn MessageList(
    #[prop(into)] entries: Signal<Vec<ChatEntry>>,
    #[prop(into)] example_prompts: Signal<Vec<String>>,
    #[prop(into)] token_streaming: Signal<bool>,
    on_use_prompt: impl Fn(String) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    let list_ref = NodeRef::<Div>::new();

    // Keep the latest message in view. Tracks total content length (not
    // just entry count) so the list also re-scrolls as streamed text grows
    // an existing bubble, not only when a new entry is pushed.
    Effect::new(move |_| {
        let total_content_len: usize = entries.get().iter().map(|entry| entry.content.len()).sum();
        let _ = total_content_len;
        scroll_list_to_bottom(list_ref);
    });

    // No separate "pending" flag needed: the streaming placeholder entry
    // itself (spinner until the first token, then a blinking cursor) is the
    // pending indicator, so suggestions only need to check for an empty
    // transcript.
    let show_suggestions = move || entries.get().is_empty() && !example_prompts.get().is_empty();

    view! {
        <div node_ref=list_ref class="flex flex-1 flex-col gap-3 overflow-y-auto px-4 py-4">
            <Show when=show_suggestions>
                <SuggestionPrompts prompts=example_prompts on_use_prompt=on_use_prompt />
            </Show>
            <For each=move || entries.get() key=entry_render_key let(entry)>
                <ChatEntryBubble entry=entry token_streaming=token_streaming />
            </For>
        </div>
    }
}

/// Clickable example-prompt bubbles shown in an otherwise-empty chat pane.
/// Clicking a prompt fills the composer draft (via `on_use_prompt`) without
/// sending it, letting the user tweak it before submitting.
#[component]
fn SuggestionPrompts(
    #[prop(into)] prompts: Signal<Vec<String>>,
    on_use_prompt: impl Fn(String) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    view! {
        <div class="flex flex-col items-start gap-2">
            <p class="px-1 text-xs font-medium text-slate-400">"Try one of these:"</p>
            <For each=move || prompts.get() key=|prompt| prompt.clone() let(prompt)>
                <button
                    type="button"
                    class="max-w-[85%] rounded-2xl border border-dashed border-slate-300 bg-white px-4 py-2 text-left text-sm text-slate-600 shadow-sm transition hover:border-orange-400 hover:bg-orange-50 hover:text-orange-700"
                    on:click=move |_| on_use_prompt(prompt.clone())
                >
                    {prompt.clone()}
                </button>
            </For>
        </div>
    }
}
