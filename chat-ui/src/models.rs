use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A file attached to an outgoing (or previously sent) message.
///
/// `url` is a gateway `/v1/media/...` path, an `http(s)://` reference, or a
/// `data:image/...;base64,...` URL produced client-side from a
/// picked/dropped/pasted file. `label` is the display filename when known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub url: String,
    #[serde(default)]
    pub label: Option<String>,
}

/// A single tool invocation's lifecycle, as surfaced by the gateway's live
/// progress stream.
///
/// Mirrors the backend's `ToolEvent` shape (`src/bus/outbound_events.rs`) so
/// `websockets-chat` can deserialize gateway events directly into this type;
/// `chat-ui` has no dependency on the backend crate itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolEvent {
    pub name: String,
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatEntry {
    pub id: u64,
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<ImageAttachment>,
    /// True while an assistant reply is still streaming in (websockets-chat
    /// only; web-chat never sets this).
    #[serde(default)]
    pub streaming: bool,
    /// Live tool-activity chips for this entry (websockets-chat only).
    #[serde(default)]
    pub tool_events: Option<Vec<ToolEvent>>,
    /// Streamed reasoning/thinking text for this entry (websockets-chat only).
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Stable identity of the user who sent this row (JWT `sub`, currently
    /// the account email). `None` for assistant rows, guests, and history
    /// that predates identity stamping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// RFC3339 timestamp of when this row was sent or produced. `None` for
    /// history that predates stamping, and for host-side test fixtures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// Session lifetime token/cost totals, as surfaced by the gateway on
/// `attached` and `session_updated`.
///
/// Mirrors the backend's `LLMUsage` shape (`src/providers/base.rs`) field for
/// field — `chat-ui` has no dependency on the backend crate itself, so this
/// is a plain re-declaration rather than a shared type. `None` means the
/// provider/session never reported that field, distinct from a real zero.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionTokenUsage {
    #[serde(default)]
    pub input_tokens: Option<u32>,
    #[serde(default)]
    pub output_tokens: Option<u32>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub input_cost: Option<f64>,
    #[serde(default)]
    pub output_cost: Option<f64>,
}

impl SessionTokenUsage {
    /// Incoming tokens billed as prompt: uncached input plus cache write/read.
    /// `None` when `input_tokens` itself was never reported.
    pub fn prompt_tokens(&self) -> Option<u32> {
        Some(
            self.input_tokens?
                .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
                .saturating_add(self.cache_read_input_tokens.unwrap_or(0)),
        )
    }

    /// Prompt plus output. Reasoning is not added; it is already inside output.
    pub fn total_tokens(&self) -> Option<u32> {
        Some(self.prompt_tokens()?.saturating_add(self.output_tokens?))
    }

    pub fn total_cost(&self) -> Option<f64> {
        match (self.input_cost, self.output_cost) {
            (None, None) => None,
            (input, output) => Some(input.unwrap_or(0.0) + output.unwrap_or(0.0)),
        }
    }

    /// Whether there is anything at all to show — an all-`None` blob (e.g. a
    /// brand new chat) should hide the chip entirely rather than render "0".
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One discovered skill, as surfaced by the gateway's `skills` event.
///
/// Mirrors the backend's `SkillSummary` (`src/agent/skills.rs`) — `chat-ui`
/// has no dependency on the backend crate itself, so this is a plain
/// re-declaration rather than a shared type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Current UTC time as RFC3339 (`Date.toISOString()`). Used to stamp live
/// [`ChatEntry`]s in the browser; not called from host `#[test]`s, where
/// `js_sys::Date` has no runtime.
pub fn now_rfc3339() -> String {
    js_sys::Date::new_0().to_iso_string().into()
}

/// Format an RFC3339 timestamp for the message action row, in the browser's
/// local timezone: `5th Sep 2026, 5:16 PM`. `None` when the string isn't a
/// parseable RFC3339 timestamp.
pub fn format_message_time(rfc3339: &str) -> Option<String> {
    let dt = chrono::DateTime::parse_from_rfc3339(rfc3339).ok()?;
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(
        dt.timestamp_millis() as f64
    ));
    Some(format_message_time_parts(
        date.get_full_year() as i32,
        date.get_month() + 1,
        date.get_date(),
        date.get_hours(),
        date.get_minutes(),
    ))
}

const MONTH_ABBREVS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn day_ordinal_suffix(day: u32) -> &'static str {
    match day {
        11 | 12 | 13 => "th",
        _ => match day % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    }
}

/// `5th Sep 2026, 5:16 PM` from local civil-time parts. `month` is 1–12;
/// `hour` is 0–23. Out-of-range month falls back to `Jan`.
pub fn format_message_time_parts(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> String {
    let month_name = MONTH_ABBREVS
        .get(month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("Jan");
    let suffix = day_ordinal_suffix(day);
    let (hour12, meridiem) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    };
    format!("{day}{suffix} {month_name} {year}, {hour12}:{minute:02} {meridiem}")
}

/// Compact token-count formatting for the composer chip: `412`, `1.2K`,
/// `38.6M`. Matches common chat-UI conventions — one decimal place above
/// 1,000, no decimal below.
pub fn format_compact_tokens(n: u32) -> String {
    let n = n as f64;
    if n < 1_000.0 {
        format!("{}", n as u64)
    } else if n < 1_000_000.0 {
        format!("{:.1}K", n / 1_000.0)
    } else {
        format!("{:.1}M", n / 1_000_000.0)
    }
}

/// A message assembled by the composer, ready to be sent.
#[derive(Debug, Clone, Default)]
pub struct OutgoingMessage {
    pub text: String,
    pub attachments: Vec<ImageAttachment>,
}

/// One entry in the sessions sidebar (see `session_groups` and
/// `components::SessionsSidebar`).
///
/// Deliberately a thin, channel-agnostic shape: `id` is whatever key the
/// owning frontend uses to identify a session (a WebSocket `chat_id` for
/// `websockets-chat`, a raw session key for `web-chat`'s `GET /v1/sessions`).
/// `created_at`/`updated_at` are RFC3339 strings exactly as the backend
/// produces them (`chrono::DateTime::to_rfc3339`) — kept as strings rather
/// than parsed here so this crate has no opinion on parse failure; grouping
/// (`session_groups::group_sessions`) does its own tolerant parsing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListItem {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    /// Whether this session has a persisted idle-compact summary. The
    /// sidebar kebab uses this to hide "Summary" until one exists.
    #[serde(default)]
    pub has_summary: bool,
}

/// Content for the sessions-sidebar Summary dialog.
///
/// `text` / `last_active` are `None` while the gateway reply is in flight.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionSummaryPopup {
    pub chat_id: String,
    pub text: Option<String>,
    pub last_active: Option<String>,
}

/// One child directory in the workspace-scope folder picker.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceDirectoryEntry {
    pub name: String,
    pub path: String,
}

/// Content for the sessions-sidebar Workspace dialog.
///
/// `path` is `None` while the first `directories` reply is in flight.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceDialogState {
    pub chat_id: String,
    pub path: Option<String>,
    pub parent: Option<String>,
    pub entries: Vec<WorkspaceDirectoryEntry>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_message_time_parts_matches_requested_display() {
        assert_eq!(
            format_message_time_parts(2026, 9, 5, 17, 16),
            "5th Sep 2026, 5:16 PM"
        );
    }

    #[test]
    fn format_message_time_parts_ordinals_and_clock() {
        assert_eq!(
            format_message_time_parts(2026, 1, 1, 0, 5),
            "1st Jan 2026, 12:05 AM"
        );
        assert_eq!(
            format_message_time_parts(2026, 2, 2, 12, 0),
            "2nd Feb 2026, 12:00 PM"
        );
        assert_eq!(
            format_message_time_parts(2026, 3, 3, 11, 59),
            "3rd Mar 2026, 11:59 AM"
        );
        assert_eq!(
            format_message_time_parts(2026, 11, 11, 13, 1),
            "11th Nov 2026, 1:01 PM"
        );
        assert_eq!(
            format_message_time_parts(2026, 12, 21, 23, 9),
            "21st Dec 2026, 11:09 PM"
        );
        assert_eq!(
            format_message_time_parts(2026, 8, 22, 8, 0),
            "22nd Aug 2026, 8:00 AM"
        );
        assert_eq!(
            format_message_time_parts(2026, 7, 23, 16, 30),
            "23rd Jul 2026, 4:30 PM"
        );
    }

    #[test]
    fn format_compact_tokens_below_thousand_has_no_suffix() {
        assert_eq!(format_compact_tokens(0), "0");
        assert_eq!(format_compact_tokens(412), "412");
        assert_eq!(format_compact_tokens(999), "999");
    }

    #[test]
    fn format_compact_tokens_thousands_get_one_decimal_and_k_suffix() {
        assert_eq!(format_compact_tokens(1_000), "1.0K");
        assert_eq!(format_compact_tokens(1_234), "1.2K");
        assert_eq!(format_compact_tokens(999_999), "1000.0K");
    }

    #[test]
    fn format_compact_tokens_millions_get_one_decimal_and_m_suffix() {
        assert_eq!(format_compact_tokens(1_000_000), "1.0M");
        assert_eq!(format_compact_tokens(38_600_000), "38.6M");
    }

    #[test]
    fn session_token_usage_is_empty_only_when_every_field_is_none() {
        assert!(SessionTokenUsage::default().is_empty());
        assert!(!SessionTokenUsage {
            input_tokens: Some(0),
            ..SessionTokenUsage::default()
        }
        .is_empty());
    }

    #[test]
    fn session_token_usage_prompt_tokens_folds_in_cache_fields() {
        let usage = SessionTokenUsage {
            input_tokens: Some(10),
            cache_creation_input_tokens: Some(5),
            cache_read_input_tokens: Some(2),
            ..SessionTokenUsage::default()
        };
        assert_eq!(usage.prompt_tokens(), Some(17));
        assert_eq!(SessionTokenUsage::default().prompt_tokens(), None);
    }

    #[test]
    fn session_token_usage_total_tokens_is_prompt_plus_output() {
        let usage = SessionTokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(4),
            ..SessionTokenUsage::default()
        };
        assert_eq!(usage.total_tokens(), Some(14));
        assert_eq!(
            SessionTokenUsage {
                input_tokens: Some(10),
                ..SessionTokenUsage::default()
            }
            .total_tokens(),
            None,
            "missing output_tokens must propagate as unknown, not zero"
        );
    }

    #[test]
    fn session_token_usage_total_cost_sums_available_fields() {
        assert_eq!(SessionTokenUsage::default().total_cost(), None);
        let usage = SessionTokenUsage {
            input_cost: Some(0.01),
            ..SessionTokenUsage::default()
        };
        assert_eq!(usage.total_cost(), Some(0.01));
        let usage = SessionTokenUsage {
            input_cost: Some(0.01),
            output_cost: Some(0.02),
            ..SessionTokenUsage::default()
        };
        assert_eq!(usage.total_cost(), Some(0.03));
    }
}
