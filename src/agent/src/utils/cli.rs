use std::fmt;

use anstyle::{AnsiColor, Color, Style};
use termimad::MadSkin;

pub fn print_markdown(text: &str) {
    print!("{}", MadSkin::default().term_text(text));
}

pub fn convert_text_to_markdown(text: &str) -> String {
    format!("{}", MadSkin::default().term_text(text))
}

/// Print a warning line to stdout (yellow "Warning:" prefix when the terminal supports color).
pub fn print_warning(message: impl fmt::Display) {
    let style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
    println!(
        "{}Warning:{} {message}",
        style.render(),
        style.render_reset()
    );
}

/// True when the bind address listens on all network interfaces.
pub fn is_all_interfaces_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::")
}
