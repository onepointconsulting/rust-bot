//! Card for a gateway `question_request`: the agent is paused until the user
//! picks one of the offered options or types a free-text answer.
//!
//! Unlike [`crate::components::ToolApprovalCard`], picking an option *is*
//! the answer — there's no multi-step "Run selected", since a question only
//! ever has one reply. The card stays visible until a `question_resolved`
//! event actually clears it (not on click), same non-optimistic discipline
//! as tool approval.

use leptos::prelude::*;

use crate::state::PendingQuestion;

#[component]
pub fn AskQuestionCard(
    #[prop(into)] question: Signal<Option<PendingQuestion>>,
    on_select: Callback<String>,
    on_free_text: Callback<String>,
) -> impl IntoView {
    let draft = RwSignal::new(String::new());
    let options = move || question.get().map(|p| p.options).unwrap_or_default();
    let question_text = move || question.get().map(|p| p.question).unwrap_or_default();

    let submit_free_text = move || {
        let text = draft.get_untracked();
        if !text.trim().is_empty() {
            on_free_text.run(text);
            draft.set(String::new());
        }
    };

    view! {
        <Show when=move || question.get().is_some()>
            <div class="ask-question" role="region" aria-label="Question">
                <p class="ask-question__title">{question_text}</p>
                <ul class="ask-question__list">
                    <For
                        each=options
                        key=|option| option.id.clone()
                        children=move |option| {
                            let option_id = option.id.clone();
                            view! {
                                <li>
                                    <button
                                        type="button"
                                        class="ask-question__option"
                                        on:click=move |_| on_select.run(option_id.clone())
                                    >
                                        <span class="ask-question__option-label">{option.label.clone()}</span>
                                        {option.description.clone().map(|desc| view! {
                                            <span class="ask-question__option-description">{desc}</span>
                                        })}
                                    </button>
                                </li>
                            }
                        }
                    />
                </ul>
                <div class="ask-question__free-text">
                    <input
                        type="text"
                        class="ask-question__free-text-input"
                        placeholder="Or type your own answer..."
                        prop:value=draft
                        on:input=move |ev| draft.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" {
                                ev.prevent_default();
                                submit_free_text();
                            }
                        }
                    />
                    <button
                        type="button"
                        class="ask-question__free-text-submit"
                        on:click=move |_| submit_free_text()
                    >
                        "Send"
                    </button>
                </div>
            </div>
        </Show>
    }
}
