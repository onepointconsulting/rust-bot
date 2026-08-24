use chat_ui::components::{ChatHeaderActions, ChatInput, SessionsSidebar, SessionsSidebarToggle};
use chat_ui::models::{ChatEntry, OutgoingMessage, SessionListItem};
use leptos::prelude::*;

use crate::components::MessageList;
use crate::state::ConnectionStatus;

/// Small dot + label in the header showing the gateway WebSocket's current
/// lifecycle state (see [`ConnectionStatus`] for what drives it).
#[component]
fn ConnectionBadge(#[prop(into)] status: Signal<ConnectionStatus>) -> impl IntoView {
    view! {
        <div class="flex shrink-0 items-center gap-1.5 rounded-full bg-slate-100 px-2.5 py-1 text-xs text-slate-500">
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
    #[prop(into)] token_streaming: Signal<bool>,
    #[prop(into)] sessions: Signal<Vec<SessionListItem>>,
    #[prop(into)] active_session_id: Signal<Option<String>>,
    #[prop(into)] sidebar_open: Signal<bool>,
    #[prop(into)] user_email: Signal<Option<String>>,
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
    on_toggle_sidebar: impl Fn() + 'static + Send + Sync + Copy,
    on_close_sidebar: impl Fn() + 'static + Send + Sync + Copy,
    on_select_session: impl Fn(String) + 'static + Send + Sync + Copy,
    on_rename_session: impl Fn(String, String) + 'static + Send + Sync + Copy,
    on_fork_session: impl Fn(String) + 'static + Send + Sync + Copy,
    on_delete_session: impl Fn(String) + 'static + Send + Sync + Copy,
    on_abort_turn: impl Fn() + 'static + Send + Sync + Copy,
    #[prop(into)] model_presets: Signal<Vec<String>>,
    #[prop(into)] selected_model_preset: Signal<String>,
    on_select_model_preset: impl Fn(String) + 'static + Send + Sync + Copy,
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
                on_rename=Callback::new(move |(id, title)| on_rename_session(id, title))
                on_fork=Callback::new(move |id| on_fork_session(id))
                on_delete=Callback::new(move |id| on_delete_session(id))
            />
            <div class="flex min-w-0 flex-1 flex-col overflow-hidden">
                <header class="relative z-10 flex items-center justify-between gap-2 border-b border-slate-200 bg-white px-4 py-3">
                    <div class="flex min-w-0 items-center gap-2">
                        <SessionsSidebarToggle open=sidebar_open on_toggle=on_toggle_sidebar />
                        <div class="min-w-0">
                            <h1 class="text-base font-semibold text-slate-900">"Rust Bot"</h1>
                            <p class="text-xs text-slate-400">"Live chat"</p>
                        </div>
                    </div>
                    <div class="flex min-w-0 items-center gap-1">
                        <ConnectionBadge status=connection_status />
                        <ChatHeaderActions
                            expanded=expanded
                            on_new_chat=on_new_chat
                            on_logout=on_logout
                            on_minimize=on_minimize
                            on_toggle_expand=on_toggle_expand
                        />
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

                <MessageList
                    entries=entries
                    example_prompts=example_prompts
                    token_streaming=token_streaming
                    on_use_prompt=on_use_prompt
                />
                <ChatInput
                    pending=pending
                    draft=draft
                    on_send=on_send
                    on_abort=Callback::new(move |()| on_abort_turn())
                    model_presets=model_presets
                    selected_model_preset=selected_model_preset
                    on_select_model_preset=Callback::new(move |name| on_select_model_preset(name))
                />
            </div>
        </div>
    }
}
