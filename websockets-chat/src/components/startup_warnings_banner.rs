//! Amber strip listing the tool sections the gateway left out at start-up (for
//! example Gmail without credentials), so an administrator notices the warning
//! the gateway printed on its console.
//!
//! The gateway only sends these to connections that may change settings. A
//! dismissed warning stays hidden for the page's lifetime, across reconnects,
//! keyed by its message: a different problem shows up again.

use leptos::prelude::*;

use crate::protocol::StartupWarning;

/// The warnings still to show: those whose message is not in `dismissed`, in order.
pub fn visible_startup_warnings(
    warnings: &[StartupWarning],
    dismissed: &[String],
) -> Vec<StartupWarning> {
    warnings
        .iter()
        .filter(|warning| !dismissed.contains(&warning.message))
        .cloned()
        .collect()
}

/// The messages to remember as dismissed after the user closes the strip
/// showing `shown`: the ones already dismissed plus every shown message, once each.
pub fn dismiss_startup_warnings(dismissed: &[String], shown: &[StartupWarning]) -> Vec<String> {
    let mut remembered = dismissed.to_vec();
    for warning in shown {
        if !remembered.contains(&warning.message) {
            remembered.push(warning.message.clone());
        }
    }
    remembered
}

/// One line per warning in `warnings` (already filtered with
/// [`visible_startup_warnings`]) and a Dismiss button that calls `on_dismiss`
/// with the warnings shown. Renders nothing when `warnings` is empty.
#[component]
pub fn StartupWarningsBanner(
    #[prop(into)] warnings: Signal<Vec<StartupWarning>>,
    on_dismiss: Callback<Vec<StartupWarning>>,
) -> impl IntoView {
    view! {
        <Show when=move || !warnings.get().is_empty()>
            <div
                class="flex items-start justify-between gap-2 border-b border-amber-100 bg-amber-50 px-4 py-2 text-xs text-amber-700"
                role="status"
                aria-label="Start-up warnings"
            >
                <ul class="min-w-0 space-y-1">
                    <For
                        each=move || warnings.get()
                        key=|warning| warning.message.clone()
                        children=move |warning| {
                            view! { <li class="break-words">{warning.message}</li> }
                        }
                    />
                </ul>
                <button
                    type="button"
                    class="shrink-0 rounded-full bg-amber-600 px-3 py-1 font-medium text-white hover:bg-amber-700"
                    on:click=move |_| on_dismiss.run(warnings.get_untracked())
                >
                    "Dismiss"
                </button>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warning(message: &str) -> StartupWarning {
        StartupWarning {
            section: "gmail".to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn dismissed_messages_are_hidden() {
        let warnings = vec![warning("a"), warning("b")];
        let visible = visible_startup_warnings(&warnings, &["a".to_string()]);
        assert_eq!(visible, vec![warning("b")]);
    }

    #[test]
    fn nothing_dismissed_shows_everything() {
        let warnings = vec![warning("a")];
        assert_eq!(visible_startup_warnings(&warnings, &[]), warnings);
    }

    #[test]
    fn dismissing_remembers_each_message_once() {
        let remembered =
            dismiss_startup_warnings(&["a".to_string()], &[warning("a"), warning("b")]);
        assert_eq!(remembered, vec!["a".to_string(), "b".to_string()]);
    }
}
