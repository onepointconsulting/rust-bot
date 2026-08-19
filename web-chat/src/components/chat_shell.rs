use chat_ui::components::{ChatHeaderActions, ChatInput, SessionsSidebar, SessionsSidebarToggle};
use chat_ui::models::{ChatEntry, OutgoingMessage, SessionListItem};
use leptos::prelude::*;

use crate::components::MessageList;

#[component]
pub fn ChatShell(
    #[prop(into)] entries: Signal<Vec<ChatEntry>>,
    #[prop(into)] pending: Signal<bool>,
    #[prop(into)] error: Signal<Option<String>>,
    #[prop(into)] example_prompts: Signal<Vec<String>>,
    #[prop(into)] sessions: Signal<Vec<SessionListItem>>,
    #[prop(into)] active_session_id: Signal<Option<String>>,
    #[prop(into)] sidebar_open: Signal<bool>,
    #[prop(into)] user_email: Signal<Option<String>>,
    draft: RwSignal<String>,
    on_send: impl Fn(OutgoingMessage) + 'static + Copy,
    on_new_chat: impl Fn() + 'static + Send + Sync + Copy,
    on_logout: impl Fn() + 'static + Copy,
    on_minimize: impl Fn() + 'static + Copy,
    #[prop(into)] expanded: Signal<bool>,
    on_toggle_expand: impl Fn() + 'static + Copy,
    on_toggle_sidebar: impl Fn() + 'static + Send + Sync + Copy,
    on_close_sidebar: impl Fn() + 'static + Send + Sync + Copy,
    on_select_session: impl Fn(String) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    let on_use_prompt = move |prompt: String| draft.set(prompt);
    let shell_class = move || {
        if expanded.get() {
            "fixed inset-0 z-50 flex h-full w-full flex-row overflow-hidden bg-slate-50"
        } else {
            "fixed bottom-6 right-6 z-50 flex h-[min(720px,calc(100vh-3rem))] w-[min(42rem,calc(100vw-3rem))] flex-row overflow-hidden rounded-2xl bg-slate-50 shadow-2xl ring-1 ring-slate-200"
        }
    };
    view! {
        <div class=shell_class>
            <SessionsSidebar
                sessions=sessions
                active_id=active_session_id
                open=sidebar_open
                user_email=user_email
                on_close=on_close_sidebar
                on_select=on_select_session
            />
            <div class="flex min-w-0 flex-1 flex-col overflow-hidden">
                <header class="relative z-10 flex items-center justify-between gap-2 border-b border-slate-200 bg-white px-4 py-3">
                    <div class="flex min-w-0 items-center gap-2">
                        <SessionsSidebarToggle open=sidebar_open on_toggle=on_toggle_sidebar />
                        <div class="min-w-0">
                            <h1 class="text-base font-semibold text-slate-900">"Rust Bot"</h1>
                            <p class="text-xs text-slate-400">"Ask AI"</p>
                        </div>
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
        </div>
    }
}
