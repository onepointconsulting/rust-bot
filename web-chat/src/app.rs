use gloo_storage::{SessionStorage, Storage};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::{ChatShell, LoginForm};
use crate::models::{ChatEntry, Role};

const TOKEN_STORAGE_KEY: &str = "rust-bot-web-chat-token";

fn read_stored_token() -> Option<String> {
    SessionStorage::get::<String>(TOKEN_STORAGE_KEY).ok()
}

fn next_session_id() -> String {
    format!("web-{}", js_sys::Date::now() as u64)
}

#[component]
pub fn App() -> impl IntoView {
    let token = RwSignal::new(read_stored_token());
    let login_error = RwSignal::new(None::<String>);
    let login_pending = RwSignal::new(false);

    let session_id = RwSignal::new(next_session_id());
    let entries = RwSignal::new(Vec::<ChatEntry>::new());
    let chat_pending = RwSignal::new(false);
    let chat_error = RwSignal::new(None::<String>);
    let next_id = RwSignal::new(0u64);

    let push_entry = move |role: Role, content: String| {
        let id = next_id.get();
        next_id.set(id + 1);
        entries.update(|list| list.push(ChatEntry { id, role, content }));
    };

    let do_login = move |email: String, password: String| {
        login_error.set(None);
        login_pending.set(true);
        spawn_local(async move {
            match api::login(&email, &password).await {
                Ok(jwt) => {
                    let _ = SessionStorage::set(TOKEN_STORAGE_KEY, &jwt);
                    token.set(Some(jwt));
                }
                Err(err) => login_error.set(Some(err.to_string())),
            }
            login_pending.set(false);
        });
    };

    let do_logout = move || {
        SessionStorage::delete(TOKEN_STORAGE_KEY);
        token.set(None);
        entries.set(Vec::new());
    };

    let do_send = move |text: String| {
        let Some(jwt) = token.get() else { return };
        push_entry(Role::User, text.clone());
        chat_error.set(None);
        chat_pending.set(true);
        let session = session_id.get();
        spawn_local(async move {
            match api::send_chat_message(&jwt, &session, &text).await {
                Ok(reply) => push_entry(Role::Assistant, reply),
                Err(err) => chat_error.set(Some(err.to_string())),
            }
            chat_pending.set(false);
        });
    };

    let do_new_chat = move || {
        let Some(jwt) = token.get() else { return };
        entries.set(Vec::new());
        chat_error.set(None);
        let session = session_id.get();
        spawn_local(async move {
            if let Err(err) = api::start_new_session(&jwt, &session).await {
                chat_error.set(Some(err.to_string()));
            }
        });
    };

    view! {
        <Show
            when=move || token.get().is_some()
            fallback=move || {
                view! {
                    <LoginForm
                        error=Signal::derive(move || login_error.get())
                        pending=Signal::derive(move || login_pending.get())
                        on_submit=do_login
                    />
                }
            }
        >
            <ChatShell
                entries=Signal::derive(move || entries.get())
                pending=Signal::derive(move || chat_pending.get())
                error=Signal::derive(move || chat_error.get())
                on_send=do_send
                on_new_chat=do_new_chat
                on_logout=do_logout
            />
        </Show>
    }
}
