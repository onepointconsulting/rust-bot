use std::io::IsTerminal;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::lock::Mutex;
use inquire::{InquireError, Select, Text};
use serde_json::Value;

use crate::agent::question_broker::{QuestionAnswer, QuestionBroker, QuestionChoice};
use crate::agent::tools::base::Tool;
use crate::bus::outbound_events::{OutboundEvent, QuestionRequestEvent, outbound_message_for_event};
use crate::bus::queue::MessageBus;
use crate::channels::websocket::CHANNEL_NAME as WEBSOCKET_CHANNEL;
use crate::cli::pause_for_prompt;
use crate::cli::stream::StreamRenderer;
use crate::integrations::herdr::HerdrReporter;

pub const QUESTION_TOOL_NAME: &str = "question";

/// Sentinel choice appended to every CLI prompt so the user can always
/// escape a closed option list with a free-text answer.
const OTHER_SENTINEL: &str = "Other (type your own answer)";

/// How long a websocket-channel question waits for an answer before the
/// agent gets told the user cancelled. Longer than tool-approval's 300s
/// timeout since a clarifying question may need more thought than a yes/no.
const QUESTION_TIMEOUT: Duration = Duration::from_secs(600);

/// One selectable option in a `question` tool call.
#[derive(Debug, Clone, PartialEq)]
struct QuestionOption {
    label: String,
    description: Option<String>,
}

/// Pause the conversation and ask the user a clarifying multiple-choice
/// question, with an always-available free-text fallback.
///
/// The answer is returned as a plain string tool result (never a synthetic
/// `role: "user"` message) so it flows through the runner's normal
/// `role: "tool"` handling like every other tool. Works over two channels:
/// a CLI terminal (blocking `inquire` prompt) or the websockets-chat gateway
/// (round-trips an `OutboundEvent::QuestionRequest` through a
/// [`QuestionBroker`], answered by a `question_response` envelope) — which
/// path runs is decided by the channel `set_tool_context` last reported.
pub struct QuestionTool {
    renderer: StdMutex<Option<Arc<Mutex<StreamRenderer>>>>,
    herdr: StdMutex<Option<Arc<HerdrReporter>>>,
    channel: StdMutex<String>,
    chat_id: StdMutex<String>,
    bus: StdMutex<Option<Arc<MessageBus>>>,
    broker: Arc<QuestionBroker>,
}

impl QuestionTool {
    pub fn new(bus: Option<Arc<MessageBus>>) -> Self {
        Self {
            renderer: StdMutex::new(None),
            herdr: StdMutex::new(None),
            channel: StdMutex::new(String::new()),
            chat_id: StdMutex::new(String::new()),
            bus: StdMutex::new(bus),
            broker: Arc::new(QuestionBroker::new()),
        }
    }

    /// The broker backing this tool's websocket-channel questions, so
    /// `commands.rs` can inject it into `GatewayServices` for the WebSocket
    /// channel's inbound handler to resolve against.
    pub fn broker(&self) -> Arc<QuestionBroker> {
        Arc::clone(&self.broker)
    }

    pub fn set_renderer(&self, renderer: Arc<Mutex<StreamRenderer>>) {
        *self.renderer.lock().unwrap_or_else(|e| e.into_inner()) = Some(renderer);
    }

    pub fn clear_renderer(&self) {
        *self.renderer.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn set_herdr(&self, herdr: Arc<HerdrReporter>) {
        *self.herdr.lock().unwrap_or_else(|e| e.into_inner()) = Some(herdr);
    }

    pub fn clear_herdr(&self) {
        *self.herdr.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn renderer(&self) -> Option<Arc<Mutex<StreamRenderer>>> {
        self.renderer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn herdr(&self) -> Option<Arc<HerdrReporter>> {
        self.herdr.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn channel(&self) -> String {
        self.channel.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn chat_id(&self) -> String {
        self.chat_id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn bus(&self) -> Option<Arc<MessageBus>> {
        self.bus.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Blocking CLI prompt, unchanged from the terminal-only original.
    async fn execute_cli(&self, question: String, options: Vec<QuestionOption>) -> String {
        if !std::io::stdin().is_terminal() {
            return "Error: interactive input not available in this context (no TTY); \
                    cannot ask a clarifying question here"
                .to_string();
        }

        if let Some(renderer) = self.renderer() {
            renderer.lock().await.stop_for_input();
        }
        if let Some(herdr) = self.herdr() {
            herdr.report_blocked(Some(question.clone()));
        }

        let _pause = pause_for_prompt();
        let question_for_prompt = question.clone();
        let options_for_prompt = options.clone();
        let result = tokio::task::spawn_blocking(move || {
            run_blocking_prompt(&question_for_prompt, &options_for_prompt)
        })
        .await;
        drop(_pause);

        if let Some(herdr) = self.herdr() {
            herdr.report_working();
        }
        if let Some(renderer) = self.renderer() {
            renderer.lock().await.resume_after_input();
        }

        match result {
            Ok(Ok(answer)) => answer,
            Ok(Err(InquireError::OperationCanceled | InquireError::OperationInterrupted)) => {
                CANCELLED_ANSWER.to_string()
            }
            Ok(Err(e)) => format!("Error: failed to prompt user: {e}"),
            Err(join_err) => format!("Error: prompt task failed: {join_err}"),
        }
    }

    /// Round-trip the question through the websockets-chat gateway: publish
    /// an `OutboundEvent::QuestionRequest`, then wait on the `QuestionBroker`
    /// for a `question_response` envelope to resolve it.
    async fn execute_websocket(&self, question: String, options: Vec<QuestionOption>) -> String {
        let Some(bus) = self.bus() else {
            return "Error: message bus not configured; cannot ask a clarifying question here"
                .to_string();
        };
        let chat_id = self.chat_id();
        if chat_id.is_empty() {
            return "Error: no chat to ask a clarifying question in".to_string();
        }

        let choices: Vec<QuestionChoice> = options
            .iter()
            .enumerate()
            .map(|(i, opt)| QuestionChoice {
                id: i.to_string(),
                label: opt.label.clone(),
                description: opt.description.clone(),
            })
            .collect();

        let (request_id, rx) = self
            .broker
            .register(&chat_id, question.clone(), choices.clone());
        let _guard = QuestionPendingGuard {
            broker: &self.broker,
            request_id: request_id.clone(),
        };

        let outbound = outbound_message_for_event(
            WEBSOCKET_CHANNEL,
            &chat_id,
            OutboundEvent::QuestionRequest(QuestionRequestEvent {
                request_id,
                question,
                options: choices,
            }),
            None,
            None,
        );
        if let Err(e) = bus.publish_outbound(outbound) {
            return format!("Error: failed to publish question: {e}");
        }

        let answer: QuestionAnswer = match tokio::time::timeout(QUESTION_TIMEOUT, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) | Err(_) => return CANCELLED_ANSWER.to_string(),
        };

        let chosen = answer
            .option_id
            .as_ref()
            .and_then(|id| id.parse::<usize>().ok())
            .and_then(|i| options.get(i));
        format_answer(chosen, answer.free_text.as_deref())
    }
}

impl Default for QuestionTool {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Removes the broker entry if the tool call is dropped/aborted while
/// waiting — mirrors `confirm_tools.rs`'s `PendingGuard`.
struct QuestionPendingGuard<'a> {
    broker: &'a QuestionBroker,
    request_id: String,
}

impl Drop for QuestionPendingGuard<'_> {
    fn drop(&mut self) {
        self.broker.cancel(&self.request_id);
    }
}

/// Validate and extract `question`/`options` from the tool call parameters.
fn parse_options(params: &Value) -> Result<(String, Vec<QuestionOption>), String> {
    let question = params
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if question.is_empty() {
        return Err("missing required parameter 'question'".to_string());
    }

    let raw_options = params
        .get("options")
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
        .ok_or_else(|| "missing required parameter 'options' (must be a non-empty array)".to_string())?;

    let mut options = Vec::with_capacity(raw_options.len());
    for (i, raw) in raw_options.iter().enumerate() {
        let label = raw
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if label.is_empty() {
            return Err(format!("options[{i}] missing required field 'label'"));
        }
        let description = raw
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        options.push(QuestionOption {
            label: label.to_string(),
            description,
        });
    }

    Ok((question.to_string(), options))
}

/// Render each option as `"label"` or `"label (description)"`, plus a
/// trailing free-text sentinel.
fn build_choice_labels(options: &[QuestionOption]) -> Vec<String> {
    let mut labels: Vec<String> = options
        .iter()
        .map(|opt| match &opt.description {
            Some(desc) => format!("{} ({})", opt.label, desc),
            None => opt.label.clone(),
        })
        .collect();
    labels.push(OTHER_SENTINEL.to_string());
    labels
}

/// Build the natural-language string returned to the LLM as the tool result.
fn format_answer(chosen: Option<&QuestionOption>, free_text: Option<&str>) -> String {
    if let Some(free_text) = free_text {
        return format!("User answered: {free_text}");
    }
    match chosen {
        Some(QuestionOption {
            label,
            description: Some(description),
        }) => format!("User selected: {label} ({description})"),
        Some(QuestionOption { label, .. }) => format!("User selected: {label}"),
        None => CANCELLED_ANSWER.to_string(),
    }
}

const CANCELLED_ANSWER: &str = "User cancelled the question without answering.";

/// Blocking prompt: runs on a `spawn_blocking` thread, never on the async runtime.
fn run_blocking_prompt(question: &str, options: &[QuestionOption]) -> Result<String, InquireError> {
    let choices = build_choice_labels(options);
    let selection = Select::new(question, choices).prompt()?;

    if selection == OTHER_SENTINEL {
        let free_text = Text::new("Your answer:").prompt()?;
        Ok(format_answer(None, Some(&free_text)))
    } else {
        let chosen = options.iter().find(|opt| {
            selection == opt.label
                || opt
                    .description
                    .as_deref()
                    .is_some_and(|desc| selection == format!("{} ({})", opt.label, desc))
        });
        Ok(format_answer(chosen, None))
    }
}

#[async_trait]
impl Tool for QuestionTool {
    fn name(&self) -> String {
        QUESTION_TOOL_NAME.to_string()
    }

    fn description(&self) -> String {
        "Pause and ask the user a clarifying multiple-choice question when a task is \
         ambiguous — do not guess. The user can pick one of your options or type a \
         free-text answer instead. Only usable in an interactive CLI terminal session \
         or a websockets-chat web session; it will fail in any other context."
            .to_string()
    }

    fn exclusive(&self) -> bool {
        true
    }

    fn set_tool_context(&self, channel: &str, chat_id: &str, _message_id: Option<&str>) {
        *self.channel.lock().unwrap_or_else(|e| e.into_inner()) = channel.to_string();
        *self.chat_id.lock().unwrap_or_else(|e| e.into_inner()) = chat_id.to_string();
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The clarifying question to ask the user.",
                },
                "options": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Multiple-choice options to present. The user can \
                        always type a free-text answer instead of picking one.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Short option text shown in the picker.",
                            },
                            "description": {
                                "type": "string",
                                "description": "Optional longer explanation of this option.",
                            },
                        },
                        "required": ["label"],
                    },
                },
            },
            "required": ["question", "options"],
        })
    }

    async fn execute(&self, params: &Value) -> String {
        let (question, options) = match parse_options(params) {
            Ok(v) => v,
            Err(e) => return format!("Error: {e}"),
        };

        if self.channel() == WEBSOCKET_CHANNEL {
            self.execute_websocket(question, options).await
        } else {
            self.execute_cli(question, options).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_options ───────────────────────────────────────────────────────

    #[test]
    fn parse_options_missing_question_errors() {
        let err = parse_options(&serde_json::json!({ "options": [{"label": "A"}] })).unwrap_err();
        assert!(err.contains("question"));
    }

    #[test]
    fn parse_options_blank_question_errors() {
        let err = parse_options(&serde_json::json!({
            "question": "   ",
            "options": [{"label": "A"}],
        }))
        .unwrap_err();
        assert!(err.contains("question"));
    }

    #[test]
    fn parse_options_missing_options_errors() {
        let err = parse_options(&serde_json::json!({ "question": "Which one?" })).unwrap_err();
        assert!(err.contains("options"));
    }

    #[test]
    fn parse_options_empty_options_errors() {
        let err = parse_options(&serde_json::json!({
            "question": "Which one?",
            "options": [],
        }))
        .unwrap_err();
        assert!(err.contains("options"));
    }

    #[test]
    fn parse_options_option_missing_label_errors() {
        let err = parse_options(&serde_json::json!({
            "question": "Which one?",
            "options": [{"description": "no label here"}],
        }))
        .unwrap_err();
        assert!(err.contains("label"));
    }

    #[test]
    fn parse_options_happy_path_with_and_without_description() {
        let (question, options) = parse_options(&serde_json::json!({
            "question": "Which one?",
            "options": [
                {"label": "A", "description": "first"},
                {"label": "B"},
            ],
        }))
        .unwrap();
        assert_eq!(question, "Which one?");
        assert_eq!(
            options,
            vec![
                QuestionOption {
                    label: "A".to_string(),
                    description: Some("first".to_string()),
                },
                QuestionOption {
                    label: "B".to_string(),
                    description: None,
                },
            ]
        );
    }

    // ── build_choice_labels ─────────────────────────────────────────────────

    #[test]
    fn build_choice_labels_appends_sentinel_and_renders_descriptions() {
        let options = vec![
            QuestionOption {
                label: "A".to_string(),
                description: Some("first".to_string()),
            },
            QuestionOption {
                label: "B".to_string(),
                description: None,
            },
        ];
        let labels = build_choice_labels(&options);
        assert_eq!(
            labels,
            vec![
                "A (first)".to_string(),
                "B".to_string(),
                OTHER_SENTINEL.to_string(),
            ]
        );
    }

    // ── format_answer ───────────────────────────────────────────────────────

    #[test]
    fn format_answer_option_without_description() {
        let opt = QuestionOption {
            label: "A".to_string(),
            description: None,
        };
        assert_eq!(format_answer(Some(&opt), None), "User selected: A");
    }

    #[test]
    fn format_answer_option_with_description() {
        let opt = QuestionOption {
            label: "A".to_string(),
            description: Some("first".to_string()),
        };
        assert_eq!(
            format_answer(Some(&opt), None),
            "User selected: A (first)"
        );
    }

    #[test]
    fn format_answer_free_text() {
        assert_eq!(
            format_answer(None, Some("my own answer")),
            "User answered: my own answer"
        );
    }

    #[test]
    fn format_answer_cancelled() {
        assert_eq!(format_answer(None, None), CANCELLED_ANSWER);
    }

    // ── Tool surface ────────────────────────────────────────────────────────

    #[test]
    fn name_is_question() {
        let tool = QuestionTool::new(None);
        assert_eq!(tool.name(), "question");
    }

    #[test]
    fn exclusive_is_true_and_concurrency_unsafe() {
        let tool = QuestionTool::new(None);
        assert!(tool.exclusive());
        assert!(!tool.concurrency_safe());
    }

    #[test]
    fn parameters_require_question_and_options_with_label() {
        let tool = QuestionTool::new(None);
        let params = tool.parameters();
        assert_eq!(
            params["required"],
            serde_json::json!(["question", "options"])
        );
        assert_eq!(
            params["properties"]["options"]["items"]["required"],
            serde_json::json!(["label"])
        );
    }

    // ── execute: schema-error path (TTY-independent) ───────────────────────

    #[tokio::test]
    async fn execute_missing_question_returns_error() {
        let tool = QuestionTool::new(None);
        let result = tool
            .execute(&serde_json::json!({ "options": [{"label": "A"}] }))
            .await;
        assert!(result.starts_with("Error:"));
        assert!(result.contains("question"));
    }

    #[tokio::test]
    async fn execute_missing_options_returns_error() {
        let tool = QuestionTool::new(None);
        let result = tool
            .execute(&serde_json::json!({ "question": "Which one?" }))
            .await;
        assert!(result.starts_with("Error:"));
        assert!(result.contains("options"));
    }

    // ── execute: non-TTY path (default/CLI channel) ─────────────────────────

    #[tokio::test]
    async fn execute_non_tty_returns_error_without_hanging() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let tool = QuestionTool::new(None);
        tool.set_herdr(HerdrReporter::disabled_for_test());
        let result = tool
            .execute(&serde_json::json!({
                "question": "Which one?",
                "options": [{"label": "A"}],
            }))
            .await;
        assert_eq!(
            result,
            "Error: interactive input not available in this context (no TTY); \
             cannot ask a clarifying question here"
        );
    }

    // ── execute: websocket channel ──────────────────────────────────────────

    #[tokio::test]
    async fn execute_websocket_without_bus_returns_error() {
        let tool = QuestionTool::new(None);
        tool.set_tool_context(WEBSOCKET_CHANNEL, "chat-1", None);
        let result = tool
            .execute(&serde_json::json!({
                "question": "Which one?",
                "options": [{"label": "A"}],
            }))
            .await;
        assert_eq!(
            result,
            "Error: message bus not configured; cannot ask a clarifying question here"
        );
    }

    #[tokio::test]
    async fn execute_websocket_without_chat_id_returns_error() {
        let bus = Arc::new(MessageBus::new());
        let tool = QuestionTool::new(Some(bus));
        tool.set_tool_context(WEBSOCKET_CHANNEL, "", None);
        let result = tool
            .execute(&serde_json::json!({
                "question": "Which one?",
                "options": [{"label": "A"}],
            }))
            .await;
        assert_eq!(
            result,
            "Error: no chat to ask a clarifying question in"
        );
    }

    #[tokio::test]
    async fn execute_websocket_resolves_selected_option() {
        let bus = Arc::new(MessageBus::new());
        let tool = QuestionTool::new(Some(Arc::clone(&bus)));
        tool.set_tool_context(WEBSOCKET_CHANNEL, "chat-1", None);
        let broker = tool.broker();

        let run = tokio::spawn(async move {
            tool.execute(&serde_json::json!({
                "question": "Which one?",
                "options": [{"label": "A"}, {"label": "B", "description": "second"}],
            }))
            .await
        });

        // Wait for the request to actually be registered before resolving it.
        let request_id = loop {
            let pending = broker.pending_for_chat("chat-1");
            if let Some(entry) = pending.into_iter().next() {
                break entry.request_id;
            }
            tokio::task::yield_now().await;
        };
        broker
            .resolve("chat-1", &request_id, Some("1".to_string()), None)
            .unwrap();

        let result = run.await.unwrap();
        assert_eq!(result, "User selected: B (second)");
    }

    #[tokio::test]
    async fn execute_websocket_publishes_question_request_event() {
        let bus = Arc::new(MessageBus::new());
        let tool = QuestionTool::new(Some(Arc::clone(&bus)));
        tool.set_tool_context(WEBSOCKET_CHANNEL, "chat-1", None);
        let broker = tool.broker();

        let run = tokio::spawn(async move {
            tool.execute(&serde_json::json!({
                "question": "Which one?",
                "options": [{"label": "A"}],
            }))
            .await
        });

        let outbound = bus.consume_outbound().await.expect("event published");
        assert_eq!(outbound.channel, WEBSOCKET_CHANNEL);
        assert_eq!(outbound.chat_id, "chat-1");
        let Some(OutboundEvent::QuestionRequest(event)) = outbound.event else {
            panic!("expected QuestionRequest event");
        };
        assert_eq!(event.question, "Which one?");
        assert_eq!(event.options, vec![QuestionChoice {
            id: "0".to_string(),
            label: "A".to_string(),
            description: None,
        }]);

        broker
            .resolve("chat-1", &event.request_id, None, Some("free text".to_string()))
            .unwrap();
        assert_eq!(run.await.unwrap(), "User answered: free text");
    }

    #[tokio::test]
    async fn execute_websocket_cancel_returns_cancelled_answer() {
        let bus = Arc::new(MessageBus::new());
        let tool = QuestionTool::new(Some(Arc::clone(&bus)));
        tool.set_tool_context(WEBSOCKET_CHANNEL, "chat-1", None);
        let broker = tool.broker();

        let run = tokio::spawn(async move {
            tool.execute(&serde_json::json!({
                "question": "Which one?",
                "options": [{"label": "A"}],
            }))
            .await
        });

        let request_id = loop {
            let pending = broker.pending_for_chat("chat-1");
            if let Some(entry) = pending.into_iter().next() {
                break entry.request_id;
            }
            tokio::task::yield_now().await;
        };
        broker.cancel(&request_id);

        assert_eq!(run.await.unwrap(), CANCELLED_ANSWER);
    }
}
