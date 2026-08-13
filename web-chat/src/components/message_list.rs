use chat_ui::components::MessageBubble;
use chat_ui::models::ChatEntry;
use leptos::html::Div;
use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

fn scroll_list_to_bottom(list_ref: NodeRef<Div>) {
    // Wait one frame so newly rendered bubbles are in the layout.
    if let Some(window) = web_sys::window() {
        let cb = Closure::once(move || {
            if let Some(el) = list_ref.get() {
                el.set_scroll_top(el.scroll_height());
            }
        });
        let _ = window.request_animation_frame(cb.as_ref().unchecked_ref());
        cb.forget();
    }
}

#[component]
pub fn MessageList(
    #[prop(into)] entries: Signal<Vec<ChatEntry>>,
    #[prop(into)] pending: Signal<bool>,
    #[prop(into)] example_prompts: Signal<Vec<String>>,
    on_use_prompt: impl Fn(String) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    let list_ref = NodeRef::<Div>::new();

    // Keep the latest message (and "Thinking..." indicator) in view.
    Effect::new(move |_| {
        let _ = entries.get().len();
        let _ = pending.get();
        scroll_list_to_bottom(list_ref);
    });

    let show_suggestions =
        move || entries.get().is_empty() && !pending.get() && !example_prompts.get().is_empty();

    view! {
        <div
            node_ref=list_ref
            class="flex flex-1 flex-col gap-3 overflow-y-auto px-4 py-4"
        >
            <Show when=show_suggestions>
                <SuggestionPrompts prompts=example_prompts on_use_prompt=on_use_prompt />
            </Show>
            <For
                each=move || entries.get()
                key=|entry| entry.id
                let(entry)
            >
                <MessageBubble entry=entry streaming=Signal::derive(|| false) />
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

/// Clickable example-prompt bubbles shown in an otherwise-empty chat pane.
/// Clicking a prompt fills the composer draft (via `on_use_prompt`) without
/// sending it, letting the user tweak it before submitting.
#[component]
fn SuggestionPrompts(
    #[prop(into)] prompts: Signal<Vec<String>>,
    on_use_prompt: impl Fn(String) + 'static + Send + Sync + Copy,
) -> impl IntoView {
    view! {
        <div class="flex flex-col items-start gap-2">
            <p class="px-1 text-xs font-medium text-slate-400">"Try one of these:"</p>
            <For
                each=move || prompts.get()
                key=|prompt| prompt.clone()
                let(prompt)
            >
                <button
                    type="button"
                    class="max-w-[85%] rounded-2xl border border-dashed border-slate-300 bg-white px-4 py-2 text-left text-sm text-slate-600 shadow-sm transition hover:border-orange-400 hover:bg-orange-50 hover:text-orange-700"
                    on:click=move |_| on_use_prompt(prompt.clone())
                >
                    {prompt.clone()}
                </button>
            </For>
        </div>
    }
}
