//! Renders assistant Markdown replies as sanitized HTML.
//!
//! The bot's replies are Markdown; user-authored text is shown as plain
//! (escaped) text and never passed through this path.

use pulldown_cmark::{html, CowStr, Event, Options, Parser};
use pulldown_latex::config::DisplayMode;
use pulldown_latex::{push_mathml, Parser as LatexParser, RenderConfig, Storage};

/// True when `source` would not produce any user-visible output in [`render`].
///
/// Catches empty/whitespace strings, zero-width-only fragments (LLMs
/// sometimes emit U+200B), and markdown that sanitizes down to empty tags
/// (`****`, stripped HTML) — those still fail a naive `trim().is_empty()`
/// check but paint an empty bubble with a copy button.
pub fn is_blank(source: &str) -> bool {
    if !has_visible_chars(source) {
        return true;
    }
    !html_is_visible(&render(source))
}

/// True when `s` contains at least one character that would show up as
/// text, as opposed to whitespace, controls, or zero-width format chars.
pub fn has_visible_chars(s: &str) -> bool {
    s.chars().any(is_visible_char)
}

fn is_visible_char(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !is_zero_width(c)
}

fn is_zero_width(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
    )
}

fn html_is_visible(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    if ["<img", "<pre", "<table", "<math", "<hr", "<video", "<svg"]
        .iter()
        .any(|tag| lower.contains(tag))
    {
        return true;
    }
    has_visible_chars(&strip_tags(html))
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ").replace("&#160;", " ")
}

/// Convert Markdown to sanitized HTML suitable for `inner_html`.
///
/// Ammonia's defaults already strip scripts/event handlers and attach
/// `rel="noopener noreferrer"` to links, which is sufficient protection
/// against untrusted model output.
///
/// TeX math (`$…$`, `$$…$$`, `\(…\)`, `\[…\]`) is turned into MathML and
/// spliced in *after* sanitization so the MathML tag set never has to be
/// allowlisted in ammonia.
pub fn render(markdown: &str) -> String {
    let prepared = rewrite_tex_delimiters(markdown);
    let prepared = normalize_gfm_tables(&prepared);

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    options.insert(Options::ENABLE_MATH);

    let mut math_slots = Vec::new();
    let parser = Parser::new_ext(&prepared, options).map(|event| match event {
        Event::InlineMath(tex) => slot_event(&mut math_slots, &tex, false),
        Event::DisplayMath(tex) => slot_event(&mut math_slots, &tex, true),
        other => other,
    });

    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, parser);

    let mut sanitized = ammonia::clean(&unsafe_html);
    for (index, mathml) in math_slots.iter().enumerate() {
        sanitized = sanitized.replace(&math_placeholder(index), mathml);
    }
    sanitized
}

fn slot_event<'a>(slots: &mut Vec<String>, tex: &str, display: bool) -> Event<'a> {
    let index = slots.len();
    slots.push(latex_to_mathml(tex, display));
    Event::Text(CowStr::from(math_placeholder(index)))
}

fn math_placeholder(index: usize) -> String {
    format!("\u{E000}MATH{index}\u{E001}")
}

fn latex_to_mathml(tex: &str, display: bool) -> String {
    let storage = Storage::new();
    let parser = LatexParser::new(tex, &storage);
    // `DisplayMode::Block` still selects displaystyle (larger ∑/∫, limits
    // above/below). The HTML `display="block"` attribute that pulldown-latex
    // then writes is rewritten below — Chromium + Tailwind Preflight stacks
    // every MathML token vertically when that attribute is present.
    let config = RenderConfig {
        display_mode: if display {
            DisplayMode::Block
        } else {
            DisplayMode::Inline
        },
        ..RenderConfig::default()
    };
    let mut mathml = String::new();
    match push_mathml(&mut mathml, parser, config) {
        Ok(()) if !mathml.is_empty() => {
            let mathml = mathml.replace(r#"display="block""#, r#"display="inline""#);
            if display {
                format!(r#"<span class="math-display">{mathml}</span>"#)
            } else {
                mathml
            }
        }
        _ => format!("<code>{}</code>", escape_html(tex)),
    }
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

/// CommonMark treats `\(` as an escaped parenthesis, so LLM-style TeX
/// delimiters never reach the math parser unless they are rewritten first.
///
/// `\( x \)` becomes `$x$` and `\[ x \]` becomes `$$x$$`. Fenced and inline
/// code are left untouched.
fn rewrite_tex_delimiters(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut at_line_start = true;
    let mut fence: Option<(u8, usize)> = None;

    while i < bytes.len() {
        if let Some((fence_char, fence_len)) = fence {
            if at_line_start {
                if let Some(close_end) = closing_fence_end(bytes, i, fence_char, fence_len) {
                    out.push_str(&input[i..close_end]);
                    i = close_end;
                    fence = None;
                    at_line_start = true;
                    continue;
                }
            }
            let len = utf8_len(bytes, i);
            at_line_start = bytes[i] == b'\n';
            out.push_str(&input[i..i + len]);
            i += len;
            continue;
        }

        if at_line_start {
            if let Some((fence_char, fence_len, open_end)) = opening_fence(bytes, i) {
                out.push_str(&input[i..open_end]);
                i = open_end;
                fence = Some((fence_char, fence_len));
                at_line_start = false;
                continue;
            }
        }

        if bytes[i] == b'`' {
            let (run, end) = take_inline_code(input, i);
            out.push_str(run);
            i = end;
            at_line_start = false;
            continue;
        }

        if bytes[i] == b'\\' && i + 1 < bytes.len() && !is_escaped_backslash(bytes, i) {
            let closer = match bytes[i + 1] {
                b'(' => Some((b')', "$")),
                b'[' => Some((b']', "$$")),
                _ => None,
            };
            if let Some((close_char, delim)) = closer {
                if let Some(inner) = tex_group_inner(bytes, i + 2, close_char) {
                    out.push_str(delim);
                    let trimmed = inner.trim();
                    out.push_str(trimmed);
                    out.push_str(delim);
                    i += 2 + inner.len() + 2;
                    at_line_start = false;
                    continue;
                }
            }
        }

        let len = utf8_len(bytes, i);
        at_line_start = bytes[i] == b'\n';
        out.push_str(&input[i..i + len]);
        i += len;
    }

    out
}

fn utf8_len(bytes: &[u8], i: usize) -> usize {
    match bytes[i] {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

fn is_escaped_backslash(bytes: &[u8], i: usize) -> bool {
    let mut n = 0usize;
    let mut j = i;
    loop {
        if bytes[j] != b'\\' {
            break;
        }
        n += 1;
        if j == 0 {
            break;
        }
        j -= 1;
    }
    n % 2 == 0
}

fn tex_group_inner(bytes: &[u8], inner_start: usize, close_char: u8) -> Option<&str> {
    let mut j = inner_start;
    while j + 1 < bytes.len() {
        if bytes[j] == b'\\' && bytes[j + 1] == close_char && !is_escaped_backslash(bytes, j) {
            return std::str::from_utf8(&bytes[inner_start..j]).ok();
        }
        j += utf8_len(bytes, j);
    }
    None
}

fn opening_fence(bytes: &[u8], mut i: usize) -> Option<(u8, usize, usize)> {
    let start = i;
    let mut indent = 0;
    while i < bytes.len() && bytes[i] == b' ' && indent < 3 {
        i += 1;
        indent += 1;
    }
    if i >= bytes.len() || (bytes[i] != b'`' && bytes[i] != b'~') {
        return None;
    }
    let fence_char = bytes[i];
    let mut fence_len = 0;
    while i < bytes.len() && bytes[i] == fence_char {
        fence_len += 1;
        i += 1;
    }
    if fence_len < 3 {
        return None;
    }
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    if i < bytes.len() {
        i += 1;
    }
    Some((fence_char, fence_len, i.max(start)))
}

fn closing_fence_end(
    bytes: &[u8],
    mut i: usize,
    fence_char: u8,
    fence_len: usize,
) -> Option<usize> {
    let mut indent = 0;
    while i < bytes.len() && bytes[i] == b' ' && indent < 3 {
        i += 1;
        indent += 1;
    }
    let mut close_len = 0;
    while i < bytes.len() && bytes[i] == fence_char {
        close_len += 1;
        i += 1;
    }
    if close_len < fence_len {
        return None;
    }
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i < bytes.len() && bytes[i] != b'\n' {
        return None;
    }
    if i < bytes.len() {
        i += 1;
    }
    Some(i)
}

/// Models often emit a 2+ column header with a 1-column delimiter (`|---|`).
/// GFM requires the delimiter to have at least as many cells as the header,
/// otherwise pulldown-cmark rejects the table and CommonMark joins the rows
/// into one paragraph of raw pipes.
fn normalize_gfm_tables(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    let mut fence: Option<(u8, usize)> = None;

    while i < lines.len() {
        let line = lines[i];
        let bytes = line.as_bytes();

        if let Some((fence_char, fence_len)) = fence {
            if closing_fence_end(bytes, 0, fence_char, fence_len).is_some() {
                fence = None;
            }
            out.push(line.to_string());
            i += 1;
            continue;
        }

        if let Some((fence_char, fence_len, _)) = opening_fence(bytes, 0) {
            fence = Some((fence_char, fence_len));
            out.push(line.to_string());
            i += 1;
            continue;
        }

        if i + 1 < lines.len() {
            if let Some(header_cols) = table_column_count(line).filter(|_| !is_delimiter_row(line))
            {
                if is_delimiter_row(lines[i + 1]) {
                    if let Some(delim_cols) = table_column_count(lines[i + 1]) {
                        if delim_cols < header_cols {
                            out.push(line.to_string());
                            out.push(pad_delimiter_row(lines[i + 1], header_cols));
                            i += 2;
                            continue;
                        }
                    }
                }
            }
        }

        out.push(line.to_string());
        i += 1;
    }

    out.join("\n")
}

fn table_column_count(line: &str) -> Option<usize> {
    table_cells(line).map(|cells| cells.len())
}

fn table_cells(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let mut cells: Vec<&str> = trimmed.split('|').collect();
    if cells.first().is_some_and(|cell| cell.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|cell| cell.trim().is_empty()) {
        cells.pop();
    }
    if cells.is_empty() {
        None
    } else {
        Some(cells)
    }
}

fn is_delimiter_row(line: &str) -> bool {
    table_cells(line).is_some_and(|cells| cells.iter().all(|cell| is_delimiter_cell(cell)))
}

fn is_delimiter_cell(cell: &str) -> bool {
    let trimmed = cell.trim();
    let mut body = trimmed;
    if let Some(rest) = body.strip_prefix(':') {
        body = rest;
    }
    if let Some(rest) = body.strip_suffix(':') {
        body = rest;
    }
    !body.is_empty() && body.chars().all(|ch| ch == '-')
}

fn pad_delimiter_row(line: &str, target_cols: usize) -> String {
    let Some(cells) = table_cells(line) else {
        return line.to_string();
    };
    if cells.len() >= target_cols {
        return line.to_string();
    }

    let leading_ws_len = line.len() - line.trim_start().len();
    let leading_ws = &line[..leading_ws_len];
    let core = line.trim_end();
    let has_trailing_pipe = core.ends_with('|');
    let extra = target_cols - cells.len();

    let mut padded = String::with_capacity(core.len() + extra * 4);
    padded.push_str(leading_ws);
    padded.push_str(core.trim_start());
    for _ in 0..extra {
        if has_trailing_pipe {
            padded.push_str("---|");
        } else {
            padded.push_str("|---");
        }
    }
    padded
}

fn take_inline_code(input: &str, start: usize) -> (&str, usize) {
    let bytes = input.as_bytes();
    let mut ticks = 0;
    while start + ticks < bytes.len() && bytes[start + ticks] == b'`' {
        ticks += 1;
    }
    let mut i = start + ticks;
    while i + ticks <= bytes.len() {
        if bytes[i..i + ticks].iter().all(|&b| b == b'`')
            && (i + ticks == bytes.len() || bytes[i + ticks] != b'`')
        {
            let end = i + ticks;
            return (&input[start..end], end);
        }
        i += 1;
    }
    (&input[start..start + ticks], start + ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_backslash_paren_tex_as_mathml() {
        let html = render(r"Sample a joint state \(\hat x_{\mathrm{rand}}\).");
        assert!(html.contains("<math"), "expected MathML, got: {html}");
        assert!(!html.contains(r"\("), "delimiters should not leak: {html}");
        assert!(
            !html.contains("\\mathrm"),
            "tex source should not leak: {html}"
        );
    }

    #[test]
    fn renders_dollar_tex_as_mathml() {
        let html = render(r"The gain is $m^{|A|}$ samples.");
        assert!(html.contains("<math"), "expected MathML, got: {html}");
        assert!(html.contains("<msup") || html.contains("<sup") || html.contains("mrow"));
    }

    #[test]
    fn renders_display_tex() {
        let html = render(r"See: \[ E = mc^2 \]");
        assert!(html.contains("<math"), "expected MathML, got: {html}");
        assert!(
            html.contains(r#"class="math-display""#),
            "display math should be wrapped for block layout, got: {html}"
        );
        assert!(
            !html.contains(r#"display="block""#) && !html.contains("display='block'"),
            "display=\"block\" stacks tokens in Chromium, got: {html}"
        );
        assert!(
            html.contains(r#"display="inline""#),
            "expected inline math attribute, got: {html}"
        );
    }

    #[test]
    fn leaves_tex_inside_code_fences_literal() {
        let html = render("```\n\\(\\varepsilon\\)\n```");
        assert!(!html.contains("<math"), "got: {html}");
        assert!(
            html.contains(r"\(") || html.contains("\\varepsilon"),
            "got: {html}"
        );
    }

    #[test]
    fn leaves_tex_inside_inline_code_literal() {
        let html = render(r"Use `\(\hat x\)` in the paper.");
        assert!(!html.contains("<math"), "got: {html}");
        assert!(
            html.contains(r"\(") || html.contains("hat x"),
            "got: {html}"
        );
    }

    #[test]
    fn still_renders_regular_markdown() {
        let html = render("**bold** and [a](https://example.com)");
        assert!(html.contains("<strong>bold</strong>"), "got: {html}");
        assert!(html.contains("href=\"https://example.com\""), "got: {html}");
    }

    #[test]
    fn sanitizes_script_tags() {
        let html = render("<script>alert(1)</script>hello");
        assert!(!html.contains("<script"), "got: {html}");
        assert!(html.contains("hello"), "got: {html}");
    }

    #[test]
    fn rewrite_converts_paren_delimiters() {
        let rewritten = rewrite_tex_delimiters(r"path \(\varepsilon\)-optimal");
        assert_eq!(rewritten, r"path $\varepsilon$-optimal");
    }

    #[test]
    fn renders_user_sample_formulas() {
        let html = render(
            r#"1. Sample a joint state \(\hat x_{\mathrm{rand}}\).
2. Independent blocks \(X^{A(\hat x_{\mathrm{rand}})}_{\mathrm{rand}}\).
3. An \((\varepsilon)\)-optimal path, \( |A| \cdot m \), and \(m^{|A|}\)."#,
        );
        assert_eq!(
            html.matches("<math").count(),
            5,
            "expected five formulas, got: {html}"
        );
    }

    #[test]
    fn renders_well_formed_gfm_table() {
        let html = render("| Paper | Why |\n| --- | --- |\n| **A-MEM** | Named the field. |\n");
        assert!(html.contains("<table"), "expected table, got: {html}");
        assert!(html.contains("<th>"), "got: {html}");
        assert!(html.contains("<td>"), "got: {html}");
        assert!(html.contains("<strong>A-MEM</strong>"), "got: {html}");
        assert!(!html.contains("| ---"), "raw pipes should not leak: {html}");
    }

    #[test]
    fn renders_llm_table_with_short_delimiter() {
        let html = render(
            "| Paper | Why it matters |\n|---|\n| **[A-MEM](https://arxiv.org/abs/2502.1210)** | Named the field. |\n",
        );
        assert!(html.contains("<table"), "expected table, got: {html}");
        assert!(html.contains("Why it matters"), "got: {html}");
        assert!(html.contains("A-MEM"), "got: {html}");
        assert!(
            html.contains("href=\"https://arxiv.org/abs/2502.1210\""),
            "got: {html}"
        );
        assert!(
            !html.contains("|---"),
            "raw delimiter should not leak: {html}"
        );
    }

    #[test]
    fn pads_short_delimiter_to_header_width() {
        let rewritten =
            normalize_gfm_tables("| Paper | Why it matters |\n|---|\n| A-MEM | Named it. |");
        assert_eq!(
            rewritten,
            "| Paper | Why it matters |\n|---|---|\n| A-MEM | Named it. |"
        );
    }

    #[test]
    fn leaves_table_inside_code_fence_literal() {
        let html = render("```\n| Paper | Why |\n|---|\n| A | B |\n```");
        assert!(!html.contains("<table"), "got: {html}");
        assert!(
            html.contains("| Paper |") || html.contains("| Paper | Why |"),
            "got: {html}"
        );
    }

    #[test]
    fn is_blank_treats_empty_whitespace_and_zero_width_as_blank() {
        assert!(is_blank(""));
        assert!(is_blank("   \n\t  "));
        assert!(is_blank("\u{200B}\u{FEFF}"));
        assert!(is_blank("&nbsp;&nbsp;"));
        assert!(is_blank("<script>alert(1)</script>"));
    }

    #[test]
    fn is_blank_keeps_real_markdown_and_media() {
        assert!(!is_blank("hello"));
        assert!(!is_blank("**bold**"));
        assert!(!is_blank("![](https://example.com/a.png)"));
        assert!(!is_blank("| A | B |\n| --- | --- |\n| 1 | 2 |"));
    }
}
