use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::components::MarkdownView;
use crate::markdown;
use crate::models::{ChatEntry, Role};

fn copy_text_to_clipboard(text: &str) -> Result<js_sys::Promise, String> {
    let window = web_sys::window().ok_or_else(|| "No window".to_string())?;
    let clipboard = window.navigator().clipboard();
    Ok(clipboard.write_text(text))
}

/// A single chat entry rendered as a bubble.
///
/// `extra` is an optional slot rendered below the message text/attachments,
/// letting a consumer (e.g. `websockets-chat`) inject tool-activity or
/// reasoning panels without this crate knowing anything about them; `chat-ui`
/// itself renders nothing there when `None`.
///
/// While `streaming` is true, the in-progress indicator is either a blinking
/// token cursor (`token_streaming` true / omitted) or a thinking spinner
/// (`token_streaming` false) — see `.streaming-cursor` / `.thinking-indicator`
/// in `chat-ui/style/shared.css`.
#[component]
pub fn MessageBubble(
    entry: ChatEntry,
    #[prop(optional)] extra: Option<Children>,
    #[prop(into)] streaming: Signal<bool>,
    #[prop(into, optional)] token_streaming: Option<Signal<bool>>,
) -> impl IntoView {
    let is_user = entry.role == Role::User;
    let content = entry.content.clone();
    let attachments = entry.attachments.clone();
    let extra_view = extra.map(|children| children());
    let token_streaming = token_streaming.unwrap_or_else(|| Signal::derive(|| true));
    let content_empty = content.is_empty();

    let pending_view = move || {
        if !streaming.get() {
            return ().into_any();
        }
        if token_streaming.get() {
            view! { <span class="streaming-cursor" aria-hidden="true"></span> }.into_any()
        } else if content_empty {
            view! {
                <div class="thinking-indicator" role="status" aria-live="polite">
                    <span class="thinking-spinner" aria-hidden="true"></span>
                    <span>"Rust Bot is thinking"</span>
                </div>
            }
            .into_any()
        } else {
            ().into_any()
        }
    };

    if is_user {
        let has_text = markdown::has_visible_chars(&content);
        let text_view = if has_text {
            view! {
                <p class="whitespace-pre-wrap text-sm overflow-hidden">
                    {content}
                    {pending_view()}
                </p>
            }
            .into_any()
        } else {
            pending_view()
        };
        let has_attachments = !attachments.is_empty();
        let attachments_view = if !has_attachments {
            ().into_any()
        } else {
            let row_class = if has_text {
                "mt-2 flex flex-wrap gap-1.5"
            } else {
                "flex flex-wrap gap-1.5"
            };
            view! {
                <div class=row_class>
                    <For
                        each=move || attachments.clone()
                        key=|attachment| attachment.url.clone()
                        let(attachment)
                    >
                        <img
                            src=attachment.url.clone()
                            alt=attachment.label.clone().unwrap_or_default()
                            class="h-24 max-w-full rounded-lg object-cover"
                        />
                    </For>
                </div>
            }
            .into_any()
        };
        if !has_text && !has_attachments && extra_view.is_none() && !streaming.get() {
            return ().into_any();
        }
        view! {
            <div class="flex justify-end">
                <div class="max-w-[80%] rounded-2xl bg-orange-600 px-4 py-2 text-sm text-white shadow-sm">
                    {text_view}
                    {attachments_view}
                    {extra_view}
                </div>
            </div>
        }
        .into_any()
    } else {
        // `trim().is_empty()` is not enough: markdown like `****` or a
        // zero-width fragment still produces a padded bubble + copy button
        // with nothing visible inside it.
        let has_text = !markdown::is_blank(&content);
        if has_text {
            view! {
                <div class="flex items-end gap-1.5 justify-start">
                    <div class="max-w-[80%] rounded-2xl bg-white px-4 py-2 text-slate-800 shadow-sm">
                        <MarkdownView source=content.clone() />
                        {pending_view()}
                        {extra_view}
                    </div>
                    <CopyButton text=content />
                </div>
            }
            .into_any()
        } else if extra_view.is_some() || streaming.get() {
            // Keep the thinking spinner / tool+reasoning extras; only the
            // empty padded pill is dropped.
            view! {
                <div class="flex flex-col items-start gap-2">
                    {pending_view()}
                    {extra_view}
                </div>
            }
            .into_any()
        } else {
            ().into_any()
        }
    }
}

#[component]
pub fn CopyButton(text: String) -> impl IntoView {
    let copied = RwSignal::new(false);

    let on_copy = move |_| {
        let text = text.clone();
        spawn_local(async move {
            let Ok(promise) = copy_text_to_clipboard(&text) else {
                return;
            };
            if JsFuture::from(promise).await.is_ok() {
                copied.set(true);
                if let Some(window) = web_sys::window() {
                    let reset = Closure::once(move || copied.set(false));
                    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                        reset.as_ref().unchecked_ref(),
                        1500,
                    );
                    reset.forget();
                }
            }
        });
    };

    view! {
        <button
            type="button"
            aria-label="Copy answer"
            title="Copy answer"
            class="mb-1 shrink-0 rounded-md p-1.5 text-slate-400 transition hover:bg-slate-100 hover:text-slate-600 focus:outline-none focus:ring-2 focus:ring-slate-300"
            on:click=on_copy
        >
            <Show
                when=move || copied.get()
                fallback=move || view! {
                    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class="h-4 w-4" aria-hidden="true">
                        <rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect>
                        <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
                    </svg>
                }
            >
                <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" class="h-4 w-4 text-emerald-600" aria-hidden="true">
                    <polyline points="20 6 9 17 4 12"></polyline>
                </svg>
            </Show>
        </button>
    }
}
