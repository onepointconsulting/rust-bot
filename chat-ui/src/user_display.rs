//! Helpers for showing the logged-in user in the chat chrome.

/// First alphanumeric character of the email's local-part, uppercased.
///
/// Used as the avatar glyph in [`crate::components::UserAccountMenu`]. The
/// registry has no separate display name, so the local-part is the closest
/// stand-in for "the name".
pub fn email_initial(email: &str) -> char {
    email
        .split('@')
        .next()
        .unwrap_or(email)
        .chars()
        .find(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('?')
}

/// The signed-in user's role as shown in the account menu (`"admin"`, `"user"`).
///
/// The gateway sends the role on the websocket `ready` frame. The value is
/// trimmed and lowercased; `None` when it is blank, so the menu hides the row.
pub fn role_label(role: &str) -> Option<String> {
    let trimmed_role = role.trim();
    (!trimmed_role.is_empty()).then(|| trimmed_role.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{email_initial, role_label};

    #[test]
    fn uses_first_alphanumeric_of_local_part() {
        assert_eq!(email_initial("gilfe@onepoint.pt"), 'G');
        assert_eq!(email_initial("alice.bob@example.com"), 'A');
        assert_eq!(email_initial("123@x.com"), '1');
    }

    #[test]
    fn skips_leading_punctuation_in_local_part() {
        assert_eq!(email_initial(".alice@example.com"), 'A');
        assert_eq!(email_initial("_bob@example.com"), 'B');
    }

    #[test]
    fn falls_back_when_there_is_no_usable_character() {
        assert_eq!(email_initial(""), '?');
        assert_eq!(email_initial("@nodomain"), '?');
        assert_eq!(email_initial("...@x.com"), '?');
    }

    #[test]
    fn role_label_returns_the_known_roles() {
        assert_eq!(role_label("admin"), Some("admin".to_string()));
        assert_eq!(role_label("user"), Some("user".to_string()));
    }

    #[test]
    fn role_label_trims_and_lowercases() {
        assert_eq!(role_label("  Admin \n"), Some("admin".to_string()));
        assert_eq!(role_label("USER"), Some("user".to_string()));
    }

    #[test]
    fn role_label_hides_blank_roles() {
        assert_eq!(role_label(""), None);
        assert_eq!(role_label("   \t"), None);
    }
}
