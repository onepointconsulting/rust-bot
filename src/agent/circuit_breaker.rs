use std::collections::HashMap;

use serde_json::Value;

use crate::agent::tools::message::MESSAGE_TOOL_NAME;

/// Consecutive identical `message` tool sends that trip the circuit breaker.
pub const DEFAULT_N_IDENTICAL_MESSAGES: usize = 5;

/// `AgentRunResult::stop_reason` when the breaker aborts a turn.
pub const CIRCUIT_BREAKER_STOP_REASON: &str = "circuit_breaker";

/// Short user-facing notice delivered when the breaker trips.
pub const CIRCUIT_BREAKER_USER_MESSAGE: &str = "Repeated identical messages were stopped.";

const TRIPPED_ERROR_PREFIX: &str = "Error: message circuit breaker tripped";

/// Outcome of observing one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CircuitDecision {
    Allow,
    Tripped { streak: usize },
}

/// Detects a run-away streak of identical `message` tool contents.
///
/// Once tripped, stays open for the rest of the run so remaining tool calls
/// in the same batch are not executed.
pub struct MessageCircuitBreaker {
    threshold: usize,
    last_content: Option<String>,
    streak: usize,
    tripped: bool,
}

impl MessageCircuitBreaker {
    pub fn new(threshold: usize) -> Self {
        Self {
            threshold: threshold.max(1),
            last_content: None,
            streak: 0,
            tripped: false,
        }
    }

    /// Record a tool call. Returns [`CircuitDecision::Tripped`] on the Nth
    /// consecutive identical `message` content, and on every later call.
    pub fn observe(
        &mut self,
        tool_name: &str,
        arguments: &HashMap<String, Value>,
    ) -> CircuitDecision {
        if self.tripped {
            return CircuitDecision::Tripped {
                streak: self.streak,
            };
        }

        if tool_name != MESSAGE_TOOL_NAME {
            self.reset();
            return CircuitDecision::Allow;
        }

        let Some(content) = message_content(arguments) else {
            self.reset();
            return CircuitDecision::Allow;
        };

        if self.last_content.as_deref() == Some(content.as_str()) {
            self.streak += 1;
        } else {
            self.last_content = Some(content);
            self.streak = 1;
        }

        if self.streak >= self.threshold {
            self.tripped = true;
            return CircuitDecision::Tripped {
                streak: self.streak,
            };
        }

        CircuitDecision::Allow
    }

    fn reset(&mut self) {
        self.last_content = None;
        self.streak = 0;
    }
}

/// Tool-result / fatal-error string for a tripped breaker. Always fatal,
/// including when `fail_on_tool_error` is false.
pub fn tripped_error_message(streak: usize) -> String {
    format!("{TRIPPED_ERROR_PREFIX} after {streak} identical messages")
}

pub fn is_tripped_error(message: &str) -> bool {
    message.starts_with(TRIPPED_ERROR_PREFIX)
}

fn message_content(arguments: &HashMap<String, Value>) -> Option<String> {
    let raw = arguments.get("content").and_then(Value::as_str)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(content: &str) -> HashMap<String, Value> {
        HashMap::from([("content".to_string(), Value::String(content.to_string()))])
    }

    fn empty_args() -> HashMap<String, Value> {
        HashMap::new()
    }

    fn blank_args() -> HashMap<String, Value> {
        HashMap::from([("content".to_string(), Value::String("   ".to_string()))])
    }

    #[test]
    fn allows_four_identical_messages_and_trips_on_fifth() {
        let mut breaker = MessageCircuitBreaker::new(DEFAULT_N_IDENTICAL_MESSAGES);
        for _ in 0..4 {
            assert_eq!(
                breaker.observe(MESSAGE_TOOL_NAME, &args("placeholder")),
                CircuitDecision::Allow
            );
        }
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("placeholder")),
            CircuitDecision::Tripped { streak: 5 }
        );
    }

    #[test]
    fn trims_content_before_comparing() {
        let mut breaker = MessageCircuitBreaker::new(2);
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("  hi")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("hi  ")),
            CircuitDecision::Tripped { streak: 2 }
        );
    }

    #[test]
    fn comparison_is_case_sensitive() {
        let mut breaker = MessageCircuitBreaker::new(2);
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("Placeholder")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("placeholder")),
            CircuitDecision::Allow
        );
    }

    #[test]
    fn different_content_resets_streak() {
        let mut breaker = MessageCircuitBreaker::new(3);
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("a")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("a")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("b")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("a")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("a")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("a")),
            CircuitDecision::Tripped { streak: 3 }
        );
    }

    #[test]
    fn non_message_tool_resets_streak() {
        let mut breaker = MessageCircuitBreaker::new(3);
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("x")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("x")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe("shell", &empty_args()),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("x")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("x")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("x")),
            CircuitDecision::Tripped { streak: 3 }
        );
    }

    #[test]
    fn empty_or_missing_content_does_not_trip_and_resets_streak() {
        let mut breaker = MessageCircuitBreaker::new(2);
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("same")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &empty_args()),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("same")),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &blank_args()),
            CircuitDecision::Allow
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("same")),
            CircuitDecision::Allow
        );
    }

    #[test]
    fn stays_tripped_for_later_calls() {
        let mut breaker = MessageCircuitBreaker::new(2);
        breaker.observe(MESSAGE_TOOL_NAME, &args("loop"));
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("loop")),
            CircuitDecision::Tripped { streak: 2 }
        );
        assert_eq!(
            breaker.observe("shell", &empty_args()),
            CircuitDecision::Tripped { streak: 2 }
        );
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("other")),
            CircuitDecision::Tripped { streak: 2 }
        );
    }

    #[test]
    fn custom_threshold_trips_on_nth() {
        let mut breaker = MessageCircuitBreaker::new(1);
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("once")),
            CircuitDecision::Tripped { streak: 1 }
        );
    }

    #[test]
    fn zero_threshold_is_clamped_to_one() {
        let mut breaker = MessageCircuitBreaker::new(0);
        assert_eq!(
            breaker.observe(MESSAGE_TOOL_NAME, &args("once")),
            CircuitDecision::Tripped { streak: 1 }
        );
    }

    #[test]
    fn tripped_error_helpers_round_trip() {
        let msg = tripped_error_message(5);
        assert!(is_tripped_error(&msg));
        assert!(msg.contains("5"));
        assert!(!is_tripped_error("Error: something else"));
    }
}
