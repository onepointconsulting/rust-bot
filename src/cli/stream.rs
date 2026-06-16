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

/// Rich Live-style streaming renderer for CLI assistant output.
///
/// Deltas arrive pre-filtered (no think tags) from the agent loop.
///
/// Flow per round:
///   spinner -> first visible delta -> header + live redraws ->
///   on_end -> live clears, final render persists on screen
pub struct StreamRenderer {
    render_markdown: bool,
    show_spinner: bool,
    bot_name: String,
    buf: String,
    pub streamed: bool,
    spinner: Option<ThinkingSpinner>,
    live_active: bool,
    live_lines: usize,
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
            live_active: false,
            live_lines: 0,
            header_printed: false,
            is_tty: io::stdout().is_terminal(),
        };
        renderer.start_spinner();
        renderer
    }

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

    fn refresh_live(&mut self) {
        let body = self.render_body();
        let mut out = io::stdout();
        if self.live_lines > 0 {
            let _ = write!(out, "\x1b[{live_lines}A", live_lines = self.live_lines);
            let _ = write!(out, "\x1b[J");
        }
        let _ = write!(out, "{body}");
        let _ = out.flush();
        self.live_lines = body.lines().count().max(1);
    }

    fn stop_live(&mut self) {
        if !self.live_active {
            return;
        }
        if self.live_lines > 0 && self.is_tty {
            let mut out = io::stdout();
            let _ = write!(out, "\x1b[{live_lines}A", live_lines = self.live_lines);
            let _ = write!(out, "\x1b[J");
            let _ = out.flush();
        }
        self.live_lines = 0;
        self.live_active = false;
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
        if self.live_active {
            self.stop_live();
        }
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
        if !self.live_active {
            if self.buf.trim().is_empty() {
                return;
            }
            self.ensure_header();
            self.live_active = true;
        }
        self.refresh_live();
    }

    pub async fn on_end(&mut self, resuming: bool) {
        if self.live_active {
            self.refresh_live();
            self.stop_live();
        }
        self.stop_spinner();
        if !resuming && !self.buf.trim().is_empty() {
            let rendered = self.render_body();
            let mut out = io::stdout();
            let _ = write!(out, "{rendered}");
            let _ = out.flush();
        }
        if resuming {
            self.buf.clear();
            self.start_spinner();
        } else {
            let _ = writeln!(io::stdout());
        }
    }

    /// Stop spinner before user input to avoid prompt conflicts.
    pub fn stop_for_input(&mut self) {
        self.stop_spinner();
    }

    /// Stop spinner/live without rendering a final streamed round.
    pub async fn close(&mut self) {
        if self.live_active {
            self.stop_live();
        }
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
