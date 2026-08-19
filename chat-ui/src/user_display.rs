//! Helpers for showing the logged-in user in the chat chrome.

/// First alphanumeric character of the email's local-part, uppercased.
///
/// Used as the avatar glyph in [`crate::components::UserAccountChip`]. The
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

#[cfg(test)]
mod tests {
    use super::email_initial;

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
}
