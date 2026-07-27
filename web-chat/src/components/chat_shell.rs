use leptos::prelude::*;

use crate::components::{ChatInput, MessageList};
use crate::models::ChatEntry;

#[component]
pub fn ChatShell(
    #[prop(into)] entries: Signal<Vec<ChatEntry>>,
    #[prop(into)] pending: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    on_send: impl Fn(String) + 'static + Copy,
    on_new_chat: impl Fn() + 'static + Copy,
    on_logout: impl Fn() + 'static + Copy,
) -> impl IntoView {
    view! {
        <div class="mx-auto flex h-screen max-w-2xl flex-col bg-slate-100 md:py-6">
            <div class="flex h-full flex-col overflow-hidden bg-slate-50 shadow-sm md:rounded-2xl">
                <header class="flex items-center justify-between border-b border-slate-200 bg-white px-4 py-3">
                    <div>
                        <h1 class="text-base font-semibold text-slate-900">"Rust Bot"</h1>
                        <p class="text-xs text-slate-400">"Ask AI"</p>
                    </div>
                    <div class="flex items-center gap-2">
                        <button
                            class="rounded-full px-3 py-1.5 text-xs font-medium text-slate-600 hover:bg-slate-100"
                            on:click=move |_| on_new_chat()
                        >
                            "New chat"
                        </button>
                        <button
                            class="rounded-full px-3 py-1.5 text-xs font-medium text-slate-400 hover:bg-slate-100"
                            on:click=move |_| on_logout()
                        >
                            "Sign out"
                        </button>
                    </div>
                </header>

                <Show when=move || error.get().is_some()>
                    <p class="border-b border-red-100 bg-red-50 px-4 py-2 text-xs text-red-600">
                        {move || error.get().unwrap_or_default()}
                    </p>
                </Show>

                <MessageList entries=entries pending=pending />
                <ChatInput pending=pending on_send=on_send />
            </div>
        </div>
    }
}
