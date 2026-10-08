//! Keyboard shortcut for toggling the sessions sidebar.
//!
//! The matching/labelling logic is kept as pure functions (no DOM types) so
//! it is unit-testable natively; `chat_shell.rs` feeds them the browser's
//! `KeyboardEvent` fields and `navigator.platform`.

/// `true` when a keydown is the sidebar-toggle shortcut: `Ctrl+B` (Windows /
/// Linux) or `Cmd+B` (macOS), with no Alt or Shift.
///
/// Plain Ctrl/Cmd+B is deliberately used instead of `Ctrl+Alt+B`: Ctrl+Alt is
/// AltGr on Windows and can swallow typed characters on some keyboard
/// layouts. `key` is `KeyboardEvent.key`, compared case-insensitively
/// (Caps Lock reports `"B"`).
pub fn is_sidebar_toggle_shortcut(ctrl: bool, meta: bool, alt: bool, shift: bool, key: &str) -> bool {
    (ctrl || meta) && !alt && !shift && key.eq_ignore_ascii_case("b")
}

/// Human-readable shortcut for tooltips, given `navigator.platform`:
/// `"Cmd+B"` on Apple platforms, `"Ctrl+B"` elsewhere.
pub fn label_for_platform(platform: &str) -> &'static str {
    let platform = platform.to_ascii_lowercase();
    if platform.contains("mac") || platform.contains("iphone") || platform.contains("ipad") {
        "Cmd+B"
    } else {
        "Ctrl+B"
    }
}

/// The shortcut label for the current browser (see [`label_for_platform`]).
pub fn sidebar_shortcut_label() -> &'static str {
    let platform = leptos::prelude::window().navigator().platform().unwrap_or_default();
    label_for_platform(&platform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_b_matches() {
        assert!(is_sidebar_toggle_shortcut(true, false, false, false, "b"));
    }

    #[test]
    fn cmd_b_matches() {
        assert!(is_sidebar_toggle_shortcut(false, true, false, false, "b"));
    }

    #[test]
    fn uppercase_b_matches_for_caps_lock() {
        assert!(is_sidebar_toggle_shortcut(true, false, false, false, "B"));
    }

    #[test]
    fn ctrl_alt_b_does_not_match() {
        assert!(!is_sidebar_toggle_shortcut(true, false, true, false, "b"));
    }

    #[test]
    fn ctrl_shift_b_does_not_match() {
        assert!(!is_sidebar_toggle_shortcut(true, false, false, true, "b"));
    }

    #[test]
    fn plain_b_does_not_match() {
        assert!(!is_sidebar_toggle_shortcut(false, false, false, false, "b"));
    }

    #[test]
    fn other_key_does_not_match() {
        assert!(!is_sidebar_toggle_shortcut(true, false, false, false, "n"));
    }

    #[test]
    fn mac_platforms_use_cmd() {
        assert_eq!(label_for_platform("MacIntel"), "Cmd+B");
        assert_eq!(label_for_platform("iPhone"), "Cmd+B");
    }

    #[test]
    fn other_platforms_use_ctrl() {
        assert_eq!(label_for_platform("Win32"), "Ctrl+B");
        assert_eq!(label_for_platform("Linux x86_64"), "Ctrl+B");
        assert_eq!(label_for_platform(""), "Ctrl+B");
    }
}
