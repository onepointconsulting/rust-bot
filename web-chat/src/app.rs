use std::time::Duration;

use gloo_storage::{SessionStorage, Storage};
use leptos::prelude::*;
use leptos::task::spawn_local;

use chat_ui::api::login;
use chat_ui::components::LoginForm;
use chat_ui::models::{ChatEntry, ImageAttachment, OutgoingMessage, Role, SessionListItem};

use crate::api;
use crate::components::ChatShell;

const TOKEN_STORAGE_KEY: &str = "rust-bot-web-chat-token";
const SESSION_STORAGE_KEY: &str = "rust-bot-web-chat-session";
const EMAIL_STORAGE_KEY: &str = "rust-bot-web-chat-email";
const ENTRIES_STORAGE_KEY: &str = "rust-bot-web-chat-entries";

/// Max user/assistant exchanges kept in SessionStorage across refresh.
const MAX_STORED_TURNS: usize = 10;

/// How long after a reply lands to re-request the sessions list, to pick up
/// a title that was still generating when the immediately-on-completion
/// refresh ran (see `load_sessions`'s call sites in `App`). Title generation
/// is a fire-and-forget background LLM call kicked off only once the turn
/// finishes server-side, so it is essentially never done by the time this
/// request's own response comes back. There is no push notification for
/// "title ready", so this is a one-shot, best-effort timer rather than a
/// guarantee: if the LLM call is unusually slow, the real title only shows
/// up on the *next* refresh (another message, a new chat, or a reload).
const TITLE_REFRESH_DELAY_MS: u32 = 4_000;

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
    entries
        .iter()
        .map(|e| e.id)
        .max()
        .map(|id| id + 1)
        .unwrap_or(0)
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

/// Shared prefix for every session key this app mints for `email` (see
/// `session_id_for`). Used to filter `GET /v1/sessions`'s all-channel result
/// down to just this user's `web-chat` sessions for the sidebar.
fn session_prefix_for(email: &str) -> String {
    format!("web-{}-", email.replace('@', "_at_"))
}

fn session_id_for(email: &str) -> String {
    format!(
        "{}{}",
        session_prefix_for(email),
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
    let sessions = RwSignal::new(Vec::<SessionListItem>::new());
    let sidebar_open = RwSignal::new(false);

    let load_example_prompts = move |jwt: String| {
        spawn_local(async move {
            if let Ok(prompts) = api::fetch_example_prompts(&jwt).await {
                example_prompts.set(prompts);
            }
        });
    };

    // Refresh the sessions sidebar: fetch every persisted session and keep
    // only this user's `web-chat` keys (see `session_prefix_for`), in the
    // all-channel order `GET /v1/sessions` already returns them
    // (most-recently-updated first).
    let load_sessions = move |jwt: String, email: String| {
        spawn_local(async move {
            if let Ok(all) = api::fetch_sessions(&jwt).await {
                let prefix = session_prefix_for(&email);
                let filtered: Vec<SessionListItem> = all
                    .into_iter()
                    .filter(|session| session.id.starts_with(&prefix))
                    .collect();
                sessions.set(filtered);
            }
        });
    };

    // Session restored from a previous page load: fetch prompts up front so
    // they're ready the moment the (empty) chat pane renders, and refresh
    // the sidebar with this user's persisted sessions.
    if let Some(jwt) = token.get_untracked() {
        load_example_prompts(jwt.clone());
        if let Some(email) = read_stored_email() {
            load_sessions(jwt, email);
        }
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
                    load_sessions(jwt.clone(), email);
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
        sessions.set(Vec::new());
        sidebar_open.set(false);
    };

    let do_send = move |outgoing: OutgoingMessage| {
        let Some(jwt) = token.get() else { return };
        let OutgoingMessage { text, attachments } = outgoing;
        push_entry(Role::User, text.clone(), attachments.clone());
        chat_error.set(None);
        chat_pending.set(true);
        let session = session_id.get();
        let image_urls: Vec<String> = attachments.into_iter().map(|a| a.url).collect();
        let jwt_for_refresh = jwt.clone();
        spawn_local(async move {
            match api::send_chat_message(&jwt, &session, &text, &image_urls).await {
                Ok(reply) => {
                    push_entry(Role::Assistant, reply, Vec::new());
                    // Refresh now (title generation was just scheduled
                    // server-side, so this almost always still shows the
                    // "New chat" placeholder) and again after a short delay
                    // to pick up the title once that background LLM call
                    // actually finishes — see `TITLE_REFRESH_DELAY_MS`.
                    if let Some(email) = read_stored_email() {
                        load_sessions(jwt_for_refresh.clone(), email.clone());
                        spawn_local(async move {
                            gloo_timers::future::sleep(Duration::from_millis(u64::from(
                                TITLE_REFRESH_DELAY_MS,
                            )))
                            .await;
                            load_sessions(jwt_for_refresh, email);
                        });
                    }
                }
                Err(err) => chat_error.set(Some(err.to_string())),
            }
            chat_pending.set(false);
        });
    };

    let do_new_chat = move || {
        let Some(jwt) = token.get() else { return };
        let Some(email) = read_stored_email() else {
            return;
        };
        clear_stored_entries();
        entries.set(Vec::new());
        next_id.set(0);
        chat_error.set(None);
        let session = session_id_for(&email);
        persist_session(&session);
        session_id.set(session.clone());
        let jwt_for_refresh = jwt.clone();
        let email_for_refresh = email.clone();
        spawn_local(async move {
            if let Err(err) = api::start_new_session(&jwt, &session).await {
                chat_error.set(Some(err.to_string()));
            }
        });
        load_sessions(jwt_for_refresh, email_for_refresh);
    };

    let open_chat = move || chat_open.set(true);
    let close_chat = move || chat_open.set(false);
    let toggle_expand = move || expanded.update(|value| *value = !*value);
    let toggle_sidebar = move || sidebar_open.update(|open| *open = !*open);
    let close_sidebar = move || sidebar_open.set(false);
    // Switching sessions isn't implemented yet (list-only sidebar for now —
    // see `chat_ui::components::SessionsSidebar`'s doc comment): there is no
    // history-fetch API to repopulate `entries` from a past session.
    let on_select_session = move |_session_id: String| {};

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
                    sessions=Signal::derive(move || sessions.get())
                    active_session_id=Signal::derive(move || Some(session_id.get()))
                    sidebar_open=Signal::derive(move || sidebar_open.get())
                    draft=composer_draft
                    on_send=do_send
                    on_new_chat=do_new_chat
                    on_logout=do_logout
                    on_minimize=close_chat
                    expanded=Signal::derive(move || expanded.get())
                    on_toggle_expand=toggle_expand
                    on_toggle_sidebar=toggle_sidebar
                    on_close_sidebar=close_sidebar
                    on_select_session=on_select_session
                />
            </Show>
        </Show>
    }
}
