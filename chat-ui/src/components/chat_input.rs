use leptos::html::{Input, Textarea};
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{File, FileList, HtmlInputElement, HtmlTextAreaElement};

use crate::models::{
    ImageAttachment, OutgoingMessage, SessionTokenUsage, SkillSummary, format_compact_tokens,
};

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

/// Model-preset drop-up trigger + menu, rendered on the toolbar row under
/// the composer. Opens **upward** (`bottom-full`) since the composer sits at
/// the bottom of the chat — same overlay + click-outside pattern as
/// [`crate::components::ChatHeaderActions`]'s mobile menu, just flipped.
///
/// Absent entirely (not just disabled) when `presets` is empty: `web-chat`
/// passes no picker props at all, and an older/HTTP gateway with no catalog
/// yet shouldn't show a trigger with nothing to pick.
#[component]
fn ModelPresetPicker(
    presets: Signal<Vec<String>>,
    selected: Signal<String>,
    on_select: Callback<String>,
) -> impl IntoView {
    let menu_open = RwSignal::new(false);

    view! {
        <Show when=move || !presets.get().is_empty()>
            <div class="relative">
                <button
                    type="button"
                    aria-haspopup="menu"
                    aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                    class="flex items-center gap-1 rounded-full px-2.5 py-1 text-xs font-medium text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                    on:click=move |_| menu_open.update(|open| *open = !*open)
                >
                    <span>{move || selected.get()}</span>
                    <svg
                        xmlns="http://www.w3.org/2000/svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                        class="h-3 w-3"
                        aria-hidden="true"
                    >
                        <path d="m18 15-6-6-6 6" />
                    </svg>
                </button>
                <div class=move || {
                    if menu_open.get() { "block" } else { "hidden" }
                }>
                    <div
                        class="fixed inset-0 z-10"
                        aria-hidden="true"
                        on:click=move |_| menu_open.set(false)
                    ></div>
                    <div
                        role="menu"
                        class="absolute bottom-full z-20 mb-1 w-40 overflow-hidden rounded-xl bg-white py-1 shadow-lg ring-1 ring-slate-200"
                    >
                        <For
                            each=move || presets.get()
                            key=|name| name.clone()
                            let(name)
                        >
                            {
                                let name_for_click = name.clone();
                                let name_for_check = name.clone();
                                view! {
                                    <button
                                        type="button"
                                        role="menuitem"
                                        class="flex w-full items-center justify-between gap-2.5 px-3 py-2 text-left text-sm font-medium text-slate-700 hover:bg-slate-50"
                                        on:click=move |_| {
                                            menu_open.set(false);
                                            on_select.run(name_for_click.clone());
                                        }
                                    >
                                        <span>{name.clone()}</span>
                                        <Show when=move || selected.get() == name_for_check>
                                            <span aria-hidden="true">"\u{2713}"</span>
                                        </Show>
                                    </button>
                                }
                            }
                        </For>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// Compact `"38.6K in · 412 out"` summary for the chip trigger. A field the
/// provider never reported renders as `?` rather than `0`, matching
/// [`SessionTokenUsage`]'s "missing is not zero" convention.
fn usage_summary_text(usage: SessionTokenUsage) -> String {
    let input = usage
        .prompt_tokens()
        .map(format_compact_tokens)
        .unwrap_or_else(|| "?".to_string());
    let output = usage
        .output_tokens
        .map(format_compact_tokens)
        .unwrap_or_else(|| "?".to_string());
    format!("{input} in · {output} out")
}

fn format_usd(amount: f64) -> String {
    format!("${amount:.6}")
}

/// Labeled rows for the popup detail list — only fields the provider
/// actually reported, in roughly the order they were incurred.
fn usage_detail_rows(usage: SessionTokenUsage) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if let Some(v) = usage.input_tokens {
        rows.push(("Input", v.to_string()));
    }
    if let Some(v) = usage.cache_creation_input_tokens {
        rows.push(("Cache write", v.to_string()));
    }
    if let Some(v) = usage.cache_read_input_tokens {
        rows.push(("Cache read", v.to_string()));
    }
    if let Some(v) = usage.output_tokens {
        rows.push(("Output", v.to_string()));
    }
    if let Some(v) = usage.reasoning_tokens {
        rows.push(("Reasoning", v.to_string()));
    }
    if let Some(v) = usage.total_tokens() {
        rows.push(("Total tokens", v.to_string()));
    }
    if let Some(v) = usage.input_cost {
        rows.push(("Input cost", format_usd(v)));
    }
    if let Some(v) = usage.output_cost {
        rows.push(("Output cost", format_usd(v)));
    }
    if let Some(v) = usage.total_cost() {
        rows.push(("Total cost", format_usd(v)));
    }
    rows
}

/// Session usage drop-up, rendered on the toolbar row right after
/// [`ModelPresetPicker`]. Absent entirely (not just disabled) once `usage`
/// resolves to `None`/empty — a brand new chat, or an older gateway that
/// doesn't send `token_usage` yet — same reasoning as the preset picker's
/// own `Show`.
///
/// Two triggers share one drop-up: a text chip on `sm+` (desktop) and an
/// icon-only button below `sm` (mobile), toggled by the same `menu_open`
/// signal — see the component doc for [`crate::components::ChatHeaderActions`]
/// for the equivalent desktop/mobile split elsewhere in this crate.
#[component]
fn SessionUsageChip(usage: Signal<Option<SessionTokenUsage>>) -> impl IntoView {
    let menu_open = RwSignal::new(false);
    let has_usage = move || usage.get().is_some_and(|u| !u.is_empty());

    view! {
        <Show when=has_usage>
            <div class="relative">
                <button
                    type="button"
                    aria-haspopup="menu"
                    aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                    class="hidden items-center gap-1 rounded-full px-2.5 py-1 text-xs font-medium text-slate-500 hover:bg-slate-100 hover:text-slate-700 sm:flex"
                    on:click=move |_| menu_open.update(|open| *open = !*open)
                >
                    <span>{move || usage.get().map(usage_summary_text).unwrap_or_default()}</span>
                </button>
                <button
                    type="button"
                    aria-haspopup="menu"
                    aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                    aria-label="Session usage"
                    title="Session usage"
                    class="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700 sm:hidden"
                    on:click=move |_| menu_open.update(|open| *open = !*open)
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
                        <path d="M3 3v18h18" />
                        <path d="M18 17V9" />
                        <path d="M13 17V5" />
                        <path d="M8 17v-3" />
                    </svg>
                </button>
                <div class=move || {
                    if menu_open.get() { "block" } else { "hidden" }
                }>
                    <div
                        class="fixed inset-0 z-10"
                        aria-hidden="true"
                        on:click=move |_| menu_open.set(false)
                    ></div>
                    <div
                        role="menu"
                        class="absolute bottom-full left-0 z-20 mb-1 w-56 overflow-hidden rounded-xl bg-white py-2 shadow-lg ring-1 ring-slate-200"
                    >
                        <p class="px-3 pb-1 text-xs font-semibold uppercase tracking-wide text-slate-400">
                            "Session usage"
                        </p>
                        <For
                            each=move || usage.get().map(usage_detail_rows).unwrap_or_default()
                            key=|(label, value)| format!("{label}:{value}")
                            let((label, value))
                        >
                            <div class="flex items-center justify-between gap-3 px-3 py-1 text-sm text-slate-700">
                                <span class="text-slate-500">{label}</span>
                                <span class="font-medium">{value}</span>
                            </div>
                        </For>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// Skills drop-up, rendered on the toolbar row immediately before
/// [`ModelPresetPicker`]. Absent entirely (not just disabled) when `skills`
/// is empty — `web-chat` (no gateway skills protocol) never sets this, and
/// an older/HTTP gateway with no catalog yet shouldn't show a trigger with
/// nothing to browse. Read-only: name + description, no activation.
#[component]
fn SkillsPopup(skills: Signal<Vec<SkillSummary>>) -> impl IntoView {
    let menu_open = RwSignal::new(false);

    view! {
        <Show when=move || !skills.get().is_empty()>
            <div class="relative">
                <button
                    type="button"
                    aria-haspopup="menu"
                    aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                    aria-label="Skills"
                    title="Skills"
                    class="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                    on:click=move |_| menu_open.update(|open| *open = !*open)
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
                        <path d="M9.937 15.5A2 2 0 0 0 8.5 14.063l-6.135-1.582a.5.5 0 0 1 0-.962L8.5 9.936A2 2 0 0 0 9.937 8.5l1.582-6.135a.5.5 0 0 1 .963 0L14.063 8.5A2 2 0 0 0 15.5 9.937l6.135 1.581a.5.5 0 0 1 0 .964L15.5 14.063a2 2 0 0 0-1.437 1.437l-1.582 6.135a.5.5 0 0 1-.963 0z" />
                        <path d="M20 3v4" />
                        <path d="M22 5h-4" />
                        <path d="M4 17v2" />
                        <path d="M5 18H3" />
                    </svg>
                </button>
                <div class=move || {
                    if menu_open.get() { "block" } else { "hidden" }
                }>
                    <div
                        class="fixed inset-0 z-10"
                        aria-hidden="true"
                        on:click=move |_| menu_open.set(false)
                    ></div>
                    <div
                        role="menu"
                        class="absolute bottom-full left-0 z-20 mb-1 max-h-64 w-72 overflow-y-auto rounded-xl bg-white py-1 shadow-lg ring-1 ring-slate-200"
                    >
                        <p class="px-3 pb-1 pt-2 text-xs font-semibold uppercase tracking-wide text-slate-400">
                            "Skills"
                        </p>
                        <For
                            each=move || skills.get()
                            key=|skill| skill.name.clone()
                            let(skill)
                        >
                            <div class="px-3 py-2">
                                <p class="text-sm font-medium text-slate-700">{skill.name.clone()}</p>
                                <p class="line-clamp-2 text-xs text-slate-500">{skill.description.clone()}</p>
                            </div>
                        </For>
                    </div>
                </div>
            </div>
        </Show>
    }
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
    /// The process/session model-preset catalog, e.g. `["default", "fast"]`.
    /// Omitted or empty hides the picker entirely — `web-chat` (no gateway
    /// preset protocol) never sets this.
    #[prop(into, optional)]
    model_presets: Option<Signal<Vec<String>>>,
    /// Currently-selected preset name, resolved server-side (never a raw
    /// stale/unknown name — see `model_preset_attached_fields`).
    #[prop(into, optional)]
    selected_model_preset: Option<Signal<String>>,
    #[prop(optional)] on_select_model_preset: Option<Callback<String>>,
    /// This session's lifetime token/cost totals. Omitted, or resolving to
    /// `None`/empty, hides the usage chip entirely — `web-chat` (no gateway
    /// usage protocol) never sets this.
    #[prop(into, optional)]
    session_usage: Option<Signal<Option<SessionTokenUsage>>>,
    /// Skills installed on this process. Omitted or empty hides the popup
    /// entirely — `web-chat` (no gateway skills protocol) never sets this.
    #[prop(into, optional)]
    skills: Option<Signal<Vec<SkillSummary>>>,
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

            <div class="mt-1.5 flex items-center gap-1.5">
                <div class="flex items-center gap-1">
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
                        class="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
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
                        class="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
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
                </div>

                {move || {
                    match skills {
                        Some(skills) => view! { <SkillsPopup skills=skills /> }.into_any(),
                        None => view! { <></> }.into_any(),
                    }
                }}
                {move || {
                    match (model_presets, selected_model_preset, on_select_model_preset) {
                        (Some(presets), Some(selected), Some(on_select)) => {
                            view! {
                                <ModelPresetPicker
                                    presets=presets
                                    selected=selected
                                    on_select=on_select
                                />
                            }
                                .into_any()
                        }
                        _ => view! { <></> }.into_any(),
                    }
                }}
                {move || {
                    match session_usage {
                        Some(usage) => view! { <SessionUsageChip usage=usage /> }.into_any(),
                        None => view! { <></> }.into_any(),
                    }
                }}
            </div>
        </div>
    }
}
