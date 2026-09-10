//! Syntax highlighting for fenced code blocks in [`crate::markdown`].
//!
//! Token HTML is spliced in *after* ammonia sanitization (same as the
//! code-block chrome), so the `<span class="…">` wrappers never have to be
//! allowlisted. Unknown languages and highlighter failures fall back to
//! escaped plaintext — the same output the renderer used before highlighting.
//!
//! Classes are `syn-` prefixed (`ClassStyle::SpacedPrefixed`). Unprefixed
//! TextMate fragments like `block` collide with Tailwind utilities
//! (`display: block`) and wrap JS braces onto their own lines.

use std::sync::OnceLock;

use syntect::html::{ClassStyle, ClassedHTMLGenerator};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Highlight `body` as `lang`, or HTML-escape it if the language is empty,
/// unknown, or syntect errors.
pub(crate) fn code_inner_html(lang: &str, body: &str) -> String {
    if lang.is_empty() {
        return escape_html(body);
    }
    match highlight(lang, body) {
        Some(html) => html,
        None => escape_html(body),
    }
}

fn highlight(lang: &str, body: &str) -> Option<String> {
    let syntax_set = syntax_set();
    let syntax = syntax_set.find_syntax_by_token(lang)?;
    let mut generator = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        syntax_set,
        ClassStyle::SpacedPrefixed { prefix: "syn-" },
    );
    // `parse_html_for_line_which_includes_newline` requires a trailing `\n`
    // on every line, including the last. Pad a copy when the source doesn't
    // have one, then drop the extra newline from the HTML so Copy stays exact.
    let mut padded;
    let source = if body.ends_with('\n') || body.is_empty() {
        body
    } else {
        padded = String::with_capacity(body.len() + 1);
        padded.push_str(body);
        padded.push('\n');
        padded.as_str()
    };
    for line in LinesWithEndings::from(source) {
        generator
            .parse_html_for_line_which_includes_newline(line)
            .ok()?;
    }
    let mut html = generator.finalize();
    if !body.ends_with('\n') && html.ends_with('\n') {
        html.pop();
    }
    Some(html)
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_keywords_get_token_spans() {
        let html = code_inner_html("rust", "fn main() {}\n");
        assert!(
            html.contains("<span class="),
            "expected syntect token spans, got: {html}"
        );
        assert!(html.contains("fn"), "source text missing, got: {html}");
        assert!(html.contains("main"), "source text missing, got: {html}");
    }

    #[test]
    fn rust_without_trailing_newline_still_highlights() {
        let html = code_inner_html("rust", "fn main() {}");
        assert!(
            html.contains("<span class="),
            "expected syntect token spans, got: {html}"
        );
        assert!(
            !html.ends_with('\n'),
            "must not invent a trailing newline, got: {html:?}"
        );
        assert!(html.contains("fn"), "source text missing, got: {html}");
    }

    #[test]
    fn empty_lang_is_escaped_plaintext() {
        let html = code_inner_html("", "fn main() {}");
        assert!(
            !html.contains("<span"),
            "unlabeled fences must not be highlighted, got: {html}"
        );
        assert_eq!(html, "fn main() {}");
    }

    #[test]
    fn unknown_lang_is_escaped_plaintext() {
        let html = code_inner_html("not-a-real-language", "<script>");
        assert!(!html.contains("<script"), "got: {html}");
        assert!(html.contains("&lt;script&gt;"), "got: {html}");
    }

    #[test]
    fn html_source_stays_escaped() {
        let html = code_inner_html("html", "<script>alert(1)</script>\n");
        assert!(
            !html.contains("<script"),
            "highlighter must still escape tags, got: {html}"
        );
        assert!(html.contains("&lt;"), "got: {html}");
    }

    fn class_names(html: &str) -> Vec<String> {
        html.split("class=\"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .flat_map(|attr| attr.split_whitespace())
            .map(str::to_string)
            .collect()
    }

    fn strip_tags(html: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for c in html.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
    }

    /// TextMate `meta.block.js` used to emit a bare `block` class, which
    /// Tailwind's `display: block` utility then applied to every `{` / `}`.
    #[test]
    fn js_token_classes_are_prefixed_away_from_tailwind() {
        let src = concat!(
            "async function generateImage(prompt, {\n",
            "  size = \"1280x1280\", quality = \"hd\"\n",
            "} = {}) {\n",
            "  return prompt;\n",
            "}\n",
        );
        let html = code_inner_html("js", src);
        let classes = class_names(&html);
        for banned in [
            "block", "inline", "flex", "grid", "hidden", "table", "contents", "absolute",
            "relative", "fixed", "sticky", "static",
        ] {
            assert!(
                !classes.iter().any(|c| c == banned),
                "unprefixed `{banned}` collides with Tailwind, got: {html}"
            );
        }
        assert!(
            classes.iter().any(|c| c.starts_with("syn-")),
            "expected syn- prefix, got: {html}"
        );
        assert_eq!(strip_tags(&html), src, "highlighted text must match source");
    }

    #[test]
    fn rust_token_classes_are_prefixed() {
        let html = code_inner_html("rust", "fn main() {}\n");
        let classes = class_names(&html);
        assert!(
            classes.iter().any(|c| c.starts_with("syn-")),
            "expected syn- prefix, got: {html}"
        );
        assert!(
            !classes.iter().any(|c| c == "block"),
            "unprefixed block class, got: {html}"
        );
    }
}
