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

/// The text appended to the spinner line for the background-agents `banner`:
/// empty without one, otherwise ` · <banner>`.
fn spinner_banner_suffix(banner: Option<&str>) -> String {
    banner.map(|text| format!(" · {text}")).unwrap_or_default()
}

impl ThinkingSpinner {
    pub fn start() -> Self {
        Self::start_with_banner(None)
    }

    /// Start the spinner, showing the background-agents `banner` after the
    /// "thinking" text when there is one.
    pub fn start_with_banner(banner: Option<&str>) -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner:.dim} rust-bot is thinking...{msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        bar.set_message(spinner_banner_suffix(banner));
        bar.enable_steady_tick(Duration::from_millis(80));
        Self { bar, active: true }
    }

    /// Show, change or (with `None`) remove the background-agents banner on the
    /// running spinner line.
    pub fn set_banner(&self, banner: Option<&str>) {
        self.bar.set_message(spinner_banner_suffix(banner));
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
    reasoning_started: bool,
    /// The background-agents banner shown on the spinner line, kept here so a
    /// spinner that is stopped and restarted between rounds shows it again.
    background_banner: Option<String>,
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
            reasoning_started: false,
            background_banner: None,
        };
        renderer.start_spinner();
        renderer
    }

    /// The background-agents banner currently set, if any.
    pub fn background_banner(&self) -> Option<&str> {
        self.background_banner.as_deref()
    }

    /// Show, change or (with `None`) remove the background-agents banner on the
    /// spinner line, now and whenever the spinner is restarted.
    pub fn set_background_banner(&mut self, banner: Option<String>) {
        if let Some(spinner) = &self.spinner {
            spinner.set_banner(banner.as_deref());
        }
        self.background_banner = banner;
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
            self.spinner = Some(ThinkingSpinner::start_with_banner(
                self.background_banner.as_deref(),
            ));
        }
    }

    fn stop_spinner(&mut self) {
        if let Some(mut spinner) = self.spinner.take() {
            spinner.stop();
        }
    }

    /// Append reasoning text in dim italic, without markdown.
    pub fn write_reasoning_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.stop_spinner();
        let mut out = io::stdout();
        if !self.reasoning_started {
            let _ = writeln!(out);
            let prefix = crate::cli::progress::ProgressType::Reasoning.icon();
            if self.is_tty {
                let style = Style::new().dimmed().italic();
                let _ = write!(out, "{}{prefix} {}", style.render(), style.render_reset());
            } else {
                let _ = write!(out, "{prefix} ");
            }
            self.reasoning_started = true;
        }
        if self.is_tty {
            let style = Style::new().dimmed().italic();
            let _ = write!(out, "{}{delta}{}", style.render(), style.render_reset());
        } else {
            let _ = write!(out, "{delta}");
        }
        let _ = out.flush();
    }

    /// End the reasoning line so assistant content can start cleanly.
    pub fn finish_reasoning(&mut self) {
        if !self.reasoning_started {
            return;
        }
        let _ = writeln!(io::stdout());
        let _ = io::stdout().flush();
        self.reasoning_started = false;
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
        let printed = self.streamed && !self.buf.trim().is_empty();
        if printed {
            // Streamed text was emitted this round: stop the spinner and finish
            // the line so the next round (or the prompt) starts cleanly.
            self.stop_spinner();
            let _ = writeln!(io::stdout());
        }
        if resuming {
            // Tool-call round: keep a SINGLE spinner running across the round
            // rather than stopping and recreating it. Recreating churns
            // indicatif's live line and races with its steady-tick thread,
            // which leaves an orphaned "thinking" line behind.
            self.buf.clear();
            self.start_spinner();
        } else {
            self.stop_spinner();
        }
    }

    /// Stop spinner before user input to avoid prompt conflicts.
    pub fn stop_for_input(&mut self) {
        self.stop_spinner();
    }

    /// Restart the thinking spinner after a blocking prompt returns.
    pub fn resume_after_input(&mut self) {
        self.start_spinner();
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

    #[test]
    fn spinner_banner_suffix_is_empty_without_a_banner() {
        assert_eq!(spinner_banner_suffix(None), "");
    }

    #[test]
    fn spinner_banner_suffix_separates_the_banner_from_the_thinking_text() {
        assert_eq!(
            spinner_banner_suffix(Some("Waiting for 1 background agent to finish")),
            " · Waiting for 1 background agent to finish"
        );
    }

    #[test]
    fn background_banner_survives_a_spinner_restart() {
        let mut renderer = StreamRenderer::new(false, true);
        renderer.set_background_banner(Some("Waiting for 2 background agents to finish".into()));
        renderer.stop_for_input();
        renderer.resume_after_input();
        assert_eq!(
            renderer.background_banner.as_deref(),
            Some("Waiting for 2 background agents to finish")
        );
        assert!(renderer.spinner.as_ref().is_some_and(|s| s.is_active()));
    }

    #[test]
    fn clearing_the_background_banner_forgets_it() {
        let mut renderer = StreamRenderer::new(false, true);
        renderer.set_background_banner(Some("Waiting for 1 background agent to finish".into()));
        renderer.set_background_banner(None);
        assert_eq!(renderer.background_banner, None);
    }

    #[test]
    fn stop_for_input_then_resume_restarts_spinner() {
        let mut renderer = StreamRenderer::new(false, true);
        renderer.stop_for_input();
        assert!(renderer.spinner.is_none());
        renderer.resume_after_input();
        assert!(
            renderer.spinner.as_ref().is_some_and(|s| s.is_active()),
            "spinner should resume after a blocking confirmation prompt"
        );
    }
}
