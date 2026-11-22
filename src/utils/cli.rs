use std::fmt;

use anstyle::{AnsiColor, Color, Style};
use termimad::MadSkin;

pub fn print_markdown(text: &str) {
    print!("{}", MadSkin::default().term_text(text));
}

pub fn convert_text_to_markdown(text: &str) -> String {
    format!("{}", MadSkin::default().term_text(text))
}

/// `message` with a yellow `Warning:` prefix, shared by [`print_warning`] and
/// [`eprint_warning`] so both look the same.
fn format_warning(message: impl fmt::Display) -> String {
    let style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
    format!(
        "{}Warning:{} {message}",
        style.render(),
        style.render_reset()
    )
}

/// Print a warning line to stdout (yellow "Warning:" prefix when the terminal supports color).
pub fn print_warning(message: impl fmt::Display) {
    println!("{}", format_warning(message));
}

/// Print a warning line to stderr, so piped stdout (for example `agent -m`)
/// stays clean.
pub fn eprint_warning(message: impl fmt::Display) {
    eprintln!("{}", format_warning(message));
}

/// True when the bind address listens on all network interfaces.
pub fn is_all_interfaces_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_text_has_prefix_and_message() {
        let text = format_warning("Gmail tools disabled");
        assert!(text.contains("Warning:"));
        assert!(text.ends_with(" Gmail tools disabled"));
    }
}
