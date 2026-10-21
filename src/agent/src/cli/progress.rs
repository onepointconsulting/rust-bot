use std::sync::Arc;

use anstyle::Style;
use futures::lock::Mutex;

use crate::agent::agent_loop::ProgressCallback;
use crate::bus::outbound_events::ProgressKind;
use crate::cli::stream::StreamRenderer;
use crate::config::schema::ChannelsConfig;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProgressType {
    Progress,
    ToolHint,
    Image,
    Reasoning,
}

impl ProgressType {
    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::Progress => "↳",
            Self::ToolHint => "⚙",
            Self::Image => "📷",
            Self::Reasoning => "💭",
        }
    }
}

pub(crate) fn print_cli_progress_line(
    renderer: &mut StreamRenderer,
    text: &str,
    progress_type: ProgressType,
) {
    if text.trim().is_empty() {
        return;
    }

    renderer.pause_spinner(|renderer| {
        renderer.ensure_header();
        let dim = Style::new().dimmed();
        if progress_type == ProgressType::ToolHint {
            println!(
                "  {}{}  {text}{}",
                dim.render(),
                progress_type.icon(),
                dim.render_reset()
            );
        } else {
            println!(
                "  {}{} {text}{}",
                dim.render(),
                progress_type.icon(),
                dim.render_reset()
            );
        }
    });
}

pub(crate) fn create_on_progress(
    channels: ChannelsConfig,
    renderer: Arc<Mutex<StreamRenderer>>,
) -> ProgressCallback {
    Arc::new(move |content, kind| {
        let renderer = Arc::clone(&renderer);
        Box::pin(async move {
            let progress_type = match kind {
                ProgressKind::ToolHint => {
                    if !channels.send_tool_hints {
                        return;
                    }
                    ProgressType::ToolHint
                }
                ProgressKind::Plain => {
                    if !channels.send_progress {
                        return;
                    }
                    ProgressType::Progress
                }
                ProgressKind::Reasoning | ProgressKind::ReasoningDelta => {
                    if !channels.show_reasoning {
                        return;
                    }
                    let mut renderer_guard = renderer.lock().await;
                    renderer_guard.write_reasoning_delta(&content);
                    return;
                }
                ProgressKind::ReasoningEnd => {
                    if !channels.show_reasoning {
                        return;
                    }
                    let mut renderer_guard = renderer.lock().await;
                    renderer_guard.finish_reasoning();
                    return;
                }
            };
            let mut renderer_guard = renderer.lock().await;
            print_cli_progress_line(&mut renderer_guard, &content, progress_type);
        })
    })
}
