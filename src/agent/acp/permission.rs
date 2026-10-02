//! How a parent answers its child's `session/request_permission`.
//!
//! The child asks before every call that can change something (plan decision 9).
//! The parent answers from `tools.acp.permissionPolicy`:
//!
//! * `auto-approve-read` (default): approve `read` / `search`, deny the rest;
//! * `escalate`: forward the question to the human through an [`Escalator`] (the
//!   tool-approval broker on the parent turn's channel); when nobody can be asked,
//!   **deny**;
//! * `allow-all`: approve everything, an explicit opt-in.
//!
//! Everything fails closed: a request the parent cannot map to an allow option is
//! answered `cancelled`, never approved.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SelectedPermissionOutcome, ToolKind,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::config::schema::AcpPermissionPolicy;

/// What the child wants to do, as the parent sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionAsk {
    pub tool_call_id: String,
    /// Human-readable title, e.g. `shell cargo test`.
    pub title: String,
    pub kind: ToolKind,
    pub raw_input: Option<Value>,
}

/// Decides one permission request: `true` = allow.
#[async_trait]
pub trait PermissionResponder: Send + Sync {
    async fn decide(&self, ask: &PermissionAsk) -> bool;
}

/// Asks a human. Returns `false` when it cannot ask or the human refuses.
#[async_trait]
pub trait Escalator: Send + Sync {
    async fn ask(&self, ask: &PermissionAsk) -> bool;
}

/// The answer of `tools.acp.permissionPolicy`.
pub struct PolicyResponder {
    policy: AcpPermissionPolicy,
    escalator: Option<Arc<dyn Escalator>>,
}

impl PolicyResponder {
    pub fn new(policy: AcpPermissionPolicy, escalator: Option<Arc<dyn Escalator>>) -> Self {
        Self { policy, escalator }
    }
}

#[async_trait]
impl PermissionResponder for PolicyResponder {
    async fn decide(&self, ask: &PermissionAsk) -> bool {
        match self.policy {
            AcpPermissionPolicy::AllowAll => true,
            AcpPermissionPolicy::AutoApproveRead => is_read_only(ask.kind),
            AcpPermissionPolicy::Escalate => match &self.escalator {
                Some(escalator) => escalator.ask(ask).await,
                // Nobody to ask: fail closed.
                None => false,
            },
        }
    }
}

/// Whether a tool kind cannot change anything.
fn is_read_only(kind: ToolKind) -> bool {
    matches!(kind, ToolKind::Read | ToolKind::Search)
}

/// The facts of a request, for the policy.
pub fn ask_from_request(request: &RequestPermissionRequest) -> PermissionAsk {
    let fields = &request.tool_call.fields;
    PermissionAsk {
        tool_call_id: request.tool_call.tool_call_id.0.to_string(),
        title: fields
            .title
            .clone()
            .unwrap_or_else(|| request.tool_call.tool_call_id.0.to_string()),
        // A request that does not say what it is counts as `other`: never read-only.
        kind: fields.kind.unwrap_or(ToolKind::Other),
        raw_input: fields.raw_input.clone(),
    }
}

/// The outcome that answers `request` with `allow`.
///
/// Picks the matching option the agent offered (once before always). With no
/// allow option an allow cannot be expressed, so it becomes `cancelled`: the
/// parent never approves by guessing an option id. With no reject option a deny
/// is `cancelled` too, which the child treats as a denial.
pub fn outcome_for(request: &RequestPermissionRequest, allow: bool) -> RequestPermissionOutcome {
    let wanted: [PermissionOptionKind; 2] = if allow {
        [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    } else {
        [
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]
    };
    wanted
        .iter()
        .find_map(|kind| request.options.iter().find(|option| option.kind == *kind))
        .map(|option| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ))
        })
        .unwrap_or(RequestPermissionOutcome::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        PermissionOption, ToolCallUpdate, ToolCallUpdateFields,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ask(kind: ToolKind) -> PermissionAsk {
        PermissionAsk {
            tool_call_id: "call-1".to_string(),
            title: "do something".to_string(),
            kind,
            raw_input: None,
        }
    }

    fn request_with(
        options: Vec<PermissionOption>,
        fields: ToolCallUpdateFields,
    ) -> RequestPermissionRequest {
        RequestPermissionRequest::new("sess-1", ToolCallUpdate::new("call-1", fields), options)
    }

    fn standard_options() -> Vec<PermissionOption> {
        vec![
            PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
        ]
    }

    fn selected_id(outcome: &RequestPermissionOutcome) -> Option<String> {
        match outcome {
            RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.0.to_string()),
            _ => None,
        }
    }

    struct Counting {
        answer: bool,
        asked: AtomicUsize,
    }

    #[async_trait]
    impl Escalator for Counting {
        async fn ask(&self, _ask: &PermissionAsk) -> bool {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer
        }
    }

    // ── policies ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn auto_approve_read_approves_only_reading_and_searching() {
        let responder = PolicyResponder::new(AcpPermissionPolicy::AutoApproveRead, None);
        assert!(responder.decide(&ask(ToolKind::Read)).await);
        assert!(responder.decide(&ask(ToolKind::Search)).await);
        for kind in [
            ToolKind::Edit,
            ToolKind::Execute,
            ToolKind::Fetch,
            ToolKind::Other,
            ToolKind::Delete,
            ToolKind::Move,
        ] {
            assert!(!responder.decide(&ask(kind)).await, "{kind:?}");
        }
    }

    #[tokio::test]
    async fn allow_all_approves_everything() {
        let responder = PolicyResponder::new(AcpPermissionPolicy::AllowAll, None);
        assert!(responder.decide(&ask(ToolKind::Execute)).await);
        assert!(responder.decide(&ask(ToolKind::Edit)).await);
    }

    #[tokio::test]
    async fn escalate_forwards_the_question_and_honours_the_answer() {
        let yes = Arc::new(Counting {
            answer: true,
            asked: AtomicUsize::new(0),
        });
        let responder = PolicyResponder::new(AcpPermissionPolicy::Escalate, Some(yes.clone()));
        assert!(responder.decide(&ask(ToolKind::Execute)).await);
        assert_eq!(yes.asked.load(Ordering::SeqCst), 1);

        let no = Arc::new(Counting {
            answer: false,
            asked: AtomicUsize::new(0),
        });
        let responder = PolicyResponder::new(AcpPermissionPolicy::Escalate, Some(no));
        assert!(!responder.decide(&ask(ToolKind::Execute)).await);
    }

    #[tokio::test]
    async fn escalate_with_nobody_to_ask_denies_even_a_read() {
        let responder = PolicyResponder::new(AcpPermissionPolicy::Escalate, None);
        assert!(!responder.decide(&ask(ToolKind::Execute)).await);
        assert!(!responder.decide(&ask(ToolKind::Read)).await);
    }

    // ── mapping a decision to the agent's options ───────────────────────────

    #[test]
    fn allow_selects_the_allow_option_and_deny_the_reject_option() {
        let request = request_with(standard_options(), ToolCallUpdateFields::new());
        assert_eq!(
            selected_id(&outcome_for(&request, true)).as_deref(),
            Some("allow")
        );
        assert_eq!(
            selected_id(&outcome_for(&request, false)).as_deref(),
            Some("reject")
        );
    }

    #[test]
    fn allow_once_is_preferred_to_allow_always() {
        let options = vec![
            PermissionOption::new("always", "Always", PermissionOptionKind::AllowAlways),
            PermissionOption::new("once", "Once", PermissionOptionKind::AllowOnce),
        ];
        let request = request_with(options, ToolCallUpdateFields::new());
        assert_eq!(
            selected_id(&outcome_for(&request, true)).as_deref(),
            Some("once")
        );
    }

    #[test]
    fn allow_always_is_used_when_it_is_the_only_allow_option() {
        let options = vec![PermissionOption::new(
            "always",
            "Always",
            PermissionOptionKind::AllowAlways,
        )];
        let request = request_with(options, ToolCallUpdateFields::new());
        assert_eq!(
            selected_id(&outcome_for(&request, true)).as_deref(),
            Some("always")
        );
    }

    #[test]
    fn an_allow_without_an_allow_option_is_cancelled_not_guessed() {
        let options = vec![PermissionOption::new(
            "reject",
            "Reject",
            PermissionOptionKind::RejectOnce,
        )];
        let request = request_with(options, ToolCallUpdateFields::new());
        assert_eq!(
            outcome_for(&request, true),
            RequestPermissionOutcome::Cancelled
        );
        assert_eq!(
            selected_id(&outcome_for(&request, false)).as_deref(),
            Some("reject")
        );
    }

    #[test]
    fn a_deny_without_a_reject_option_is_cancelled() {
        let options = vec![PermissionOption::new(
            "allow",
            "Allow",
            PermissionOptionKind::AllowOnce,
        )];
        let request = request_with(options, ToolCallUpdateFields::new());
        assert_eq!(
            outcome_for(&request, false),
            RequestPermissionOutcome::Cancelled
        );
    }

    #[test]
    fn a_request_without_options_is_cancelled_both_ways() {
        let request = request_with(Vec::new(), ToolCallUpdateFields::new());
        assert_eq!(
            outcome_for(&request, true),
            RequestPermissionOutcome::Cancelled
        );
        assert_eq!(
            outcome_for(&request, false),
            RequestPermissionOutcome::Cancelled
        );
    }

    // ── reading the request ─────────────────────────────────────────────────

    #[test]
    fn the_ask_carries_title_kind_and_input() {
        let fields = ToolCallUpdateFields::new()
            .title("shell cargo test")
            .kind(ToolKind::Execute)
            .raw_input(serde_json::json!({"command": "cargo test"}));
        let ask = ask_from_request(&request_with(standard_options(), fields));
        assert_eq!(ask.tool_call_id, "call-1");
        assert_eq!(ask.title, "shell cargo test");
        assert_eq!(ask.kind, ToolKind::Execute);
        assert_eq!(
            ask.raw_input,
            Some(serde_json::json!({"command": "cargo test"}))
        );
    }

    #[test]
    fn a_request_that_does_not_say_what_it_is_counts_as_other() {
        let ask = ask_from_request(&request_with(
            standard_options(),
            ToolCallUpdateFields::new(),
        ));
        assert_eq!(ask.kind, ToolKind::Other);
        assert_eq!(ask.title, "call-1");
    }
}
