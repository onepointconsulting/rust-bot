//! Pending tool-call approvals shared between an approval hook (which waits)
//! and a channel (which relays the user's answer).
//!
//! One broker instance is shared process-wide, so every pending request is
//! keyed by a generated `request_id` and pinned to the `chat_id` that asked.
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use serde::Serialize;
use tokio::sync::oneshot;

use crate::providers::base::ToolCallRequest;

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
    pub fn from_request(call: &ToolCallRequest) -> Self {
        Self {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_preview: preview_arguments(&call.arguments, APPROVAL_ARG_PREVIEW_LIMIT),
        }
    }
}

/// JSON-encode `arguments`, truncated to `limit` characters (with `...`).
pub fn preview_arguments(arguments: &HashMap<String, serde_json::Value>, limit: usize) -> String {
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
    tx: oneshot::Sender<Vec<String>>,
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
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.lock().insert(
            request_id.clone(),
            PendingApproval {
                chat_id: chat_id.to_string(),
                calls,
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
        let approved = approved_ids
            .into_iter()
            .filter(|id| entry.calls.iter().any(|call| &call.id == id))
            .collect();
        // The waiter may already be gone (turn aborted); nothing to do then.
        let _ = entry.tx.send(approved);
        Ok(())
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
        let mut arguments = HashMap::new();
        arguments.insert(
            "content".to_string(),
            serde_json::Value::String("x".repeat(200)),
        );
        let preview = preview_arguments(&arguments, 50);
        assert!(preview.ends_with("..."));
        assert_eq!(preview.chars().count(), 53);
    }
}
