use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use anstyle::{AnsiColor, Color, Style};
use futures::lock::Mutex;
use indicatif::{ProgressBar, ProgressStyle};
use termimad::MadSkin;

use crate::agent::agent_loop::{StreamCallback, StreamEndCallback};

const BOT_NAME: &str = "rust-bot";

pub struct ThinkingSpinner {
    bar: ProgressBar,
    active: bool,
}

impl ThinkingSpinner {
    pub fn start() -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner:.dim} rust-bot is thinking...")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        bar.enable_steady_tick(Duration::from_millis(80));
        Self { bar, active: true }
    }

    pub fn stop(&mut self) {
        if self.active {
            self.bar.finish_and_clear();
            self.active = false;
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Run `f` while the spinner is hidden (like Rich's pause context manager).
    pub fn pause<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.bar.suspend(f)
    }
}

/// Streaming renderer for CLI assistant output.
///
/// Deltas arrive pre-filtered (no think tags) from the agent loop.
///
/// Flow per round:
///   spinner -> first visible delta -> header printed once -> deltas appended
///   inline as they arrive -> on_end finishes the line.
///
/// Output is append-only: each delta is written immediately and never
/// redrawn. This avoids any cursor-positioning that would be unreliable once
/// the output scrolls the viewport (which previously caused duplicated lines).
pub struct StreamRenderer {
    render_markdown: bool,
    show_spinner: bool,
    bot_name: String,
    buf: String,
    pub streamed: bool,
    spinner: Option<ThinkingSpinner>,
    pub header_printed: bool,
    is_tty: bool,
}

impl StreamRenderer {
    pub fn new(render_markdown: bool, show_spinner: bool) -> Self {
        Self::with_bot_name(render_markdown, show_spinner, BOT_NAME)
    }

    pub fn with_bot_name(
        render_markdown: bool,
        show_spinner: bool,
        bot_name: impl Into<String>,
    ) -> Self {
        let mut renderer = Self {
            render_markdown,
            show_spinner,
            bot_name: bot_name.into(),
            buf: String::new(),
            streamed: false,
            spinner: None,
            header_printed: false,
            is_tty: io::stdout().is_terminal(),
        };
        renderer.start_spinner();
        renderer
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn render_body(&self) -> String {
        if self.render_markdown && !self.buf.is_empty() {
            format!("{}", MadSkin::default().term_text(&self.buf))
        } else {
            self.buf.clone()
        }
    }

    fn start_spinner(&mut self) {
        if self.show_spinner && self.spinner.is_none() {
            self.spinner = Some(ThinkingSpinner::start());
        }
    }

    fn stop_spinner(&mut self) {
        if let Some(mut spinner) = self.spinner.take() {
            spinner.stop();
        }
    }

    fn write_delta(&self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let mut out = io::stdout();
        let _ = write!(out, "{delta}");
        let _ = out.flush();
    }

    /// Stop transient status and print the assistant header once.
    pub fn ensure_header(&mut self) {
        self.stop_spinner();
        if self.header_printed {
            return;
        }
        let mut out = io::stdout();
        let _ = writeln!(out);
        if self.is_tty {
            let header = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan)));
            let _ = writeln!(
                out,
                "{}{bot_name}{}",
                header.render(),
                header.render_reset(),
                bot_name = self.bot_name
            );
        } else {
            let _ = writeln!(out, "{}", self.bot_name);
        }
        let _ = out.flush();
        self.header_printed = true;
    }

    /// Temporarily stop transient output for clean trace/progress lines.
    pub fn pause_spinner<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        let spinner = self.spinner.take();
        let result = if let Some(ref spinner) = spinner {
            spinner.pause(|| f(self))
        } else {
            f(self)
        };
        self.spinner = spinner;
        result
    }

    pub async fn on_delta(&mut self, delta: String) {
        self.streamed = true;
        self.buf.push_str(&delta);
        // Don't print until there is some non-whitespace content, so the header
        // isn't emitted ahead of leading blank deltas.
        if self.buf.trim().is_empty() {
            return;
        }
        self.ensure_header();
        self.write_delta(&delta);
    }

    pub async fn on_end(&mut self, resuming: bool) {
        self.stop_spinner();
        if resuming {
            // Tool-call round: keep what was streamed, separate it from the next
            // round, and resume the spinner while the agent keeps working.
            if self.streamed && !self.buf.trim().is_empty() {
                let _ = writeln!(io::stdout());
            }
            self.buf.clear();
            self.start_spinner();
        } else if self.streamed && !self.buf.trim().is_empty() {
            // Final round: finish the streamed line.
            let _ = writeln!(io::stdout());
        }
    }

    /// Stop spinner before user input to avoid prompt conflicts.
    pub fn stop_for_input(&mut self) {
        self.stop_spinner();
    }

    /// Stop spinner without rendering a final streamed round.
    pub async fn close(&mut self) {
        self.stop_spinner();
    }
}

pub fn stream_callbacks(
    renderer: Arc<Mutex<StreamRenderer>>,
) -> (StreamCallback, StreamEndCallback) {
    let on_stream: StreamCallback = {
        let renderer = Arc::clone(&renderer);
        Arc::new(move |delta| {
            let renderer = Arc::clone(&renderer);
            Box::pin(async move {
                renderer.lock().await.on_delta(delta).await;
            })
        })
    };
    let on_stream_end: StreamEndCallback = {
        let renderer = Arc::clone(&renderer);
        Arc::new(move |resuming| {
            let renderer = Arc::clone(&renderer);
            Box::pin(async move {
                renderer.lock().await.on_end(resuming).await;
            })
        })
    };
    (on_stream, on_stream_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_renderer_plain_text_mode() {
        let renderer = StreamRenderer::new(false, false);
        assert_eq!(renderer.render_body(), "");
    }

    #[test]
    fn stream_renderer_renders_markdown_buffer() {
        let mut renderer = StreamRenderer::new(true, false);
        renderer.buf = "**hi**".to_string();
        assert!(renderer.render_body().contains("hi"));
    }

    #[tokio::test]
    async fn on_end_resuming_restarts_thinking_spinner() {
        let mut renderer = StreamRenderer::new(false, true);
        assert!(
            renderer.spinner.as_ref().is_some_and(|s| s.is_active()),
            "spinner should start with the renderer"
        );

        renderer.on_end(true).await;

        assert!(
            renderer.spinner.as_ref().is_some_and(|s| s.is_active()),
            "spinner should restart while the agent continues after a tool-call round"
        );
    }
}
