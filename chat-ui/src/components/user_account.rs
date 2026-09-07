//! Header avatar that opens an account menu: User ID (email) + Log Out.
//!
//! Always rendered so guest sessions still have a way to close the chat.
//! The User ID row is omitted when `email` is `None` or blank.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::user_display::email_initial;

const MENU_ITEM: &str =
    "flex w-full items-center gap-2.5 px-3 py-2.5 text-left text-sm font-medium text-slate-700 hover:bg-slate-50";

fn copy_text_to_clipboard(text: &str) -> Result<js_sys::Promise, String> {
    let window = web_sys::window().ok_or_else(|| "No window".to_string())?;
    let clipboard = window.navigator().clipboard();
    Ok(clipboard.write_text(text))
}

fn icon_class() -> &'static str {
    "h-4 w-4"
}

#[component]
fn IconCopy() -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.75"
            stroke-linecap="round"
            stroke-linejoin="round"
            class=icon_class()
            aria-hidden="true"
        >
            <rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect>
            <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path>
        </svg>
    }
}

#[component]
fn IconCheck() -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.75"
            stroke-linecap="round"
            stroke-linejoin="round"
            class="h-4 w-4 text-emerald-600"
            aria-hidden="true"
        >
            <polyline points="20 6 9 17 4 12"></polyline>
        </svg>
    }
}

#[component]
fn IconLogout() -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class=icon_class()
            aria-hidden="true"
        >
            <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" />
            <polyline points="16 17 21 12 16 7" />
            <line x1="21" x2="9" y1="12" y2="12" />
        </svg>
    }
}

/// Circular-initial trigger whose popover shows the logged-in email (as
/// User ID) and a Log Out action.
#[component]
pub fn UserAccountMenu(
    #[prop(into)] email: Signal<Option<String>>,
    on_logout: impl Fn() + 'static + Copy,
) -> impl IntoView {
    let menu_open = RwSignal::new(false);
    let copied = RwSignal::new(false);

    let display_email = move || email.get().filter(|value| !value.trim().is_empty());
    let initial = move || {
        display_email()
            .as_deref()
            .map(email_initial)
            .unwrap_or('?')
            .to_string()
    };

    let do_logout = move |_| {
        menu_open.set(false);
        on_logout();
    };

    let on_copy = move |_| {
        let Some(text) = display_email() else {
            return;
        };
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
        <div class="relative">
            <button
                type="button"
                class="relative z-20 flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-orange-600 text-sm font-semibold text-white hover:opacity-90 focus:outline-none focus:ring-2 focus:ring-slate-300"
                aria-label=move || {
                    if menu_open.get() { "Close account menu" } else { "Open account menu" }
                }
                aria-haspopup="menu"
                aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                on:click=move |_| {
                    menu_open.update(|open| {
                        *open = !*open;
                        if !*open {
                            copied.set(false);
                        }
                    });
                }
            >
                {initial}
            </button>
            <div class=move || { if menu_open.get() { "block" } else { "hidden" } }>
                <div
                    class="fixed inset-0 z-10"
                    aria-hidden="true"
                    on:click=move |_| {
                        menu_open.set(false);
                        copied.set(false);
                    }
                ></div>
                <div
                    role="menu"
                    class="absolute right-0 z-20 mt-1 w-64 overflow-hidden rounded-xl bg-white py-1 shadow-lg ring-1 ring-slate-200"
                >
                    <Show when=move || display_email().is_some()>
                        <div class="flex items-center justify-between gap-2 px-3 py-2.5">
                            <div class="min-w-0">
                                <p class="text-xs font-medium text-slate-400">"User ID"</p>
                                <p
                                    class="mt-0.5 truncate text-sm text-slate-800"
                                    title=move || display_email().unwrap_or_default()
                                >
                                    {move || display_email().unwrap_or_default()}
                                </p>
                            </div>
                            <button
                                type="button"
                                class="shrink-0 rounded-md p-1 text-slate-400 hover:bg-slate-100 hover:text-slate-600 focus:outline-none focus:ring-2 focus:ring-slate-300"
                                aria-label="Copy email"
                                title="Copy email"
                                on:click=on_copy
                            >
                                <Show when=move || copied.get() fallback=|| view! { <IconCopy /> }>
                                    <IconCheck />
                                </Show>
                            </button>
                        </div>
                        <div class="my-1 border-t border-slate-100"></div>
                    </Show>
                    <button type="button" role="menuitem" class=MENU_ITEM on:click=do_logout>
                        <IconLogout />
                        "Log Out"
                    </button>
                </div>
            </div>
        </div>
    }
}
