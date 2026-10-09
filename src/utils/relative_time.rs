//! Compact "how long ago" rendering for listings (`/subagents`, `rust-bot subagents`).

use chrono::{DateTime, Utc};

/// Render how long before `now` the instant `then` was, e.g. `just now`,
/// `3m ago`, `5h ago`, `2d ago`. An instant in the future (clock skew) is
/// rendered as `just now`.
pub fn format_relative_time(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let seconds = (now - then).num_seconds();
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

/// Like [`format_relative_time`] for an RFC 3339 timestamp as stored on disk.
/// An unparsable timestamp is returned unchanged, so a listing never hides it.
pub fn format_relative_rfc3339(now: DateTime<Utc>, rfc3339: &str) -> String {
    match rfc3339.parse::<DateTime<Utc>>() {
        Ok(then) => format_relative_time(now, then),
        Err(_) => rfc3339.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn renders_each_unit() {
        let now = Utc::now();
        assert_eq!(
            format_relative_time(now, now - Duration::seconds(5)),
            "just now"
        );
        assert_eq!(
            format_relative_time(now, now - Duration::minutes(3)),
            "3m ago"
        );
        assert_eq!(
            format_relative_time(now, now - Duration::hours(5)),
            "5h ago"
        );
        assert_eq!(format_relative_time(now, now - Duration::days(2)), "2d ago");
    }

    #[test]
    fn future_instant_is_just_now() {
        let now = Utc::now();
        assert_eq!(
            format_relative_time(now, now + Duration::hours(1)),
            "just now"
        );
    }

    #[test]
    fn rfc3339_parses_or_passes_through() {
        let now = Utc::now();
        let stamp = (now - Duration::minutes(10)).to_rfc3339();
        assert_eq!(format_relative_rfc3339(now, &stamp), "10m ago");
        assert_eq!(format_relative_rfc3339(now, "garbage"), "garbage");
    }
}
