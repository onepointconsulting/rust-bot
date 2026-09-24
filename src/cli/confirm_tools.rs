use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::lock::Mutex;
use inquire::Confirm;

use crate::agent::hook::{AgentHook, AgentHookContext, ToolHookDecision};
use crate::agent::tool_approval::{self, ToolApprovalBroker, ToolApprovalCall};
use crate::bus::outbound_events::{
    OutboundEvent, ToolApprovalRequestEvent, outbound_message_for_event,
};
use crate::bus::queue::MessageBus;
use crate::channels::websocket::CHANNEL_NAME as WEBSOCKET_CHANNEL;
use crate::cli::cancel::pause_for_prompt;
use crate::cli::stream::StreamRenderer;
use crate::providers::base::ToolCallRequest;

const ARG_PREVIEW_LIMIT: usize = 120;
/// How long a WebSocket approval request waits before denying the batch.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// CLI-only hook that asks before each tool call when stdin is a terminal.
pub struct CliAskHook {
    renderer: StdMutex<Option<Arc<Mutex<StreamRenderer>>>>,
}

impl CliAskHook {
    pub fn new() -> Self {
        Self {
            renderer: StdMutex::new(None),
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

fn preview_arguments(arguments: &HashMap<String, serde_json::Value>) -> String {
    tool_approval::preview_arguments(arguments, ARG_PREVIEW_LIMIT)
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

fn prompt_tool_calls(tool_calls: &[ToolCallRequest]) -> Vec<String> {
    let mut denied_ids = Vec::new();
    for tool_call in tool_calls {
        let args_preview = preview_arguments(&tool_call.arguments);
        let approved = prompt_for_tool(&tool_call.name, &args_preview);
        if !matches!(approved, Ok(true)) {
            denied_ids.push(tool_call.id.clone());
        }
    }
    denied_ids
}

fn decision_from_denied_ids(ids: Vec<String>) -> ToolHookDecision {
    if ids.is_empty() {
        ToolHookDecision::Continue
    } else {
        ToolHookDecision::DenyCalls {
            ids,
            reason: "User denied tool execution".into(),
        }
    }
}

#[async_trait]
impl AgentHook for CliAskHook {
    async fn before_execute_tools(&self, context: &mut AgentHookContext) -> ToolHookDecision {
        if context.tool_calls.is_empty() {
            return ToolHookDecision::Continue;
        }

        if !std::io::stdin().is_terminal() {
            log::warn!(
                "tools.confirmBeforeExecute is enabled but stdin is not a terminal; executing tools without prompts"
            );
            return ToolHookDecision::Continue;
        }

        if let Some(renderer) = self.renderer() {
            renderer.lock().await.stop_for_input();
        }
        let _pause = pause_for_prompt();
        let tool_calls = context.tool_calls.clone();
        let denied_ids = tokio::task::spawn_blocking(move || prompt_tool_calls(&tool_calls))
            .await
            .unwrap_or_else(|_| {
                context
                    .tool_calls
                    .iter()
                    .map(|tool_call| tool_call.id.clone())
                    .collect()
            });
        drop(_pause);
        if let Some(renderer) = self.renderer() {
            renderer.lock().await.resume_after_input();
        }
        decision_from_denied_ids(denied_ids)
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

fn denied_ids_from_approved(tool_calls: &[ToolCallRequest], approved: &[String]) -> Vec<String> {
    tool_calls
        .iter()
        .filter(|call| !approved.contains(&call.id))
        .map(|call| call.id.clone())
        .collect()
}

#[async_trait]
impl AgentHook for WebsocketsAskHook {
    async fn before_execute_tools(&self, context: &mut AgentHookContext) -> ToolHookDecision {
        if context.tool_calls.is_empty() {
            return ToolHookDecision::Continue;
        }
        let chat_id = match (context.channel.as_deref(), context.chat_id.as_deref()) {
            (Some(WEBSOCKET_CHANNEL), Some(chat_id)) if !chat_id.is_empty() => chat_id.to_string(),
            (channel, _) => {
                log::warn!(
                    "tools.confirmBeforeExecute is enabled but channel {:?} cannot prompt; executing tools without confirmation",
                    channel
                );
                return ToolHookDecision::Continue;
            }
        };
        let Some(bus) = self.bus() else {
            log::warn!(
                "tools.confirmBeforeExecute is enabled but no bus is wired; executing tools without confirmation"
            );
            return ToolHookDecision::Continue;
        };

        let calls: Vec<ToolApprovalCall> = context
            .tool_calls
            .iter()
            .map(ToolApprovalCall::from_request)
            .collect();
        let (request_id, rx) = self.broker.register(&chat_id, calls.clone());
        let _guard = PendingGuard {
            broker: &self.broker,
            request_id: request_id.clone(),
        };
        let outbound = outbound_message_for_event(
            WEBSOCKET_CHANNEL,
            &chat_id,
            OutboundEvent::ToolApprovalRequest(ToolApprovalRequestEvent {
                request_id: request_id.clone(),
                calls,
            }),
            None,
            None,
        );
        if let Err(e) = bus.publish_outbound(outbound) {
            log::error!("Failed to publish tool approval request: {e}");
            return decision_from_denied_ids(
                context.tool_calls.iter().map(|c| c.id.clone()).collect(),
            );
        }

        let approved = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(approved)) => approved,
            Ok(Err(_)) => Vec::new(),
            Err(_) => {
                log::warn!("Tool approval request {request_id} timed out; denying all calls");
                Vec::new()
            }
        };
        decision_from_denied_ids(denied_ids_from_approved(&context.tool_calls, &approved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_call(
        id: &str,
        name: &str,
        arguments: HashMap<String, serde_json::Value>,
    ) -> ToolCallRequest {
        ToolCallRequest {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }
    }

    #[test]
    fn preview_arguments_truncates_long_payloads() {
        let mut arguments = HashMap::new();
        arguments.insert(
            "content".to_string(),
            serde_json::Value::String("x".repeat(200)),
        );
        let preview = preview_arguments(&arguments);
        assert!(preview.ends_with("..."));
        assert!(preview.chars().count() <= ARG_PREVIEW_LIMIT + 3);
    }

    #[test]
    fn preview_arguments_keeps_short_payloads() {
        let mut arguments = HashMap::new();
        arguments.insert("path".to_string(), serde_json::json!("/tmp/foo.txt"));
        assert_eq!(
            preview_arguments(&arguments),
            serde_json::to_string(&arguments).unwrap()
        );
    }

    #[test]
    fn decision_from_denied_ids_continue_when_empty() {
        assert_eq!(
            decision_from_denied_ids(Vec::new()),
            ToolHookDecision::Continue
        );
    }

    #[test]
    fn decision_from_denied_ids_collects_only_declined_calls() {
        assert_eq!(
            decision_from_denied_ids(vec!["call_2".into(), "call_3".into()]),
            ToolHookDecision::DenyCalls {
                ids: vec!["call_2".into(), "call_3".into()],
                reason: "User denied tool execution".into(),
            }
        );
    }

    #[tokio::test]
    async fn skips_prompts_when_stdin_is_not_a_terminal() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let hook = CliAskHook::new();
        let mut ctx = AgentHookContext::new(0, vec![]);
        ctx.tool_calls.push(make_tool_call(
            "call_1",
            "read_file",
            HashMap::from([("path".to_string(), serde_json::json!("/tmp/foo.txt"))]),
        ));
        let decision = hook.before_execute_tools(&mut ctx).await;
        assert_eq!(decision, ToolHookDecision::Continue);
        assert_eq!(ctx.tool_calls.len(), 1);
    }

    fn websocket_ctx(ids: &[&str]) -> AgentHookContext {
        let mut ctx = AgentHookContext::new(0, vec![]);
        ctx.channel = Some(WEBSOCKET_CHANNEL.to_string());
        ctx.chat_id = Some("chat-1".to_string());
        for id in ids {
            ctx.tool_calls
                .push(make_tool_call(id, "exec", HashMap::new()));
        }
        ctx
    }

    fn websocket_hook(broker: &Arc<ToolApprovalBroker>) -> (Arc<WebsocketsAskHook>, Arc<MessageBus>) {
        let bus = Arc::new(MessageBus::new());
        let hook = WebsocketsAskHook::new(Arc::clone(broker));
        hook.set_bus(Arc::clone(&bus));
        (Arc::new(hook), bus)
    }

    /// Run the hook, answering the request it publishes with `answer`.
    async fn run_with_answer(
        ids: &[&str],
        answer: Option<Vec<String>>,
        timeout: Duration,
    ) -> ToolHookDecision {
        let broker = Arc::new(ToolApprovalBroker::new());
        let bus = Arc::new(MessageBus::new());
        let hook = WebsocketsAskHook::new(Arc::clone(&broker)).with_timeout(timeout);
        hook.set_bus(Arc::clone(&bus));
        let mut ctx = websocket_ctx(ids);
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
                if let Some(approved) = answer {
                    broker
                        .resolve("chat-1", &event.request_id, approved)
                        .unwrap();
                }
            })
        };
        let decision = hook.before_execute_tools(&mut ctx).await;
        responder.await.unwrap();
        assert!(broker.pending_for_chat("chat-1").is_empty());
        decision
    }

    #[tokio::test]
    async fn websocket_hook_continues_when_all_approved() {
        let decision = run_with_answer(
            &["a", "b"],
            Some(vec!["a".into(), "b".into()]),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(decision, ToolHookDecision::Continue);
    }

    #[tokio::test]
    async fn websocket_hook_denies_unapproved_calls() {
        let decision =
            run_with_answer(&["a", "b"], Some(vec!["b".into()]), Duration::from_secs(5)).await;
        assert_eq!(
            decision,
            ToolHookDecision::DenyCalls {
                ids: vec!["a".into()],
                reason: "User denied tool execution".into(),
            }
        );
    }

    #[tokio::test]
    async fn websocket_hook_denies_all_on_timeout() {
        let decision = run_with_answer(&["a", "b"], None, Duration::from_millis(50)).await;
        assert_eq!(
            decision,
            ToolHookDecision::DenyCalls {
                ids: vec!["a".into(), "b".into()],
                reason: "User denied tool execution".into(),
            }
        );
    }

    #[tokio::test]
    async fn websocket_hook_denies_all_when_request_cancelled() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let (hook, bus) = websocket_hook(&broker);
        let mut ctx = websocket_ctx(&["a"]);
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
        let decision = hook.before_execute_tools(&mut ctx).await;
        canceller.await.unwrap();
        assert!(matches!(decision, ToolHookDecision::DenyCalls { .. }));
    }

    #[tokio::test]
    async fn websocket_hook_skips_non_websocket_channels() {
        let broker = Arc::new(ToolApprovalBroker::new());
        let (hook, bus) = websocket_hook(&broker);
        let mut ctx = websocket_ctx(&["a"]);
        ctx.channel = Some("cli".into());
        assert_eq!(
            hook.before_execute_tools(&mut ctx).await,
            ToolHookDecision::Continue
        );
        assert_eq!(bus.outbound_size(), 0);
    }
}
