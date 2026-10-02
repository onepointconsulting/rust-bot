//! Agent-loop hook that turns tool activity into ACP events and asks the
//! client for permission, failing closed.
//!
//! * `before_execute_tools`: one `tool_call` per call, then
//!   `session/request_permission` for kinds that need approval. A refused,
//!   cancelled, failed or unanswered request **denies** the call; nobody
//!   answering never means "allow".
//! * `after_iteration`: one `tool_call_update` per call with status and result.

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SessionUpdate, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use async_trait::async_trait;
use serde_json::Value;

use super::ACP_CHANNEL;
use super::link::AcpLink;
use super::mapping::{
    build_completion_update, build_status_update, build_tool_call, event_is_failure,
    requires_permission, tool_kind_for, tool_locations, tool_result_text,
};
use super::registry::SessionRegistry;
use crate::agent::hook::{AgentHook, AgentHookContext, ToolHookDecision};
use crate::providers::base::ToolCallRequest;
use crate::utils::tool_hints::format_tool_hints;

/// How long to wait for the client to answer a permission request.
pub const DEFAULT_PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// Option id of the "allow" choice offered to the client.
const ALLOW_OPTION_ID: &str = "allow";
/// Option id of the "reject" choice offered to the client.
const REJECT_OPTION_ID: &str = "reject";
/// Reason shown to the model when a call was not approved.
const DENIED_REASON: &str = "denied by the ACP client (permission was not granted)";

/// See the module documentation.
pub struct AcpSessionHook {
    link: Arc<dyn AcpLink>,
    registry: Arc<SessionRegistry>,
    confirm_before_execute: bool,
    permission_timeout: Duration,
}

impl AcpSessionHook {
    pub fn new(
        link: Arc<dyn AcpLink>,
        registry: Arc<SessionRegistry>,
        confirm_before_execute: bool,
        permission_timeout: Duration,
    ) -> Self {
        Self {
            link,
            registry,
            confirm_before_execute,
            permission_timeout,
        }
    }

    /// ACP session id of the turn, or `None` for turns that are not ACP turns.
    fn session_id(context: &AgentHookContext) -> Option<String> {
        if context.channel.as_deref() == Some(ACP_CHANNEL) {
            context.chat_id.clone()
        } else {
            None
        }
    }

    /// Send an update; a failure only means the client is gone and is logged.
    async fn notify(&self, session_id: &str, update: SessionUpdate) {
        if let Err(error) = self.link.send_update(session_id, update).await {
            log::warn!("ACP: could not send session/update for {session_id}: {error}");
        }
    }

    /// Ask the client to approve `call`. Anything but an explicit "allow" is a no.
    async fn permission_granted(
        &self,
        session_id: &str,
        call: &ToolCallRequest,
        base_dir: Option<&std::path::Path>,
    ) -> bool {
        let request = build_permission_request(session_id, call, base_dir);
        let answer = tokio::time::timeout(
            self.permission_timeout,
            self.link.request_permission(request),
        )
        .await;
        match answer {
            Ok(Ok(response)) => outcome_is_allow(&response.outcome),
            Ok(Err(error)) => {
                log::warn!("ACP: permission request for {} failed: {error}", call.name);
                false
            }
            Err(_) => {
                log::warn!(
                    "ACP: no answer to the permission request for {} within {:?}; denying",
                    call.name,
                    self.permission_timeout
                );
                false
            }
        }
    }
}

/// Build the `session/request_permission` request for one tool call.
pub fn build_permission_request(
    session_id: &str,
    call: &ToolCallRequest,
    base_dir: Option<&std::path::Path>,
) -> RequestPermissionRequest {
    let title = format_tool_hints(vec![call.clone()]);
    let fields = ToolCallUpdateFields::new()
        .kind(tool_kind_for(&call.name))
        .title(if title.is_empty() {
            call.name.clone()
        } else {
            title
        })
        .locations(tool_locations(&call.arguments, base_dir))
        .raw_input(Value::Object(call.arguments.clone().into_iter().collect()));
    RequestPermissionRequest::new(
        session_id.to_string(),
        ToolCallUpdate::new(call.id.clone(), fields),
        vec![
            PermissionOption::new(ALLOW_OPTION_ID, "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new(REJECT_OPTION_ID, "Reject", PermissionOptionKind::RejectOnce),
        ],
    )
}

/// Whether the client's answer is an explicit "allow".
pub fn outcome_is_allow(outcome: &RequestPermissionOutcome) -> bool {
    match outcome {
        RequestPermissionOutcome::Selected(selected) => {
            selected.option_id.0.as_ref() == ALLOW_OPTION_ID
        }
        _ => false,
    }
}

#[async_trait]
impl AgentHook for AcpSessionHook {
    async fn before_execute_tools(&self, context: &mut AgentHookContext) -> ToolHookDecision {
        let Some(session_id) = Self::session_id(context) else {
            return ToolHookDecision::Continue;
        };
        let base_dir = self.registry.cwd_of(&session_id);
        let mut denied_ids = Vec::new();

        for call in &context.tool_calls {
            self.notify(
                &session_id,
                SessionUpdate::ToolCall(build_tool_call(call, base_dir.as_deref())),
            )
            .await;

            let kind = tool_kind_for(&call.name);
            if requires_permission(kind, self.confirm_before_execute)
                && !self
                    .permission_granted(&session_id, call, base_dir.as_deref())
                    .await
            {
                // The runner answers denied calls with a synthetic result;
                // `after_iteration` reports it to the client as failed.
                denied_ids.push(call.id.clone());
                continue;
            }

            self.notify(
                &session_id,
                SessionUpdate::ToolCallUpdate(build_status_update(
                    &call.id,
                    ToolCallStatus::InProgress,
                )),
            )
            .await;
        }

        if denied_ids.is_empty() {
            ToolHookDecision::Continue
        } else {
            ToolHookDecision::DenyCalls {
                ids: denied_ids,
                reason: DENIED_REASON.to_string(),
            }
        }
    }

    async fn after_iteration(&self, context: &mut AgentHookContext) {
        let Some(session_id) = Self::session_id(context) else {
            return;
        };
        // Events line up with calls one-to-one; if they ever do not, fall back
        // to the "Error…" prefix the runner puts on failed results.
        let events_line_up = context.tool_events.len() == context.tool_calls.len();

        for (index, call) in context.tool_calls.iter().enumerate() {
            let Some(result) = context.tool_results.iter().find(|message| {
                message.get("tool_call_id").and_then(Value::as_str) == Some(&call.id)
            }) else {
                continue;
            };
            let output = tool_result_text(result);
            let failed = if events_line_up {
                event_is_failure(&context.tool_events[index])
            } else {
                output.starts_with("Error")
            };
            self.notify(
                &session_id,
                SessionUpdate::ToolCallUpdate(build_completion_update(&call.id, &output, failed)),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        RequestPermissionResponse, SelectedPermissionOutcome, ToolKind,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// What the fake client does when asked for permission.
    #[derive(Clone)]
    enum Answer {
        Allow,
        Reject,
        Cancelled,
        Error,
        NeverAnswers,
    }

    struct FakeLink {
        answer: Answer,
        updates: Mutex<Vec<SessionUpdate>>,
        permission_requests: Mutex<Vec<RequestPermissionRequest>>,
    }

    impl FakeLink {
        fn new(answer: Answer) -> Arc<Self> {
            Arc::new(Self {
                answer,
                updates: Mutex::new(Vec::new()),
                permission_requests: Mutex::new(Vec::new()),
            })
        }

        fn updates(&self) -> Vec<SessionUpdate> {
            self.updates.lock().unwrap().clone()
        }

        fn permission_request_count(&self) -> usize {
            self.permission_requests.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl AcpLink for FakeLink {
        async fn send_update(
            &self,
            _session_id: &str,
            update: SessionUpdate,
        ) -> Result<(), String> {
            self.updates.lock().unwrap().push(update);
            Ok(())
        }

        async fn request_permission(
            &self,
            request: RequestPermissionRequest,
        ) -> Result<RequestPermissionResponse, String> {
            self.permission_requests.lock().unwrap().push(request);
            match self.answer {
                Answer::Allow => Ok(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                        ALLOW_OPTION_ID,
                    )),
                )),
                Answer::Reject => Ok(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                        REJECT_OPTION_ID,
                    )),
                )),
                Answer::Cancelled => Ok(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                )),
                Answer::Error => Err("client exploded".to_string()),
                Answer::NeverAnswers => std::future::pending().await,
            }
        }
    }

    fn hook_with(link: &Arc<FakeLink>, confirm_before_execute: bool) -> AcpSessionHook {
        let registry = Arc::new(SessionRegistry::new());
        registry.insert("s1".to_string(), std::env::temp_dir());
        AcpSessionHook::new(
            link.clone(),
            registry,
            confirm_before_execute,
            Duration::from_millis(100),
        )
    }

    fn call(id: &str, name: &str, arguments: Value) -> ToolCallRequest {
        ToolCallRequest {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments
                .as_object()
                .map(|map| map.clone().into_iter().collect::<HashMap<_, _>>())
                .unwrap_or_default(),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }
    }

    fn acp_context(calls: Vec<ToolCallRequest>) -> AgentHookContext {
        let mut context = AgentHookContext::new(0, vec![]);
        context.channel = Some(ACP_CHANNEL.to_string());
        context.chat_id = Some("s1".to_string());
        context.tool_calls = calls;
        context
    }

    fn tool_call_ids(updates: &[SessionUpdate]) -> Vec<String> {
        updates
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::ToolCall(call) => Some(call.tool_call_id.0.to_string()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn read_file_is_announced_but_never_asks() {
        let link = FakeLink::new(Answer::Reject);
        let hook = hook_with(&link, false);
        let mut context = acp_context(vec![call("c1", "read_file", json!({"path": "a.txt"}))]);

        let decision = hook.before_execute_tools(&mut context).await;

        assert_eq!(decision, ToolHookDecision::Continue);
        assert_eq!(link.permission_request_count(), 0);
        let updates = link.updates();
        assert_eq!(tool_call_ids(&updates), vec!["c1"]);
        match &updates[0] {
            SessionUpdate::ToolCall(tool_call) => {
                assert_eq!(tool_call.kind, ToolKind::Read);
                assert_eq!(tool_call.raw_input, Some(json!({"path": "a.txt"})));
                assert_eq!(tool_call.locations.len(), 1);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert!(matches!(
            &updates[1],
            SessionUpdate::ToolCallUpdate(update) if update.fields.status == Some(ToolCallStatus::InProgress)
        ));
    }

    #[tokio::test]
    async fn shell_asks_and_runs_when_allowed() {
        let link = FakeLink::new(Answer::Allow);
        let hook = hook_with(&link, false);
        let mut context = acp_context(vec![call("c1", "shell", json!({"command": "ls"}))]);

        let decision = hook.before_execute_tools(&mut context).await;

        assert_eq!(decision, ToolHookDecision::Continue);
        assert_eq!(link.permission_request_count(), 1);
    }

    #[tokio::test]
    async fn shell_is_denied_when_rejected() {
        let link = FakeLink::new(Answer::Reject);
        let hook = hook_with(&link, false);
        let mut context = acp_context(vec![call("c1", "shell", json!({"command": "ls"}))]);

        let decision = hook.before_execute_tools(&mut context).await;

        assert!(matches!(
            decision,
            ToolHookDecision::DenyCalls { ref ids, .. } if ids == &vec!["c1".to_string()]
        ));
    }

    #[tokio::test]
    async fn cancelled_error_and_silence_all_deny() {
        for answer in [Answer::Cancelled, Answer::Error, Answer::NeverAnswers] {
            let link = FakeLink::new(answer);
            let hook = hook_with(&link, false);
            let mut context = acp_context(vec![call("c1", "shell", json!({"command": "ls"}))]);

            let decision = hook.before_execute_tools(&mut context).await;

            assert!(
                matches!(decision, ToolHookDecision::DenyCalls { .. }),
                "must fail closed"
            );
        }
    }

    #[tokio::test]
    async fn confirm_before_execute_makes_reads_ask_too() {
        let link = FakeLink::new(Answer::Reject);
        let hook = hook_with(&link, true);
        let mut context = acp_context(vec![call("c1", "read_file", json!({"path": "a.txt"}))]);

        let decision = hook.before_execute_tools(&mut context).await;

        assert_eq!(link.permission_request_count(), 1);
        assert!(matches!(decision, ToolHookDecision::DenyCalls { .. }));
    }

    #[tokio::test]
    async fn only_the_refused_calls_of_a_batch_are_denied() {
        let link = FakeLink::new(Answer::Reject);
        let hook = hook_with(&link, false);
        let mut context = acp_context(vec![
            call("c1", "read_file", json!({"path": "a.txt"})),
            call("c2", "shell", json!({"command": "ls"})),
        ]);

        let decision = hook.before_execute_tools(&mut context).await;

        assert!(matches!(
            decision,
            ToolHookDecision::DenyCalls { ref ids, .. } if ids == &vec!["c2".to_string()]
        ));
        assert_eq!(tool_call_ids(&link.updates()), vec!["c1", "c2"]);
    }

    #[tokio::test]
    async fn turns_from_other_channels_are_ignored() {
        let link = FakeLink::new(Answer::Reject);
        let hook = hook_with(&link, false);
        let mut context = acp_context(vec![call("c1", "shell", json!({"command": "ls"}))]);
        context.channel = Some("websocket".to_string());

        let decision = hook.before_execute_tools(&mut context).await;

        assert_eq!(decision, ToolHookDecision::Continue);
        assert!(link.updates().is_empty());
        assert_eq!(link.permission_request_count(), 0);
    }

    #[tokio::test]
    async fn permission_request_names_the_call_and_offers_allow_and_reject() {
        let link = FakeLink::new(Answer::Allow);
        let hook = hook_with(&link, false);
        let mut context = acp_context(vec![call("c9", "shell", json!({"command": "ls"}))]);

        hook.before_execute_tools(&mut context).await;

        let requests = link.permission_requests.lock().unwrap();
        let request = &requests[0];
        assert_eq!(request.session_id.0.as_ref(), "s1");
        assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "c9");
        assert_eq!(request.tool_call.fields.kind, Some(ToolKind::Execute));
        let kinds: Vec<_> = request.options.iter().map(|option| option.kind).collect();
        assert_eq!(
            kinds,
            vec![
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::RejectOnce
            ]
        );
    }

    #[tokio::test]
    async fn after_iteration_reports_completed_failed_and_blocked_calls() {
        let link = FakeLink::new(Answer::Allow);
        let hook = hook_with(&link, false);
        let mut context = acp_context(vec![
            call("ok", "read_file", json!({"path": "a.txt"})),
            call("bad", "shell", json!({"command": "false"})),
            call("blocked", "shell", json!({"command": "rm"})),
        ]);
        context.tool_results = vec![
            json!({"role": "tool", "tool_call_id": "ok", "content": "hello"}),
            json!({"role": "tool", "tool_call_id": "bad", "content": "Error: exit 1"}),
            json!({"role": "tool", "tool_call_id": "blocked", "content": "Error: blocked"}),
        ];
        let event = |status: &str| HashMap::from([("status".to_string(), status.to_string())]);
        context.tool_events = vec![event("ok"), event("error"), event("blocked")];

        hook.after_iteration(&mut context).await;

        let statuses: Vec<_> = link
            .updates()
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::ToolCallUpdate(update) => {
                    Some((update.tool_call_id.0.to_string(), update.fields.status))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("ok".to_string(), Some(ToolCallStatus::Completed)),
                ("bad".to_string(), Some(ToolCallStatus::Failed)),
                ("blocked".to_string(), Some(ToolCallStatus::Failed)),
            ]
        );
    }

    #[tokio::test]
    async fn after_iteration_without_tool_results_sends_nothing() {
        let link = FakeLink::new(Answer::Allow);
        let hook = hook_with(&link, false);
        let mut context = acp_context(vec![]);

        hook.after_iteration(&mut context).await;

        assert!(link.updates().is_empty());
    }

    #[test]
    fn only_an_explicit_allow_outcome_is_allow() {
        let selected = |id: &str| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id.to_string()))
        };
        assert!(outcome_is_allow(&selected(ALLOW_OPTION_ID)));
        assert!(!outcome_is_allow(&selected(REJECT_OPTION_ID)));
        assert!(!outcome_is_allow(&selected("something-else")));
        assert!(!outcome_is_allow(&RequestPermissionOutcome::Cancelled));
    }
}
