//! Searchable session picker for `rust-bot agent --resume`.
//!
//! Lists resumable CLI sessions (title, relative age, key, file size) under a
//! search box. Typing filters the list, Space previews the highlighted
//! session's recent turns, Ctrl+R renames it, Enter resumes it and Esc
//! cancels. [`SessionPickerState`] holds all behaviour and rendering and has
//! no terminal I/O; [`run_session_picker`] is the thin crossterm loop around it.

use std::collections::HashMap;
use std::io::{self, Write};

use chrono::{DateTime, Utc};
use crossterm::cursor::Hide;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::style::Stylize;
use crossterm::terminal;

use crate::cli::session_listing::{ResumableSession, format_size, session_matches_query};
use crate::cli::terminal_draw::{RawGuard, clear_drawn, redraw};
use crate::utils::relative_time::format_relative_time_long;

const PICKER_TITLE: &str = "Resume session";
const POINTER: &str = "❯";
const SEARCH_ICON: &str = "⌕";
const SEARCH_PLACEHOLDER: &str = "Search…";
const RENAME_LABEL: &str = "Rename:";
const NO_MATCHES_TEXT: &str = "No sessions match";
const NO_SESSIONS_TEXT: &str = "No sessions";
const BROWSE_HINT: &str = "Space to preview · Ctrl+R to rename · Type to search · Esc to cancel";
const RENAME_HINT: &str = "Enter to save · Esc to stop renaming";
/// Number of user/assistant turns shown in the inline preview.
pub const PREVIEW_TURN_COUNT: usize = 6;
/// Widest the search box grows, so it stays readable on wide terminals.
const MAX_SEARCH_BOX_WIDTH: usize = 80;
/// Lines above the list: title plus the three-line search box.
const HEADER_LINE_COUNT: usize = 4;
/// Lines below the list: a blank line and the key hints.
const FOOTER_LINE_COUNT: usize = 2;
/// Each session uses a label line and a details line.
const LINES_PER_SESSION: usize = 2;
/// Indent of the preview lines under the highlighted session.
const PREVIEW_INDENT: usize = 4;
/// Narrowest width the picker lays itself out for.
const MIN_RENDER_WIDTH: usize = 20;
/// Fewest terminal rows the picker lays itself out for.
const MIN_RENDER_HEIGHT: usize = 10;

/// What a key press asks the picker loop to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerEvent {
    /// Resume this session.
    Selected(String),
    /// Leave without resuming anything.
    Cancelled,
    /// Persist `title` as the new title of `key`, then call
    /// [`SessionPickerState::apply_rename`].
    RenameRequested { key: String, title: String },
}

/// Final result of [`run_session_picker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPickOutcome {
    Selected(String),
    Cancelled,
}

/// Picker behaviour and rendering, free of terminal I/O.
pub struct SessionPickerState {
    sessions: Vec<ResumableSession>,
    query: String,
    /// Indices into `sessions` that match `query`, in list order.
    filtered_indices: Vec<usize>,
    /// Highlighted position within `filtered_indices`.
    cursor: usize,
    /// First position of `filtered_indices` drawn on screen.
    scroll_offset: usize,
    /// Sessions that fit on screen at the last render; PgUp/PgDn step by this.
    page_size: usize,
    preview_open: bool,
    /// Loaded previews by session key.
    previews: HashMap<String, Vec<(String, String)>>,
    /// `Some` while the search box is a rename field.
    rename_buffer: Option<String>,
    /// One-line message shown above the hints (e.g. a failed rename).
    status_message: Option<String>,
    /// Reference instant for the relative ages.
    now: DateTime<Utc>,
}

impl SessionPickerState {
    /// Picker over `sessions` (already sorted) with the search box holding
    /// `initial_query`; ages are rendered relative to `now`.
    pub fn new(sessions: Vec<ResumableSession>, initial_query: &str, now: DateTime<Utc>) -> Self {
        let mut state = Self {
            sessions,
            query: initial_query.trim().to_string(),
            filtered_indices: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            page_size: 1,
            preview_open: false,
            previews: HashMap::new(),
            rename_buffer: None,
            status_message: None,
            now,
        };
        state.refilter();
        state
    }

    /// The session under the cursor, if any session matches the search.
    pub fn highlighted_session(&self) -> Option<&ResumableSession> {
        self.filtered_indices
            .get(self.cursor)
            .map(|&index| &self.sessions[index])
    }

    /// Key whose preview must be loaded before the next render, if any.
    pub fn pending_preview_key(&self) -> Option<String> {
        if !self.preview_open {
            return None;
        }
        self.highlighted_session()
            .filter(|session| !self.previews.contains_key(&session.key))
            .map(|session| session.key.clone())
    }

    /// Cache the preview turns of `key`.
    pub fn set_preview(&mut self, key: String, turns: Vec<(String, String)>) {
        self.previews.insert(key, turns);
    }

    /// Show `title` for `key` after it was persisted.
    pub fn apply_rename(&mut self, key: &str, title: &str) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.key == key) {
            let title = title.trim();
            session.title = (!title.is_empty()).then(|| title.to_string());
        }
        self.refilter_keeping_highlight(key);
    }

    /// Show a one-line message (cleared by the next key press).
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
    }

    /// Apply one key press; returns an event when the loop must act.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PickerEvent> {
        self.status_message = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
            return Some(PickerEvent::Cancelled);
        }
        if self.rename_buffer.is_some() {
            return self.handle_rename_key(key, ctrl);
        }
        match key.code {
            KeyCode::Esc => return Some(PickerEvent::Cancelled),
            KeyCode::Enter => {
                return self
                    .highlighted_session()
                    .map(|session| PickerEvent::Selected(session.key.clone()));
            }
            KeyCode::Up => self.move_cursor_wrapping(-1),
            KeyCode::Down | KeyCode::Tab => self.move_cursor_wrapping(1),
            KeyCode::PageUp => self.move_cursor_clamped(-(self.page_size as isize)),
            KeyCode::PageDown => self.move_cursor_clamped(self.page_size as isize),
            KeyCode::Home => self.move_cursor_clamped(isize::MIN / 2),
            KeyCode::End => self.move_cursor_clamped(isize::MAX / 2),
            KeyCode::Char('r') if ctrl => self.start_rename(),
            KeyCode::Char('u') if ctrl => self.set_query(String::new()),
            KeyCode::Char(' ') if self.query.is_empty() => {
                self.preview_open = !self.preview_open;
            }
            KeyCode::Char(c) if !ctrl => {
                let mut query = std::mem::take(&mut self.query);
                query.push(c);
                self.set_query(query);
            }
            KeyCode::Backspace => {
                let mut query = std::mem::take(&mut self.query);
                query.pop();
                self.set_query(query);
            }
            _ => {}
        }
        None
    }

    /// Append pasted text to the search box (or the rename field).
    pub fn handle_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).take(512).collect();
        match self.rename_buffer.as_mut() {
            Some(buffer) => buffer.push_str(&clean),
            None => {
                let query = format!("{}{clean}", self.query);
                self.set_query(query);
            }
        }
    }

    /// Lines to draw for a terminal of `width` × `height`. Also scrolls the
    /// list so the highlighted session stays visible.
    pub fn render_lines(&mut self, width: usize, height: usize) -> Vec<String> {
        // Stay one column short of the edge so the terminal never auto-wraps,
        // which would break the line count used for redraws.
        let width = width.saturating_sub(1).max(MIN_RENDER_WIDTH);
        let height = height.max(MIN_RENDER_HEIGHT);

        let mut lines = vec![PICKER_TITLE.bold().to_string()];
        lines.extend(self.render_search_box(width));

        let preview_lines = self.render_preview(width);
        let status_lines = usize::from(self.status_message.is_some());
        // Keep one row free: a block as tall as the terminal scrolls it, and
        // the redraw could no longer move back to its first line.
        let list_budget = height.saturating_sub(
            1 + HEADER_LINE_COUNT + FOOTER_LINE_COUNT + status_lines + preview_lines.len(),
        );
        // `n` sessions take `n * LINES_PER_SESSION` lines plus `n - 1` blanks.
        let visible_count = ((list_budget + 1) / (LINES_PER_SESSION + 1)).max(1);
        self.page_size = visible_count;
        self.keep_cursor_visible(visible_count);

        if self.filtered_indices.is_empty() {
            let message = if self.sessions.is_empty() {
                NO_SESSIONS_TEXT
            } else {
                NO_MATCHES_TEXT
            };
            lines.push(format!("  {}", message.dim()));
        }
        let visible_end = (self.scroll_offset + visible_count).min(self.filtered_indices.len());
        for position in self.scroll_offset..visible_end {
            if position > self.scroll_offset {
                lines.push(String::new());
            }
            let session = &self.sessions[self.filtered_indices[position]];
            let selected = position == self.cursor;
            lines.extend(self.render_session(session, selected, width));
            if selected {
                lines.extend(preview_lines.iter().cloned());
            }
        }

        lines.push(String::new());
        if let Some(message) = &self.status_message {
            lines.push(fit_to_width(message, width).yellow().to_string());
        }
        let hint = if self.rename_buffer.is_some() {
            RENAME_HINT
        } else {
            BROWSE_HINT
        };
        lines.push(fit_to_width(hint, width).dim().to_string());
        lines
    }

    /// One-line summary printed after the picker closes.
    pub fn summary_line(&self, event: &PickerEvent) -> String {
        let summary = match event {
            PickerEvent::Selected(key) => self
                .sessions
                .iter()
                .find(|session| &session.key == key)
                .map_or(key.as_str(), ResumableSession::display_label)
                .to_string()
                .cyan()
                .to_string(),
            _ => "(cancelled)".dim().to_string(),
        };
        format!("{} {} {}", "?".green(), PICKER_TITLE.bold(), summary)
    }

    fn handle_rename_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<PickerEvent> {
        let buffer = self.rename_buffer.as_mut()?;
        match key.code {
            KeyCode::Esc => self.rename_buffer = None,
            KeyCode::Enter => {
                let title = buffer.trim().to_string();
                if title.is_empty() {
                    return None;
                }
                self.rename_buffer = None;
                let key = self.highlighted_session()?.key.clone();
                return Some(PickerEvent::RenameRequested { key, title });
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char('u') if ctrl => buffer.clear(),
            KeyCode::Char(c) if !ctrl => buffer.push(c),
            _ => {}
        }
        None
    }

    fn start_rename(&mut self) {
        if let Some(session) = self.highlighted_session() {
            self.rename_buffer = Some(session.title.clone().unwrap_or_default());
        }
    }

    fn set_query(&mut self, query: String) {
        self.query = query;
        self.refilter();
    }

    /// Recompute the matches and put the cursor back on the first one.
    fn refilter(&mut self) {
        self.filtered_indices = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session_matches_query(session, &self.query))
            .map(|(index, _)| index)
            .collect();
        self.cursor = 0;
        self.scroll_offset = 0;
    }

    /// Recompute the matches but keep `key` highlighted when it still matches.
    fn refilter_keeping_highlight(&mut self, key: &str) {
        let scroll_offset = self.scroll_offset;
        self.refilter();
        if let Some(position) = self
            .filtered_indices
            .iter()
            .position(|&index| self.sessions[index].key == key)
        {
            self.cursor = position;
            self.scroll_offset = scroll_offset.min(position);
        }
    }

    fn move_cursor_wrapping(&mut self, delta: isize) {
        let count = self.filtered_indices.len() as isize;
        if count == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).rem_euclid(count) as usize;
    }

    fn move_cursor_clamped(&mut self, delta: isize) {
        let count = self.filtered_indices.len() as isize;
        if count == 0 {
            return;
        }
        self.cursor = (self.cursor as isize)
            .saturating_add(delta)
            .clamp(0, count - 1) as usize;
    }

    /// Scroll so the cursor lies within the `visible_count` rows on screen.
    fn keep_cursor_visible(&mut self, visible_count: usize) {
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        } else if self.cursor >= self.scroll_offset + visible_count {
            self.scroll_offset = self.cursor + 1 - visible_count;
        }
        let last_full_page = self.filtered_indices.len().saturating_sub(visible_count);
        self.scroll_offset = self.scroll_offset.min(last_full_page);
    }

    /// The three-line search box (or rename field in rename mode).
    fn render_search_box(&self, width: usize) -> Vec<String> {
        let box_width = width.min(MAX_SEARCH_BOX_WIDTH);
        // Two border columns plus one space of padding on each side.
        let inner_width = box_width.saturating_sub(4);
        let content = match &self.rename_buffer {
            Some(buffer) => {
                let label = format!("{RENAME_LABEL} ");
                let field = fit_tail(
                    buffer,
                    inner_width.saturating_sub(label.chars().count() + 1),
                );
                let used = label.chars().count() + field.chars().count() + 1;
                format!(
                    "{}{field}{}{}",
                    label.yellow(),
                    " ".reverse(),
                    " ".repeat(inner_width.saturating_sub(used))
                )
            }
            None if self.query.is_empty() => {
                let text = format!("{SEARCH_ICON} {SEARCH_PLACEHOLDER}");
                let used = text.chars().count();
                format!(
                    "{}{}",
                    text.dim(),
                    " ".repeat(inner_width.saturating_sub(used))
                )
            }
            None => {
                let prefix = format!("{SEARCH_ICON} ");
                let field = fit_tail(
                    &self.query,
                    inner_width.saturating_sub(prefix.chars().count() + 1),
                );
                let used = prefix.chars().count() + field.chars().count() + 1;
                format!(
                    "{prefix}{field}{}{}",
                    " ".reverse(),
                    " ".repeat(inner_width.saturating_sub(used))
                )
            }
        };
        let rule = "─".repeat(box_width.saturating_sub(2));
        vec![
            format!("╭{rule}╮").dim().to_string(),
            format!("{} {content} {}", "│".dim(), "│".dim()),
            format!("╰{rule}╯").dim().to_string(),
        ]
    }

    /// Label line and details line of one session.
    fn render_session(
        &self,
        session: &ResumableSession,
        selected: bool,
        width: usize,
    ) -> [String; 2] {
        let label = fit_to_width(session.display_label(), width.saturating_sub(2));
        let label_line = if selected {
            format!("{} {}", POINTER.cyan(), label.cyan().bold())
        } else {
            format!("  {label}")
        };
        let details = format!(
            "{} · {} · {}",
            format_relative_time_long(self.now, session.updated_at),
            session.key,
            format_size(session.size_bytes)
        );
        let details_line = format!(
            "  {}",
            fit_to_width(&details, width.saturating_sub(2)).dim()
        );
        [label_line, details_line]
    }

    /// Preview lines for the highlighted session (empty when closed).
    fn render_preview(&self, width: usize) -> Vec<String> {
        if !self.preview_open {
            return Vec::new();
        }
        let Some(session) = self.highlighted_session() else {
            return Vec::new();
        };
        let indent = " ".repeat(PREVIEW_INDENT);
        let text_width = width.saturating_sub(PREVIEW_INDENT);
        match self.previews.get(&session.key) {
            None => vec![format!("{indent}{}", "Loading preview…".dim())],
            Some(turns) if turns.is_empty() => {
                vec![format!("{indent}{}", "(no messages)".dim())]
            }
            Some(turns) => turns
                .iter()
                .map(|(role, text)| {
                    let speaker = if role == "user" { "you" } else { "rust-bot" };
                    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    let line = fit_to_width(&format!("{speaker}: {one_line}"), text_width);
                    format!("{indent}{}", line.dim())
                })
                .collect(),
        }
    }
}

/// Cut `text` to at most `width` characters, ending in `…` when cut.
fn fit_to_width(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Keep the last `width` characters of `text`, starting with `…` when cut,
/// so the end of a long search or rename (where the user types) stays visible.
fn fit_tail(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count + 1 - width.max(1)).collect();
    format!("…{kept}")
}

/// Run the picker on the current terminal until the user resumes a session
/// or cancels. Blocking: call from `block_in_place` or `spawn_blocking`.
///
/// `preview_loader` returns the recent turns of a session key (called at most
/// once per key). `renamer` persists a new title and returns a message on
/// failure.
pub fn run_session_picker(
    sessions: Vec<ResumableSession>,
    initial_query: &str,
    mut preview_loader: impl FnMut(&str) -> Vec<(String, String)>,
    mut renamer: impl FnMut(&str, &str) -> Result<(), String>,
) -> io::Result<SessionPickOutcome> {
    let mut state = SessionPickerState::new(sessions, initial_query, Utc::now());
    let mut out = io::stdout();

    terminal::enable_raw_mode()?;
    let _raw = RawGuard;
    execute!(out, Hide)?;

    let mut drawn = 0usize;
    let final_event = loop {
        if let Some(key) = state.pending_preview_key() {
            let turns = preview_loader(&key);
            state.set_preview(key, turns);
        }
        let (width, height) = terminal::size()
            .map(|(w, h)| (w as usize, h as usize))
            .unwrap_or((80, 24));
        let lines = state.render_lines(width, height);
        redraw(&mut out, drawn, &lines)?;
        drawn = lines.len();

        let event = match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => state.handle_key(key),
            Event::Paste(text) => {
                state.handle_paste(&text);
                None
            }
            _ => None,
        };
        match event {
            Some(PickerEvent::RenameRequested { key, title }) => match renamer(&key, &title) {
                Ok(()) => state.apply_rename(&key, &title),
                Err(message) => state.set_status(format!("Rename failed: {message}")),
            },
            Some(done) => break done,
            None => {}
        }
    };

    clear_drawn(&mut out, drawn)?;
    write!(out, "{}\r\n", state.summary_line(&final_event))?;
    out.flush()?;
    Ok(match final_event {
        PickerEvent::Selected(key) => SessionPickOutcome::Selected(key),
        _ => SessionPickOutcome::Cancelled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::terminal_draw::strip_ansi;
    use chrono::Duration;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_text(state: &mut SessionPickerState, text: &str) {
        for c in text.chars() {
            assert_eq!(state.handle_key(key(KeyCode::Char(c))), None);
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-08T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sessions() -> Vec<ResumableSession> {
        vec![
            ResumableSession {
                key: "ems".to_string(),
                title: Some("HMRC R&D evidence report for chatbot engine".to_string()),
                updated_at: now() - Duration::days(8),
                size_bytes: 694_784,
            },
            ResumableSession {
                key: "cli:direct".to_string(),
                title: Some("Rust-bot-side-bar plan review".to_string()),
                updated_at: now() - Duration::days(15),
                size_bytes: 1_258_291,
            },
            ResumableSession {
                key: "research-chat".to_string(),
                title: None,
                updated_at: now() - Duration::minutes(5),
                size_bytes: 300,
            },
        ]
    }

    fn state() -> SessionPickerState {
        SessionPickerState::new(sessions(), "", now())
    }

    fn plain(lines: Vec<String>) -> Vec<String> {
        lines.into_iter().map(|line| strip_ansi(&line)).collect()
    }

    #[test]
    fn arrows_wrap_around_and_page_keys_clamp() {
        let mut s = state();
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.cursor, 2);
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.cursor, 0);
        s.handle_key(key(KeyCode::PageUp));
        assert_eq!(s.cursor, 0);
        s.handle_key(key(KeyCode::PageDown));
        s.handle_key(key(KeyCode::PageDown));
        s.handle_key(key(KeyCode::PageDown));
        s.handle_key(key(KeyCode::PageDown));
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn typing_filters_and_resets_the_cursor() {
        let mut s = state();
        s.handle_key(key(KeyCode::Down));
        type_text(&mut s, "r");
        assert_eq!(s.cursor, 0);
        // "r" matches all three (report, review, research-chat).
        assert_eq!(s.filtered_indices.len(), 3);
        type_text(&mut s, "ev");
        assert_eq!(s.filtered_indices, vec![1]);
        s.handle_key(key(KeyCode::Backspace));
        s.handle_key(key(KeyCode::Backspace));
        assert_eq!(s.query, "r");
        s.handle_key(ctrl('u'));
        assert_eq!(s.query, "");
        assert_eq!(s.filtered_indices.len(), 3);
    }

    #[test]
    fn enter_returns_the_highlighted_filtered_session() {
        let mut s = state();
        type_text(&mut s, "plan");
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(PickerEvent::Selected("cli:direct".to_string()))
        );
    }

    #[test]
    fn enter_with_no_match_does_nothing() {
        let mut s = state();
        type_text(&mut s, "zzz");
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None);
    }

    #[test]
    fn esc_and_ctrl_c_cancel() {
        assert_eq!(
            state().handle_key(key(KeyCode::Esc)),
            Some(PickerEvent::Cancelled)
        );
        assert_eq!(state().handle_key(ctrl('c')), Some(PickerEvent::Cancelled));
        let mut renaming = state();
        renaming.handle_key(ctrl('r'));
        assert_eq!(renaming.handle_key(ctrl('c')), Some(PickerEvent::Cancelled));
    }

    #[test]
    fn space_toggles_preview_only_while_the_search_is_empty() {
        let mut s = state();
        s.handle_key(key(KeyCode::Char(' ')));
        assert!(s.preview_open);
        assert_eq!(s.pending_preview_key(), Some("ems".to_string()));
        s.set_preview("ems".to_string(), vec![]);
        assert_eq!(s.pending_preview_key(), None);
        s.handle_key(key(KeyCode::Char(' ')));
        assert!(!s.preview_open);

        type_text(&mut s, "hmrc evidence");
        assert_eq!(s.query, "hmrc evidence");
        assert!(!s.preview_open);
        assert_eq!(s.filtered_indices, vec![0]);
    }

    #[test]
    fn ctrl_r_renames_the_highlighted_session() {
        let mut s = state();
        s.handle_key(key(KeyCode::Down));
        s.handle_key(ctrl('r'));
        assert_eq!(
            s.rename_buffer.as_deref(),
            Some("Rust-bot-side-bar plan review")
        );
        s.handle_key(ctrl('u'));
        type_text(&mut s, "Sidebar");
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(PickerEvent::RenameRequested {
                key: "cli:direct".to_string(),
                title: "Sidebar".to_string(),
            })
        );
        assert!(s.rename_buffer.is_none());
    }

    #[test]
    fn esc_leaves_rename_mode_without_cancelling() {
        let mut s = state();
        s.handle_key(ctrl('r'));
        type_text(&mut s, " more");
        assert_eq!(s.handle_key(key(KeyCode::Esc)), None);
        assert!(s.rename_buffer.is_none());
        assert_eq!(s.query, "");
        assert_eq!(
            s.sessions[0].title.as_deref(),
            Some("HMRC R&D evidence report for chatbot engine")
        );
    }

    #[test]
    fn empty_rename_is_ignored() {
        let mut s = state();
        s.handle_key(key(KeyCode::Up));
        s.handle_key(ctrl('r'));
        assert_eq!(s.rename_buffer.as_deref(), Some(""));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None);
        assert!(s.rename_buffer.is_some());
    }

    #[test]
    fn apply_rename_updates_the_label_and_keeps_the_highlight() {
        let mut s = state();
        s.handle_key(key(KeyCode::Up));
        s.apply_rename("research-chat", "Research notes");
        assert_eq!(s.sessions[2].display_label(), "Research notes");
        assert_eq!(s.highlighted_session().unwrap().key, "research-chat");
    }

    #[test]
    fn scrolling_keeps_the_cursor_on_screen() {
        let many: Vec<ResumableSession> = (0..20)
            .map(|i| ResumableSession {
                key: format!("session-{i}"),
                title: None,
                updated_at: now(),
                size_bytes: 0,
            })
            .collect();
        let mut s = SessionPickerState::new(many, "", now());
        for _ in 0..15 {
            s.handle_key(key(KeyCode::Down));
            let lines = plain(s.render_lines(80, 20));
            assert!(lines.len() < 20, "picker must leave a free row");
            let pointer_line = format!("❯ session-{}", s.cursor);
            assert!(lines.contains(&pointer_line), "{pointer_line} not drawn");
        }
        s.handle_key(key(KeyCode::Home));
        let lines = plain(s.render_lines(80, 20));
        assert!(lines.contains(&"❯ session-0".to_string()));
    }

    #[test]
    fn render_matches_the_screenshot_layout() {
        let mut s = state();
        let lines = plain(s.render_lines(50, 40));
        let rule = "─".repeat(47);
        let mut expected = vec![
            "Resume session".to_string(),
            format!("╭{rule}╮"),
            format!("│ ⌕ Search…{} │", " ".repeat(36)),
            format!("╰{rule}╯"),
            "❯ HMRC R&D evidence report for chatbot engine".to_string(),
            "  1 week ago · ems · 678.5KB".to_string(),
            String::new(),
            "  Rust-bot-side-bar plan review".to_string(),
            "  2 weeks ago · cli:direct · 1.2MB".to_string(),
            String::new(),
            "  research-chat".to_string(),
            "  5 minutes ago · research-chat · 300B".to_string(),
            String::new(),
        ];
        expected.push(fit_to_width(BROWSE_HINT, 49));
        assert_eq!(lines, expected);
    }

    #[test]
    fn render_shows_the_preview_under_the_highlighted_session() {
        let mut s = state();
        s.handle_key(key(KeyCode::Char(' ')));
        s.set_preview(
            "ems".to_string(),
            vec![
                ("user".to_string(), "write the\nreport".to_string()),
                ("assistant".to_string(), "done".to_string()),
            ],
        );
        let lines = plain(s.render_lines(80, 40));
        assert_eq!(lines[5], "  1 week ago · ems · 678.5KB");
        assert_eq!(lines[6], "    you: write the report");
        assert_eq!(lines[7], "    rust-bot: done");
        assert_eq!(lines[8], "");
    }

    #[test]
    fn render_reports_no_matches_and_no_sessions() {
        let mut s = state();
        type_text(&mut s, "zzz");
        let lines = plain(s.render_lines(80, 40));
        assert!(lines.contains(&"  No sessions match".to_string()));

        let mut empty = SessionPickerState::new(vec![], "", now());
        let lines = plain(empty.render_lines(80, 40));
        assert!(lines.contains(&"  No sessions".to_string()));
    }

    #[test]
    fn render_shows_rename_field_and_hint() {
        let mut s = state();
        s.handle_key(key(KeyCode::Up));
        s.handle_key(ctrl('r'));
        type_text(&mut s, "Notes");
        let lines = plain(s.render_lines(80, 40));
        assert!(lines[2].starts_with("│ Rename: Notes "));
        assert_eq!(lines.last().unwrap(), RENAME_HINT);
    }

    #[test]
    fn initial_query_prefills_the_search() {
        let mut s = SessionPickerState::new(sessions(), " review ", now());
        assert_eq!(s.query, "review");
        assert_eq!(s.filtered_indices, vec![1]);
        let lines = plain(s.render_lines(80, 40));
        assert!(lines[2].starts_with("│ ⌕ review "));
    }

    #[test]
    fn summary_line_names_the_chosen_session() {
        let s = state();
        assert_eq!(
            strip_ansi(&s.summary_line(&PickerEvent::Selected("research-chat".to_string()))),
            "? Resume session research-chat"
        );
        assert_eq!(
            strip_ansi(&s.summary_line(&PickerEvent::Cancelled)),
            "? Resume session (cancelled)"
        );
    }

    #[test]
    fn fit_helpers_cut_with_an_ellipsis() {
        assert_eq!(fit_to_width("abcdef", 4), "abc…");
        assert_eq!(fit_to_width("abc", 4), "abc");
        assert_eq!(fit_tail("abcdef", 4), "…def");
        assert_eq!(fit_tail("abc", 4), "abc");
    }
}
