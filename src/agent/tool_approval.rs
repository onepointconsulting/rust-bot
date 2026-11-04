//! Pending tool-call approvals shared between an approval hook (which waits)
//! and a channel (which relays the user's answer).
//!
//! One broker instance is shared process-wide, so every pending request is
//! keyed by a generated `request_id` and pinned to the `chat_id` that asked.
//!
//! The broker is in memory only, and crash recovery (`agent::recovery`,
//! `agent::resume`) deliberately never touches it: a tool call that a crash
//! interrupted is either re-executed because it is `replay_safe()` (read-only
//! or idempotent, so it needs no approval) or is never executed again and the
//! model gets a synthesized "interrupted" result. Approvals are therefore not
//! re-requested after a restart. Durable approvals are out of scope.
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex as StdMutex;

use serde::Serialize;
use tokio::sync::oneshot;

use crate::agent::hook::ToolCallAuthorization;

/// Maximum characters of JSON-encoded arguments shown to the approver.
pub const APPROVAL_ARG_PREVIEW_LIMIT: usize = 2000;

/// One tool call as presented to the approver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolApprovalCall {
    pub id: String,
    pub name: String,
    pub arguments_preview: String,
}

impl ToolApprovalCall {
    /// The call under authorization, as shown to the approver.
    pub fn from_authorization(request: &ToolCallAuthorization<'_>) -> Self {
        Self {
            id: request.call_id.to_string(),
            name: request.tool_name.to_string(),
            arguments_preview: preview_arguments(request.arguments, APPROVAL_ARG_PREVIEW_LIMIT),
        }
    }
}

/// JSON-encode `arguments`, truncated to `limit` characters (with `...`).
pub fn preview_arguments(arguments: &serde_json::Value, limit: usize) -> String {
    let args_str = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string());
    let mut chars = args_str.chars();
    let preview: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    NotFound,
    ChatMismatch,
}

impl ResolveError {
    /// Wire-level `detail` string for an `error` frame.
    pub fn as_detail(&self) -> &'static str {
        match self {
            Self::NotFound => "approval_not_found",
            Self::ChatMismatch => "access_denied",
        }
    }
}

struct PendingApproval {
    chat_id: String,
    calls: Vec<ToolApprovalCall>,
    /// The agent turn that asked, so a turn-wide grant can be pinned to it.
    turn_id: Option<String>,
    tx: oneshot::Sender<Vec<String>>,
}

/// How far an approval reaches beyond the calls it answers.
///
/// Without a grant an approval covers only the calls it names. A grant makes the
/// gateway answer later calls of the same chat without asking again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalGrant {
    /// Every tool, until the turn that asked ends.
    Turn,
    /// Every tool, until the chat is cleared or revoked.
    Session,
    /// The tools named by the approved calls, until the chat is cleared or revoked.
    SessionTool,
}

impl ApprovalGrant {
    /// Parse the wire name (`turn`, `session`, `session_tool`).
    pub fn from_wire(name: &str) -> Option<Self> {
        match name {
            "turn" => Some(Self::Turn),
            "session" => Some(Self::Session),
            "session_tool" => Some(Self::SessionTool),
            _ => None,
        }
    }
}

/// What a chat has pre-approved.
#[derive(Default)]
struct ChatGrants {
    /// The turn a [`ApprovalGrant::Turn`] grant belongs to.
    turn_id: Option<String>,
    all_tools: bool,
    tools: BTreeSet<String>,
}

/// A chat's grants as shown to its user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GrantsView {
    /// Every tool is approved for the rest of the session.
    pub session_all: bool,
    /// Tools approved for the rest of the session, sorted.
    pub tools: Vec<String>,
    /// Every tool is approved for the running turn.
    pub turn: bool,
}

impl GrantsView {
    pub fn is_empty(&self) -> bool {
        !self.session_all && !self.turn && self.tools.is_empty()
    }
}

/// A request still awaiting an answer, as needed to re-send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalView {
    pub request_id: String,
    pub calls: Vec<ToolApprovalCall>,
}

#[derive(Default)]
pub struct ToolApprovalBroker {
    pending: StdMutex<HashMap<String, PendingApproval>>,
    /// Grants per chat id. In memory only: a restart forgets them on purpose.
    grants: StdMutex<HashMap<String, ChatGrants>>,
}

impl ToolApprovalBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new request. The receiver yields the approved call ids.
    pub fn register(
        &self,
        chat_id: &str,
        calls: Vec<ToolApprovalCall>,
    ) -> (String, oneshot::Receiver<Vec<String>>) {
        self.register_for_turn(chat_id, calls, None)
    }

    /// Like [`Self::register`], remembering the `turn_id` that asks so that a
    /// [`ApprovalGrant::Turn`] answer can be pinned to that turn.
    pub fn register_for_turn(
        &self,
        chat_id: &str,
        calls: Vec<ToolApprovalCall>,
        turn_id: Option<&str>,
    ) -> (String, oneshot::Receiver<Vec<String>>) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.lock().insert(
            request_id.clone(),
            PendingApproval {
                chat_id: chat_id.to_string(),
                calls,
                turn_id: turn_id.map(str::to_string),
                tx,
            },
        );
        (request_id, rx)
    }

    /// Deliver the user's answer. Ids that don't belong to the request are
    /// dropped, so a client can only approve what it was actually shown.
    pub fn resolve(
        &self,
        chat_id: &str,
        request_id: &str,
        approved_ids: Vec<String>,
    ) -> Result<(), ResolveError> {
        self.resolve_with_grant(chat_id, request_id, approved_ids, None)
            .map(|_| ())
    }

    /// Like [`Self::resolve`], and when at least one call is approved also
    /// records `grant` for the chat. Returns whether a grant was recorded.
    ///
    /// The tools of a [`ApprovalGrant::SessionTool`] grant and the turn of a
    /// [`ApprovalGrant::Turn`] grant come from the stored request, never from
    /// the client, so an answer can only widen what it was actually shown.
    pub fn resolve_with_grant(
        &self,
        chat_id: &str,
        request_id: &str,
        approved_ids: Vec<String>,
        grant: Option<ApprovalGrant>,
    ) -> Result<bool, ResolveError> {
        let mut pending = self.lock();
        match pending.get(request_id) {
            None => return Err(ResolveError::NotFound),
            Some(entry) if entry.chat_id != chat_id => return Err(ResolveError::ChatMismatch),
            Some(_) => {}
        }
        let entry = pending
            .remove(request_id)
            .expect("entry checked under the same lock");
        drop(pending);
        let approved: Vec<String> = approved_ids
            .into_iter()
            .filter(|id| entry.calls.iter().any(|call| &call.id == id))
            .collect();
        let granted = match grant {
            Some(grant) if !approved.is_empty() => {
                let tool_names = entry
                    .calls
                    .iter()
                    .filter(|call| approved.contains(&call.id))
                    .map(|call| call.name.clone())
                    .collect();
                self.add_grant(chat_id, grant, entry.turn_id.as_deref(), tool_names)
            }
            _ => false,
        };
        // The waiter may already be gone (turn aborted); nothing to do then.
        let _ = entry.tx.send(approved);
        Ok(granted)
    }

    /// Record a grant for `chat_id`. Returns whether anything was recorded: a
    /// turn grant needs the `turn_id` it is pinned to.
    pub fn add_grant(
        &self,
        chat_id: &str,
        grant: ApprovalGrant,
        turn_id: Option<&str>,
        tool_names: Vec<String>,
    ) -> bool {
        let mut grants = self.lock_grants();
        match grant {
            ApprovalGrant::Turn => {
                let Some(turn_id) = turn_id else {
                    return false;
                };
                grants.entry(chat_id.to_string()).or_default().turn_id = Some(turn_id.to_string());
            }
            ApprovalGrant::Session => {
                grants.entry(chat_id.to_string()).or_default().all_tools = true;
            }
            ApprovalGrant::SessionTool => {
                grants
                    .entry(chat_id.to_string())
                    .or_default()
                    .tools
                    .extend(tool_names);
            }
        }
        true
    }

    /// Whether `chat_id` already approved a call of `tool_name` made in the turn
    /// `turn_id`.
    pub fn is_granted(&self, chat_id: &str, turn_id: Option<&str>, tool_name: &str) -> bool {
        let grants = self.lock_grants();
        let Some(grants) = grants.get(chat_id) else {
            return false;
        };
        grants.all_tools
            || grants.tools.contains(tool_name)
            || (turn_id.is_some() && grants.turn_id.as_deref() == turn_id)
    }

    /// Forget every grant of `chat_id`. Returns whether there was any.
    pub fn revoke_grants(&self, chat_id: &str) -> bool {
        self.lock_grants().remove(chat_id).is_some()
    }

    /// What `chat_id` has pre-approved. The turn grant is only reported when
    /// `include_turn` is set: nothing here knows whether its turn is still running.
    pub fn grants_for_chat(&self, chat_id: &str, include_turn: bool) -> GrantsView {
        let grants = self.lock_grants();
        let Some(grants) = grants.get(chat_id) else {
            return GrantsView::default();
        };
        GrantsView {
            session_all: grants.all_tools,
            tools: grants.tools.iter().cloned().collect(),
            turn: include_turn && grants.turn_id.is_some(),
        }
    }

    /// Forget a request without answering it (the waiter was cancelled).
    pub fn cancel(&self, request_id: &str) {
        self.lock().remove(request_id);
    }

    pub fn pending_for_chat(&self, chat_id: &str) -> Vec<PendingApprovalView> {
        self.lock()
            .iter()
            .filter(|(_, entry)| entry.chat_id == chat_id)
            .map(|(request_id, entry)| PendingApprovalView {
                request_id: request_id.clone(),
                calls: entry.calls.clone(),
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingApproval>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_grants(&self) -> std::sync::MutexGuard<'_, HashMap<String, ChatGrants>> {
        self.grants.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> ToolApprovalCall {
        ToolApprovalCall {
            id: id.to_string(),
            name: "read_file".to_string(),
            arguments_preview: "{}".to_string(),
        }
    }

    #[tokio::test]
    async fn resolve_delivers_only_known_ids() {
        let broker = ToolApprovalBroker::new();
        let (request_id, rx) = broker.register("chat-1", vec![call("a"), call("b")]);
        broker
            .resolve("chat-1", &request_id, vec!["b".into(), "zzz".into()])
            .unwrap();
        assert_eq!(rx.await.unwrap(), vec!["b".to_string()]);
        assert!(broker.pending_for_chat("chat-1").is_empty());
    }

    #[test]
    fn resolve_rejects_unknown_request() {
        let broker = ToolApprovalBroker::new();
        assert_eq!(
            broker.resolve("chat-1", "nope", vec![]),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn resolve_rejects_other_chat_and_keeps_request() {
        let broker = ToolApprovalBroker::new();
        let (request_id, _rx) = broker.register("chat-1", vec![call("a")]);
        assert_eq!(
            broker.resolve("chat-2", &request_id, vec!["a".into()]),
            Err(ResolveError::ChatMismatch)
        );
        assert_eq!(broker.pending_for_chat("chat-1").len(), 1);
    }

    #[tokio::test]
    async fn cancel_drops_sender() {
        let broker = ToolApprovalBroker::new();
        let (request_id, rx) = broker.register("chat-1", vec![call("a")]);
        broker.cancel(&request_id);
        assert!(rx.await.is_err());
        assert!(broker.pending_for_chat("chat-1").is_empty());
    }

    #[test]
    fn pending_for_chat_filters_by_chat() {
        let broker = ToolApprovalBroker::new();
        let (first, _rx1) = broker.register("chat-1", vec![call("a")]);
        let (_second, _rx2) = broker.register("chat-2", vec![call("b")]);
        let pending = broker.pending_for_chat("chat-1");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, first);
        assert_eq!(pending[0].calls, vec![call("a")]);
    }

    #[test]
    fn preview_arguments_truncates_long_payloads() {
        let arguments = serde_json::json!({"content": "x".repeat(200)});
        let preview = preview_arguments(&arguments, 50);
        assert!(preview.ends_with("..."));
        assert_eq!(preview.chars().count(), 53);
    }

    #[test]
    fn an_approval_call_shows_the_validated_arguments_of_the_authorization() {
        use crate::agent::hook::ToolCallAuthorization;
        let arguments = serde_json::json!({"path": "a.txt"});
        let request = ToolCallAuthorization {
            call_id: "c1",
            tool_name: "read_file",
            arguments: &arguments,
            channel: None,
            chat_id: None,
            turn_id: None,
        };
        assert_eq!(
            ToolApprovalCall::from_authorization(&request),
            ToolApprovalCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments_preview: r#"{"path":"a.txt"}"#.into(),
            }
        );
    }

    // ── grants ────────────────────────────────────────────────────────────────

    fn named_call(id: &str, name: &str) -> ToolApprovalCall {
        ToolApprovalCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments_preview: "{}".to_string(),
        }
    }

    #[test]
    fn grant_names_round_trip_and_unknown_names_are_rejected() {
        assert_eq!(ApprovalGrant::from_wire("turn"), Some(ApprovalGrant::Turn));
        assert_eq!(
            ApprovalGrant::from_wire("session"),
            Some(ApprovalGrant::Session)
        );
        assert_eq!(
            ApprovalGrant::from_wire("session_tool"),
            Some(ApprovalGrant::SessionTool)
        );
        assert_eq!(ApprovalGrant::from_wire("forever"), None);
        assert_eq!(ApprovalGrant::from_wire(""), None);
    }

    #[test]
    fn nothing_is_granted_by_default() {
        let broker = ToolApprovalBroker::new();
        assert!(!broker.is_granted("chat-1", Some("t1"), "shell"));
        assert!(broker.grants_for_chat("chat-1", true).is_empty());
    }

    #[test]
    fn a_session_grant_covers_every_tool_of_that_chat_only() {
        let broker = ToolApprovalBroker::new();
        broker.add_grant("chat-1", ApprovalGrant::Session, None, vec![]);
        assert!(broker.is_granted("chat-1", None, "shell"));
        assert!(broker.is_granted("chat-1", Some("t9"), "write_file"));
        assert!(!broker.is_granted("chat-2", None, "shell"));
    }

    #[test]
    fn a_tool_grant_covers_only_the_named_tools() {
        let broker = ToolApprovalBroker::new();
        broker.add_grant(
            "chat-1",
            ApprovalGrant::SessionTool,
            None,
            vec!["read_file".into()],
        );
        assert!(broker.is_granted("chat-1", None, "read_file"));
        assert!(!broker.is_granted("chat-1", None, "shell"));
        let view = broker.grants_for_chat("chat-1", true);
        assert_eq!(view.tools, vec!["read_file".to_string()]);
        assert!(!view.session_all && !view.turn);
    }

    #[test]
    fn a_turn_grant_expires_with_its_turn() {
        let broker = ToolApprovalBroker::new();
        assert!(broker.add_grant("chat-1", ApprovalGrant::Turn, Some("t1"), vec![]));
        assert!(broker.is_granted("chat-1", Some("t1"), "shell"));
        assert!(!broker.is_granted("chat-1", Some("t2"), "shell"));
        assert!(
            !broker.is_granted("chat-1", None, "shell"),
            "a call of unknown turn is never covered"
        );
    }

    #[test]
    fn a_turn_grant_needs_a_turn_to_be_pinned_to() {
        let broker = ToolApprovalBroker::new();
        assert!(!broker.add_grant("chat-1", ApprovalGrant::Turn, None, vec![]));
        assert!(broker.grants_for_chat("chat-1", true).is_empty());
    }

    #[test]
    fn the_turn_grant_is_only_reported_when_asked_for() {
        let broker = ToolApprovalBroker::new();
        broker.add_grant("chat-1", ApprovalGrant::Turn, Some("t1"), vec![]);
        assert!(broker.grants_for_chat("chat-1", true).turn);
        assert!(broker.grants_for_chat("chat-1", false).is_empty());
    }

    #[test]
    fn revoking_forgets_every_grant_of_the_chat_and_reports_whether_there_was_any() {
        let broker = ToolApprovalBroker::new();
        broker.add_grant("chat-1", ApprovalGrant::Session, None, vec![]);
        broker.add_grant("chat-2", ApprovalGrant::Session, None, vec![]);
        assert!(broker.revoke_grants("chat-1"));
        assert!(!broker.revoke_grants("chat-1"));
        assert!(!broker.is_granted("chat-1", None, "shell"));
        assert!(broker.is_granted("chat-2", None, "shell"));
    }

    #[tokio::test]
    async fn answering_with_a_tool_grant_grants_the_tools_of_the_approved_calls_only() {
        let broker = ToolApprovalBroker::new();
        let (request_id, rx) = broker.register_for_turn(
            "chat-1",
            vec![named_call("a", "read_file"), named_call("b", "shell")],
            Some("t1"),
        );

        let granted = broker
            .resolve_with_grant(
                "chat-1",
                &request_id,
                vec!["a".into()],
                Some(ApprovalGrant::SessionTool),
            )
            .unwrap();

        assert!(granted);
        assert_eq!(rx.await.unwrap(), vec!["a".to_string()]);
        assert!(broker.is_granted("chat-1", None, "read_file"));
        assert!(
            !broker.is_granted("chat-1", None, "shell"),
            "b was not approved"
        );
    }

    #[tokio::test]
    async fn a_turn_grant_is_pinned_to_the_turn_that_asked_not_one_the_client_names() {
        let broker = ToolApprovalBroker::new();
        let (request_id, _rx) = broker.register_for_turn(
            "chat-1",
            vec![named_call("a", "shell")],
            Some("turn-that-asked"),
        );

        broker
            .resolve_with_grant(
                "chat-1",
                &request_id,
                vec!["a".into()],
                Some(ApprovalGrant::Turn),
            )
            .unwrap();

        assert!(broker.is_granted("chat-1", Some("turn-that-asked"), "shell"));
        assert!(!broker.is_granted("chat-1", Some("another-turn"), "shell"));
    }

    #[tokio::test]
    async fn denying_every_call_grants_nothing_whatever_the_client_asked_for() {
        let broker = ToolApprovalBroker::new();
        let (request_id, rx) = broker.register("chat-1", vec![named_call("a", "shell")]);

        let granted = broker
            .resolve_with_grant("chat-1", &request_id, vec![], Some(ApprovalGrant::Session))
            .unwrap();

        assert!(!granted);
        assert!(rx.await.unwrap().is_empty());
        assert!(!broker.is_granted("chat-1", None, "shell"));
    }

    #[tokio::test]
    async fn a_rejected_answer_records_no_grant() {
        let broker = ToolApprovalBroker::new();
        let (request_id, _rx) = broker.register("chat-1", vec![named_call("a", "shell")]);

        let result = broker.resolve_with_grant(
            "chat-2",
            &request_id,
            vec!["a".into()],
            Some(ApprovalGrant::Session),
        );

        assert_eq!(result, Err(ResolveError::ChatMismatch));
        assert!(!broker.is_granted("chat-1", None, "shell"));
        assert!(!broker.is_granted("chat-2", None, "shell"));
    }
}
