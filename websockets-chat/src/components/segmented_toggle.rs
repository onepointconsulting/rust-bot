//! A small segmented control: a row of buttons of which exactly one is selected, like the
//! "Dedicated model provider / Custom model provider" switch of the add provider panel.
//!
//! It is generic: it takes the option labels and a signal holding the selected index, and
//! knows nothing about what the options mean. Each button reports its state with
//! `aria-pressed`. The classes of a button are a plain function so they are
//! unit-testable on the host target without Leptos.

use leptos::prelude::*;

/// Tailwind classes shared by every segment.
const SEGMENT_BASE_CLASS: &str =
    "flex-1 rounded-lg px-3 py-1.5 text-sm font-medium transition-colors";

/// Extra classes of the selected segment.
const SEGMENT_SELECTED_CLASS: &str = "bg-white text-slate-900 shadow-sm";

/// Extra classes of a segment that is not selected.
const SEGMENT_IDLE_CLASS: &str = "text-slate-500 hover:text-slate-800";

/// The classes of one segment, by whether it is the selected one.
pub fn segment_class(selected: bool) -> String {
    let state = if selected {
        SEGMENT_SELECTED_CLASS
    } else {
        SEGMENT_IDLE_CLASS
    };
    format!("{SEGMENT_BASE_CLASS} {state}")
}

/// The index to select after `requested` was clicked among `option_count` options: the
/// click itself, or the current selection when `requested` is out of range.
pub fn clamp_selection(requested: usize, current: usize, option_count: usize) -> usize {
    if requested < option_count {
        requested
    } else {
        current
    }
}

/// The row of buttons, `options` being their labels in order; `selected` is the index of
/// the pressed one and is written when another is clicked.
#[component]
pub fn SegmentedToggle(
    options: Vec<&'static str>,
    selected: RwSignal<usize>,
    /// Accessible name of the whole control.
    #[prop(optional)]
    label: &'static str,
) -> impl IntoView {
    let option_count = options.len();
    view! {
        <div
            role="group"
            aria-label=label
            class="mb-4 flex gap-1 rounded-xl bg-slate-200/70 p-1"
        >
            {options
                .into_iter()
                .enumerate()
                .map(|(index, option_label)| {
                    view! {
                        <button
                            type="button"
                            aria-pressed=move || (selected.get() == index).to_string()
                            class=move || segment_class(selected.get() == index)
                            on:click=move |_| {
                                selected
                                    .update(|current| {
                                        *current = clamp_selection(index, *current, option_count)
                                    })
                            }
                        >
                            {option_label}
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selected_segment_is_raised_and_the_others_are_muted() {
        assert!(segment_class(true).contains("bg-white"));
        assert!(!segment_class(false).contains("bg-white"));
        assert!(segment_class(false).contains("text-slate-500"));
        // Both keep the shared layout classes.
        assert!(segment_class(true).starts_with(SEGMENT_BASE_CLASS));
        assert!(segment_class(false).starts_with(SEGMENT_BASE_CLASS));
    }

    #[test]
    fn a_click_selects_its_own_index_unless_it_is_out_of_range() {
        assert_eq!(clamp_selection(1, 0, 2), 1);
        assert_eq!(clamp_selection(0, 1, 2), 0);
        assert_eq!(clamp_selection(2, 1, 2), 1);
    }
}
