use leptos::html::Div;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::components::MarkdownView;
use crate::models::{ChatEntry, Role};

fn scroll_list_to_bottom(list_ref: NodeRef<Div>) {
    // Wait one frame so newly rendered bubbles are in the layout.
    if let Some(window) = web_sys::window() {
        let cb = Closure::once(move || {
            if let Some(el) = list_ref.get() {
                el.set_scroll_top(el.scroll_height());
            }
        });
        let _ = window.request_animation_frame(cb.as_ref().unchecked_ref());
        cb.forget();
    }
}

fn copy_text_to_clipboard(text: &str) -> Result<js_sys::Promise, String> {
    let window = web_sys::window().ok_or_else(|| "No window".to_string())?;
    let clipboard = window.navigator().clipboard();
    Ok(clipboard.write_text(text))
}

#[component]
pub fn MessageList(
    #[prop(into)] entries: Signal<Vec<ChatEntry>>,
    #[prop(into)] pending: Signal<bool>,
    #[prop(into)] example_prompts: Signal<Vec<String>>,
    on_use_prompt: impl Fn(String) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    let list_ref = NodeRef::<Div>::new();

    // Keep the latest message (and "Thinking..." indicator) in view.
    Effect::new(move |_| {
        let _ = entries.get().len();
        let _ = pending.get();
        scroll_list_to_bottom(list_ref);
    });

    let show_suggestions =
        move || entries.get().is_empty() && !pending.get() && !example_prompts.get().is_empty();

    view! {
        <div
            node_ref=list_ref
            class="flex flex-1 flex-col gap-3 overflow-y-auto px-4 py-4"
        >
            <Show when=show_suggestions>
                <SuggestionPrompts prompts=example_prompts on_use_prompt=on_use_prompt />
            </Show>
            <For
                each=move || entries.get()
                key=|entry| entry.id
                let(entry)
            >
                <MessageBubble entry=entry />
            </For>
            <Show when=move || pending.get()>
                <div class="flex justify-start">
                    <div class="rounded-2xl bg-white px-4 py-2 text-sm text-slate-400 shadow-sm">
                        "Thinking..."
                    </div>
                </div>
            </Show>
        </div>
    }
}

/// Clickable example-prompt bubbles shown in an otherwise-empty chat pane.
/// Clicking a prompt fills the composer draft (via `on_use_prompt`) without
/// sending it, letting the user tweak it before submitting.
#[component]
fn SuggestionPrompts(
    #[prop(into)] prompts: Signal<Vec<String>>,
    on_use_prompt: impl Fn(String) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    view! {
        <div class="flex flex-col items-start gap-2">
            <p class="px-1 text-xs font-medium text-slate-400">"Try one of these:"</p>
            <For
                each=move || prompts.get()
                key=|prompt| prompt.clone()
                let(prompt)
            >
                <button
                    type="button"
                    class="max-w-[85%] rounded-2xl border border-dashed border-slate-300 bg-white px-4 py-2 text-left text-sm text-slate-600 shadow-sm transition hover:border-orange-400 hover:bg-orange-50 hover:text-orange-700"
                    on:click=move |_| on_use_prompt(prompt.clone())
                >
                    {prompt.clone()}
                </button>
            </For>
        </div>
    }
}

#[component]
fn MessageBubble(entry: ChatEntry) -> impl IntoView {
    let is_user = entry.role == Role::User;
    let content = entry.content.clone();
    let attachments = entry.attachments.clone();

    if is_user {
        let has_text = !content.trim().is_empty();
        let text_view = if has_text {
            view! { <p class="whitespace-pre-wrap text-sm">{content}</p> }.into_any()
        } else {
            ().into_any()
        };
        let attachments_view = if attachments.is_empty() {
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
        view! {
            <div class="flex justify-end">
                <div class="max-w-[80%] rounded-2xl bg-orange-600 px-4 py-2 text-sm text-white shadow-sm">
                    {text_view}
                    {attachments_view}
                </div>
            </div>
        }
        .into_any()
    } else {
        view! {
            <div class="flex items-end gap-1.5 justify-start">
                <div class="max-w-[80%] rounded-2xl bg-white px-4 py-2 text-slate-800 shadow-sm">
                    <MarkdownView source=content.clone() />
                </div>
                <CopyButton text=content />
            </div>
        }
        .into_any()
    }
}

#[component]
fn CopyButton(text: String) -> impl IntoView {
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
