use std::sync::{Arc, Mutex};

use crossterm::event::Event;
use reedline::{
    EditCommand, EditMode, Emacs, Keybindings, PromptEditMode, ReedlineEvent, ReedlineRawEvent,
};

use crate::utils::clipboard::{format_image_paste_sentinel, format_text_paste_sentinel};

pub fn normalize_paste_text(text: String) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Returns the string to insert into the line buffer for a text paste.
/// Multi-line pastes are stored in `captures` and represented by a numbered sentinel.
pub fn prepare_text_paste_insert(captures: &mut Vec<String>, text: String) -> String {
    let normalized = normalize_paste_text(text);
    let line_count = normalized.lines().count();
    if line_count > 1 {
        let index = captures.len();
        captures.push(normalized);
        format_text_paste_sentinel(index, line_count)
    } else {
        normalized
    }
}

pub fn prepare_image_paste_insert(captures: &mut Vec<String>, image_path: String) -> String {
    let index = captures.len();
    captures.push(image_path);
    format_image_paste_sentinel(index)
}

/// Emacs edit mode that intercepts terminal bracketed paste (`Event::Paste`) and
/// routes it through the same capture/sentinel pipeline as Alt+V.
pub struct PasteCapturingEmacs {
    inner: Emacs,
    text_captures: Arc<Mutex<Vec<String>>>,
}

impl PasteCapturingEmacs {
    pub fn new(keybindings: Keybindings, text_captures: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            inner: Emacs::new(keybindings),
            text_captures,
        }
    }
}

impl EditMode for PasteCapturingEmacs {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        match Event::from(event) {
            Event::Paste(body) => {
                let insert = prepare_text_paste_insert(
                    &mut self.text_captures.lock().expect("text captures lock"),
                    body,
                );
                ReedlineEvent::Edit(vec![EditCommand::InsertString(insert)])
            }
            other => {
                let raw =
                    ReedlineRawEvent::try_from(other).expect("unsupported reedline raw event");
                self.inner.parse_event(raw)
            }
        }
    }

    fn edit_mode(&self) -> PromptEditMode {
        PromptEditMode::Emacs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_text_paste_insert_uses_sentinel_for_multiline() {
        let mut captures = Vec::new();
        let insert = prepare_text_paste_insert(&mut captures, "a\nb".to_string());
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0], "a\nb");
        assert_eq!(insert, format_text_paste_sentinel(0, 2));
    }

    #[test]
    fn prepare_text_paste_insert_inlines_single_line() {
        let mut captures = Vec::new();
        let insert = prepare_text_paste_insert(&mut captures, "hello".to_string());
        assert!(captures.is_empty());
        assert_eq!(insert, "hello");
    }

    #[test]
    fn prepare_image_paste_insert_stores_path_and_returns_sentinel() {
        let mut captures = Vec::new();
        let insert = prepare_image_paste_insert(&mut captures, "/tmp/a.png".to_string());
        assert_eq!(captures, vec!["/tmp/a.png".to_string()]);
        assert_eq!(insert, format_image_paste_sentinel(0));
    }

    #[test]
    fn normalize_paste_text_converts_crlf() {
        assert_eq!(normalize_paste_text("a\r\nb".to_string()), "a\nb");
    }
}
