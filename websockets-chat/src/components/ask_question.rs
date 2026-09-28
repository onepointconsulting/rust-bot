//! Card for a gateway `question_request`: the agent is paused until the user
//! picks one of the offered options, types a free-text answer, or chooses
//! "Chat about this" to discuss the question instead.
//!
//! Laid out like Claude's picker: numbered rows with each description under
//! its label, a "Type something." row that turns into an inline input (Esc,
//! ↑ or "Back" returns to the options, keeping the draft), then a divider and
//! "Chat about this". The card takes focus when it appears so number keys,
//! ↑/↓ and Enter work without clicking.
//!
//! Unlike [`crate::components::ToolApprovalCard`], picking an option *is*
//! the answer — there's no multi-step "Run selected", since a question only
//! ever has one reply. The card stays visible until a `question_resolved`
//! event actually clears it (not on click), same non-optimistic discipline
//! as tool approval.

use leptos::{ev, html, prelude::*};

use crate::state::PendingQuestion;

const FREE_TEXT_PLACEHOLDER: &str = "Type something.";
const CHAT_ABOUT_LABEL: &str = "Chat about this";

/// Result of a key press on the card while no text is being typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyOutcome {
    /// Move the highlight to this row.
    Move(usize),
    /// Act on this row (select an option, open the text box, or chat).
    Activate(usize),
}

/// Rows are `0..option_count` options, then the free-text row, then the
/// "Chat about this" row. `None` means the key isn't handled by the card.
fn card_key_outcome(key: &str, highlight: usize, option_count: usize) -> Option<KeyOutcome> {
    let rows = option_count + 2;
    match key {
        "ArrowDown" => Some(KeyOutcome::Move((highlight + 1) % rows)),
        "ArrowUp" => Some(KeyOutcome::Move((highlight + rows - 1) % rows)),
        "Enter" => Some(KeyOutcome::Activate(highlight.min(rows - 1))),
        _ => {
            let n = key.parse::<usize>().ok().filter(|_| key.len() == 1)?;
            (1..=rows).contains(&n).then(|| KeyOutcome::Activate(n - 1))
        }
    }
}

fn row_class(active: bool) -> &'static str {
    if active {
        "ask-question__option ask-question__option--active"
    } else {
        "ask-question__option"
    }
}

#[component]
pub fn AskQuestionCard(
    #[prop(into)] question: Signal<Option<PendingQuestion>>,
    on_select: Callback<String>,
    on_free_text: Callback<String>,
    on_chat_about: Callback<()>,
) -> impl IntoView {
    let draft = RwSignal::new(String::new());
    let highlight = RwSignal::new(0usize);
    let last_option = RwSignal::new(0usize);
    let typing = RwSignal::new(false);
    let card_ref = NodeRef::<html::Div>::new();
    let input_ref = NodeRef::<html::Input>::new();

    let indexed_options = move || {
        let options = question.get().map(|p| p.options).unwrap_or_default();
        options.into_iter().enumerate().collect::<Vec<_>>()
    };
    let question_text = move || question.get().map(|p| p.question).unwrap_or_default();
    let option_count = move || question.with(|q| q.as_ref().map_or(0, |p| p.options.len()));
    let option_count_untracked =
        move || question.with_untracked(|q| q.as_ref().map_or(0, |p| p.options.len()));

    Effect::new(move |_| {
        question.with(|q| q.as_ref().map(|p| p.request_id.clone()));
        draft.set(String::new());
        highlight.set(0);
        last_option.set(0);
        typing.set(false);
    });
    Effect::new(move |_| {
        if let Some(card) = card_ref.get() {
            let _ = card.focus();
        }
    });
    Effect::new(move |_| {
        let input = input_ref.get();
        if typing.get() {
            if let Some(input) = input {
                let _ = input.focus();
            }
        }
    });

    let focus_card = move || {
        if let Some(card) = card_ref.get_untracked() {
            let _ = card.focus();
        }
    };
    let set_highlight = move |row: usize| {
        if row < option_count_untracked() {
            last_option.set(row);
        }
        highlight.set(row);
    };
    let activate = move |row: usize| {
        let count = option_count_untracked();
        if row < count {
            let id = question.with_untracked(|q| {
                q.as_ref()
                    .and_then(|p| p.options.get(row))
                    .map(|opt| opt.id.clone())
            });
            if let Some(id) = id {
                on_select.run(id);
            }
        } else if row == count {
            set_highlight(row);
            typing.set(true);
        } else {
            on_chat_about.run(());
        }
    };
    let leave_text_box = move |row: usize| {
        typing.set(false);
        set_highlight(row);
        focus_card();
    };
    let submit_free_text = move || {
        let text = draft.get_untracked();
        if !text.trim().is_empty() {
            on_free_text.run(text.trim().to_string());
        }
    };

    let on_card_keydown = move |ev: ev::KeyboardEvent| {
        if typing.get_untracked() {
            return;
        }
        let outcome = card_key_outcome(&ev.key(), highlight.get_untracked(), option_count_untracked());
        match outcome {
            Some(KeyOutcome::Move(row)) => {
                ev.prevent_default();
                set_highlight(row);
            }
            Some(KeyOutcome::Activate(row)) => {
                ev.prevent_default();
                activate(row);
            }
            None => {}
        }
    };
    let on_input_keydown = move |ev: ev::KeyboardEvent| match ev.key().as_str() {
        "Enter" => {
            ev.prevent_default();
            submit_free_text();
        }
        "Escape" | "ArrowUp" => {
            ev.prevent_default();
            leave_text_box(last_option.get_untracked());
        }
        "ArrowDown" => {
            ev.prevent_default();
            leave_text_box(option_count_untracked() + 1);
        }
        _ => {}
    };

    let pointer = move |row: usize| move || if highlight.get() == row { "❯" } else { "" };
    let free_text_row = move || option_count();
    let chat_row = move || option_count() + 1;

    view! {
        <Show when=move || question.get().is_some()>
            <div
                class="ask-question"
                role="region"
                aria-label="Question"
                tabindex="-1"
                node_ref=card_ref
                on:keydown=on_card_keydown
            >
                <p class="ask-question__title">{question_text}</p>
                <ul class="ask-question__list">
                    <For
                        each=indexed_options
                        key=|(_, option)| option.id.clone()
                        children=move |(i, option)| {
                            view! {
                                <li>
                                    <button
                                        type="button"
                                        tabindex="-1"
                                        class=move || row_class(highlight.get() == i)
                                        on:mouseenter=move |_| set_highlight(i)
                                        on:click=move |_| activate(i)
                                    >
                                        <span class="ask-question__pointer">{pointer(i)}</span>
                                        <span class="ask-question__number">{format!("{}.", i + 1)}</span>
                                        <span class="ask-question__option-body">
                                            <span class="ask-question__option-label">{option.label.clone()}</span>
                                            {option.description.clone().map(|desc| view! {
                                                <span class="ask-question__option-description">{desc}</span>
                                            })}
                                        </span>
                                    </button>
                                </li>
                            }
                        }
                    />
                    <li>
                        <Show
                            when=move || typing.get()
                            fallback=move || view! {
                                <button
                                    type="button"
                                    tabindex="-1"
                                    class=move || row_class(highlight.get() == free_text_row())
                                    on:mouseenter=move |_| set_highlight(free_text_row())
                                    on:click=move |_| activate(free_text_row())
                                >
                                    <span class="ask-question__pointer">
                                        {move || pointer(free_text_row())()}
                                    </span>
                                    <span class="ask-question__number">
                                        {move || format!("{}.", free_text_row() + 1)}
                                    </span>
                                    <span class="ask-question__option-body">
                                        {move || {
                                            let text = draft.get();
                                            if text.is_empty() {
                                                view! {
                                                    <span class="ask-question__placeholder">{FREE_TEXT_PLACEHOLDER}</span>
                                                }
                                                    .into_any()
                                            } else {
                                                view! { <span class="ask-question__option-label">{text}</span> }
                                                    .into_any()
                                            }
                                        }}
                                    </span>
                                </button>
                            }
                        >
                            <div class="ask-question__option ask-question__option--active ask-question__free-text">
                                <span class="ask-question__pointer">"❯"</span>
                                <span class="ask-question__number">
                                    {move || format!("{}.", free_text_row() + 1)}
                                </span>
                                <input
                                    type="text"
                                    class="ask-question__free-text-input"
                                    placeholder=FREE_TEXT_PLACEHOLDER
                                    node_ref=input_ref
                                    prop:value=draft
                                    on:input=move |ev| draft.set(event_target_value(&ev))
                                    on:keydown=on_input_keydown
                                />
                                <button
                                    type="button"
                                    class="ask-question__free-text-submit"
                                    on:click=move |_| submit_free_text()
                                >
                                    "Send"
                                </button>
                                <button
                                    type="button"
                                    class="ask-question__back"
                                    title="Back to the options (Esc)"
                                    on:click=move |_| leave_text_box(last_option.get_untracked())
                                >
                                    "Back"
                                </button>
                            </div>
                        </Show>
                    </li>
                </ul>
                <hr class="ask-question__divider" />
                <button
                    type="button"
                    tabindex="-1"
                    class=move || row_class(highlight.get() == chat_row())
                    on:mouseenter=move |_| set_highlight(chat_row())
                    on:click=move |_| activate(chat_row())
                >
                    <span class="ask-question__pointer">{move || pointer(chat_row())()}</span>
                    <span class="ask-question__number">{move || format!("{}.", chat_row() + 1)}</span>
                    <span class="ask-question__option-body">
                        <span class="ask-question__option-label">{CHAT_ABOUT_LABEL}</span>
                    </span>
                </button>
                <p class="ask-question__hint">
                    {move || {
                        if typing.get() {
                            "Enter to send · Esc or ↑ to go back to the options"
                        } else {
                            "Press a number, or ↑/↓ then Enter"
                        }
                    }}
                </p>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_wrap_across_all_rows() {
        assert_eq!(card_key_outcome("ArrowUp", 0, 2), Some(KeyOutcome::Move(3)));
        assert_eq!(card_key_outcome("ArrowDown", 3, 2), Some(KeyOutcome::Move(0)));
    }

    #[test]
    fn enter_activates_highlighted_row() {
        assert_eq!(card_key_outcome("Enter", 1, 2), Some(KeyOutcome::Activate(1)));
    }

    #[test]
    fn number_keys_cover_options_free_text_and_chat_rows() {
        assert_eq!(card_key_outcome("1", 0, 2), Some(KeyOutcome::Activate(0)));
        assert_eq!(card_key_outcome("3", 0, 2), Some(KeyOutcome::Activate(2)));
        assert_eq!(card_key_outcome("4", 0, 2), Some(KeyOutcome::Activate(3)));
        assert_eq!(card_key_outcome("5", 0, 2), None);
        assert_eq!(card_key_outcome("0", 0, 2), None);
    }

    #[test]
    fn other_keys_are_ignored() {
        assert_eq!(card_key_outcome("a", 0, 2), None);
        assert_eq!(card_key_outcome("Escape", 0, 2), None);
    }
}
