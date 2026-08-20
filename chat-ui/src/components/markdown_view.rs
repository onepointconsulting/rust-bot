use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::Element;

use crate::markdown;

fn copy_text_to_clipboard(text: &str) -> Result<js_sys::Promise, String> {
    let window = web_sys::window().ok_or_else(|| "No window".to_string())?;
    let clipboard = window.navigator().clipboard();
    Ok(clipboard.write_text(text))
}

fn element_from_event_target(target: &web_sys::EventTarget) -> Option<Element> {
    target.dyn_ref::<Element>().cloned().or_else(|| {
        target
            .dyn_ref::<web_sys::Node>()
            .and_then(web_sys::Node::parent_element)
    })
}

/// Copy the `<pre><code>` body of the nearest `.code-block`. The chrome is
/// static HTML from [`markdown::render`], so clicks are handled here by
/// delegation rather than per-button Leptos listeners.
fn copy_code_block_from_button(button: &Element) {
    let Ok(Some(block)) = button.closest(".code-block") else {
        return;
    };
    let Ok(Some(code)) = block.query_selector("pre code") else {
        return;
    };
    let Some(text) = code.text_content() else {
        return;
    };
    if button.get_attribute("data-busy").as_deref() == Some("1") {
        return;
    }
    let _ = button.set_attribute("data-busy", "1");
    let previous = button.text_content();
    button.set_text_content(Some("Copied"));

    let button = button.clone();
    spawn_local(async move {
        let copied = match copy_text_to_clipboard(&text) {
            Ok(promise) => JsFuture::from(promise).await.is_ok(),
            Err(_) => false,
        };
        if !copied {
            button.set_text_content(previous.as_deref());
            let _ = button.remove_attribute("data-busy");
            return;
        }
        if let Some(window) = web_sys::window() {
            let reset = Closure::once(move || {
                button.set_text_content(previous.as_deref());
                let _ = button.remove_attribute("data-busy");
            });
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                reset.as_ref().unchecked_ref(),
                1500,
            );
            reset.forget();
        }
    });
}

/// Renders Markdown as sanitized HTML inside a `.markdown`-styled `<div>`.
///
/// Click handling for fenced-block Copy buttons is delegated on this
/// container: those buttons are part of the `inner_html` string, not Leptos
/// nodes, so they have no component-level `on:click`.
#[component]
pub fn MarkdownView(#[prop(into)] source: Signal<String>) -> impl IntoView {
    let html_string = move || markdown::render(&source.get());

    view! {
        <div
            class="markdown"
            inner_html=html_string
            on:click=move |ev| {
                let Some(target) = ev.target() else {
                    return;
                };
                let Some(element) = element_from_event_target(&target) else {
                    return;
                };
                let Ok(Some(button)) = element.closest(".code-block__copy") else {
                    return;
                };
                copy_code_block_from_button(&button);
            }
        ></div>
    }
}
