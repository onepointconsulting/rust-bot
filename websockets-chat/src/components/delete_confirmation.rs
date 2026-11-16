//! The inline "Delete …?" question with Cancel and Delete buttons, shown under a row of
//! the Settings pages (model presets, providers, channels) before anything is deleted.

use leptos::prelude::*;

/// The red question bar: `question`, then Cancel (runs `on_cancel`) and Delete (runs
/// `on_confirm`; greyed out while `disabled`).
#[component]
pub fn DeleteConfirmation(
    /// What is about to be deleted, e.g. "Delete provider 'x'? This cannot be undone."
    #[prop(into)]
    question: String,
    on_cancel: Callback<()>,
    on_confirm: Callback<()>,
    /// Whether Delete cannot be pressed right now (e.g. offline or read-only).
    #[prop(into)]
    disabled: Signal<bool>,
) -> impl IntoView {
    view! {
        <div class="mt-3 flex flex-wrap items-center gap-2 rounded-lg bg-red-50 px-3 py-2 text-sm text-red-800">
            <span class="flex-1">{question}</span>
            <button
                type="button"
                class="rounded-full border border-slate-300 bg-white px-3 py-1 text-sm text-slate-700 hover:bg-slate-100"
                on:click=move |_| on_cancel.run(())
            >
                "Cancel"
            </button>
            <button
                type="button"
                class="rounded-full bg-red-600 px-3 py-1 text-sm font-medium text-white hover:bg-red-700 disabled:opacity-40"
                disabled=move || disabled.get()
                on:click=move |_| on_confirm.run(())
            >
                "Delete"
            </button>
        </div>
    }
}
