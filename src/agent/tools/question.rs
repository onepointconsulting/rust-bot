use std::io::IsTerminal;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::lock::Mutex;
use inquire::{InquireError, Select, Text};
use serde_json::Value;

use crate::agent::tools::base::Tool;
use crate::cli::pause_for_prompt;
use crate::cli::stream::StreamRenderer;
use crate::integrations::herdr::HerdrReporter;

pub const QUESTION_TOOL_NAME: &str = "question";

/// Sentinel choice appended to every prompt so the user can always escape a
/// closed option list with a free-text answer.
const OTHER_SENTINEL: &str = "Other (type your own answer)";

/// One selectable option in a `question` tool call.
#[derive(Debug, Clone, PartialEq)]
struct QuestionOption {
    label: String,
    description: Option<String>,
}

/// Pause the CLI and ask the user a clarifying multiple-choice question,
/// with an always-available free-text fallback.
///
/// The answer is returned as a plain string tool result (never a synthetic
/// `role: "user"` message) so it flows through the runner's normal
/// `role: "tool"` handling like every other tool.
pub struct QuestionTool {
    renderer: StdMutex<Option<Arc<Mutex<StreamRenderer>>>>,
    herdr: StdMutex<Option<Arc<HerdrReporter>>>,
}

impl QuestionTool {
    pub fn new() -> Self {
        Self {
            renderer: StdMutex::new(None),
            herdr: StdMutex::new(None),
        }
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
}

impl Default for QuestionTool {
    fn default() -> Self {
        Self::new()
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
         free-text answer instead. Only usable in an interactive CLI terminal session; \
         it will fail in non-interactive contexts."
            .to_string()
    }

    fn exclusive(&self) -> bool {
        true
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
        let tool = QuestionTool::new();
        assert_eq!(tool.name(), "question");
    }

    #[test]
    fn exclusive_is_true_and_concurrency_unsafe() {
        let tool = QuestionTool::new();
        assert!(tool.exclusive());
        assert!(!tool.concurrency_safe());
    }

    #[test]
    fn parameters_require_question_and_options_with_label() {
        let tool = QuestionTool::new();
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
        let tool = QuestionTool::new();
        let result = tool
            .execute(&serde_json::json!({ "options": [{"label": "A"}] }))
            .await;
        assert!(result.starts_with("Error:"));
        assert!(result.contains("question"));
    }

    #[tokio::test]
    async fn execute_missing_options_returns_error() {
        let tool = QuestionTool::new();
        let result = tool
            .execute(&serde_json::json!({ "question": "Which one?" }))
            .await;
        assert!(result.starts_with("Error:"));
        assert!(result.contains("options"));
    }

    // ── execute: non-TTY path ────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_non_tty_returns_error_without_hanging() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let tool = QuestionTool::new();
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
}
