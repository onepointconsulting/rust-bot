use leptos::prelude::*;

use crate::components::MarkdownView;
use crate::models::{ChatEntry, Role};

#[component]
pub fn MessageList(
    #[prop(into)] entries: Signal<Vec<ChatEntry>>,
    #[prop(into)] pending: Signal<bool>,
) -> impl IntoView {
    view! {
        <div class="flex flex-1 flex-col gap-3 overflow-y-auto px-4 py-4">
            <For
                each=move || entries.get()
                key=|entry| entry.id
                let(entry)
            >
                <MessageBubble entry=entry />
            </For>
            <Show when=move || pending.get()>
                <div class="flex justify-start">
                    <div class="rounded-2xl bg-white px-4 py-2 text-sm text-slate-400 shadow-sm">
                        "Thinking..."
                    </div>
                </div>
            </Show>
        </div>
    }
}

#[component]
fn MessageBubble(entry: ChatEntry) -> impl IntoView {
    let is_user = entry.role == Role::User;
    let content = entry.content.clone();

    let bubble_class = if is_user {
        "max-w-[80%] rounded-2xl bg-orange-600 px-4 py-2 text-sm text-white shadow-sm"
    } else {
        "max-w-[80%] rounded-2xl bg-white px-4 py-2 text-slate-800 shadow-sm"
    };
    let row_class = if is_user {
        "flex justify-end"
    } else {
        "flex justify-start"
    };

    view! {
        <div class=row_class>
            <div class=bubble_class>
                {
                    let plain_text = content.clone();
                    view! {
                        <Show
                            when=move || !is_user
                            fallback=move || view! { <p class="whitespace-pre-wrap text-sm">{plain_text.clone()}</p> }
                        >
                            <MarkdownView source=content.clone() />
                        </Show>
                    }
                }
            </div>
        </div>
    }
}
