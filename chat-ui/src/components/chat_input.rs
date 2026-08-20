use leptos::html::{Input, Textarea};
use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{File, FileList, HtmlInputElement, HtmlTextAreaElement};

use crate::models::{ImageAttachment, OutgoingMessage};

const MAX_TEXTAREA_HEIGHT_PX: f64 = 160.0;
/// Client-side guard against attaching huge images before base64 encoding.
const MAX_ATTACHMENT_BYTES: f64 = 8.0 * 1024.0 * 1024.0;

fn resize_textarea(el: &HtmlTextAreaElement) {
    // Collapse first so scrollHeight reflects the real content height.
    // Fully-qualify HtmlElement::style so Leptos's ElementExt::style doesn't win.
    let style = web_sys::HtmlElement::style(el);
    let _ = style.set_property("height", "auto");
    let height = el.scroll_height() as f64;
    let capped = height.min(MAX_TEXTAREA_HEIGHT_PX);
    let _ = style.set_property("height", &format!("{capped}px"));
    let overflow = if height > MAX_TEXTAREA_HEIGHT_PX {
        "auto"
    } else {
        "hidden"
    };
    let _ = style.set_property("overflow-y", overflow);
}

/// Read a `File`'s bytes as a `data:` URL and hand the result to `on_loaded`.
fn read_file_as_data_url(file: File, on_loaded: impl Fn(String) + 'static) {
    let Ok(reader) = web_sys::FileReader::new() else {
        return;
    };
    let reader_for_closure = reader.clone();
    let onload = Closure::wrap(Box::new(move |_event: web_sys::ProgressEvent| {
        if let Ok(result) = reader_for_closure.result() {
            if let Some(data_url) = result.as_string() {
                on_loaded(data_url);
            }
        }
    }) as Box<dyn FnMut(web_sys::ProgressEvent)>);
    reader.set_onload(Some(onload.as_ref().unchecked_ref()));
    onload.forget();
    let _ = reader.read_as_data_url(&file);
}

/// Validate a single file (must be an image within the size cap) and, once
/// read, append it to `attachments`.
fn queue_image_file(file: File, attachments: RwSignal<Vec<ImageAttachment>>) {
    use web_sys::Blob;

    let blob: &Blob = file.as_ref();
    if !blob.type_().starts_with("image/") {
        return;
    }
    if blob.size() > MAX_ATTACHMENT_BYTES {
        return;
    }
    let name = file.name();
    let label = if name.is_empty() { None } else { Some(name) };
    read_file_as_data_url(file, move |data_url| {
        let label = label.clone();
        attachments.update(|list| {
            list.push(ImageAttachment {
                url: data_url,
                label,
            });
        });
    });
}

/// Validate and queue every image file found in a `FileList` (file input or drop).
fn queue_image_files(files: FileList, attachments: RwSignal<Vec<ImageAttachment>>) {
    for index in 0..files.length() {
        if let Some(file) = files.get(index) {
            queue_image_file(file, attachments);
        }
    }
}

/// Snapshot pending attachments with their index, for the chip list `<For>`.
/// Pulled into a plain function (rather than inline turbofish in the view!
/// macro) since `::<Vec<_>>` angle brackets confuse the view! tag parser.
fn indexed_attachments(
    attachments: RwSignal<Vec<ImageAttachment>>,
) -> Vec<(usize, ImageAttachment)> {
    attachments.get().into_iter().enumerate().collect()
}

#[component]
fn AttachmentChip(
    index: usize,
    attachment: ImageAttachment,
    on_remove: impl Fn(usize) + 'static + Copy,
) -> impl IntoView {
    view! {
        <div class="group relative h-14 w-14 shrink-0 overflow-hidden rounded-lg border border-slate-200">
            <img
                src=attachment.url.clone()
                alt=attachment.label.clone().unwrap_or_default()
                class="h-full w-full object-cover"
            />
            <button
                type="button"
                aria-label="Remove attachment"
                title="Remove"
                class="absolute right-0.5 top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-slate-900/70 text-[10px] leading-none text-white opacity-0 transition group-hover:opacity-100"
                on:click=move |_| on_remove(index)
            >
                "\u{00D7}"
            </button>
        </div>
    }
}

/// The composer.
///
/// `on_abort`, when supplied, turns the send button into a Stop button for as
/// long as `pending` is true, so a caller whose backend can cancel an
/// in-flight turn gets that affordance and one that cannot (`web-chat`,
/// whose HTTP request has nothing to cancel) simply omits the prop and keeps
/// today's disabled-while-pending button.
#[component]
pub fn ChatInput(
    #[prop(into)] pending: Signal<bool>,
    draft: RwSignal<String>,
    on_send: impl Fn(OutgoingMessage) + 'static + Copy,
    #[prop(optional)] on_abort: Option<Callback<()>>,
) -> impl IntoView {
    let attachments = RwSignal::new(Vec::<ImageAttachment>::new());
    let show_url_field = RwSignal::new(false);
    let url_draft = RwSignal::new(String::new());
    let drag_over = RwSignal::new(false);
    let textarea_ref = NodeRef::<Textarea>::new();
    let file_input_ref = NodeRef::<Input>::new();

    // When something outside the composer (e.g. an example-prompt click)
    // sets `draft`, keep the textarea's auto-sized height in sync too.
    Effect::new(move |_| {
        let _ = draft.get();
        if let Some(el) = textarea_ref.get() {
            resize_textarea(&el);
        }
    });

    let send = move || {
        if pending.get() {
            return;
        }
        let text = draft.get().trim().to_string();
        let current_attachments = attachments.get();
        if text.is_empty() && current_attachments.is_empty() {
            return;
        }
        on_send(OutgoingMessage {
            text,
            attachments: current_attachments,
        });
        draft.set(String::new());
        attachments.set(Vec::new());
        show_url_field.set(false);
        url_draft.set(String::new());
        if let Some(el) = textarea_ref.get() {
            resize_textarea(&el);
        }
    };

    let add_url_attachment = move || {
        let raw = url_draft.get();
        let trimmed = raw.trim();
        let is_supported = trimmed.starts_with("http://")
            || trimmed.starts_with("https://")
            || trimmed.starts_with("data:image/");
        if trimmed.is_empty() || !is_supported {
            return;
        }
        let url = trimmed.to_string();
        attachments.update(|list| {
            list.push(ImageAttachment { url, label: None });
        });
        url_draft.set(String::new());
        show_url_field.set(false);
    };

    let remove_attachment = move |index: usize| {
        attachments.update(|list| {
            if index < list.len() {
                list.remove(index);
            }
        });
    };

    let can_abort = on_abort.is_some();
    let show_stop_button = move || can_abort && pending.get();

    let composer_class = move || {
        if drag_over.get() {
            "border-t border-slate-200 bg-white px-3 py-3 ring-2 ring-orange-400 ring-inset"
        } else {
            "border-t border-slate-200 bg-white px-3 py-3"
        }
    };

    view! {
        <div
            class=composer_class
            on:dragover=move |ev| {
                ev.prevent_default();
                drag_over.set(true);
            }
            on:dragleave=move |_| drag_over.set(false)
            on:drop=move |ev| {
                ev.prevent_default();
                drag_over.set(false);
                if let Some(data_transfer) = ev.data_transfer() {
                    if let Some(files) = data_transfer.files() {
                        queue_image_files(files, attachments);
                    }
                }
            }
        >
            <Show when=move || !attachments.get().is_empty()>
                <div class="mb-2 flex flex-wrap gap-2">
                    <For
                        each=move || indexed_attachments(attachments)
                        key=|(index, attachment)| format!("{index}-{}", attachment.url)
                        let(item)
                    >
                        <AttachmentChip index=item.0 attachment=item.1.clone() on_remove=remove_attachment />
                    </For>
                </div>
            </Show>

            <Show when=move || show_url_field.get()>
                <div class="mb-2 flex items-center gap-2">
                    <input
                        type="text"
                        placeholder="Paste image URL..."
                        class="flex-1 rounded-lg border border-slate-200 px-2 py-1.5 text-xs outline-none focus:border-orange-500"
                        prop:value=url_draft
                        on:input=move |ev| url_draft.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" {
                                ev.prevent_default();
                                add_url_attachment();
                            }
                        }
                    />
                    <button
                        type="button"
                        class="shrink-0 rounded-lg bg-slate-100 px-2.5 py-1.5 text-xs font-medium text-slate-600 hover:bg-slate-200"
                        on:click=move |_| add_url_attachment()
                    >
                        "Add"
                    </button>
                </div>
            </Show>

            <form
                class="flex items-end gap-1.5"
                on:submit=move |ev| {
                    ev.prevent_default();
                    send();
                }
            >
                <textarea
                    node_ref=textarea_ref
                    rows="1"
                    placeholder="Ask follow up..."
                    class="flex-1 resize-none overflow-hidden rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-orange-500"
                    prop:value=draft
                    on:input=move |ev| {
                        draft.set(event_target_value(&ev));
                        if let Some(el) = ev
                            .target()
                            .and_then(|t| t.dyn_into::<HtmlTextAreaElement>().ok())
                        {
                            resize_textarea(&el);
                        }
                    }
                    on:keydown=move |ev| {
                        // Enter sends; Shift+Enter / Ctrl+Enter insert a newline.
                        if ev.key() == "Enter" && !ev.shift_key() && !ev.ctrl_key() {
                            ev.prevent_default();
                            send();
                        }
                    }
                    on:paste=move |ev| {
                        let Some(data_transfer) = ev.clipboard_data() else {
                            return;
                        };
                        let items = data_transfer.items();
                        let mut had_image = false;
                        for index in 0..items.length() {
                            let Some(item) = items.get(index) else { continue };
                            if item.kind() == "file" && item.type_().starts_with("image/") {
                                had_image = true;
                                if let Ok(Some(file)) = item.get_as_file() {
                                    queue_image_file(file, attachments);
                                }
                            }
                        }
                        // Only intercept the paste when it actually carried an
                        // image; plain-text pastes still insert normally.
                        if had_image {
                            ev.prevent_default();
                        }
                    }
                ></textarea>
                <input
                    node_ref=file_input_ref
                    type="file"
                    accept="image/*"
                    multiple
                    class="hidden"
                    on:change=move |ev| {
                        if let Some(input) = ev
                            .target()
                            .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
                        {
                            if let Some(files) = input.files() {
                                queue_image_files(files, attachments);
                            }
                            input.set_value("");
                        }
                    }
                />
                <button
                    type="button"
                    aria-label="Attach image file"
                    title="Attach image file"
                    class="flex h-9 w-9 shrink-0 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                    on:click=move |_| {
                        if let Some(el) = file_input_ref.get() {
                            el.click();
                        }
                    }
                >
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
                        <path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" />
                    </svg>
                </button>
                <button
                    type="button"
                    aria-label="Attach image URL"
                    title="Attach image URL"
                    class="flex h-9 w-9 shrink-0 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                    on:click=move |_| show_url_field.update(|value| *value = !*value)
                >
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
                        <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
                        <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
                    </svg>
                </button>
                <Show
                    when=move || show_stop_button()
                    fallback=move || {
                        view! {
                            <button
                                type="submit"
                                disabled=move || {
                                    pending.get()
                                        || (draft.get().trim().is_empty()
                                            && attachments.get().is_empty())
                                }
                                class="rounded-full bg-orange-600 px-4 py-2 text-sm font-semibold text-white transition hover:bg-orange-700 disabled:cursor-not-allowed disabled:opacity-50"
                            >
                                "Send"
                            </button>
                        }
                    }
                >
                    <button
                        type="button"
                        aria-label="Stop generating"
                        title="Stop generating"
                        class="flex items-center gap-1.5 rounded-full bg-slate-800 px-4 py-2 text-sm font-semibold text-white transition hover:bg-slate-900"
                        on:click=move |_| {
                            if let Some(on_abort) = on_abort {
                                on_abort.run(());
                            }
                        }
                    >
                        <span class="h-2.5 w-2.5 rounded-sm bg-white" aria-hidden="true"></span>
                        "Stop"
                    </button>
                </Show>
            </form>
            <p class="mt-2 text-center text-xs text-slate-400">
                "AI-powered. The assistant can make mistakes."
            </p>
        </div>
    }
}
