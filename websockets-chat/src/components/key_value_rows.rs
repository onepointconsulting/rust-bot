//! An editor for a name -> text map (HTTP headers, for instance) as one row per entry,
//! each with a name, a value and a delete button, and "+ Add header" below.
//!
//! The rows read and write the same `NAME=value` lines draft the plain map textarea of
//! [`super::tool_config_form`] uses, so validation (`lines_to_string_map`) and
//! `changed_values` behave exactly as for every other map field. A row without a name
//! is kept on screen while it is being typed but not written to the draft.
//!
//! The line round trip is plain functions so it is unit-testable on the host target
//! without Leptos.

use leptos::prelude::*;

/// Separator between the name and the value of an entry in its draft line.
const ENTRY_SEPARATOR: char = '=';

/// One entry being edited: name and value.
pub type KeyValueRow = (String, String);

/// The rows of a `NAME=value` lines `draft`, in line order. The name ends at the first
/// `=`, so a value may contain `=`; a line without one is a name with an empty value.
/// Blank lines are skipped.
pub fn draft_to_rows(draft: &str) -> Vec<KeyValueRow> {
    draft
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| match line.split_once(ENTRY_SEPARATOR) {
            Some((name, value)) => (name.trim().to_string(), value.trim().to_string()),
            None => (line.trim().to_string(), String::new()),
        })
        .collect()
}

/// The `NAME=value` lines draft of `rows`. A row whose name is blank is left out (it is
/// not an entry yet); a `=` typed into a name is dropped, since it would end the name.
pub fn rows_to_draft(rows: &[KeyValueRow]) -> String {
    rows.iter()
        .filter_map(|(name, value)| {
            let name = name.replace(ENTRY_SEPARATOR, "");
            let name = name.trim();
            (!name.is_empty()).then(|| format!("{name}{ENTRY_SEPARATOR}{}", value.trim()))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `rows` without the one at `index` (nothing changes for an index past the end).
pub fn without_row(rows: &[KeyValueRow], index: usize) -> Vec<KeyValueRow> {
    rows.iter()
        .enumerate()
        .filter(|(row_index, _)| *row_index != index)
        .map(|(_, row)| row.clone())
        .collect()
}

/// `rows` with the name or the value (`edit_name`) of the row at `index` replaced.
pub fn with_row_text(
    rows: &[KeyValueRow],
    index: usize,
    edit_name: bool,
    text: String,
) -> Vec<KeyValueRow> {
    let mut edited = rows.to_vec();
    if let Some((name, value)) = edited.get_mut(index) {
        if edit_name {
            *name = text;
        } else {
            *value = text;
        }
    }
    edited
}

/// The rows, one per entry of `draft`, with an add button. `draft` is the same
/// `NAME=value` lines text a map field edits; it is rewritten on every change.
#[component]
pub fn KeyValueRows(
    draft: RwSignal<String>,
    /// Label of the add button, e.g. "+ Add header".
    add_label: &'static str,
    name_placeholder: &'static str,
    value_placeholder: &'static str,
    /// Whether the rows can be edited right now.
    #[prop(into)]
    enabled: Signal<bool>,
) -> impl IntoView {
    // The rows live here, not only in the draft: a row typed without a name yet has no
    // line in the draft but must stay on screen.
    let rows = RwSignal::new(draft_to_rows(&draft.get_untracked()));
    let set_rows = move |new_rows: Vec<KeyValueRow>| {
        draft.set(rows_to_draft(&new_rows));
        rows.set(new_rows);
    };
    let input_class =
        "min-w-0 flex-1 rounded-lg border border-slate-300 bg-white px-3 py-1.5 text-sm";
    view! {
        <div class="space-y-2">
            {move || {
                rows.get()
                    .into_iter()
                    .enumerate()
                    .map(|(index, (name, value))| {
                        view! {
                            <div class="flex items-center gap-2">
                                <input
                                    type="text"
                                    autocomplete="off"
                                    spellcheck="false"
                                    aria-label="Header name"
                                    class=input_class
                                    placeholder=name_placeholder
                                    disabled=move || !enabled.get()
                                    prop:value=name
                                    on:input=move |event| {
                                        set_rows(
                                            with_row_text(
                                                &rows.get_untracked(),
                                                index,
                                                true,
                                                event_target_value(&event),
                                            ),
                                        )
                                    }
                                />
                                <input
                                    type="text"
                                    autocomplete="off"
                                    spellcheck="false"
                                    aria-label="Header value"
                                    class=input_class
                                    placeholder=value_placeholder
                                    disabled=move || !enabled.get()
                                    prop:value=value
                                    on:input=move |event| {
                                        set_rows(
                                            with_row_text(
                                                &rows.get_untracked(),
                                                index,
                                                false,
                                                event_target_value(&event),
                                            ),
                                        )
                                    }
                                />
                                <button
                                    type="button"
                                    aria-label="Delete header"
                                    class="rounded-full px-2 py-1 text-sm text-slate-400 hover:bg-slate-200 hover:text-red-600 disabled:opacity-40"
                                    disabled=move || !enabled.get()
                                    on:click=move |_| {
                                        set_rows(without_row(&rows.get_untracked(), index))
                                    }
                                >
                                    "✕"
                                </button>
                            </div>
                        }
                    })
                    .collect_view()
            }}
            <button
                type="button"
                class="rounded-full border border-dashed border-slate-300 px-3 py-1 text-sm text-slate-600 hover:border-orange-500 hover:text-orange-600 disabled:opacity-40"
                disabled=move || !enabled.get()
                on:click=move |_| {
                    let mut with_blank = rows.get_untracked();
                    with_blank.push((String::new(), String::new()));
                    rows.set(with_blank);
                }
            >
                {add_label}
            </button>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, value: &str) -> KeyValueRow {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn lines_become_rows_and_back() {
        let draft = "anthropic-beta=tools-2024\nX-Team=a=b";
        let rows = draft_to_rows(draft);
        assert_eq!(
            rows,
            vec![row("anthropic-beta", "tools-2024"), row("X-Team", "a=b")]
        );
        assert_eq!(rows_to_draft(&rows), draft);
    }

    #[test]
    fn a_line_without_a_separator_is_a_name_with_an_empty_value() {
        assert_eq!(draft_to_rows("X-Flag"), vec![row("X-Flag", "")]);
        assert_eq!(rows_to_draft(&[row("X-Flag", "")]), "X-Flag=");
    }

    #[test]
    fn blank_lines_and_rows_without_a_name_are_left_out() {
        assert_eq!(draft_to_rows("\n  \nA=1\n"), vec![row("A", "1")]);
        assert_eq!(
            rows_to_draft(&[row("", "orphan"), row("  ", ""), row("A", "1")]),
            "A=1"
        );
        assert_eq!(rows_to_draft(&[]), "");
    }

    #[test]
    fn names_and_values_are_trimmed_and_a_typed_separator_leaves_the_name() {
        assert_eq!(rows_to_draft(&[row(" A ", " 1 ")]), "A=1");
        assert_eq!(rows_to_draft(&[row("A=B", "1")]), "AB=1");
    }

    #[test]
    fn the_draft_matches_what_the_map_validator_reads() {
        // `lines_to_string_map` is the parser the form uses for the same draft.
        let draft = rows_to_draft(&[row("A", "1"), row("B", "x=y")]);
        let parsed =
            super::super::tool_config_form::lines_to_string_map("Extra headers", &draft).unwrap();
        assert_eq!(parsed, serde_json::json!({"A": "1", "B": "x=y"}));
    }

    #[test]
    fn deleting_and_editing_a_row_leave_the_others_alone() {
        let rows = vec![row("A", "1"), row("B", "2"), row("C", "3")];
        assert_eq!(without_row(&rows, 1), vec![row("A", "1"), row("C", "3")]);
        assert_eq!(without_row(&rows, 9), rows);
        assert_eq!(
            with_row_text(&rows, 1, true, "Z".to_string()),
            vec![row("A", "1"), row("Z", "2"), row("C", "3")]
        );
        assert_eq!(
            with_row_text(&rows, 2, false, "9".to_string())[2],
            row("C", "9")
        );
        assert_eq!(with_row_text(&rows, 7, true, "x".to_string()), rows);
    }
}
