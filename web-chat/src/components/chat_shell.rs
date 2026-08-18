use chat_ui::components::{ChatHeaderActions, ChatInput};
use chat_ui::models::{ChatEntry, OutgoingMessage};
use leptos::prelude::*;

use crate::components::MessageList;

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
            <header class="relative z-10 flex items-center justify-between gap-2 border-b border-slate-200 bg-white px-4 py-3">
                <div class="min-w-0">
                    <h1 class="text-base font-semibold text-slate-900">"Rust Bot"</h1>
                    <p class="text-xs text-slate-400">"Ask AI"</p>
                </div>
                <ChatHeaderActions
                    expanded=expanded
                    on_new_chat=on_new_chat
                    on_logout=on_logout
                    on_minimize=on_minimize
                    on_toggle_expand=on_toggle_expand
                />
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
