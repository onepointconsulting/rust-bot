//! "How long ago" rendering for listings: compact (`/subagents`,
//! `rust-bot subagents`) and spelled out (`rust-bot agent --resume`).

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

/// Spelled-out variant of [`format_relative_time`], e.g. `just now`,
/// `1 minute ago`, `3 hours ago`, `2 weeks ago`, `1 year ago`. Weeks start at
/// 7 days, months at 30 days and years at 365 days. An instant in the future
/// (clock skew) is rendered as `just now`.
pub fn format_relative_time_long(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    let seconds = (now - then).num_seconds();
    let (count, unit) = if seconds < MINUTE {
        return "just now".to_string();
    } else if seconds < HOUR {
        (seconds / MINUTE, "minute")
    } else if seconds < DAY {
        (seconds / HOUR, "hour")
    } else if seconds < WEEK {
        (seconds / DAY, "day")
    } else if seconds < MONTH {
        (seconds / WEEK, "week")
    } else if seconds < YEAR {
        (seconds / MONTH, "month")
    } else {
        (seconds / YEAR, "year")
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
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

    #[test]
    fn long_form_spells_out_units_and_plurals() {
        let now = Utc::now();
        let ago = |d: Duration| format_relative_time_long(now, now - d);
        assert_eq!(ago(Duration::seconds(59)), "just now");
        assert_eq!(ago(Duration::seconds(60)), "1 minute ago");
        assert_eq!(ago(Duration::minutes(59)), "59 minutes ago");
        assert_eq!(ago(Duration::minutes(60)), "1 hour ago");
        assert_eq!(ago(Duration::hours(23)), "23 hours ago");
        assert_eq!(ago(Duration::hours(24)), "1 day ago");
        assert_eq!(ago(Duration::days(6)), "6 days ago");
        assert_eq!(ago(Duration::days(7)), "1 week ago");
        assert_eq!(ago(Duration::days(29)), "4 weeks ago");
        assert_eq!(ago(Duration::days(30)), "1 month ago");
        assert_eq!(ago(Duration::days(364)), "12 months ago");
        assert_eq!(ago(Duration::days(365)), "1 year ago");
        assert_eq!(ago(Duration::days(800)), "2 years ago");
        assert_eq!(
            format_relative_time_long(now, now + Duration::hours(1)),
            "just now"
        );
    }
}
