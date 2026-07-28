use leptos::prelude::*;

/// Email/password login screen. Calls `on_submit` and lets the parent own
/// the request lifecycle so error/loading state can be shared with the
/// rest of the app if needed later.
#[component]
pub fn LoginForm(
    #[prop(into)] error: Signal<Option<String>>,
    #[prop(into)] pending: Signal<bool>,
    on_submit: impl Fn(String, String) + 'static + Copy,
    on_minimize: impl Fn() + 'static + Copy,
) -> impl IntoView {
    let email = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());

    let submit = move || {
        let email = email.get();
        let password = password.get();
        if !email.trim().is_empty() && !password.is_empty() {
            on_submit(email, password);
        }
    };

    view! {
        <div class="fixed bottom-6 right-6 z-50 w-[min(24rem,calc(100vw-3rem))] overflow-hidden rounded-2xl bg-white shadow-2xl ring-1 ring-slate-200">
            <div class="flex items-start justify-between border-b border-slate-100 px-6 pb-0 pt-5">
                <div>
                    <h1 class="mb-1 text-xl font-semibold text-slate-900">"Rust Bot"</h1>
                    <p class="mb-5 text-sm text-slate-500">"Sign in to start chatting."</p>
                </div>
                <button
                    type="button"
                    aria-label="Minimize chat"
                    title="Minimize"
                    class="-mr-2 -mt-1 flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100 hover:text-slate-700"
                    on:click=move |_| on_minimize()
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
                        <path d="M5 12h14" />
                    </svg>
                </button>
            </div>

            <form
                class="space-y-4 px-6 pb-6"
                on:submit=move |ev| {
                    ev.prevent_default();
                    submit();
                }
            >
                <div>
                    <label class="mb-1 block text-xs font-medium text-slate-600">"Email"</label>
                    <input
                        type="email"
                        required
                        class="w-full rounded-lg border border-slate-200 px-3 py-2 text-sm outline-none focus:border-orange-500"
                        prop:value=email
                        on:input=move |ev| email.set(event_target_value(&ev))
                    />
                </div>
                <div>
                    <label class="mb-1 block text-xs font-medium text-slate-600">"Password"</label>
                    <input
                        type="password"
                        required
                        class="w-full rounded-lg border border-slate-200 px-3 py-2 text-sm outline-none focus:border-orange-500"
                        prop:value=password
                        on:input=move |ev| password.set(event_target_value(&ev))
                    />
                </div>

                <Show when=move || error.get().is_some()>
                    <p class="text-sm text-red-600">{move || error.get().unwrap_or_default()}</p>
                </Show>

                <button
                    type="submit"
                    disabled=move || pending.get()
                    class="w-full rounded-lg bg-orange-600 px-4 py-2 text-sm font-semibold text-white transition hover:bg-orange-700 disabled:cursor-not-allowed disabled:opacity-60"
                >
                    {move || if pending.get() { "Signing in..." } else { "Sign in" }}
                </button>
            </form>
        </div>
    }
}
