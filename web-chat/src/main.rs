mod api;
mod app;
mod components;
mod markdown;
mod models;

use app::App;
use leptos::mount::mount_to;
use leptos::prelude::document;
use wasm_bindgen::JsCast;
use web_sys::HtmlElement;

fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Warn);

    let el = document()
        .get_element_by_id("rust-bot-chat")
        .expect("missing #rust-bot-chat")
        .unchecked_into::<HtmlElement>();

    mount_to(el, App).forget();
}
