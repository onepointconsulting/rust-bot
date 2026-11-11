//! Terminal drawing helpers shared by the inline CLI pickers (the `question`
//! tool prompt and the `agent --resume` session picker).
//!
//! Each picker redraws a block of lines in place: it clears what it drew last
//! time and writes the new frame, staying in raw mode for the duration.

use std::io::{self, Write};

use crossterm::cursor::{MoveToColumn, MoveUp, Show};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};

/// Restores the cursor and leaves raw mode when dropped, so a picker never
/// leaves the terminal broken — even when it returns early with an error.
pub struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show);
        let _ = terminal::disable_raw_mode();
    }
}

/// Move back to the top of the previously drawn block and clear it.
pub fn clear_drawn(out: &mut impl Write, drawn: usize) -> io::Result<()> {
    queue!(out, MoveToColumn(0))?;
    if drawn > 1 {
        queue!(out, MoveUp((drawn - 1) as u16))?;
    }
    queue!(out, Clear(ClearType::FromCursorDown))
}

/// Replace the previously drawn block (`drawn` lines) with `lines`.
///
/// Lines are joined with explicit `\r\n` because raw mode on Unix clears
/// `OPOST`, so a bare `\n` would not return to column 0.
pub fn redraw(out: &mut impl Write, drawn: usize, lines: &[String]) -> io::Result<()> {
    if drawn > 0 {
        clear_drawn(out, drawn)?;
    }
    write!(out, "{}", lines.join("\r\n"))?;
    out.flush()
}

/// Word-wrap on whitespace, hard-breaking words longer than `width`.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
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
pub fn wrap_hard(text: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Remove ANSI escape sequences so rendered lines can be compared in tests.
#[cfg(test)]
pub fn strip_ansi(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_words_and_long_tokens() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(wrap("", 5), vec![""]);
    }

    #[test]
    fn wrap_hard_ignores_word_boundaries() {
        assert_eq!(wrap_hard("ab cd", 2), vec!["ab", " c", "d"]);
        assert_eq!(wrap_hard("", 4), vec![""]);
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        assert_eq!(strip_ansi("\x1b[1mbold\x1b[0m text"), "bold text");
    }
}
