use leptos::html;
use leptos::prelude::*;

use crate::markdown;

/// Renders Markdown as sanitized HTML inside a `.markdown`-styled `<div>`.
#[component]
pub fn MarkdownView(#[prop(into)] source: Signal<String>) -> impl IntoView {
    let html_string = move || markdown::render(&source.get());

    html::div().class("markdown").inner_html(html_string)
}
