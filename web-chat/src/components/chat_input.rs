use leptos::html::Textarea;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, HtmlTextAreaElement};

const MAX_TEXTAREA_HEIGHT_PX: f64 = 160.0;

fn resize_textarea(el: &HtmlTextAreaElement) {
    // Collapse first so scrollHeight reflects the real content height.
    // Fully-qualify HtmlElement::style so Leptos's ElementExt::style doesn't win.
    let style = HtmlElement::style(el);
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

#[component]
pub fn ChatInput(
    #[prop(into)] pending: Signal<bool>,
    on_send: impl Fn(String) + 'static + Copy,
) -> impl IntoView {
    let draft = RwSignal::new(String::new());
    let textarea_ref = NodeRef::<Textarea>::new();

    let send = move || {
        let text = draft.get();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            on_send(trimmed.to_string());
            draft.set(String::new());
            if let Some(el) = textarea_ref.get() {
                resize_textarea(&el);
            }
        }
    };

    view! {
        <div class="border-t border-slate-200 bg-white px-3 py-3">
            <form
                class="flex items-end gap-2"
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
                ></textarea>
                <button
                    type="submit"
                    disabled=move || pending.get() || draft.get().trim().is_empty()
                    class="rounded-full bg-orange-600 px-4 py-2 text-sm font-semibold text-white transition hover:bg-orange-700 disabled:cursor-not-allowed disabled:opacity-50"
                >
                    "Send"
                </button>
            </form>
            <p class="mt-2 text-center text-xs text-slate-400">
                "AI-powered. The assistant can make mistakes."
            </p>
        </div>
    }
}
