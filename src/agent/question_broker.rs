//! Pending clarifying-question requests shared between `QuestionTool`
//! (which waits) and the WebSocket channel (which relays the user's answer).
//!
//! Mirrors [`crate::agent::tool_approval::ToolApprovalBroker`]'s shape: an
//! `oneshot` per request, an `Arc<Mutex<HashMap<request_id, Pending>>>`, a
//! drop-guard cancel, and a chat-id-checked `resolve` — but for a question's
//! answer instead of a set of approved tool-call ids.
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

pub use crate::agent::tool_approval::ResolveError;

/// One option offered to the user, as presented on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionChoice {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The user's answer: the id of a [`QuestionChoice`] they picked, free text,
/// a request to chat about the question instead, or none of these if the
/// request timed out or was cancelled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuestionAnswer {
    pub option_id: Option<String>,
    pub free_text: Option<String>,
    pub chat_about: bool,
}

struct PendingQuestion {
    chat_id: String,
    question: String,
    options: Vec<QuestionChoice>,
    tx: oneshot::Sender<QuestionAnswer>,
}

/// A request still awaiting an answer, as needed to re-send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQuestionView {
    pub request_id: String,
    pub question: String,
    pub options: Vec<QuestionChoice>,
}

#[derive(Default)]
pub struct QuestionBroker {
    pending: StdMutex<HashMap<String, PendingQuestion>>,
}

impl QuestionBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new request. The receiver yields the user's answer.
    pub fn register(
        &self,
        chat_id: &str,
        question: String,
        options: Vec<QuestionChoice>,
    ) -> (String, oneshot::Receiver<QuestionAnswer>) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.lock().insert(
            request_id.clone(),
            PendingQuestion {
                chat_id: chat_id.to_string(),
                question,
                options,
                tx,
            },
        );
        (request_id, rx)
    }

    /// Deliver the user's answer. An `option_id` that wasn't actually offered
    /// is dropped (so a client can only answer with what it was shown), but
    /// any `free_text` still comes through — a malformed/unknown option id
    /// degrades to free text or nothing, never an error.
    pub fn resolve(
        &self,
        chat_id: &str,
        request_id: &str,
        option_id: Option<String>,
        free_text: Option<String>,
        chat_about: bool,
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
        let option_id = option_id.filter(|id| entry.options.iter().any(|opt| &opt.id == id));
        // The waiter may already be gone (turn aborted); nothing to do then.
        let _ = entry.tx.send(QuestionAnswer {
            option_id,
            free_text,
            chat_about,
        });
        Ok(())
    }

    /// Forget a request without answering it (the waiter was cancelled).
    pub fn cancel(&self, request_id: &str) {
        self.lock().remove(request_id);
    }

    pub fn pending_for_chat(&self, chat_id: &str) -> Vec<PendingQuestionView> {
        self.lock()
            .iter()
            .filter(|(_, entry)| entry.chat_id == chat_id)
            .map(|(request_id, entry)| PendingQuestionView {
                request_id: request_id.clone(),
                question: entry.question.clone(),
                options: entry.options.clone(),
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingQuestion>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(id: &str) -> QuestionChoice {
        QuestionChoice {
            id: id.to_string(),
            label: format!("Option {id}"),
            description: None,
        }
    }

    #[tokio::test]
    async fn resolve_delivers_matching_option_id() {
        let broker = QuestionBroker::new();
        let (request_id, rx) =
            broker.register("chat-1", "Which?".to_string(), vec![choice("0"), choice("1")]);
        broker
            .resolve("chat-1", &request_id, Some("1".to_string()), None, false)
            .unwrap();
        let answer = rx.await.unwrap();
        assert_eq!(answer.option_id.as_deref(), Some("1"));
        assert_eq!(answer.free_text, None);
        assert!(broker.pending_for_chat("chat-1").is_empty());
    }

    #[tokio::test]
    async fn resolve_drops_unknown_option_id_but_keeps_free_text() {
        let broker = QuestionBroker::new();
        let (request_id, rx) = broker.register("chat-1", "Which?".to_string(), vec![choice("0")]);
        broker
            .resolve(
                "chat-1",
                &request_id,
                Some("zzz".to_string()),
                Some("my own answer".to_string()),
                false,
            )
            .unwrap();
        let answer = rx.await.unwrap();
        assert_eq!(answer.option_id, None);
        assert_eq!(answer.free_text.as_deref(), Some("my own answer"));
    }

    #[tokio::test]
    async fn resolve_with_no_answer_delivers_empty_answer() {
        let broker = QuestionBroker::new();
        let (request_id, rx) = broker.register("chat-1", "Which?".to_string(), vec![choice("0")]);
        broker.resolve("chat-1", &request_id, None, None, false).unwrap();
        assert_eq!(rx.await.unwrap(), QuestionAnswer::default());
    }

    #[tokio::test]
    async fn resolve_delivers_chat_about() {
        let broker = QuestionBroker::new();
        let (request_id, rx) = broker.register("chat-1", "Which?".to_string(), vec![choice("0")]);
        broker.resolve("chat-1", &request_id, None, None, true).unwrap();
        assert!(rx.await.unwrap().chat_about);
    }

    #[test]
    fn resolve_rejects_unknown_request() {
        let broker = QuestionBroker::new();
        assert_eq!(
            broker.resolve("chat-1", "nope", None, None, false),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn resolve_rejects_other_chat_and_keeps_request() {
        let broker = QuestionBroker::new();
        let (request_id, _rx) =
            broker.register("chat-1", "Which?".to_string(), vec![choice("0")]);
        assert_eq!(
            broker.resolve("chat-2", &request_id, Some("0".to_string()), None, false),
            Err(ResolveError::ChatMismatch)
        );
        assert_eq!(broker.pending_for_chat("chat-1").len(), 1);
    }

    #[tokio::test]
    async fn cancel_drops_sender() {
        let broker = QuestionBroker::new();
        let (request_id, rx) = broker.register("chat-1", "Which?".to_string(), vec![choice("0")]);
        broker.cancel(&request_id);
        assert!(rx.await.is_err());
        assert!(broker.pending_for_chat("chat-1").is_empty());
    }

    #[test]
    fn pending_for_chat_filters_by_chat() {
        let broker = QuestionBroker::new();
        let (first, _rx1) = broker.register("chat-1", "A?".to_string(), vec![choice("0")]);
        let (_second, _rx2) = broker.register("chat-2", "B?".to_string(), vec![choice("0")]);
        let pending = broker.pending_for_chat("chat-1");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, first);
        assert_eq!(pending[0].question, "A?");
        assert_eq!(pending[0].options, vec![choice("0")]);
    }
}
