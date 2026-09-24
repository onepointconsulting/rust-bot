//! Approval card for a gateway `tool_approval_request`: the agent is paused
//! until the user approves or denies each proposed tool call.
//!
//! Per-call choices live on the parent's `PendingApproval` signal, not in
//! local state, so a re-sent request (after reconnect) or a toggle never
//! desyncs the checkboxes from what "Run selected" will send.

use leptos::prelude::*;

use crate::state::PendingApproval;

#[component]
pub fn ToolApprovalCard(
    #[prop(into)] approval: Signal<Option<PendingApproval>>,
    on_toggle: Callback<String>,
    on_answer: Callback<Vec<String>>,
) -> impl IntoView {
    let calls = move || approval.get().map(|p| p.calls).unwrap_or_default();
    let approved_count = move || calls().iter().filter(|c| c.approved).count();
    let run_selected = move |_| {
        if let Some(pending) = approval.get_untracked() {
            on_answer.run(pending.approved_ids());
        }
    };
    let approve_all = move |_| {
        if let Some(pending) = approval.get_untracked() {
            on_answer.run(pending.all_ids());
        }
    };
    let deny_all = move |_| on_answer.run(Vec::new());

    view! {
        <Show when=move || approval.get().is_some()>
            <div class="tool-approval" role="region" aria-label="Tool approval">
                <p class="tool-approval__title">
                    "The assistant wants to run "
                    {move || {
                        let n = calls().len();
                        if n == 1 { "1 tool".to_string() } else { format!("{n} tools") }
                    }}
                    ". Approve?"
                </p>
                <ul class="tool-approval__list">
                    <For
                        each=calls
                        key=|entry| (entry.call.id.clone(), entry.approved)
                        children=move |entry| {
                            let call_id = entry.call.id.clone();
                            let has_args = !entry.call.arguments_preview.is_empty()
                                && entry.call.arguments_preview != "{}";
                            let preview = entry.call.arguments_preview.clone();
                            view! {
                                <li class="tool-approval__item">
                                    <label class="flex items-center gap-2">
                                        <input
                                            type="checkbox"
                                            class="accent-orange-600"
                                            prop:checked=entry.approved
                                            on:change=move |_| on_toggle.run(call_id.clone())
                                        />
                                        <span class="tool-approval__name">{entry.call.name.clone()}</span>
                                    </label>
                                    {has_args.then(|| view! {
                                        <details class="tool-approval__args">
                                            <summary>"Arguments"</summary>
                                            <pre>{preview}</pre>
                                        </details>
                                    })}
                                </li>
                            }
                        }
                    />
                </ul>
                <div class="flex flex-wrap items-center justify-end gap-2">
                    <button
                        type="button"
                        class="rounded-full px-3 py-1 text-xs font-medium text-slate-600 ring-1 ring-slate-300 hover:bg-slate-100"
                        on:click=deny_all
                    >
                        "Deny all"
                    </button>
                    <button
                        type="button"
                        class="rounded-full px-3 py-1 text-xs font-medium text-amber-800 ring-1 ring-amber-300 hover:bg-amber-100"
                        on:click=approve_all
                    >
                        "Approve all"
                    </button>
                    <button
                        type="button"
                        class="rounded-full bg-amber-600 px-3 py-1 text-xs font-medium text-white hover:bg-amber-700"
                        on:click=run_selected
                    >
                        {move || format!("Run selected ({})", approved_count())}
                    </button>
                </div>
            </div>
        </Show>
    }
}
