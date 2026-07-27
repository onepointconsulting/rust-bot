//! Renders assistant Markdown replies as sanitized HTML.
//!
//! The bot's replies are Markdown; user-authored text is shown as plain
//! (escaped) text and never passed through this path.

use pulldown_cmark::{html, Options, Parser};

/// Convert Markdown to sanitized HTML suitable for `inner_html`.
///
/// Ammonia's defaults already strip scripts/event handlers and attach
/// `rel="noopener noreferrer"` to links, which is sufficient protection
/// against untrusted model output.
pub fn render(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);

    let parser = Parser::new_ext(markdown, options);
    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, parser);

    ammonia::clean(&unsafe_html)
}
