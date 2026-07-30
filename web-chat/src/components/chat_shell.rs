use leptos::prelude::*;

use crate::components::{ChatInput, MessageList};
use crate::models::{ChatEntry, OutgoingMessage};

#[component]
pub fn ChatShell(
    #[prop(into)] entries: Signal<Vec<ChatEntry>>,
    #[prop(into)] pending: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    #[prop(into)] example_prompts: Signal<Vec<String>>,
    draft: RwSignal<String>,
    on_send: impl Fn(OutgoingMessage) + 'static + Copy,
    on_new_chat: impl Fn() + 'static + Copy,
    on_logout: impl Fn() + 'static + Copy,
    on_minimize: impl Fn() + 'static + Copy,
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
            <header class="flex items-center justify-between border-b border-slate-200 bg-white px-4 py-3">
                <div>
                    <h1 class="text-base font-semibold text-slate-900">"Rust Bot"</h1>
                    <p class="text-xs text-slate-400">"Ask AI"</p>
                </div>
                <div class="flex items-center gap-1">
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

            <Show when=move || error.get().is_some()>
                <p class="border-b border-red-100 bg-red-50 px-4 py-2 text-xs text-red-600">
                    {move || error.get().unwrap_or_default()}
                </p>
            </Show>

            <MessageList
                entries=entries
                pending=pending
                example_prompts=example_prompts
                on_use_prompt=on_use_prompt
            />
            <ChatInput pending=pending draft=draft on_send=on_send />
        </div>
    }
}
