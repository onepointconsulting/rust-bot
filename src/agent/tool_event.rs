//! Telemetry of one finished tool call.
//!
//! [`ToolEvent`] is what a hook sees through `AgentHook::on_tool_result` the
//! moment a call finishes. The runner still reports the same calls to its
//! consumers as `name` / `status` / `detail` maps ([`ToolEvent::to_map`]); the
//! typed form adds what those maps never carried, such as the duration.

use std::collections::HashMap;

/// Outcome class of a finished tool call (the `"status"` event field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Error,
    Blocked,
}

impl ToolStatus {
    /// The wire string used in telemetry events.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Blocked => "blocked",
        }
    }
}

/// One finished tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEvent {
    pub call_id: String,
    pub tool_name: String,
    pub status: ToolStatus,
    /// Short single-line summary of the result or the failure.
    pub detail: String,
    /// How long the call took once it was cleared to run; zero for a call that
    /// was rejected or denied before it started.
    pub duration_ms: u64,
}

impl ToolEvent {
    /// The `name` / `status` / `detail` map the runner's consumers read.
    pub fn to_map(&self) -> HashMap<String, String> {
        HashMap::from([
            ("name".to_string(), self.tool_name.clone()),
            ("status".to_string(), self.status.as_str().to_string()),
            ("detail".to_string(), self.detail.clone()),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_wire_names_are_stable() {
        assert_eq!(ToolStatus::Ok.as_str(), "ok");
        assert_eq!(ToolStatus::Error.as_str(), "error");
        assert_eq!(ToolStatus::Blocked.as_str(), "blocked");
    }

    #[test]
    fn the_map_form_carries_name_status_and_detail_only() {
        let event = ToolEvent {
            call_id: "c1".into(),
            tool_name: "shell".into(),
            status: ToolStatus::Error,
            detail: "boom".into(),
            duration_ms: 1200,
        };
        assert_eq!(
            event.to_map(),
            HashMap::from([
                ("name".to_string(), "shell".to_string()),
                ("status".to_string(), "error".to_string()),
                ("detail".to_string(), "boom".to_string()),
            ])
        );
    }
}
