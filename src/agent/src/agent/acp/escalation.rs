//! Asking the human on behalf of a child agent (`permissionPolicy: escalate`).
//!
//! A child asks before every call that can change something. Under `escalate`
//! the parent forwards the question to the person it is talking to, through the
//! same tool-approval broker and web-socket approval prompt that
//! `tools.confirmBeforeExecute` uses, on the chat the parent's own turn came from.
//!
//! Only a channel that can actually ask is escalated to (the web-socket chat
//! today). Everything else, and every failure on the way (no bus, a bus error, a
//! timeout, a closed request), **denies**: nobody answering is never a yes.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::agent::acp::permission::{Escalator, PermissionAsk};
use crate::agent::tool_approval::{
    APPROVAL_ARG_PREVIEW_LIMIT, ToolApprovalBroker, ToolApprovalCall, preview_arguments,
};
use crate::bus::outbound_events::{
    OutboundEvent, ToolApprovalRequestEvent, outbound_message_for_event,
};
use crate::bus::queue::MessageBus;
use crate::channels::websocket::CHANNEL_NAME as WEBSOCKET_CHANNEL;

/// How long the human has to answer before the call is denied.
pub const ESCALATION_TIMEOUT: Duration = Duration::from_secs(300);

/// The means of asking: the broker that holds pending requests and the bus that
/// carries them to the channel.
#[derive(Clone)]
pub struct EscalationWiring {
    pub broker: Arc<ToolApprovalBroker>,
    pub bus: Arc<MessageBus>,
}

/// Escalates to the web-socket chat `chat_id`.
pub struct BrokerEscalator {
    wiring: EscalationWiring,
    chat_id: String,
    timeout: Duration,
}

/// The escalator for a turn from `channel` / `chat_id`, if that channel can ask.
pub fn escalator_for(
    wiring: Option<&EscalationWiring>,
    channel: &str,
    chat_id: &str,
) -> Option<Arc<dyn Escalator>> {
    let wiring = wiring?;
    if channel != WEBSOCKET_CHANNEL || chat_id.is_empty() {
        return None;
    }
    Some(Arc::new(BrokerEscalator {
        wiring: wiring.clone(),
        chat_id: chat_id.to_string(),
        timeout: ESCALATION_TIMEOUT,
    }))
}

/// Removes the broker entry when the waiting turn ends or is dropped.
struct PendingGuard<'a> {
    broker: &'a ToolApprovalBroker,
    request_id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.broker.cancel(&self.request_id);
    }
}

/// The call as the approver sees it.
fn call_for(ask: &PermissionAsk) -> ToolApprovalCall {
    let arguments_preview = match &ask.raw_input {
        Some(input) => {
            let arguments = input
                .as_object()
                .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            preview_arguments(&arguments, APPROVAL_ARG_PREVIEW_LIMIT)
        }
        None => String::new(),
    };
    ToolApprovalCall {
        id: ask.tool_call_id.clone(),
        name: format!("agent: {}", ask.title),
        arguments_preview,
    }
}

#[async_trait]
impl Escalator for BrokerEscalator {
    async fn ask(&self, ask: &PermissionAsk) -> bool {
        let call = call_for(ask);
        let (request_id, answer) = self
            .wiring
            .broker
            .register(&self.chat_id, vec![call.clone()]);
        let _guard = PendingGuard {
            broker: &self.wiring.broker,
            request_id: request_id.clone(),
        };
        let outbound = outbound_message_for_event(
            WEBSOCKET_CHANNEL,
            &self.chat_id,
            OutboundEvent::ToolApprovalRequest(ToolApprovalRequestEvent {
                request_id: request_id.clone(),
                calls: vec![call],
            }),
            None,
            None,
        );
        if let Err(error) = self.wiring.bus.publish_outbound(outbound) {
            log::error!("cannot publish the approval request for a child agent: {error}");
            return false;
        }
        match tokio::time::timeout(self.timeout, answer).await {
            Ok(Ok(approved)) => approved.contains(&ask.tool_call_id),
            Ok(Err(_)) => false,
            Err(_) => {
                log::warn!("approval request {request_id} for a child agent timed out; denying");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ToolKind;
    use serde_json::json;

    fn ask() -> PermissionAsk {
        PermissionAsk {
            tool_call_id: "child-call-1".to_string(),
            title: "shell cargo test".to_string(),
            kind: ToolKind::Execute,
            raw_input: Some(json!({"command": "cargo test"})),
        }
    }

    fn wiring() -> EscalationWiring {
        EscalationWiring {
            broker: Arc::new(ToolApprovalBroker::new()),
            bus: Arc::new(MessageBus::new()),
        }
    }

    fn escalator(wiring: &EscalationWiring, timeout: Duration) -> BrokerEscalator {
        BrokerEscalator {
            wiring: wiring.clone(),
            chat_id: "chat-1".to_string(),
            timeout,
        }
    }

    /// Ask, and answer the request the escalator publishes with `answer`.
    async fn ask_and_answer(answer: Option<Vec<String>>, timeout: Duration) -> bool {
        let wiring = wiring();
        let escalator = escalator(&wiring, timeout);
        let responder = {
            let wiring = wiring.clone();
            tokio::spawn(async move {
                let message = wiring
                    .bus
                    .consume_outbound()
                    .await
                    .expect("a request is published");
                assert_eq!(message.channel, WEBSOCKET_CHANNEL);
                assert_eq!(message.chat_id, "chat-1");
                let Some(OutboundEvent::ToolApprovalRequest(event)) = message.event else {
                    panic!("expected a tool approval request");
                };
                assert_eq!(event.calls.len(), 1);
                assert_eq!(event.calls[0].id, "child-call-1");
                assert!(event.calls[0].name.contains("shell cargo test"));
                assert!(event.calls[0].arguments_preview.contains("cargo test"));
                if let Some(approved) = answer {
                    wiring
                        .broker
                        .resolve("chat-1", &event.request_id, approved)
                        .unwrap();
                }
            })
        };
        let allowed = escalator.ask(&ask()).await;
        responder.await.unwrap();
        assert!(
            wiring.broker.pending_for_chat("chat-1").is_empty(),
            "no request is left pending"
        );
        allowed
    }

    #[tokio::test]
    async fn an_approval_from_the_human_allows_the_call() {
        assert!(ask_and_answer(Some(vec!["child-call-1".into()]), Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn an_empty_answer_denies() {
        assert!(!ask_and_answer(Some(Vec::new()), Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn an_approval_of_some_other_call_denies() {
        assert!(!ask_and_answer(Some(vec!["something-else".into()]), Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn no_answer_in_time_denies() {
        assert!(!ask_and_answer(None, Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn a_cancelled_request_denies() {
        let wiring = wiring();
        let escalator = escalator(&wiring, Duration::from_secs(5));
        let canceller = {
            let wiring = wiring.clone();
            tokio::spawn(async move {
                let message = wiring.bus.consume_outbound().await.unwrap();
                let Some(OutboundEvent::ToolApprovalRequest(event)) = message.event else {
                    panic!("expected a tool approval request");
                };
                wiring.broker.cancel(&event.request_id);
            })
        };
        assert!(!escalator.ask(&ask()).await);
        canceller.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_the_wait_removes_the_pending_request() {
        let wiring = wiring();
        let escalator = escalator(&wiring, Duration::from_secs(60));
        let waiting = tokio::spawn(async move { escalator.ask(&ask()).await });
        // Wait until the request is registered, then abort the waiter (a stopped turn).
        let _ = wiring.bus.consume_outbound().await.unwrap();
        assert_eq!(wiring.broker.pending_for_chat("chat-1").len(), 1);
        waiting.abort();
        let _ = waiting.await;
        assert!(wiring.broker.pending_for_chat("chat-1").is_empty());
    }

    #[test]
    fn only_the_websocket_channel_gets_an_escalator() {
        let wiring = wiring();
        assert!(escalator_for(Some(&wiring), WEBSOCKET_CHANNEL, "chat-1").is_some());
        assert!(escalator_for(Some(&wiring), "cli", "direct").is_none());
        assert!(escalator_for(Some(&wiring), "telegram", "42").is_none());
        // An empty chat id cannot be addressed.
        assert!(escalator_for(Some(&wiring), WEBSOCKET_CHANNEL, "").is_none());
        // Nothing wired (no gateway): no escalator, so the policy denies.
        assert!(escalator_for(None, WEBSOCKET_CHANNEL, "chat-1").is_none());
    }

    #[test]
    fn a_request_without_input_still_describes_the_call() {
        let mut bare = ask();
        bare.raw_input = None;
        let call = call_for(&bare);
        assert_eq!(call.id, "child-call-1");
        assert!(call.arguments_preview.is_empty());
    }
}
