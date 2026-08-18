//! Live tool-invocation chips rendered under a streaming assistant bubble.
//!
//! Fed a plain `Vec<ToolEvent>` snapshot rather than a `Signal` because of
//! how `message_list.rs` renders entries: since `<For>`'s key already
//! changes whenever an entry's `tool_events` changes (see the comment on
//! `entry_render_key`), a brand-new `ToolActivity` (with a fresh snapshot)
//! is created on every update anyway, so there is nothing left for internal
//! signals to buy here.

use chat_ui::models::ToolEvent;
use leptos::prelude::*;

use crate::state::{classify_tool_status, ToolStatusBucket};

/// Tailwind component class for a chip in the given status bucket (see the
/// `.tool-chip--*` rules in `style/input.css`; `--running` carries the
/// pulsing-amber animation). The bucketing heuristic itself lives in
/// `state::classify_tool_status`, shared with that module's own
/// running-chip-cleanup logic rather than duplicated here.
fn chip_class(status: &str) -> &'static str {
    match classify_tool_status(status) {
        ToolStatusBucket::Running => "tool-chip tool-chip--running",
        ToolStatusBucket::Done => "tool-chip tool-chip--done",
        ToolStatusBucket::Failed => "tool-chip tool-chip--failed",
    }
}

/// A single tool-activity chip: the tool's `name`, and its `detail` when
/// the backend supplied one.
#[component]
fn ToolChip(event: ToolEvent) -> impl IntoView {
    let detail_view = event.detail.clone().map(|detail| {
        view! { <span class="tool-chip__detail">{detail}</span> }
    });
    view! {
        <span class=chip_class(&event.status)>
            <span class="tool-chip__name">{event.name.clone()}</span>
            {detail_view}
        </span>
    }
}

/// Renders a turn's live tool-activity chips.
#[component]
pub fn ToolActivity(events: Vec<ToolEvent>) -> impl IntoView {
    view! {
        <div class="mt-2 flex flex-wrap gap-1.5">
            <For each=move || events.clone() key=|event| event.name.clone() let(event)>
                <ToolChip event=event />
            </For>
        </div>
    }
}
