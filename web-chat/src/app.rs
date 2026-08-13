use gloo_storage::{SessionStorage, Storage};
use leptos::prelude::*;
use leptos::task::spawn_local;

use chat_ui::api::login;
use chat_ui::components::LoginForm;
use chat_ui::models::{ChatEntry, ImageAttachment, OutgoingMessage, Role};

use crate::api;
use crate::components::ChatShell;

const TOKEN_STORAGE_KEY: &str = "rust-bot-web-chat-token";
const SESSION_STORAGE_KEY: &str = "rust-bot-web-chat-session";
const EMAIL_STORAGE_KEY: &str = "rust-bot-web-chat-email";
const ENTRIES_STORAGE_KEY: &str = "rust-bot-web-chat-entries";

/// Max user/assistant exchanges kept in SessionStorage across refresh.
const MAX_STORED_TURNS: usize = 10;

fn read_stored_token() -> Option<String> {
    SessionStorage::get::<String>(TOKEN_STORAGE_KEY).ok()
}

fn read_stored_session() -> Option<String> {
    SessionStorage::get::<String>(SESSION_STORAGE_KEY).ok()
}

fn read_stored_email() -> Option<String> {
    SessionStorage::get::<String>(EMAIL_STORAGE_KEY).ok()
}

fn read_stored_entries() -> Vec<ChatEntry> {
    SessionStorage::get::<Vec<ChatEntry>>(ENTRIES_STORAGE_KEY).unwrap_or_default()
}

/// `data:` attachments are re-encoded, in-memory image bytes; they are far
/// too large (and pointless) to round-trip through SessionStorage, so only
/// `http(s)://` attachments survive a page refresh.
fn strip_data_url_attachments(entries: &[ChatEntry]) -> Vec<ChatEntry> {
    entries
        .iter()
        .map(|entry| {
            let mut entry = entry.clone();
            entry
                .attachments
                .retain(|attachment| !attachment.url.starts_with("data:"));
            entry
        })
        .collect()
}

fn persist_entries(entries: &[ChatEntry]) {
    let sanitized = strip_data_url_attachments(entries);
    let _ = SessionStorage::set(ENTRIES_STORAGE_KEY, &sanitized);
}

fn clear_stored_entries() {
    SessionStorage::delete(ENTRIES_STORAGE_KEY);
}

fn next_entry_id(entries: &[ChatEntry]) -> u64 {
    entries.iter().map(|e| e.id).max().map(|id| id + 1).unwrap_or(0)
}

/// Keep the last `max_turns` user messages and everything after the first kept user message.
fn trim_to_max_turns(entries: Vec<ChatEntry>, max_turns: usize) -> Vec<ChatEntry> {
    let user_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.role == Role::User)
        .map(|(index, _)| index)
        .collect();

    if user_indices.len() <= max_turns {
        return entries;
    }

    let start = user_indices[user_indices.len() - max_turns];
    entries[start..].to_vec()
}

fn session_id_for(email: &str) -> String {
    format!(
        "web-{}-{}",
        email.replace('@', "_at_"),
        js_sys::Date::now() as u64
    )
}

fn persist_session(session: &str) {
    let _ = SessionStorage::set(SESSION_STORAGE_KEY, session);
}

fn persist_email(email: &str) {
    let _ = SessionStorage::set(EMAIL_STORAGE_KEY, email);
}

#[component]
fn CrabLauncher(on_open: impl Fn() + 'static + Copy) -> impl IntoView {
    view! {
        <button
            type="button"
            aria-label="Open chat"
            class="fixed bottom-6 right-6 z-50 flex h-16 w-16 items-center justify-center rounded-full bg-slate-900 text-3xl shadow-lg transition hover:scale-105 hover:bg-slate-800 focus:outline-none focus:ring-2 focus:ring-slate-400 focus:ring-offset-2"
            on:click=move |_| on_open()
        >
            "🦀"
        </button>
    }
}

#[component]
pub fn App() -> impl IntoView {
    let token = RwSignal::new(read_stored_token());
    let login_error = RwSignal::new(None::<String>);
    let login_pending = RwSignal::new(false);
    let chat_open = RwSignal::new(false);
    let expanded = RwSignal::new(false);

    let session_id = RwSignal::new(
        read_stored_session().unwrap_or_else(|| format!("web-{}", js_sys::Date::now() as u64)),
    );
    let initial_entries = read_stored_entries();
    let next_id = RwSignal::new(next_entry_id(&initial_entries));
    let entries = RwSignal::new(initial_entries);
    let chat_pending = RwSignal::new(false);
    let chat_error = RwSignal::new(None::<String>);
    let example_prompts = RwSignal::new(Vec::<String>::new());
    let composer_draft = RwSignal::new(String::new());

    let load_example_prompts = move |jwt: String| {
        spawn_local(async move {
            if let Ok(prompts) = api::fetch_example_prompts(&jwt).await {
                example_prompts.set(prompts);
            }
        });
    };

    // Session restored from a previous page load: fetch prompts up front so
    // they're ready the moment the (empty) chat pane renders.
    if let Some(jwt) = token.get_untracked() {
        load_example_prompts(jwt);
    }

    let push_entry = move |role: Role, content: String, attachments: Vec<ImageAttachment>| {
        let id = next_id.get();
        next_id.set(id + 1);
        entries.update(|list| {
            list.push(ChatEntry {
                id,
                role,
                content,
                attachments,
                streaming: false,
                tool_events: None,
                reasoning: None,
            });
            *list = trim_to_max_turns(std::mem::take(list), MAX_STORED_TURNS);
        });
        persist_entries(&entries.get());
    };

    let do_login = move |email: String, password: String| {
        login_error.set(None);
        login_pending.set(true);
        spawn_local(async move {
            match login(&email, &password).await {
                Ok(jwt) => {
                    let _ = SessionStorage::set(TOKEN_STORAGE_KEY, &jwt);
                    persist_email(&email);
                    let session = session_id_for(&email);
                    persist_session(&session);
                    session_id.set(session);
                    clear_stored_entries();
                    entries.set(Vec::new());
                    next_id.set(0);
                    load_example_prompts(jwt.clone());
                    token.set(Some(jwt));
                }
                Err(err) => login_error.set(Some(err.to_string())),
            }
            login_pending.set(false);
        });
    };

    let do_logout = move || {
        SessionStorage::delete(TOKEN_STORAGE_KEY);
        SessionStorage::delete(SESSION_STORAGE_KEY);
        SessionStorage::delete(EMAIL_STORAGE_KEY);
        clear_stored_entries();
        token.set(None);
        entries.set(Vec::new());
        next_id.set(0);
        example_prompts.set(Vec::new());
        composer_draft.set(String::new());
    };

    let do_send = move |outgoing: OutgoingMessage| {
        let Some(jwt) = token.get() else { return };
        let OutgoingMessage { text, attachments } = outgoing;
        push_entry(Role::User, text.clone(), attachments.clone());
        chat_error.set(None);
        chat_pending.set(true);
        let session = session_id.get();
        let image_urls: Vec<String> = attachments.into_iter().map(|a| a.url).collect();
        spawn_local(async move {
            match api::send_chat_message(&jwt, &session, &text, &image_urls).await {
                Ok(reply) => push_entry(Role::Assistant, reply, Vec::new()),
                Err(err) => chat_error.set(Some(err.to_string())),
            }
            chat_pending.set(false);
        });
    };

    let do_new_chat = move || {
        let Some(jwt) = token.get() else { return };
        let Some(email) = read_stored_email() else { return };
        clear_stored_entries();
        entries.set(Vec::new());
        next_id.set(0);
        chat_error.set(None);
        let session = session_id_for(&email);
        persist_session(&session);
        session_id.set(session.clone());
        spawn_local(async move {
            if let Err(err) = api::start_new_session(&jwt, &session).await {
                chat_error.set(Some(err.to_string()));
            }
        });
    };

    let open_chat = move || chat_open.set(true);
    let close_chat = move || chat_open.set(false);
    let toggle_expand = move || expanded.update(|value| *value = !*value);

    view! {
        <Show
            when=move || chat_open.get()
            fallback=move || view! { <CrabLauncher on_open=open_chat /> }
        >
            <Show
                when=move || token.get().is_some()
                fallback=move || {
                    view! {
                        <LoginForm
                            error=Signal::derive(move || login_error.get())
                            pending=Signal::derive(move || login_pending.get())
                            on_submit=do_login
                            on_minimize=close_chat
                        />
                    }
                }
            >
                <ChatShell
                    entries=Signal::derive(move || entries.get())
                    pending=Signal::derive(move || chat_pending.get())
                    error=Signal::derive(move || chat_error.get())
                    example_prompts=Signal::derive(move || example_prompts.get())
                    draft=composer_draft
                    on_send=do_send
                    on_new_chat=do_new_chat
                    on_logout=do_logout
                    on_minimize=close_chat
                    expanded=Signal::derive(move || expanded.get())
                    on_toggle_expand=toggle_expand
                />
            </Show>
        </Show>
    }
}
