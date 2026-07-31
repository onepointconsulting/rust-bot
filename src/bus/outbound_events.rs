use std::collections::HashMap;
use serde_json::Value;

/// Discriminant for progress updates. Variants are mutually exclusive —
/// unlike the previous independent bool flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProgressKind {
    #[default]
    Plain,
    ToolHint,
    Reasoning,
    ReasoningDelta,
    ReasoningEnd,
}

/// Telemetry entry aligned with `AgentRunResult::tool_events` (`name`/`status`/`detail`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEvent {
    pub name: String,
    pub status: String,
    pub detail: Option<String>,
}

/// File-edit telemetry; string map keeps parity with loosely-shaped Python payloads.
pub type FileEditEvent = HashMap<String, String>;

/// Progress control fields only — display text lives on [`crate::bus::events::OutboundMessage::content`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgressEvent {
    pub kind: ProgressKind,
    pub stream_id: Option<String>,
    pub tool_events: Option<Vec<ToolEvent>>,
    pub file_edit_events: Option<Vec<FileEditEvent>>,
}

/// Marker: retry/wait notice. Text lives on `OutboundMessage::content`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryWaitEvent;

/// Streaming delta control fields. Text lives on `OutboundMessage::content`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamDeltaEvent {
    pub stream_id: Option<String>,
}

/// Streaming end control fields. Text (usually empty) lives on `OutboundMessage::content`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamEndEvent {
    pub stream_id: Option<String>,
    pub resuming: bool,
    pub merge_next: bool,
}

/// Marker: final response for a turn that was already streamed via deltas.
/// Full reply text lives on `OutboundMessage::content`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamedResponseEvent;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnEndEvent {
    pub latency_ms: Option<i64>,
    pub goal_state: Option<HashMap<String, Value>>,
}

/// Goal lifecycle status update. `status` is required (no `Default`).
#[derive(Debug, Clone, PartialEq)]
pub struct GoalStatusEvent {
    pub status: String,
    pub started_at: Option<f64>,
}

/// Full goal-state sync payload. `goal_state` is required (no `Default`).
#[derive(Debug, Clone, PartialEq)]
pub struct GoalStateSyncEvent {
    pub goal_state: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionUpdatedEvent {
    pub scope: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeModelUpdatedEvent {
    pub model: Option<String>,
    pub model_preset: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnModelUpdatedEvent {
    pub model: Option<String>,
}

/// Typed outbound control/event envelope. Display text stays on
/// [`crate::bus::events::OutboundMessage::content`]; variants carry only
/// discriminant + control fields.
#[derive(Debug, Clone, PartialEq)]
pub enum OutboundEvent {
    Progress(ProgressEvent),
    RetryWait(RetryWaitEvent),
    StreamDelta(StreamDeltaEvent),
    StreamEnd(StreamEndEvent),
    StreamedResponse(StreamedResponseEvent),
    TurnEnd(TurnEndEvent),
    GoalStatus(GoalStatusEvent),
    GoalStateSync(GoalStateSyncEvent),
    SessionUpdated(SessionUpdatedEvent),
    RuntimeModelUpdated(RuntimeModelUpdatedEvent),
    TurnModelUpdated(TurnModelUpdatedEvent),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_default_is_plain() {
        let base = ProgressEvent::default();
        assert_eq!(base.kind, ProgressKind::Plain);
        assert!(base.stream_id.is_none());
        assert!(base.tool_events.is_none());

        let updated = ProgressEvent {
            kind: ProgressKind::ToolHint,
            ..base
        };
        assert_eq!(updated.kind, ProgressKind::ToolHint);
    }

    #[test]
    fn progress_kinds_are_mutually_exclusive() {
        let event = OutboundEvent::Progress(ProgressEvent {
            kind: ProgressKind::ReasoningDelta,
            stream_id: Some("s1".into()),
            tool_events: None,
            file_edit_events: None,
        });
        match event {
            OutboundEvent::Progress(e) => {
                assert_eq!(e.kind, ProgressKind::ReasoningDelta);
                assert_ne!(e.kind, ProgressKind::ToolHint);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_outbound_event_match() {
        let event = OutboundEvent::TurnModelUpdated(TurnModelUpdatedEvent {
            model: Some("claude-sonnet-5".to_string()),
        });
        match event {
            OutboundEvent::TurnModelUpdated(e) => {
                assert_eq!(e.model.as_deref(), Some("claude-sonnet-5"))
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn goal_status_requires_status() {
        let event = GoalStatusEvent {
            status: "running".into(),
            started_at: Some(1.0),
        };
        assert_eq!(event.status, "running");
    }

    #[test]
    fn goal_state_sync_requires_goal_state() {
        let mut goal_state = HashMap::new();
        goal_state.insert("phase".into(), Value::String("plan".into()));
        let event = GoalStateSyncEvent { goal_state };
        assert_eq!(
            event.goal_state.get("phase"),
            Some(&Value::String("plan".into()))
        );
    }
}
