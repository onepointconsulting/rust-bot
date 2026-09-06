//! Cursor-style date bucketing for the sessions sidebar
//! (`components::SessionsSidebar`).
//!
//! [`group_sessions`] buckets [`SessionListItem`]s into `Today` / `Yesterday`
//! / `Last7Days` / `Last30Days` / `Older`, preserving each item's relative
//! order within its bucket (the backend already sorts by `updated_at`
//! descending, so within a bucket that means most-recent-first).
//!
//! Buckets are computed on **UTC** calendar days rather than the browser's
//! local timezone: turning an RFC3339 timestamp into a *local* calendar day
//! needs `js_sys::Date`, which only exists under `wasm32-unknown-unknown`
//! and would make this module untestable on the host target. A UTC-day
//! boundary is close enough for a "Today" / "Yesterday" grouping and keeps
//! this file pure, plain, `#[test]`-able Rust with no browser dependency.
//! `now_ms` (Unix milliseconds) is passed in by the caller — in practice
//! `js_sys::Date::now() as i64` from the sidebar component — rather than
//! read here, for the same reason.

use chrono::{DateTime, Utc};

use crate::models::SessionListItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionGroup {
    Today,
    Yesterday,
    Last7Days,
    Last30Days,
    Older,
}

impl SessionGroup {
    pub fn label(&self) -> &'static str {
        match self {
            SessionGroup::Today => "Today",
            SessionGroup::Yesterday => "Yesterday",
            SessionGroup::Last7Days => "Last 7 Days",
            SessionGroup::Last30Days => "Last 30 Days",
            SessionGroup::Older => "Older",
        }
    }
}

/// Fixed display order for the sidebar's group sections.
const GROUP_ORDER: [SessionGroup; 5] = [
    SessionGroup::Today,
    SessionGroup::Yesterday,
    SessionGroup::Last7Days,
    SessionGroup::Last30Days,
    SessionGroup::Older,
];

const MS_PER_DAY: i64 = 86_400_000;

fn utc_day_index(ms: i64) -> i64 {
    ms.div_euclid(MS_PER_DAY)
}

/// Bucket for an item whose `updated_at` is `days_ago` UTC calendar days
/// before today. Negative values (a clock-skewed timestamp in the future)
/// are treated as `Today` rather than panicking or being dropped.
fn bucket_for_days_ago(days_ago: i64) -> SessionGroup {
    match days_ago {
        d if d <= 0 => SessionGroup::Today,
        1 => SessionGroup::Yesterday,
        2..=6 => SessionGroup::Last7Days,
        7..=29 => SessionGroup::Last30Days,
        _ => SessionGroup::Older,
    }
}

/// Parse an RFC3339 timestamp (as produced by `chrono::DateTime::to_rfc3339`
/// server-side) into Unix milliseconds. An unparsable/empty timestamp falls
/// back to `fallback_ms` so a malformed entry lands in `Today` instead of
/// panicking or being silently dropped from the list.
fn parse_ms_or(timestamp: &str, fallback_ms: i64) -> i64 {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
        .unwrap_or(fallback_ms)
}

/// Group `sessions` into the sidebar's fixed date buckets, preserving input
/// order within each bucket. Empty buckets are omitted. `now_ms` is Unix
/// milliseconds for "now" — see the module doc comment for why it's an
/// argument rather than read internally.
pub fn group_sessions(
    sessions: &[SessionListItem],
    now_ms: i64,
) -> Vec<(SessionGroup, Vec<SessionListItem>)> {
    let today_index = utc_day_index(now_ms);
    let mut buckets: [Vec<SessionListItem>; 5] = Default::default();

    for session in sessions {
        let updated_ms = parse_ms_or(&session.updated_at, now_ms);
        let days_ago = today_index - utc_day_index(updated_ms);
        let bucket = bucket_for_days_ago(days_ago);
        let slot = GROUP_ORDER
            .iter()
            .position(|group| *group == bucket)
            .expect("bucket_for_days_ago only returns values present in GROUP_ORDER");
        buckets[slot].push(session.clone());
    }

    GROUP_ORDER
        .into_iter()
        .zip(buckets)
        .filter(|(_, items)| !items.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        "2024-06-15T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .unwrap()
            .timestamp_millis()
    }

    fn item(id: &str, updated_at: &str) -> SessionListItem {
        SessionListItem {
            id: id.to_string(),
            title: format!("chat {id}"),
            created_at: updated_at.to_string(),
            updated_at: updated_at.to_string(),
            has_summary: false,
        }
    }

    #[test]
    fn bucket_boundaries() {
        assert_eq!(bucket_for_days_ago(-1), SessionGroup::Today);
        assert_eq!(bucket_for_days_ago(0), SessionGroup::Today);
        assert_eq!(bucket_for_days_ago(1), SessionGroup::Yesterday);
        assert_eq!(bucket_for_days_ago(2), SessionGroup::Last7Days);
        assert_eq!(bucket_for_days_ago(6), SessionGroup::Last7Days);
        assert_eq!(bucket_for_days_ago(7), SessionGroup::Last30Days);
        assert_eq!(bucket_for_days_ago(29), SessionGroup::Last30Days);
        assert_eq!(bucket_for_days_ago(30), SessionGroup::Older);
        assert_eq!(bucket_for_days_ago(1000), SessionGroup::Older);
    }

    #[test]
    fn groups_by_fixed_order_regardless_of_input_order() {
        let sessions = vec![
            item("older", "2023-01-01T00:00:00Z"),
            item("today", "2024-06-15T08:00:00Z"),
            item("last30", "2024-05-25T12:00:00Z"),
            item("yesterday", "2024-06-14T23:59:00Z"),
            item("last7", "2024-06-10T12:00:00Z"),
        ];

        let grouped = group_sessions(&sessions, now_ms());

        let ids: Vec<(SessionGroup, Vec<String>)> = grouped
            .into_iter()
            .map(|(group, items)| (group, items.into_iter().map(|i| i.id).collect()))
            .collect();

        assert_eq!(
            ids,
            vec![
                (SessionGroup::Today, vec!["today".to_string()]),
                (SessionGroup::Yesterday, vec!["yesterday".to_string()]),
                (SessionGroup::Last7Days, vec!["last7".to_string()]),
                (SessionGroup::Last30Days, vec!["last30".to_string()]),
                (SessionGroup::Older, vec!["older".to_string()]),
            ]
        );
    }

    #[test]
    fn preserves_relative_order_within_a_bucket() {
        let sessions = vec![
            item("a", "2024-06-15T09:00:00Z"),
            item("b", "2024-06-15T08:00:00Z"),
            item("c", "2024-06-15T07:00:00Z"),
        ];

        let grouped = group_sessions(&sessions, now_ms());
        assert_eq!(grouped.len(), 1);
        let (group, items) = &grouped[0];
        assert_eq!(*group, SessionGroup::Today);
        assert_eq!(
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn empty_buckets_are_omitted() {
        let sessions = vec![item("today", "2024-06-15T00:30:00Z")];
        let grouped = group_sessions(&sessions, now_ms());
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0, SessionGroup::Today);
    }

    #[test]
    fn empty_input_produces_no_groups() {
        assert!(group_sessions(&[], now_ms()).is_empty());
    }

    #[test]
    fn unparsable_timestamp_falls_back_to_today() {
        let sessions = vec![item("bad", "not-a-timestamp")];
        let grouped = group_sessions(&sessions, now_ms());
        assert_eq!(grouped, vec![(SessionGroup::Today, sessions)]);
    }

    #[test]
    fn group_labels() {
        assert_eq!(SessionGroup::Today.label(), "Today");
        assert_eq!(SessionGroup::Yesterday.label(), "Yesterday");
        assert_eq!(SessionGroup::Last7Days.label(), "Last 7 Days");
        assert_eq!(SessionGroup::Last30Days.label(), "Last 30 Days");
        assert_eq!(SessionGroup::Older.label(), "Older");
    }
}
