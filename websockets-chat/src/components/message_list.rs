//! The chat transcript: pin-to-bottom list of message bubbles plus the
//! empty-state suggestion prompts. While a turn is in flight, auto-scroll
//! follows streamed updates if the user is near the bottom, pauses when
//! they scroll up, and resumes when they return to the bottom or send.
//! Idle conversations stay unpinned; a session switch (or first paint)
//! still jumps once to the latest message.
//!
//! Unlike `web-chat`'s otherwise-identical component, entries here can
//! mutate in place after they're first pushed (delta text growing in,
//! `streaming` flipping to `false`, tool/reasoning panels attaching) — see
//! `entry_render_key`'s doc comment for why that requires a different
//! `<For>` keying strategy than a plain `entry.id`.

use chat_ui::components::MessageBubble;
use chat_ui::models::{ChatEntry, Role};
use leptos::html::Div;
use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use crate::components::{ReasoningPanel, ToolActivity};
use crate::state::show_fork_for_entry;

/// How close to the bottom (CSS pixels) still counts as pinned. Absorbs
/// trackpad jitter and the one-frame gap after streamed content grows the
/// list, before the follow-up scroll lands.
const PIN_TO_BOTTOM_THRESHOLD_PX: i32 = 80;

fn is_near_bottom(el: &web_sys::HtmlElement) -> bool {
    el.scroll_height() - el.scroll_top() - el.client_height() <= PIN_TO_BOTTOM_THRESHOLD_PX
}

fn last_user_entry_id(entries: &[ChatEntry]) -> Option<u64> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.role == Role::User)
        .map(|entry| entry.id)
}

fn cancel_pending_auto_scroll(auto_scroll_generation: RwSignal<u64>) {
    auto_scroll_generation.update(|generation| {
        *generation = generation.wrapping_add(1);
    });
}

/// Scroll the transcript to the latest content on the next frame, unless a
/// newer auto-scroll (or a user scroll-away) superseded this one.
fn scroll_list_to_bottom(list_ref: NodeRef<Div>, auto_scroll_generation: RwSignal<u64>) {
    let generation = auto_scroll_generation.get_untracked().wrapping_add(1);
    auto_scroll_generation.set(generation);
    if let Some(window) = web_sys::window() {
        let cb = Closure::once(move || {
            if auto_scroll_generation.get_untracked() != generation {
                return;
            }
            if let Some(el) = list_ref.get_untracked() {
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
fn entry_render_key(entry: &ChatEntry, show_fork: bool) -> (u64, String, bool, String, bool) {
    (
        entry.id,
        entry.content.clone(),
        entry.streaming,
        format!("{:?}|{:?}", entry.tool_events, entry.reasoning),
        show_fork,
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
fn ChatEntryBubble(
    entry: ChatEntry,
    #[prop(into)] token_streaming: Signal<bool>,
    show_fork: bool,
    on_fork_reply: impl Fn(u64) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    let streaming = entry.streaming;
    let entry_id = entry.id;
    // Same optional-prop constraint as `extra` below: the generated setter
    // takes a bare `Callback<()>`, not `Option<Callback<()>>`, so the Fork
    // control is either passed or the prop is omitted entirely.
    let on_fork = show_fork.then(|| Callback::new(move |_| on_fork_reply(entry_id)));
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
        if let Some(on_fork) = on_fork {
            view! {
                <MessageBubble
                    entry=entry
                    streaming=Signal::derive(move || streaming)
                    token_streaming=token_streaming
                    extra=extra
                    on_fork=on_fork
                />
            }
            .into_any()
        } else {
            view! {
                <MessageBubble
                    entry=entry
                    streaming=Signal::derive(move || streaming)
                    token_streaming=token_streaming
                    extra=extra
                />
            }
            .into_any()
        }
    } else if let Some(on_fork) = on_fork {
        view! {
            <MessageBubble
                entry=entry
                streaming=Signal::derive(move || streaming)
                token_streaming=token_streaming
                on_fork=on_fork
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
    #[prop(into)] pending: Signal<bool>,
    #[prop(into)] active_session_id: Signal<Option<String>>,
    on_use_prompt: impl Fn(String) + 'static + Send + Sync + Copy,
    on_fork_reply: impl Fn(u64) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    let list_ref = NodeRef::<Div>::new();
    let pinned_to_bottom = RwSignal::new(false);
    let auto_scroll_generation = RwSignal::new(0u64);
    // `(session, last user entry)` — a new send re-pins while a turn is in
    // flight; a session switch (or first paint) one-shot jumps to latest
    // even when idle. Streaming mutates the same last user id, so it does
    // not change this identity.
    let pin_identity = RwSignal::new(None::<(Option<String>, Option<u64>)>);

    // Follow the latest message while a turn is in flight and pinned.
    // Tracks content / extra-panel size (not just entry count) so streamed
    // tokens and tool/reasoning growth also re-scroll. Idle conversations
    // unpin; session load / first paint, and a pinned turn ending, still
    // jump once to the latest message.
    Effect::new(move |_| {
        let current = entries.get();
        let identity = (active_session_id.get(), last_user_entry_id(&current));
        let identity_changed = pin_identity.get_untracked().as_ref() != Some(&identity);
        if identity_changed {
            pin_identity.set(Some(identity));
        }

        let in_flight = pending.get();
        let was_pinned = pinned_to_bottom.get_untracked();

        let total_content_len: usize = current.iter().map(|entry| entry.content.len()).sum();
        let extra_growth: usize = current
            .iter()
            .map(|entry| {
                entry.reasoning.as_ref().map(String::len).unwrap_or(0)
                    + entry.tool_events.as_ref().map(Vec::len).unwrap_or(0)
            })
            .sum();
        let _ = (total_content_len, extra_growth);

        if in_flight {
            if identity_changed {
                pinned_to_bottom.set(true);
            }
            if pinned_to_bottom.get() {
                scroll_list_to_bottom(list_ref, auto_scroll_generation);
            }
        } else {
            // Snap once on session load / first paint, or to land the final
            // layout when a pinned turn ends. Do not bump generation here:
            // unpinning can retrigger this effect, and a bump would cancel
            // the snap just scheduled. Scroll-away during a turn already
            // cancelled any leftover rAF.
            let should_snap = was_pinned || identity_changed;
            if was_pinned {
                pinned_to_bottom.set(false);
            }
            if should_snap {
                scroll_list_to_bottom(list_ref, auto_scroll_generation);
            }
        }
    });

    let on_list_scroll = move |_| {
        if !pending.get_untracked() {
            return;
        }
        let Some(el) = list_ref.get_untracked() else {
            return;
        };
        if is_near_bottom(&el) {
            pinned_to_bottom.set(true);
        } else {
            pinned_to_bottom.set(false);
            cancel_pending_auto_scroll(auto_scroll_generation);
        }
    };

    // No separate "pending" flag needed: the streaming placeholder entry
    // itself (spinner until the first token, then a blinking cursor) is the
    // pending indicator, so suggestions only need to check for an empty
    // transcript.
    let show_suggestions = move || entries.get().is_empty() && !example_prompts.get().is_empty();

    view! {
        <div
            node_ref=list_ref
            class="flex flex-1 flex-col gap-3 overflow-y-auto px-4 py-4"
            on:scroll=on_list_scroll
        >
            <Show when=show_suggestions>
                <SuggestionPrompts prompts=example_prompts on_use_prompt=on_use_prompt />
            </Show>
            <For
                each=move || {
                    let pending = pending.get();
                    entries
                        .get()
                        .into_iter()
                        .map(|entry| {
                            let show_fork = show_fork_for_entry(&entry, pending);
                            (entry, show_fork)
                        })
                        .collect::<Vec<_>>()
                }
                key=|(entry, show_fork)| entry_render_key(entry, *show_fork)
                let((entry, show_fork))
            >
                <ChatEntryBubble
                    entry=entry
                    token_streaming=token_streaming
                    show_fork=show_fork
                    on_fork_reply=on_fork_reply
                />
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
