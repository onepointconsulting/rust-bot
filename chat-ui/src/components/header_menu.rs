use leptos::prelude::*;

const ICON_BTN: &str = "flex h-8 w-8 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700";
const TEXT_BTN: &str =
    "rounded-full px-3 py-1.5 text-xs font-medium text-slate-600 hover:bg-slate-100";
const TEXT_BTN_MUTED: &str =
    "rounded-full px-3 py-1.5 text-xs font-medium text-slate-400 hover:bg-slate-100";
const MENU_ITEM: &str = "flex w-full items-center gap-2.5 px-3 py-2.5 text-left text-sm font-medium text-slate-700 hover:bg-slate-50";
const MENU_ITEM_MUTED: &str = "flex w-full items-center gap-2.5 px-3 py-2.5 text-left text-sm font-medium text-slate-500 hover:bg-slate-50";

fn icon_class() -> &'static str {
    "h-4 w-4"
}

#[component]
fn IconExpand() -> impl IntoView {
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
            <path d="M15 3h6v6" />
            <path d="M9 21H3v-6" />
            <path d="M21 3l-7 7" />
            <path d="M3 21l7-7" />
        </svg>
    }
}

#[component]
fn IconRestore() -> impl IntoView {
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
            <path d="M9 3v6H3" />
            <path d="M15 21v-6h6" />
            <path d="M3 3l7 7" />
            <path d="M21 21l-7-7" />
        </svg>
    }
}

#[component]
fn IconMinimize() -> impl IntoView {
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
            <path d="M5 12h14" />
        </svg>
    }
}

#[component]
fn IconMenu() -> impl IntoView {
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
            <path d="M4 6h16" />
            <path d="M4 12h16" />
            <path d="M4 18h16" />
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
            class=icon_class()
            aria-hidden="true"
        >
            <path d="M18 6L6 18" />
            <path d="M6 6l12 12" />
        </svg>
    }
}

/// Header actions for both chat frontends: inline on `sm+`, a hamburger
/// dropdown below that so the title/status stay readable on a narrow phone.
#[component]
pub fn ChatHeaderActions(
    #[prop(into)] expanded: Signal<bool>,
    on_new_chat: impl Fn() + 'static + Copy,
    on_logout: impl Fn() + 'static + Copy,
    on_minimize: impl Fn() + 'static + Copy,
    on_toggle_expand: impl Fn() + 'static + Copy,
) -> impl IntoView {
    let menu_open = RwSignal::new(false);

    let do_new_chat = move |_| {
        menu_open.set(false);
        on_new_chat();
    };
    let do_logout = move |_| {
        menu_open.set(false);
        on_logout();
    };
    let do_minimize = move |_| {
        menu_open.set(false);
        on_minimize();
    };
    let do_toggle_expand = move |_| {
        menu_open.set(false);
        on_toggle_expand();
    };
    let expand_label = move || {
        if expanded.get() {
            "Restore"
        } else {
            "Expand"
        }
    };
    let expand_aria = move || {
        if expanded.get() {
            "Restore chat"
        } else {
            "Expand chat"
        }
    };

    view! {
        <div class="flex shrink-0 items-center gap-1">
            <div class="hidden items-center gap-1 sm:flex">
                <button type="button" class=TEXT_BTN on:click=do_new_chat>
                    "New chat"
                </button>
                <button type="button" class=TEXT_BTN_MUTED on:click=do_logout>
                    "Sign out"
                </button>
                <button
                    type="button"
                    aria-label=expand_aria
                    title=expand_label
                    class=format!("ml-1 {ICON_BTN}")
                    on:click=do_toggle_expand
                >
                    <Show when=move || expanded.get() fallback=|| view! { <IconExpand /> }>
                        <IconRestore />
                    </Show>
                </button>
                <button
                    type="button"
                    aria-label="Minimize chat"
                    title="Minimize"
                    class=format!("ml-1 {ICON_BTN}")
                    on:click=do_minimize
                >
                    <IconMinimize />
                </button>
            </div>

            <div class="relative sm:hidden">
                <button
                    type="button"
                    class=format!("relative z-20 {ICON_BTN}")
                    aria-label=move || {
                        if menu_open.get() { "Close menu" } else { "Open menu" }
                    }
                    aria-haspopup="menu"
                    aria-expanded=move || if menu_open.get() { "true" } else { "false" }
                    on:click=move |_| menu_open.update(|open| *open = !*open)
                >
                    <Show when=move || menu_open.get() fallback=|| view! { <IconMenu /> }>
                        <IconClose />
                    </Show>
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
                        class="absolute right-0 z-20 mt-1 w-44 overflow-hidden rounded-xl bg-white py-1 shadow-lg ring-1 ring-slate-200"
                    >
                        <button type="button" role="menuitem" class=MENU_ITEM on:click=do_new_chat>
                            "New chat"
                        </button>
                        <button
                            type="button"
                            role="menuitem"
                            class=MENU_ITEM
                            on:click=do_toggle_expand
                        >
                            <Show when=move || expanded.get() fallback=|| view! { <IconExpand /> }>
                                <IconRestore />
                            </Show>
                            <span>{expand_label}</span>
                        </button>
                        <button
                            type="button"
                            role="menuitem"
                            class=MENU_ITEM
                            on:click=do_minimize
                        >
                            <IconMinimize />
                            "Minimize"
                        </button>
                        <div class="my-1 border-t border-slate-100"></div>
                        <button
                            type="button"
                            role="menuitem"
                            class=MENU_ITEM_MUTED
                            on:click=do_logout
                        >
                            "Sign out"
                        </button>
                    </div>
                </div>
            </div>
        </div>
    }
}
