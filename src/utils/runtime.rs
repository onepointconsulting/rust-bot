/// Runtime-specific helper functions and constants.
use std::collections::HashMap;

use serde_json::Value;

use crate::utils::helpers::stringify_text_blocks;

// ── constants ─────────────────────────────────────────────────────────────────

const MAX_REPEAT_EXTERNAL_LOOKUPS: usize = 3;

pub const EMPTY_FINAL_RESPONSE_MESSAGE: &str =
    "I completed the tool steps but couldn't produce a final answer. \
     Please try again or narrow the task.";

pub const FINALIZATION_RETRY_PROMPT: &str =
    "Please provide your response to the user based on the conversation above.";

pub const LENGTH_RECOVERY_PROMPT: &str =
    "Output limit reached. Continue exactly where you left off \
     — no recap, no apology. Break remaining work into smaller steps if needed.";

// ── tool result helpers ───────────────────────────────────────────────────────

/// Short prompt-safe marker for tools that completed without visible output.
pub fn empty_tool_result_message(tool_name: &str) -> String {
    format!("({} completed with no output)", tool_name)
}

/// Replace semantically empty tool results with a short marker string.
///
/// A result is considered empty when it is:
/// - `Null`
/// - A blank/whitespace-only string
/// - An empty array
/// - An array of text blocks whose joined text is blank
pub fn ensure_nonempty_tool_result(tool_name: &str, content: Value) -> Value {
    match &content {
        Value::Null => Value::String(empty_tool_result_message(tool_name)),

        Value::String(s) if s.trim().is_empty() => {
            Value::String(empty_tool_result_message(tool_name))
        }

        Value::Array(blocks) => {
            if blocks.is_empty() {
                return Value::String(empty_tool_result_message(tool_name));
            }
            if let Some(text) = stringify_text_blocks(blocks) {
                if text.trim().is_empty() {
                    return Value::String(empty_tool_result_message(tool_name));
                }
            }
            content
        }

        _ => content,
    }
}

// ── blank text check ──────────────────────────────────────────────────────────

/// Returns `true` when `content` is `None` or contains only whitespace.
pub fn is_blank_text(content: Option<&str>) -> bool {
    content.map_or(true, |s| s.trim().is_empty())
}

// ── finalization / recovery messages ─────────────────────────────────────────

/// A short no-tools-allowed prompt for final answer recovery.
pub fn build_finalization_retry_message() -> Value {
    serde_json::json!({ "role": "user", "content": FINALIZATION_RETRY_PROMPT })
}

/// Prompt the model to continue after hitting the output token limit.
pub fn build_length_recovery_message() -> Value {
    serde_json::json!({ "role": "user", "content": LENGTH_RECOVERY_PROMPT })
}

// ── repeated external lookup throttling ──────────────────────────────────────

/// Stable signature for repeated external lookups we want to throttle.
///
/// Returns `Some(signature)` for `web_fetch` and `web_search` calls,
/// `None` for all other tool names.
pub fn external_lookup_signature(
    tool_name: &str,
    arguments: &HashMap<String, Value>,
) -> Option<String> {
    match tool_name {
        "web_fetch" => {
            let url = arguments
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if !url.is_empty() {
                return Some(format!("web_fetch:{}", url));
            }
            None
        }
        "web_search" => {
            let query = arguments
                .get("query")
                .or_else(|| arguments.get("search_term"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if !query.is_empty() {
                return Some(format!("web_search:{}", query));
            }
            None
        }
        _ => None,
    }
}

/// Block repeated external lookups after a small retry budget.
///
/// Increments `seen_counts[signature]` on every call. Returns an error
/// string once the count exceeds `MAX_REPEAT_EXTERNAL_LOOKUPS`, `None`
/// otherwise.
pub fn repeated_external_lookup_error(
    tool_name: &str,
    arguments: &HashMap<String, Value>,
    seen_counts: &mut HashMap<String, usize>,
) -> Option<String> {
    let signature = external_lookup_signature(tool_name, arguments)?;

    let count = seen_counts.entry(signature.clone()).or_insert(0);
    *count += 1;

    if *count <= MAX_REPEAT_EXTERNAL_LOOKUPS {
        return None;
    }

    log::warn!(
        "Blocking repeated external lookup {} on attempt {}",
        &signature[..signature.len().min(160)],
        count,
    );

    Some(
        "Error: repeated external lookup blocked. \
         Use the results you already have to answer, or try a meaningfully different source."
            .to_string(),
    )
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    // ── empty_tool_result_message ─────────────────────────────────────────────

    #[test]
    fn test_empty_tool_result_message() {
        assert_eq!(
            empty_tool_result_message("my_tool"),
            "(my_tool completed with no output)"
        );
    }

    // ── ensure_nonempty_tool_result ───────────────────────────────────────────

    #[test]
    fn test_ensure_nonempty_null_replaced() {
        let result = ensure_nonempty_tool_result("tool", Value::Null);
        assert_eq!(result, Value::String("(tool completed with no output)".into()));
    }

    #[test]
    fn test_ensure_nonempty_blank_string_replaced() {
        let result = ensure_nonempty_tool_result("tool", Value::String("   ".into()));
        assert_eq!(result, Value::String("(tool completed with no output)".into()));
    }

    #[test]
    fn test_ensure_nonempty_empty_string_replaced() {
        let result = ensure_nonempty_tool_result("tool", Value::String("".into()));
        assert_eq!(result, Value::String("(tool completed with no output)".into()));
    }

    #[test]
    fn test_ensure_nonempty_nonempty_string_unchanged() {
        let content = Value::String("some output".into());
        let result = ensure_nonempty_tool_result("tool", content.clone());
        assert_eq!(result, content);
    }

    #[test]
    fn test_ensure_nonempty_empty_array_replaced() {
        let result = ensure_nonempty_tool_result("tool", Value::Array(vec![]));
        assert_eq!(result, Value::String("(tool completed with no output)".into()));
    }

    #[test]
    fn test_ensure_nonempty_blank_text_blocks_replaced() {
        let blocks = serde_json::json!([
            { "type": "text", "text": "   " },
            { "type": "text", "text": "" }
        ]);
        let result = ensure_nonempty_tool_result("tool", blocks);
        assert_eq!(result, Value::String("(tool completed with no output)".into()));
    }

    #[test]
    fn test_ensure_nonempty_nonempty_text_blocks_unchanged() {
        let blocks = serde_json::json!([{ "type": "text", "text": "hello" }]);
        let result = ensure_nonempty_tool_result("tool", blocks.clone());
        assert_eq!(result, blocks);
    }

    #[test]
    fn test_ensure_nonempty_mixed_blocks_unchanged() {
        // Non-text block → stringify_text_blocks returns None → content kept as-is
        let blocks = serde_json::json!([{ "type": "image_url", "url": "x" }]);
        let result = ensure_nonempty_tool_result("tool", blocks.clone());
        assert_eq!(result, blocks);
    }

    #[test]
    fn test_ensure_nonempty_number_unchanged() {
        let content = serde_json::json!(42);
        let result = ensure_nonempty_tool_result("tool", content.clone());
        assert_eq!(result, content);
    }

    // ── is_blank_text ─────────────────────────────────────────────────────────

    #[test]
    fn test_is_blank_text_none() {
        assert!(is_blank_text(None));
    }

    #[test]
    fn test_is_blank_text_empty() {
        assert!(is_blank_text(Some("")));
    }

    #[test]
    fn test_is_blank_text_whitespace() {
        assert!(is_blank_text(Some("   \t\n")));
    }

    #[test]
    fn test_is_blank_text_nonempty() {
        assert!(!is_blank_text(Some("hello")));
    }

    // ── build_finalization_retry_message ──────────────────────────────────────

    #[test]
    fn test_build_finalization_retry_message() {
        let msg = build_finalization_retry_message();
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"], FINALIZATION_RETRY_PROMPT);
    }

    // ── build_length_recovery_message ─────────────────────────────────────────

    #[test]
    fn test_build_length_recovery_message() {
        let msg = build_length_recovery_message();
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"], LENGTH_RECOVERY_PROMPT);
    }

    // ── external_lookup_signature ─────────────────────────────────────────────

    #[test]
    fn test_signature_web_fetch() {
        let a = args(&[("url", "https://Example.com/path")]);
        let sig = external_lookup_signature("web_fetch", &a).unwrap();
        assert_eq!(sig, "web_fetch:https://example.com/path");
    }

    #[test]
    fn test_signature_web_fetch_empty_url() {
        let a = args(&[("url", "  ")]);
        assert!(external_lookup_signature("web_fetch", &a).is_none());
    }

    #[test]
    fn test_signature_web_search_query() {
        let a = args(&[("query", "Rust async traits")]);
        let sig = external_lookup_signature("web_search", &a).unwrap();
        assert_eq!(sig, "web_search:rust async traits");
    }

    #[test]
    fn test_signature_web_search_search_term_fallback() {
        let a = args(&[("search_term", "Rust lifetimes")]);
        let sig = external_lookup_signature("web_search", &a).unwrap();
        assert_eq!(sig, "web_search:rust lifetimes");
    }

    #[test]
    fn test_signature_other_tool_none() {
        let a = args(&[("path", "/tmp/file")]);
        assert!(external_lookup_signature("read_file", &a).is_none());
    }

    // ── repeated_external_lookup_error ────────────────────────────────────────

    #[test]
    fn test_repeated_lookup_allowed_within_budget() {
        let mut counts = HashMap::new();
        let a = args(&[("url", "https://example.com")]);

        for _ in 0..MAX_REPEAT_EXTERNAL_LOOKUPS {
            let result = repeated_external_lookup_error("web_fetch", &a, &mut counts);
            assert!(result.is_none(), "should be allowed within budget");
        }
    }

    #[test]
    fn test_repeated_lookup_blocked_after_budget() {
        let mut counts = HashMap::new();
        let a = args(&[("url", "https://example.com")]);

        // Exhaust budget
        for _ in 0..MAX_REPEAT_EXTERNAL_LOOKUPS {
            repeated_external_lookup_error("web_fetch", &a, &mut counts);
        }

        let result = repeated_external_lookup_error("web_fetch", &a, &mut counts);
        assert!(result.is_some());
        let err = result.unwrap();
        assert!(err.contains("repeated external lookup blocked"));
    }

    #[test]
    fn test_repeated_lookup_non_external_tool_never_blocked() {
        let mut counts = HashMap::new();
        let a = args(&[("path", "/file")]);

        for _ in 0..10 {
            let result = repeated_external_lookup_error("read_file", &a, &mut counts);
            assert!(result.is_none());
        }
    }

    #[test]
    fn test_repeated_lookup_different_urls_tracked_independently() {
        let mut counts = HashMap::new();
        let a1 = args(&[("url", "https://example.com")]);
        let a2 = args(&[("url", "https://other.com")]);

        // Exhaust budget for first URL
        for _ in 0..MAX_REPEAT_EXTERNAL_LOOKUPS {
            repeated_external_lookup_error("web_fetch", &a1, &mut counts);
        }

        // Second URL should still be within budget
        let result = repeated_external_lookup_error("web_fetch", &a2, &mut counts);
        assert!(result.is_none(), "different URL should not be blocked");
    }

    #[test]
    fn test_repeated_lookup_counts_increment() {
        let mut counts = HashMap::new();
        let a = args(&[("query", "hello world")]);

        repeated_external_lookup_error("web_search", &a, &mut counts);
        repeated_external_lookup_error("web_search", &a, &mut counts);

        let sig = "web_search:hello world";
        assert_eq!(counts[sig], 2);
    }
}
