use leptos::html::Div;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::attachments::{filename_from_media_url, is_image_attachment_url, with_download_query};
use crate::components::MarkdownView;
use crate::markdown;
use crate::models::{ChatEntry, ImageAttachment, Role};

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
/// `on_fork`, when set, adds a Fork control next to the copy button on an
/// assistant bubble. The parent decides *which* bubbles show it (websockets-
/// chat shows it on every completed, non-streaming assistant reply).
///
/// While `streaming` is true, the in-progress indicator is the thinking
/// spinner until the first visible token arrives, then a blinking cursor
/// when `token_streaming` is true (omitted defaults to true). Non-streaming
/// turns keep the spinner for the whole wait. See `.streaming-cursor` /
/// `.thinking-indicator` in `chat-ui/style/shared.css`.
#[component]
pub fn MessageBubble(
    entry: ChatEntry,
    #[prop(optional)] extra: Option<Children>,
    #[prop(into)] streaming: Signal<bool>,
    #[prop(into, optional)] token_streaming: Option<Signal<bool>>,
    #[prop(optional)] on_fork: Option<Callback<()>>,
) -> impl IntoView {
    let is_user = entry.role == Role::User;
    let content = entry.content.clone();
    let attachments = entry.attachments.clone();
    let extra_view = extra.map(|children| children());
    let token_streaming = token_streaming.unwrap_or_else(|| Signal::derive(|| true));
    let awaiting_first_token = !markdown::has_visible_chars(&content);
    let fork_button = on_fork.map(|on_fork| view! { <ForkButton on_fork=on_fork /> }.into_any());

    // One lightbox per bubble instance: only ever holds the URL of the
    // attachment most recently clicked in *this* bubble, so no lifted/global
    // state is needed even though multiple bubbles can have attachments.
    let lightbox_url: RwSignal<Option<String>> = RwSignal::new(None);
    let lightbox_panel: NodeRef<Div> = NodeRef::new();

    Effect::new(move |_| {
        if lightbox_url.get().is_none() {
            return;
        }
        if let Some(el) = lightbox_panel.get() {
            let _ = el.focus();
        }
    });

    let pending_view = move || {
        if !streaming.get() {
            return ().into_any();
        }
        // Token streaming still waits on the model before the first delta.
        // A lone blinking caret in an empty bubble looks like a stuck input
        // cursor; reuse the non-streaming spinner until visible text exists.
        if awaiting_first_token {
            view! {
                <div class="thinking-indicator" role="status" aria-live="polite">
                    <span class="thinking-spinner" aria-hidden="true"></span>
                    <span>"Rust Bot is thinking"</span>
                </div>
            }
            .into_any()
        } else if token_streaming.get() {
            view! { <span class="streaming-cursor" aria-hidden="true"></span> }.into_any()
        } else {
            ().into_any()
        }
    };

    let main_view = if is_user {
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
        let attachments_view = attachments_row(attachments.clone(), lightbox_url, has_text);
        if !has_text && !has_attachments && extra_view.is_none() && !streaming.get() {
            ().into_any()
        } else {
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
        }
    } else {
        // `trim().is_empty()` is not enough: markdown like `****` or a
        // zero-width fragment still produces a padded bubble + copy button
        // with nothing visible inside it.
        let has_text = !markdown::is_blank(&content);
        let has_attachments = !attachments.is_empty();
        if has_text || has_attachments {
            let attachments_view = attachments_row(attachments.clone(), lightbox_url, has_text);
            let text_block = if has_text {
                view! {
                    <>
                        <MarkdownView source=content.clone() />
                        {pending_view()}
                    </>
                }
                .into_any()
            } else {
                pending_view()
            };
            let copy_button = has_text.then(|| view! { <CopyButton text=content.clone() /> });
            view! {
                <div class="flex flex-col items-start gap-1.5 md:flex-row md:items-end md:justify-start">
                    <div class="max-w-[80%] rounded-2xl bg-white px-4 py-2 text-slate-800 shadow-sm">
                        {text_block}
                        {attachments_view}
                        {extra_view}
                    </div>
                    <div class="flex items-end gap-1.5">
                        {copy_button}
                        {fork_button}
                    </div>
                </div>
            }
            .into_any()
        } else if extra_view.is_some() || streaming.get() || fork_button.is_some() {
            // Keep the thinking spinner / tool+reasoning extras; only the
            // empty padded pill is dropped.
            view! {
                <div class="flex flex-col items-start gap-1.5 md:flex-row md:items-end md:justify-start">
                    <div class="flex flex-col items-start gap-2">
                        {pending_view()}
                        {extra_view}
                    </div>
                    {fork_button}
                </div>
            }
            .into_any()
        } else {
            ().into_any()
        }
    };

    let lightbox_download_href = move || {
        lightbox_url
            .get()
            .map(|url| with_download_query(&url))
            .unwrap_or_default()
    };
    let lightbox_download_name = move || {
        lightbox_url
            .get()
            .as_deref()
            .and_then(filename_from_media_url)
            .unwrap_or_else(|| "image".to_string())
    };

    view! {
        <>
            {main_view}
            <Show when=move || lightbox_url.get().is_some()>
                <div class="fixed inset-0 z-[60] flex items-center justify-center p-4">
                    <div
                        class="absolute inset-0 bg-slate-900/80"
                        aria-hidden="true"
                        on:click=move |_| lightbox_url.set(None)
                    ></div>
                    <div
                        node_ref=lightbox_panel
                        role="dialog"
                        aria-modal="true"
                        aria-label="Image preview"
                        tabindex="-1"
                        class="relative max-h-[90vh] max-w-[90vw] outline-none"
                        on:keydown=move |ev| {
                            if ev.key() == "Escape" {
                                lightbox_url.set(None);
                            }
                        }
                    >
                        <img
                            src=move || lightbox_url.get().unwrap_or_default()
                            alt=""
                            class="max-h-[90vh] max-w-[90vw] rounded-lg object-contain shadow-2xl"
                        />
                        <a
                            href=lightbox_download_href
                            download=lightbox_download_name
                            aria-label="Download image"
                            title="Download"
                            class="attachment-lightbox-download"
                            on:click=move |ev| ev.stop_propagation()
                        >
                            <IconDownload />
                        </a>
                        <button
                            type="button"
                            aria-label="Close image preview"
                            class="absolute -top-3 -right-3 flex h-8 w-8 items-center justify-center rounded-full bg-white text-slate-600 shadow-lg transition hover:bg-slate-100 hover:text-slate-900"
                            on:click=move |_| lightbox_url.set(None)
                        >
                            <IconClose />
                        </button>
                    </div>
                </div>
            </Show>
        </>
    }
}

fn attachments_row(
    attachments: Vec<ImageAttachment>,
    lightbox_url: RwSignal<Option<String>>,
    has_text: bool,
) -> impl IntoView {
    if attachments.is_empty() {
        return ().into_any();
    }
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
                <AttachmentItem attachment=attachment lightbox_url=lightbox_url />
            </For>
        </div>
    }
    .into_any()
}

#[component]
fn AttachmentItem(
    attachment: ImageAttachment,
    lightbox_url: RwSignal<Option<String>>,
) -> impl IntoView {
    let url = attachment.url.clone();
    let filename = attachment
        .label
        .clone()
        .or_else(|| filename_from_media_url(&url))
        .unwrap_or_else(|| "attachment".to_string());
    let download_url = with_download_query(&url);
    if is_image_attachment_url(&url) {
        let open_url = url.clone();
        view! {
            <div class="attachment-item">
                <button
                    type="button"
                    class="block cursor-pointer rounded-lg transition hover:opacity-90"
                    on:click=move |_| lightbox_url.set(Some(open_url.clone()))
                >
                    <img
                        src=url
                        alt=filename.clone()
                        class="h-24 max-w-full rounded-lg object-cover"
                    />
                </button>
                <a
                    href=download_url
                    download=filename.clone()
                    aria-label=format!("Download {filename}")
                    title="Download"
                    class="attachment-download"
                    on:click=move |ev| ev.stop_propagation()
                >
                    <IconDownload />
                </a>
            </div>
        }
        .into_any()
    } else {
        let download_name = filename.clone();
        let chip_title = format!("Download {filename}");
        view! {
            <a
                href=download_url
                download=download_name
                class="attachment-chip"
                title=chip_title
            >
                <IconDownload />
                <span class="attachment-chip-name">{filename}</span>
            </a>
        }
        .into_any()
    }
}

#[component]
fn IconDownload() -> impl IntoView {
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
            <path d="M12 3v12" />
            <path d="M8 11l4 4 4-4" />
            <path d="M5 21h14" />
        </svg>
    }
}

#[component]
fn IconClose() -> impl IntoView {
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
            <path d="M18 6L6 18" />
            <path d="M6 6l12 12" />
        </svg>
    }
}

#[component]
fn IconFork() -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.75"
            stroke-linecap="round"
            stroke-linejoin="round"
            class="h-4 w-4"
            aria-hidden="true"
        >
            <circle cx="12" cy="18" r="3" />
            <circle cx="6" cy="6" r="3" />
            <circle cx="18" cy="6" r="3" />
            <path d="M18 9v1a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V9" />
            <path d="M12 12v3" />
        </svg>
    }
}

#[component]
fn ForkButton(on_fork: Callback<()>) -> impl IntoView {
    view! {
        <button
            type="button"
            aria-label="Fork from this reply"
            title="Fork from this reply"
            class="mb-1 shrink-0 rounded-md p-1.5 text-slate-400 transition hover:bg-slate-100 hover:text-slate-600 focus:outline-none focus:ring-2 focus:ring-slate-300"
            on:click=move |_| on_fork.run(())
        >
            <IconFork />
        </button>
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
