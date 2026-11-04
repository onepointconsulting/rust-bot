//! Banner listing what the chat has pre-approved, so an "always allow" choice
//! never goes unnoticed, with a button that revokes all of it.
//!
//! Shown only while something is granted. The gateway owns the state: pressing
//! Revoke sends `revoke_tool_grants` and the banner goes away when the
//! `tool_grants_updated` reply comes back.

use leptos::prelude::*;

use crate::state::ToolGrants;

#[component]
pub fn ToolGrantsBanner(
    #[prop(into)] grants: Signal<ToolGrants>,
    on_revoke: Callback<()>,
) -> impl IntoView {
    let lines = move || grants.get().summary_lines();
    view! {
        <Show when=move || !grants.get().is_empty()>
            <div class="tool-grants" role="status" aria-label="Auto-approved tools">
                <ul class="tool-grants__list">
                    <For
                        each=lines
                        key=|line| line.clone()
                        children=move |line| view! { <li>{line}</li> }
                    />
                </ul>
                <button
                    type="button"
                    class="shrink-0 rounded-full px-3 py-1 text-xs font-medium text-slate-700 ring-1 ring-slate-300 hover:bg-white"
                    on:click=move |_| on_revoke.run(())
                >
                    "Revoke"
                </button>
            </div>
        </Show>
    }
}
