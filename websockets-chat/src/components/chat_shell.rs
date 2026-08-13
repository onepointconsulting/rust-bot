use chat_ui::components::ChatInput;
use chat_ui::models::{ChatEntry, OutgoingMessage};
use leptos::prelude::*;

use crate::components::MessageList;
use crate::state::ConnectionStatus;

/// Small dot + label in the header showing the gateway WebSocket's current
/// lifecycle state (see [`ConnectionStatus`] for what drives it).
#[component]
fn ConnectionBadge(#[prop(into)] status: Signal<ConnectionStatus>) -> impl IntoView {
    view! {
        <div class="flex items-center gap-1.5 rounded-full bg-slate-100 px-2.5 py-1 text-xs text-slate-500">
            <span class=move || format!("connection-dot {}", status.get().dot_modifier_class())></span>
            <span>{move || status.get().label()}</span>
        </div>
    }
}

#[component]
pub fn ChatShell(
    #[prop(into)] entries: Signal<Vec<ChatEntry>>,
    #[prop(into)] pending: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    #[prop(into)] example_prompts: Signal<Vec<String>>,
    #[prop(into)] connection_status: Signal<ConnectionStatus>,
    #[prop(into)] reconnect_exhausted: Signal<bool>,
    draft: RwSignal<String>,
    on_send: impl Fn(OutgoingMessage) + 'static + Copy,
    on_new_chat: impl Fn() + 'static + Send + Sync + Copy,
    on_logout: impl Fn() + 'static + Copy,
    on_minimize: impl Fn() + 'static + Copy,
    // Called from inside the `reconnect_exhausted` `<Show>` below, whose
    // `ChildrenFn` requires everything it captures to be `Send + Sync`
    // (unlike a plain `on:click` closure outside any `<Show>`, which
    // doesn't need that bound — see `on_logout`/`on_minimize` above).
    on_retry: impl Fn() + 'static + Send + Sync + Copy,
    #[prop(into)] expanded: Signal<bool>,
    on_toggle_expand: impl Fn() + 'static + Copy,
) -> impl IntoView {
    let on_use_prompt = move |prompt: String| draft.set(prompt);
    let shell_class = move || {
        if expanded.get() {
            "fixed inset-0 z-50 flex h-full w-full flex-col overflow-hidden bg-slate-50"
        } else {
            "fixed bottom-6 right-6 z-50 flex h-[min(720px,calc(100vh-3rem))] w-[min(42rem,calc(100vw-3rem))] flex-col overflow-hidden rounded-2xl bg-slate-50 shadow-2xl ring-1 ring-slate-200"
        }
    };
    view! {
        <div class=shell_class>
            <header class="flex items-center justify-between gap-2 border-b border-slate-200 bg-white px-4 py-3">
                <div>
                    <h1 class="text-base font-semibold text-slate-900">"Rust Bot"</h1>
                    <p class="text-xs text-slate-400">"Live chat"</p>
                </div>
                <div class="flex items-center gap-1">
                    <ConnectionBadge status=connection_status />
                    <button
                        type="button"
                        class="rounded-full px-3 py-1.5 text-xs font-medium text-slate-600 hover:bg-slate-100"
                        on:click=move |_| on_new_chat()
                    >
                        "New chat"
                    </button>
                    <button
                        type="button"
                        class="rounded-full px-3 py-1.5 text-xs font-medium text-slate-400 hover:bg-slate-100"
                        on:click=move |_| on_logout()
                    >
                        "Sign out"
                    </button>
                    <button
                        type="button"
                        aria-label=move || if expanded.get() { "Contract chat" } else { "Expand chat" }
                        title=move || if expanded.get() { "Contract" } else { "Expand" }
                        class="ml-1 flex h-8 w-8 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                        on:click=move |_| on_toggle_expand()
                    >
                        <Show
                            when=move || expanded.get()
                            fallback=|| {
                                view! {
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
                                        <path d="M15 3h6v6" />
                                        <path d="M9 21H3v-6" />
                                        <path d="M21 3l-7 7" />
                                        <path d="M3 21l7-7" />
                                    </svg>
                                }
                            }
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
                                <path d="M9 3v6H3" />
                                <path d="M15 21v-6h6" />
                                <path d="M3 3l7 7" />
                                <path d="M21 21l-7-7" />
                            </svg>
                        </Show>
                    </button>
                    <button
                        type="button"
                        aria-label="Minimize chat"
                        title="Minimize"
                        class="ml-1 flex h-8 w-8 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                        on:click=move |_| on_minimize()
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
                            <path d="M5 12h14" />
                        </svg>
                    </button>
                </div>
            </header>

            <Show when=move || reconnect_exhausted.get()>
                <div class="flex items-center justify-between gap-2 border-b border-amber-100 bg-amber-50 px-4 py-2 text-xs text-amber-700">
                    <span>"Connection lost. Automatic reconnect gave up."</span>
                    <button
                        type="button"
                        class="rounded-full bg-amber-600 px-3 py-1 font-medium text-white hover:bg-amber-700"
                        on:click=move |_| on_retry()
                    >
                        "Retry"
                    </button>
                </div>
            </Show>

            <Show when=move || error.get().is_some()>
                <p class="border-b border-red-100 bg-red-50 px-4 py-2 text-xs text-red-600">
                    {move || error.get().unwrap_or_default()}
                </p>
            </Show>

            <MessageList entries=entries example_prompts=example_prompts on_use_prompt=on_use_prompt />
            <ChatInput pending=pending draft=draft on_send=on_send />
        </div>
    }
}
