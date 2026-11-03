use std::io::IsTerminal;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::lock::Mutex;
use inquire::Confirm;

use crate::agent::hook::{AgentHook, CallDecision, ToolCallAuthorization};
use crate::agent::tool_approval::{self, ToolApprovalBroker, ToolApprovalCall};
use crate::bus::outbound_events::{
    OutboundEvent, ToolApprovalRequestEvent, outbound_message_for_event,
};
use crate::bus::queue::MessageBus;
use crate::channels::websocket::CHANNEL_NAME as WEBSOCKET_CHANNEL;
use crate::cli::cancel::pause_for_prompt;
use crate::cli::stream::StreamRenderer;
use crate::integrations::herdr::HerdrReporter;

const ARG_PREVIEW_LIMIT: usize = 120;
/// How long a WebSocket approval request waits before denying the call.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
/// Reason shown to the model when the user declined (or never answered).
const USER_DENIED_REASON: &str = "User denied tool execution";

/// CLI-only hook that asks before each tool call when stdin is a terminal.
pub struct CliAskHook {
    renderer: StdMutex<Option<Arc<Mutex<StreamRenderer>>>>,
    herdr: Arc<HerdrReporter>,
}

impl CliAskHook {
    pub fn new(herdr: Arc<HerdrReporter>) -> Self {
        Self {
            renderer: StdMutex::new(None),
            herdr,
        }
    }

    pub fn set_renderer(&self, renderer: Arc<Mutex<StreamRenderer>>) {
        *self.renderer.lock().unwrap_or_else(|e| e.into_inner()) = Some(renderer);
    }

    pub fn clear_renderer(&self) {
        *self.renderer.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn renderer(&self) -> Option<Arc<Mutex<StreamRenderer>>> {
        self.renderer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn preview_arguments(arguments: &serde_json::Value) -> String {
    tool_approval::preview_arguments(arguments, ARG_PREVIEW_LIMIT)
}

/// Denial for a call that needs confirmation when nobody can be asked.
///
/// Confirmation being switched on means no call may run unconfirmed, so a
/// missing way to ask is a refusal, never a pass.
fn unanswerable(why: &str) -> CallDecision {
    log::warn!("tools.confirmBeforeExecute is enabled but {why}; denying the tool call");
    CallDecision::deny(format!(
        "confirmation is required (tools.confirmBeforeExecute) but {why}, so the call was not run"
    ))
}

fn prompt_for_tool(name: &str, args_preview: &str) -> Result<bool, inquire::InquireError> {
    let message = if args_preview == "{}" {
        format!("Execute tool `{name}`?")
    } else {
        format!("Execute tool `{name}` ({args_preview})?")
    };
    Confirm::new(&message)
        .with_default(true)
        .with_help_message("y to run this call, n to skip it")
        .prompt()
}

#[async_trait]
impl AgentHook for CliAskHook {
    async fn authorize_call(&self, request: &ToolCallAuthorization<'_>) -> CallDecision {
        if !std::io::stdin().is_terminal() {
            return unanswerable("stdin is not a terminal");
        }

        if let Some(renderer) = self.renderer() {
            renderer.lock().await.stop_for_input();
        }
        self.herdr.report_blocked(None);
        let _pause = pause_for_prompt();
        let tool_name = request.tool_name.to_string();
        let arguments_preview = preview_arguments(request.arguments);
        let approved =
            tokio::task::spawn_blocking(move || prompt_for_tool(&tool_name, &arguments_preview))
                .await;
        drop(_pause);
        self.herdr.report_working();
        if let Some(renderer) = self.renderer() {
            renderer.lock().await.resume_after_input();
        }
        match approved {
            Ok(Ok(true)) => CallDecision::Allow,
            _ => CallDecision::deny(USER_DENIED_REASON),
        }
    }
}

/// Gateway hook that asks the WebSocket chat's user to approve each tool
/// call. The request goes out over the bus; the answer comes back through
/// the shared [`ToolApprovalBroker`], resolved by the WebSocket channel.
pub struct WebsocketsAskHook {
    broker: Arc<ToolApprovalBroker>,
    bus: StdMutex<Option<Arc<MessageBus>>>,
    timeout: Duration,
}

impl WebsocketsAskHook {
    pub fn new(broker: Arc<ToolApprovalBroker>) -> Self {
        Self {
            broker,
            bus: StdMutex::new(None),
            timeout: APPROVAL_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn set_bus(&self, bus: Arc<MessageBus>) {
        *self.bus.lock().unwrap_or_else(|e| e.into_inner()) = Some(bus);
    }

    fn bus(&self) -> Option<Arc<MessageBus>> {
        self.bus.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Removes the broker entry if the waiting turn is dropped (e.g. aborted).
struct PendingGuard<'a> {
    broker: &'a ToolApprovalBroker,
    request_id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.broker.cancel(&self.request_id);
    }
}

#[async_trait]
impl AgentHook for WebsocketsAskHook {
    async fn authorize_call(&self, request: &ToolCallAuthorization<'_>) -> CallDecision {
        let chat_id = match (request.channel, request.chat_id) {
            (Some(WEBSOCKET_CHANNEL), Some(chat_id)) if !chat_id.is_empty() => chat_id.to_string(),
            (channel, _) => {
                return unanswerable(&format!("channel {channel:?} cannot ask for it"));
            }
        };
        let Some(bus) = self.bus() else {
            return unanswerable("no bus is wired to ask through");
        };

        let call = ToolApprovalCall::from_authorization(request);
        let (request_id, rx) = self.broker.register(&chat_id, vec![call.clone()]);
        let _guard = PendingGuard {
            broker: &self.broker,
            request_id: request_id.clone(),
        };
        let outbound = outbound_message_for_event(
            WEBSOCKET_CHANNEL,
            &chat_id,
            OutboundEvent::ToolApprovalRequest(ToolApprovalRequestEvent {
                request_id: request_id.clone(),
                calls: vec![call],
            }),
            None,
            None,
        );
        if let Err(e) = bus.publish_outbound(outbound) {
            log::error!("Failed to publish tool approval request: {e}");
            return CallDecision::deny(USER_DENIED_REASON);
        }

        let approved = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(approved)) => approved,
            Ok(Err(_)) => Vec::new(),
            Err(_) => {
                log::warn!("Tool approval request {request_id} timed out; denying the call");
                Vec::new()
            }
        };
        if approved.iter().any(|id| id == request.call_id) {
            CallDecision::Allow
        } else {
            CallDecision::deny(USER_DENIED_REASON)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An authorization request for call `call_id` of tool `exec` in `channel`/`chat_id`.
    fn request<'a>(
        arguments: &'a serde_json::Value,
        channel: Option<&'a str>,
        chat_id: Option<&'a str>,
    ) -> ToolCallAuthorization<'a> {
        ToolCallAuthorization {
            call_id: "a",
            tool_name: "exec",
            arguments,
            channel,
            chat_id,
        }
    }

    #[test]
    fn preview_arguments_truncates_long_payloads() {
        let arguments = serde_json::json!({"content": "x".repeat(200)});
        let preview = preview_arguments(&arguments);
        assert!(preview.ends_with("..."));
        assert!(preview.chars().count() <= ARG_PREVIEW_LIMIT + 3);
    }

    #[test]
    fn preview_arguments_keeps_short_payloads() {
        let arguments = serde_json::json!({"path": "/tmp/foo.txt"});
        assert_eq!(
            preview_arguments(&arguments),
            serde_json::to_string(&arguments).unwrap()
        );
    }

    #[tokio::test]
    async fn denies_when_stdin_is_not_a_terminal() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let hook = CliAskHook::new(HerdrReporter::disabled_for_test());
        let arguments = serde_json::json!({"path": "/tmp/foo.txt"});

        let decision = hook
            .authorize_call(&request(&arguments, Some("cli"), Some("direct")))
            .await;

        let CallDecision::Deny { reason } = decision else {
            panic!("a question nobody can answer must deny");
        };
        assert!(reason.contains("stdin is not a terminal"), "got {reason:?}");
    }

    fn websocket_hook(
        broker: &Arc<ToolApprovalBroker>,
    ) -> (Arc<WebsocketsAskHook>, Arc<MessageBus>) {
        let bus = Arc::new(MessageBus::new());
        let hook = WebsocketsAskHook::new(Arc::clone(broker));
        hook.set_bus(Arc::clone(&bus));
        (Arc::new(hook), bus)
    }

    /// Run the hook for call `a`, answering the request it publishes with `answer`.
    async fn run_with_answer(answer: Option<Vec<String>>, timeout: Duration) -> CallDecision {
        let broker = Arc::new(ToolApprovalBroker::new());
        let bus = Arc::new(MessageBus::new());
        let hook = WebsocketsAskHook::new(Arc::clone(&broker)).with_timeout(timeout);
        hook.set_bus(Arc::clone(&bus));
        let responder = {
            let broker = Arc::clone(&broker);
            let bus = Arc::clone(&bus);
            tokio::spawn(async move {
                let msg = bus.consume_outbound().await.expect("request published");
                assert_eq!(msg.channel, WEBSOCKET_CHANNEL);
                assert_eq!(msg.chat_id, "chat-1");
                let Some(OutboundEvent::ToolApprovalRequest(event)) = msg.event else {
                    panic!("expected tool approval request");
                };
                assert_eq!(event.calls.len(), 1, "one request per call");
                assert_eq!(event.calls[0].id, "a");
                if let Some(approved) = answer {
                    broker
                        .resolve("chat-1", &event.request_id, approved)
                        .unwrap();
                }
            })
        };
        let arguments = serde_json::json!({});
        let decision = hook
            .authorize_call(&request(
                &arguments,
                Some(WEBSOCKET_CHANNEL),
                Some("chat-1"),
            ))
            .await;
        responder.await.unwrap();
        assert!(broker.pending_for_chat("chat-1").is_empty());
        decision
    }

    fn user_denied() -> CallDecision {
        CallDecision::deny(USER_DENIED_REASON)
    }

    #[tokio::test]
    async fn websocket_hook_allows_an_approved_call() {
        let decision = run_with_answer(Some(vec!["a".into()]), Duration::from_secs(5)).await;
        assert_eq!(decision, CallDecision::Allow);
    }

    #[tokio::test]
    async fn websocket_hook_denies_a_call_missing_from_the_answer() {
        let decision = run_with_answer(Some(vec!["b".into()]), Duration::from_secs(5)).await;
        assert_eq!(decision, user_denied());
    }

    #[tokio::test]
    async fn websocket_hook_denies_on_timeout() {
        let decision = run_with_answer(None, Duration::from_millis(50)).await;
        assert_eq!(decision, user_denied());
    }

    #[tokio::test]
    async fn websocket_hook_denies_when_request_cancelled() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let (hook, bus) = websocket_hook(&broker);
        let canceller = {
            let broker = Arc::clone(&broker);
            tokio::spawn(async move {
                let msg = bus.consume_outbound().await.unwrap();
                let Some(OutboundEvent::ToolApprovalRequest(event)) = msg.event else {
                    panic!("expected tool approval request");
                };
                broker.cancel(&event.request_id);
            })
        };
        let arguments = serde_json::json!({});
        let decision = hook
            .authorize_call(&request(
                &arguments,
                Some(WEBSOCKET_CHANNEL),
                Some("chat-1"),
            ))
            .await;
        canceller.await.unwrap();
        assert_eq!(decision, user_denied());
    }

    #[tokio::test]
    async fn websocket_hook_denies_calls_of_channels_that_cannot_ask() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let (hook, bus) = websocket_hook(&broker);
        let arguments = serde_json::json!({});

        let decision = hook
            .authorize_call(&request(&arguments, Some("cli"), Some("chat-1")))
            .await;

        let CallDecision::Deny { reason } = decision else {
            panic!("a channel that cannot ask must be denied, not waved through");
        };
        assert!(reason.contains("cannot ask for it"), "got {reason:?}");
        assert_eq!(bus.outbound_size(), 0);
    }

    #[tokio::test]
    async fn websocket_hook_denies_a_websocket_turn_without_a_chat_id() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let (hook, bus) = websocket_hook(&broker);
        let arguments = serde_json::json!({});

        for chat_id in [None, Some("")] {
            let decision = hook
                .authorize_call(&request(&arguments, Some(WEBSOCKET_CHANNEL), chat_id))
                .await;
            assert!(!decision.is_allowed());
        }
        assert_eq!(bus.outbound_size(), 0);
    }

    #[tokio::test]
    async fn websocket_hook_denies_when_no_bus_is_wired() {
        let hook = WebsocketsAskHook::new(Arc::new(ToolApprovalBroker::new()));
        let arguments = serde_json::json!({});

        let decision = hook
            .authorize_call(&request(
                &arguments,
                Some(WEBSOCKET_CHANNEL),
                Some("chat-1"),
            ))
            .await;

        let CallDecision::Deny { reason } = decision else {
            panic!("no bus means nobody can be asked");
        };
        assert!(reason.contains("no bus is wired"), "got {reason:?}");
    }
}
