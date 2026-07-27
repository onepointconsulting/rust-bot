use leptos::prelude::*;

/// Email/password login screen. Calls `on_submit` and lets the parent own
/// the request lifecycle so error/loading state can be shared with the
/// rest of the app if needed later.
#[component]
pub fn LoginForm(
    #[prop(into)] error: Signal<Option<String>>,
    #[prop(into)] pending: Signal<bool>,
    on_submit: impl Fn(String, String) + 'static + Copy,
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
        <div class="flex min-h-screen items-center justify-center bg-slate-100 px-4">
            <div class="w-full max-w-sm rounded-2xl bg-white p-8 shadow-sm">
                <h1 class="mb-1 text-xl font-semibold text-slate-900">"Rust Bot"</h1>
                <p class="mb-6 text-sm text-slate-500">"Sign in to start chatting."</p>

                <form
                    class="space-y-4"
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
        </div>
    }
}
