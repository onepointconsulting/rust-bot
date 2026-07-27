use leptos::prelude::*;

#[component]
pub fn ChatInput(
    #[prop(into)] pending: Signal<bool>,
    on_send: impl Fn(String) + 'static + Copy,
) -> impl IntoView {
    let draft = RwSignal::new(String::new());

    let send = move || {
        let text = draft.get();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            on_send(trimmed.to_string());
            draft.set(String::new());
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
                    rows="1"
                    placeholder="Ask follow up..."
                    class="flex-1 resize-none rounded-xl border border-slate-200 px-3 py-2 text-sm outline-none focus:border-orange-500"
                    prop:value=draft
                    on:input=move |ev| draft.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" && !ev.shift_key() {
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
