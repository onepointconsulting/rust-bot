mod api;
mod app;
mod components;
mod markdown;
mod models;

use app::App;

fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Warn);
    leptos::mount::mount_to_body(App);
}
