//! Compact account chip: circular initial + truncated email.
//!
//! Shown only at the bottom of [`crate::components::SessionsSidebar`], so it
//! is visible solely while that panel is open.

use leptos::prelude::*;

use crate::user_display::email_initial;

/// Avatar + email for the currently logged-in user.
///
/// Renders nothing when `email` is `None` or blank so callers can pass the
/// signal through unconditionally.
#[component]
pub fn UserAccountChip(#[prop(into)] email: Signal<Option<String>>) -> impl IntoView {
    view! {
        {move || {
            let Some(email) = email.get().filter(|value| !value.trim().is_empty()) else {
                return ().into_any();
            };
            let initial = email_initial(&email).to_string();
            let title = email.clone();
            view! {
                <div class="flex min-w-0 items-center gap-2.5 px-3 py-3">
                    <span
                        class="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-orange-600 text-sm font-semibold text-white"
                        aria-hidden="true"
                    >
                        {initial}
                    </span>
                    <span class="min-w-0 truncate text-sm text-slate-700" title=title>
                        {email}
                    </span>
                </div>
            }
            .into_any()
        }}
    }
}
