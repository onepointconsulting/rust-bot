//! Pieces shared by the list pages that replace the chat area ("MCPs", "Sub-agents"):
//! the page frame with its title, subtitle and Refresh button, a section heading with a
//! count, and the placeholder tile that stands in for an icon.

use leptos::prelude::*;

/// Uppercase first character of `title`, shown in the placeholder tile that stands in
/// for an icon. `?` for an empty title.
pub(crate) fn tile_initial(title: &str) -> String {
    title
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Placeholder tile standing in for an icon: the name's first letter.
#[component]
pub(crate) fn Tile(title: String) -> impl IntoView {
    view! {
        <div class="flex h-12 w-12 shrink-0 items-center justify-center rounded-xl border border-slate-200 bg-slate-100 text-lg font-semibold text-slate-500">
            {tile_initial(&title)}
        </div>
    }
}

/// Section heading with a count, e.g. "Presets 13", and an optional extra detail.
#[component]
pub(crate) fn SectionHeading(
    title: &'static str,
    count: usize,
    detail: Option<String>,
) -> impl IntoView {
    view! {
        <div class="flex items-baseline gap-2 pt-6 pb-1">
            <h2 class="text-sm font-semibold text-slate-700">{title}</h2>
            <span class="text-xs text-slate-400">{count}</span>
            {detail.map(|text| view! { <span class="text-xs text-slate-400">{text}</span> })}
        </div>
    }
}

/// The frame of a list page: scrolling column, title, subtitle and a Refresh button. The
/// page's own content goes in the children.
#[component]
pub(crate) fn ListPageShell(
    title: &'static str,
    #[prop(into)] subtitle: Signal<&'static str>,
    refresh_label: &'static str,
    on_refresh: Callback<()>,
    children: Children,
) -> impl IntoView {
    view! {
        <div class="flex-1 overflow-y-auto bg-white">
            <div class="mx-auto w-full max-w-3xl px-6 py-6">
                <div class="flex items-start justify-between gap-4">
                    <div>
                        <h1 class="text-xl font-semibold text-slate-900">{title}</h1>
                        <p class="text-sm text-slate-500">{move || subtitle.get()}</p>
                    </div>
                    <button
                        type="button"
                        class="rounded-full px-3 py-1.5 text-sm text-slate-600 hover:bg-slate-100"
                        aria-label=refresh_label
                        on:click=move |_| on_refresh.run(())
                    >
                        "Refresh"
                    </button>
                </div>
                {children()}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_initial_uppercases_first_character() {
        assert_eq!(tile_initial("github"), "G");
        assert_eq!(tile_initial("Étoile"), "É");
        assert_eq!(tile_initial(""), "?");
    }
}
