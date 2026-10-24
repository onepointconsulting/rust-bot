//! The configuration file editor: a plain textarea with JSON syntax colouring.
//!
//! The colouring is a highlighted copy of the text drawn in a `<pre>` *behind* a
//! textarea whose own text is transparent (only the caret and selection show). Both use
//! the same font, padding and line height and never wrap, and the textarea's scroll
//! position is copied to the `<pre>`, so the coloured text lines up under what the user
//! types.
//!
//! The tokenizer is a plain function so it is unit-testable on the host target. It is
//! tolerant: text that is not valid JSON (a half-typed value, a stray character) still
//! gets coloured as far as it makes sense, and anything unrecognised is marked
//! [`JsonTokenKind::Invalid`] instead of failing.

use leptos::html;
use leptos::prelude::*;

/// Above this many bytes the text is shown uncoloured, so typing in a very large file
/// stays responsive.
const MAX_HIGHLIGHTED_BYTES: usize = 256 * 1024;

/// What a piece of the text is, for colouring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonTokenKind {
    /// A string followed by `:`: an object key.
    Key,
    /// Any other string.
    String,
    Number,
    /// `true`, `false` or `null`.
    Keyword,
    /// `{ } [ ] , :`
    Punctuation,
    Whitespace,
    /// Anything that is not valid JSON at that point.
    Invalid,
}

/// A run of text of one kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonToken {
    pub kind: JsonTokenKind,
    pub text: String,
}

/// The Tailwind classes that colour a token kind.
pub fn token_class(kind: JsonTokenKind) -> &'static str {
    match kind {
        JsonTokenKind::Key => "text-sky-700",
        JsonTokenKind::String => "text-emerald-700",
        JsonTokenKind::Number => "text-amber-700",
        JsonTokenKind::Keyword => "text-violet-600",
        JsonTokenKind::Punctuation => "text-slate-500",
        JsonTokenKind::Whitespace => "",
        JsonTokenKind::Invalid => "text-red-600 underline decoration-wavy",
    }
}

/// Byte length of the string literal starting at `start` (which holds the opening
/// quote). A string ends at its closing quote, or at the end of the line when it was
/// never closed.
fn string_length(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return (index + 1).min(bytes.len()) - start,
            b'\n' => return index - start,
            _ => index += 1,
        }
    }
    bytes.len() - start
}

/// Whether the next non-whitespace character at or after `from` is a colon.
fn followed_by_colon(bytes: &[u8], from: usize) -> bool {
    bytes[from.min(bytes.len())..]
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(&b':')
}

/// Byte length of the run starting at `start` whose bytes satisfy `belongs`.
fn run_length(bytes: &[u8], start: usize, belongs: impl Fn(u8) -> bool) -> usize {
    bytes[start..]
        .iter()
        .take_while(|byte| belongs(**byte))
        .count()
}

/// Split `text` into coloured tokens. Concatenating the tokens' text gives `text` back.
pub fn tokenize(text: &str) -> Vec<JsonToken> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let (kind, length) = if byte.is_ascii_whitespace() {
            (
                JsonTokenKind::Whitespace,
                run_length(bytes, index, |b| b.is_ascii_whitespace()),
            )
        } else if byte == b'"' {
            let length = string_length(bytes, index);
            let kind = if followed_by_colon(bytes, index + length) {
                JsonTokenKind::Key
            } else {
                JsonTokenKind::String
            };
            (kind, length)
        } else if matches!(byte, b'{' | b'}' | b'[' | b']' | b',' | b':') {
            (JsonTokenKind::Punctuation, 1)
        } else if byte == b'-' || byte.is_ascii_digit() {
            (
                JsonTokenKind::Number,
                run_length(bytes, index, |b| {
                    b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E')
                }),
            )
        } else if byte.is_ascii_alphabetic() {
            let length = run_length(bytes, index, |b| b.is_ascii_alphabetic());
            let kind = if matches!(&text[index..index + length], "true" | "false" | "null") {
                JsonTokenKind::Keyword
            } else {
                JsonTokenKind::Invalid
            };
            (kind, length)
        } else {
            let character_length = text[index..].chars().next().map_or(1, char::len_utf8);
            (JsonTokenKind::Invalid, character_length)
        };
        tokens.push(JsonToken {
            kind,
            text: text[index..index + length].to_string(),
        });
        index += length;
    }
    tokens
}

/// The JSON editor: edits `draft` in place. Colours it while it is small enough.
#[component]
pub fn JsonEditor(draft: RwSignal<String>, aria_label: &'static str) -> impl IntoView {
    let highlight_ref = NodeRef::<html::Pre>::new();
    let sync_scroll = move |event: leptos::ev::Event| {
        let Some(highlight) = highlight_ref.get() else {
            return;
        };
        let target = event_target::<web_sys::HtmlTextAreaElement>(&event);
        highlight.set_scroll_top(target.scroll_top());
        highlight.set_scroll_left(target.scroll_left());
    };
    // Same box model for both layers, or the colours drift away from the text.
    let layer_class = "absolute inset-0 m-0 p-3 font-mono text-xs leading-5 whitespace-pre";
    view! {
        <div class="relative min-h-0 w-full flex-1 overflow-hidden rounded-lg border border-slate-200 bg-slate-50 focus-within:border-orange-500">
            <pre
                node_ref=highlight_ref
                aria-hidden="true"
                class=format!("{layer_class} pointer-events-none overflow-hidden text-slate-900")
            >
                {move || {
                    let text = draft.get();
                    if text.len() > MAX_HIGHLIGHTED_BYTES {
                        return text.into_any();
                    }
                    // Trailing blank lines keep this layer at least as tall as the
                    // textarea's scroll range (which includes its scrollbar).
                    let mut tokens = tokenize(&text);
                    tokens
                        .push(JsonToken {
                            kind: JsonTokenKind::Whitespace,
                            text: "\n\n\n".to_string(),
                        });
                    tokens
                        .into_iter()
                        .map(|token| {
                            view! { <span class=token_class(token.kind)>{token.text}</span> }
                        })
                        .collect_view()
                        .into_any()
                }}
            </pre>
            <textarea
                aria-label=aria_label
                spellcheck="false"
                wrap="off"
                class=format!(
                    "{layer_class} h-full w-full resize-none overflow-auto bg-transparent text-transparent caret-slate-900 outline-none",
                )
                prop:value=move || draft.get()
                on:input=move |event| draft.set(event_target_value(&event))
                on:scroll=sync_scroll
            ></textarea>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use JsonTokenKind::*;

    fn kinds(text: &str) -> Vec<(JsonTokenKind, std::string::String)> {
        tokenize(text)
            .into_iter()
            .filter(|token| token.kind != Whitespace)
            .map(|token| (token.kind, token.text))
            .collect()
    }

    #[test]
    fn keys_strings_numbers_and_keywords_are_told_apart() {
        let tokens = kinds(r#"{"name": "bot", "n": -1.5e3, "on": true, "x": null}"#);
        assert_eq!(
            tokens,
            vec![
                (Punctuation, "{".to_string()),
                (Key, "\"name\"".to_string()),
                (Punctuation, ":".to_string()),
                (String, "\"bot\"".to_string()),
                (Punctuation, ",".to_string()),
                (Key, "\"n\"".to_string()),
                (Punctuation, ":".to_string()),
                (Number, "-1.5e3".to_string()),
                (Punctuation, ",".to_string()),
                (Key, "\"on\"".to_string()),
                (Punctuation, ":".to_string()),
                (Keyword, "true".to_string()),
                (Punctuation, ",".to_string()),
                (Key, "\"x\"".to_string()),
                (Punctuation, ":".to_string()),
                (Keyword, "null".to_string()),
                (Punctuation, "}".to_string()),
            ]
        );
    }

    #[test]
    fn a_key_may_be_separated_from_its_colon_by_whitespace_and_newlines() {
        assert_eq!(kinds("\"a\"\n  : 1")[0].0, Key);
        assert_eq!(kinds("[\"a\", \"b\"]")[1].0, String);
    }

    #[test]
    fn escaped_quotes_stay_inside_the_string() {
        let tokens = kinds(r#""say \"hi\" now""#);
        assert_eq!(tokens, vec![(String, r#""say \"hi\" now""#.to_string())]);
    }

    #[test]
    fn an_unterminated_string_ends_at_the_end_of_its_line() {
        let tokens = tokenize("\"open\n1");
        assert_eq!(tokens[0].kind, String);
        assert_eq!(tokens[0].text, "\"open");
        assert!(tokens.iter().any(|t| t.kind == Number && t.text == "1"));
    }

    #[test]
    fn unknown_words_and_stray_characters_are_invalid() {
        assert_eq!(kinds("tru")[0].0, Invalid);
        assert_eq!(kinds("@")[0].0, Invalid);
        assert_eq!(kinds("é")[0], (Invalid, "é".to_string()));
    }

    #[test]
    fn tokens_always_reassemble_the_original_text() {
        for text in [
            "",
            "{}",
            "{\n  \"a\": [1, 2, {\"b\": \"é\\u00e9\"}],\r\n\t\"c\": nul\n}",
            "\"unterminated",
            "\"ends with backslash\\",
            "{ \"emoji\": \"🦀\", ¿ }",
        ] {
            let joined: std::string::String = tokenize(text).into_iter().map(|t| t.text).collect();
            assert_eq!(joined, text);
        }
    }

    #[test]
    fn every_coloured_kind_has_a_class_and_whitespace_has_none() {
        for kind in [Key, String, Number, Keyword, Punctuation, Invalid] {
            assert!(token_class(kind).starts_with("text-"));
        }
        assert_eq!(token_class(Whitespace), "");
    }
}
