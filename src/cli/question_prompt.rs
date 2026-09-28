//! Claude-style multiple-choice picker for the `question` tool.
//!
//! Options are numbered, each description is shown dimmed underneath its
//! label, followed by an inline free-text field and, below a divider, a
//! "Chat about this" row that exits without answering. Moving off the
//! free-text row (↑ or Esc) returns to the options without losing what was
//! typed, so the user can change their mind.

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveToColumn, MoveUp, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Stylize;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};

const POINTER: &str = "❯";
const FREE_TEXT_PLACEHOLDER: &str = "Type something.";
const CHAT_ABOUT_LABEL: &str = "Chat about this";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptChoice {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptOutcome {
    Selected(usize),
    FreeText(String),
    /// The user wants to discuss the question rather than pick an answer.
    ChatAbout,
    Cancelled,
}

/// Run the picker on the current terminal. Blocking: call from
/// `spawn_blocking`, with the Esc-cancel poller paused.
pub fn run_question_prompt(question: &str, choices: &[PromptChoice]) -> io::Result<PromptOutcome> {
    let mut state = PickerState::new(choices.to_vec());
    let mut out = io::stdout();

    terminal::enable_raw_mode()?;
    let _raw = RawGuard;
    execute!(out, Hide)?;

    let mut drawn = 0usize;
    let outcome = loop {
        let width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
        let lines = state.render_lines(question, width);
        redraw(&mut out, drawn, &lines)?;
        drawn = lines.len();

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if let Some(outcome) = state.handle_key(key) {
                    break outcome;
                }
            }
            Event::Paste(text) => state.handle_paste(&text),
            _ => {}
        }
    };

    clear_drawn(&mut out, drawn)?;
    let summary = match &outcome {
        PromptOutcome::Selected(i) => choices[*i].label.clone().cyan().to_string(),
        PromptOutcome::FreeText(text) => text.clone().cyan().to_string(),
        PromptOutcome::ChatAbout => format!("({})", CHAT_ABOUT_LABEL.to_lowercase())
            .dim()
            .to_string(),
        PromptOutcome::Cancelled => "(cancelled)".dim().to_string(),
    };
    write!(out, "{} {} {}\r\n", "?".green(), question.bold(), summary)?;
    out.flush()?;
    Ok(outcome)
}

struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show);
        let _ = terminal::disable_raw_mode();
    }
}

/// Move back to the top of the previously drawn block and clear it.
fn clear_drawn(out: &mut impl Write, drawn: usize) -> io::Result<()> {
    queue!(out, MoveToColumn(0))?;
    if drawn > 1 {
        queue!(out, MoveUp((drawn - 1) as u16))?;
    }
    queue!(out, Clear(ClearType::FromCursorDown))
}

/// Lines are joined with explicit `\r\n` because raw mode on Unix clears
/// `OPOST`, so a bare `\n` would not return to column 0.
fn redraw(out: &mut impl Write, drawn: usize, lines: &[String]) -> io::Result<()> {
    if drawn > 0 {
        clear_drawn(out, drawn)?;
    }
    write!(out, "{}", lines.join("\r\n"))?;
    out.flush()
}

struct PickerState {
    choices: Vec<PromptChoice>,
    /// `0..choices.len()` are options, then the free-text row, then the
    /// "Chat about this" row.
    cursor: usize,
    /// Option to return to when leaving the free-text row.
    last_option: usize,
    buffer: String,
}

impl PickerState {
    fn new(choices: Vec<PromptChoice>) -> Self {
        Self {
            choices,
            cursor: 0,
            last_option: 0,
            buffer: String::new(),
        }
    }

    fn free_text_row(&self) -> usize {
        self.choices.len()
    }

    fn chat_row(&self) -> usize {
        self.choices.len() + 1
    }

    fn on_free_text(&self) -> bool {
        self.cursor == self.free_text_row()
    }

    fn move_to(&mut self, row: usize) {
        if row < self.free_text_row() {
            self.last_option = row;
        }
        self.cursor = row;
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<PromptOutcome> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
            return Some(PromptOutcome::Cancelled);
        }
        let rows = self.chat_row() + 1;

        if self.on_free_text() {
            match key.code {
                KeyCode::Esc | KeyCode::Up => self.move_to(self.last_option),
                KeyCode::Down | KeyCode::Tab => self.move_to(self.chat_row()),
                KeyCode::Enter => {
                    let text = self.buffer.trim();
                    if !text.is_empty() {
                        return Some(PromptOutcome::FreeText(text.to_string()));
                    }
                }
                KeyCode::Backspace => {
                    self.buffer.pop();
                }
                KeyCode::Char('u') if ctrl => self.buffer.clear(),
                KeyCode::Char(c) if !ctrl => self.buffer.push(c),
                _ => {}
            }
            return None;
        }

        match key.code {
            KeyCode::Esc => return Some(PromptOutcome::Cancelled),
            KeyCode::Up | KeyCode::Char('k') => self.move_to((self.cursor + rows - 1) % rows),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.move_to((self.cursor + 1) % rows)
            }
            KeyCode::Enter if self.cursor == self.chat_row() => {
                return Some(PromptOutcome::ChatAbout);
            }
            KeyCode::Enter => return Some(PromptOutcome::Selected(self.cursor)),
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let n = c.to_digit(10).unwrap_or(0) as usize;
                if (1..=self.free_text_row()).contains(&n) {
                    return Some(PromptOutcome::Selected(n - 1));
                }
                if n == self.free_text_row() + 1 {
                    self.move_to(self.free_text_row());
                }
                if n == self.chat_row() + 1 {
                    return Some(PromptOutcome::ChatAbout);
                }
            }
            _ => {}
        }
        None
    }

    fn handle_paste(&mut self, text: &str) {
        if !self.on_free_text() {
            self.move_to(self.free_text_row());
        }
        self.buffer
            .extend(text.chars().filter(|c| !c.is_control()).take(4096));
    }

    fn render_lines(&self, question: &str, width: usize) -> Vec<String> {
        // Stay one column short of the edge so the terminal never auto-wraps,
        // which would break the line count used for redraws.
        let width = width.saturating_sub(1).max(20);
        let mut lines = Vec::new();

        for line in wrap(question, width) {
            lines.push(line.bold().to_string());
        }
        lines.push(String::new());

        let number_width = (self.chat_row() + 1).to_string().len();
        let indent = 2 + number_width + 2;
        let body_width = width.saturating_sub(indent).max(10);

        for (i, choice) in self.choices.iter().enumerate() {
            let selected = i == self.cursor;
            let prefix = row_prefix(i + 1, number_width, selected);
            for (j, part) in wrap(&choice.label, body_width).into_iter().enumerate() {
                let head = if j == 0 { prefix.clone() } else { " ".repeat(indent) };
                let text = if selected {
                    part.cyan().bold().to_string()
                } else {
                    part
                };
                lines.push(format!("{head}{text}"));
            }
            if let Some(desc) = &choice.description {
                for part in wrap(desc, body_width) {
                    lines.push(format!("{}{}", " ".repeat(indent), part.dim()));
                }
            }
        }

        let row = self.free_text_row();
        let selected = self.on_free_text();
        let prefix = row_prefix(row + 1, number_width, selected);
        let field = if self.buffer.is_empty() {
            if selected {
                let mut chars = FREE_TEXT_PLACEHOLDER.chars();
                let first = chars.next().unwrap_or(' ').to_string();
                vec![format!("{}{}", first.reverse(), chars.as_str().dim())]
            } else {
                vec![FREE_TEXT_PLACEHOLDER.dim().to_string()]
            }
        } else {
            let mut parts = wrap_hard(&self.buffer, body_width.saturating_sub(1));
            if selected && let Some(last) = parts.last_mut() {
                last.push_str(&" ".reverse().to_string());
            }
            parts
        };
        for (j, part) in field.into_iter().enumerate() {
            let head = if j == 0 { prefix.clone() } else { " ".repeat(indent) };
            lines.push(format!("{head}{part}"));
        }

        lines.push("─".repeat(width).dim().to_string());
        let chat_selected = self.cursor == self.chat_row();
        let chat_label = if chat_selected {
            CHAT_ABOUT_LABEL.cyan().bold().to_string()
        } else {
            CHAT_ABOUT_LABEL.to_string()
        };
        lines.push(format!(
            "{}{chat_label}",
            row_prefix(self.chat_row() + 1, number_width, chat_selected)
        ));

        lines.push(String::new());
        let hint = if selected {
            "Enter to submit · ↑/Esc to go back to the options"
        } else {
            "Enter or number to select · ↑/↓ to navigate · Esc to cancel"
        };
        lines.push(hint.dim().to_string());
        lines
    }
}

fn row_prefix(number: usize, number_width: usize, selected: bool) -> String {
    let label = format!("{number:>number_width$}.");
    if selected {
        format!("{} {} ", POINTER.cyan(), label.cyan())
    } else {
        format!("  {} ", label.dim())
    }
}

/// Word-wrap on whitespace, hard-breaking words longer than `width`.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut current = String::new();
        let mut current_len = 0;
        for word in paragraph.split_whitespace() {
            let word_len = word.chars().count();
            if current_len > 0 && current_len + 1 + word_len > width {
                lines.push(std::mem::take(&mut current));
                current_len = 0;
            }
            if word_len > width {
                let mut chunks = wrap_hard(word, width);
                let last = chunks.pop().unwrap_or_default();
                if current_len > 0 {
                    lines.push(std::mem::take(&mut current));
                }
                lines.extend(chunks);
                current_len = last.chars().count();
                current = last;
                continue;
            }
            if current_len > 0 {
                current.push(' ');
                current_len += 1;
            }
            current.push_str(word);
            current_len += word_len;
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Break every `width` characters, ignoring word boundaries.
fn wrap_hard(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn state() -> PickerState {
        PickerState::new(vec![
            PromptChoice {
                label: "A".to_string(),
                description: Some("first".to_string()),
            },
            PromptChoice {
                label: "B".to_string(),
                description: None,
            },
        ])
    }

    #[test]
    fn enter_selects_highlighted_option() {
        let mut s = state();
        assert_eq!(s.handle_key(key(KeyCode::Down)), None);
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(PromptOutcome::Selected(1))
        );
    }

    #[test]
    fn number_key_selects_option_directly() {
        let mut s = state();
        assert_eq!(
            s.handle_key(key(KeyCode::Char('2'))),
            Some(PromptOutcome::Selected(1))
        );
    }

    #[test]
    fn number_key_for_free_text_row_focuses_it() {
        let mut s = state();
        assert_eq!(s.handle_key(key(KeyCode::Char('3'))), None);
        assert!(s.on_free_text());
    }

    #[test]
    fn up_wraps_to_chat_row_then_free_text_row() {
        let mut s = state();
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.cursor, s.chat_row());
        s.handle_key(key(KeyCode::Up));
        assert!(s.on_free_text());
    }

    #[test]
    fn enter_on_chat_row_returns_chat_about() {
        let mut s = state();
        s.handle_key(key(KeyCode::Char('3')));
        s.handle_key(key(KeyCode::Down));
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(PromptOutcome::ChatAbout)
        );
    }

    #[test]
    fn number_key_for_chat_row_returns_chat_about() {
        let mut s = state();
        assert_eq!(
            s.handle_key(key(KeyCode::Char('4'))),
            Some(PromptOutcome::ChatAbout)
        );
    }

    #[test]
    fn typing_on_free_text_row_submits_text() {
        let mut s = state();
        s.handle_key(key(KeyCode::Char('3')));
        for c in "hi 2".chars() {
            assert_eq!(s.handle_key(key(KeyCode::Char(c))), None);
        }
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(PromptOutcome::FreeText("hi 2".to_string()))
        );
    }

    #[test]
    fn empty_free_text_enter_is_ignored() {
        let mut s = state();
        s.handle_key(key(KeyCode::Char('3')));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None);
    }

    #[test]
    fn esc_on_free_text_goes_back_to_last_option_keeping_text() {
        let mut s = state();
        s.handle_key(key(KeyCode::Down));
        s.handle_key(key(KeyCode::Down));
        assert!(s.on_free_text());
        s.handle_key(key(KeyCode::Char('x')));
        assert_eq!(s.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(s.cursor, 1);
        assert_eq!(s.buffer, "x");
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(PromptOutcome::Selected(1))
        );
    }

    #[test]
    fn esc_on_options_cancels() {
        let mut s = state();
        assert_eq!(
            s.handle_key(key(KeyCode::Esc)),
            Some(PromptOutcome::Cancelled)
        );
    }

    #[test]
    fn ctrl_c_cancels_even_while_typing() {
        let mut s = state();
        s.handle_key(key(KeyCode::Char('3')));
        assert_eq!(
            s.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(PromptOutcome::Cancelled)
        );
    }

    #[test]
    fn paste_focuses_free_text_row() {
        let mut s = state();
        s.handle_paste("pasted\ntext");
        assert!(s.on_free_text());
        assert_eq!(s.buffer, "pastedtext");
    }

    #[test]
    fn render_numbers_options_and_puts_description_below() {
        let s = state();
        let lines: Vec<String> = s
            .render_lines("Which one?", 80)
            .into_iter()
            .map(|l| strip_ansi(&l))
            .collect();
        assert_eq!(
            lines,
            vec![
                "Which one?",
                "",
                "❯ 1. A",
                "     first",
                "  2. B",
                "  3. Type something.",
                "─".repeat(79).as_str(),
                "  4. Chat about this",
                "",
                "Enter or number to select · ↑/↓ to navigate · Esc to cancel",
            ]
        );
    }

    #[test]
    fn wrap_breaks_on_words_and_long_tokens() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(wrap("", 5), vec![""]);
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
