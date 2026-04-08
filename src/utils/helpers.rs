/// Utility functions for rust-bot.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use regex::Regex;
use serde_json::Value;
use uuid::Uuid;

use crate::providers::base::LLMProvider;

// ── directory helpers (pre-existing) ─────────────────────────────────────────

/// Ensure directory exists, return it.
pub fn ensure_dir<P: AsRef<Path>>(path: P) -> PathBuf {
    let path_ref = path.as_ref();
    if !path_ref.exists() {
        let _ = fs::create_dir_all(path_ref);
    }
    path_ref.to_path_buf()
}

pub fn touch_file<P: AsRef<Path>>(path: P) {
    let path_ref = path.as_ref();
    if !path_ref.exists() {
        let _ = fs::File::create(path_ref);
    }
}

// ── think-block stripping ─────────────────────────────────────────────────────

fn think_closed_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<think>[\s\S]*?</think>").unwrap())
}

fn think_open_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<think>[\s\S]*$").unwrap())
}

/// Remove `<think>…</think>` blocks and any unclosed trailing `<think>` tag.
pub fn strip_think(text: &str) -> String {
    let text = think_closed_re().replace_all(text, "");
    let text = think_open_re().replace_all(&text, "");
    text.trim().to_string()
}

// ── image utilities ───────────────────────────────────────────────────────────

/// Detect image MIME type from magic bytes, ignoring file extension.
pub fn detect_image_mime(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 8 && &data[..8] == b"\x89PNG\r\n\x1a\n" {
        return Some("image/png");
    }
    if data.len() >= 3 && &data[..3] == b"\xff\xd8\xff" {
        return Some("image/jpeg");
    }
    if data.len() >= 6 && (&data[..6] == b"GIF87a" || &data[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Build native image content blocks plus a short text label.
pub fn build_image_content_blocks(
    raw: &[u8],
    mime: &str,
    path: &str,
    label: &str,
) -> Vec<Value> {
    let b64 = BASE64.encode(raw);
    vec![
        serde_json::json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", mime, b64) },
            "_meta": { "path": path },
        }),
        serde_json::json!({ "type": "text", "text": label }),
    ]
}

// ── time ──────────────────────────────────────────────────────────────────────

/// Current ISO 8601 timestamp.
pub fn timestamp() -> String {
    chrono::Local::now().to_rfc3339()
}

/// Return a human-readable current time string with optional timezone name.
///
/// Output format: `"2026-04-08 14:30 (Wednesday) (Europe/Berlin, UTC+02:00)"`
pub fn current_time_str(timezone: Option<&str>) -> String {
    let fmt_offset = |offset: String| -> String {
        if offset.len() == 5 {
            format!("{}:{}", &offset[..3], &offset[3..])
        } else {
            offset
        }
    };

    if let Some(tz_name) = timezone {
        if let Ok(tz) = tz_name.parse::<chrono_tz::Tz>() {
            let now = chrono::Utc::now().with_timezone(&tz);
            let offset = now.format("%z").to_string();
            return format!(
                "{} ({}, UTC{})",
                now.format("%Y-%m-%d %H:%M (%A)"),
                tz_name,
                fmt_offset(offset)
            );
        }
    }

    let now = chrono::Local::now();
    let offset = now.format("%z").to_string();
    let tz_display = {
        let z = now.format("%Z").to_string();
        if z.is_empty() { "UTC".to_string() } else { z }
    };
    format!(
        "{} ({}, UTC{})",
        now.format("%Y-%m-%d %H:%M (%A)"),
        tz_display,
        fmt_offset(offset)
    )
}

// ── string / text utilities ───────────────────────────────────────────────────

const UNSAFE_PATH_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Replace unsafe path characters with underscores.
pub fn safe_filename(name: &str) -> String {
    name.chars()
        .map(|c| if UNSAFE_PATH_CHARS.contains(&c) { '_' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Build an image placeholder string.
pub fn image_placeholder_text(path: Option<&str>, empty: &str) -> String {
    match path {
        Some(p) => format!("[image: {}]", p),
        None => empty.to_string(),
    }
}

/// Truncate text to `max_chars`, appending `"\n... (truncated)"` when cut.
pub fn truncate_text(text: &str, max_chars: usize) -> String {
    if max_chars == 0 || text.len() <= max_chars {
        return text.to_string();
    }
    format!("{}\n... (truncated)", &text[..max_chars])
}

// ── message utilities ─────────────────────────────────────────────────────────

/// Find the first message index whose tool results all have matching assistant calls.
///
/// Scans forward through `messages`; if a `tool` message references a
/// `tool_call_id` that no prior `assistant` message declared, resets the
/// valid start to after that point.
pub fn find_legal_message_start(messages: &[Value]) -> usize {
    let mut declared: std::collections::HashSet<String> = Default::default();
    let mut start = 0usize;

    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role == "assistant" {
            if let Some(tcs) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        declared.insert(id.to_string());
                    }
                }
            }
        } else if role == "tool" {
            let tid = msg.get("tool_call_id").and_then(|v| v.as_str());
            if let Some(tid) = tid {
                if !declared.contains(tid) {
                    start = i + 1;
                    declared.clear();
                    for prev in &messages[start..=i] {
                        if prev.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                            if let Some(tcs) = prev.get("tool_calls").and_then(|v| v.as_array()) {
                                for tc in tcs {
                                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                        declared.insert(id.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    start
}

/// If `content` is an array of `{"type":"text","text":"..."}` blocks, join them.
/// Returns `None` if any block is non-text or malformed.
pub fn stringify_text_blocks(content: &[Value]) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for block in content {
        let obj = block.as_object()?;
        if obj.get("type").and_then(|v| v.as_str()) != Some("text") {
            return None;
        }
        let text = obj.get("text").and_then(|v| v.as_str())?;
        parts.push(text);
    }
    Some(parts.join("\n"))
}

/// Split content into chunks within `max_len`, preferring line breaks over word
/// breaks over hard cuts. Matches the Python `split_message` behaviour.
pub fn split_message(content: &str, max_len: usize) -> Vec<String> {
    if content.is_empty() {
        return vec![];
    }
    if content.len() <= max_len {
        return vec![content.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining = content;
    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }
        let cut = &remaining[..max_len];
        let pos = cut
            .rfind('\n')
            .filter(|&p| p > 0)
            .or_else(|| cut.rfind(' ').filter(|&p| p > 0))
            .unwrap_or(max_len);
        chunks.push(remaining[..pos].to_string());
        remaining = remaining[pos..].trim_start();
    }
    chunks
}

/// Build a provider-safe assistant message with optional reasoning fields.
pub fn build_assistant_message(
    content: Option<&str>,
    tool_calls: Option<Vec<Value>>,
    reasoning_content: Option<&str>,
    thinking_blocks: Option<Vec<Value>>,
) -> Value {
    let mut msg = serde_json::json!({ "role": "assistant", "content": content });
    if let Some(tc) = tool_calls {
        msg["tool_calls"] = Value::Array(tc);
    }
    if reasoning_content.is_some() || thinking_blocks.is_some() {
        msg["reasoning_content"] = Value::String(
            reasoning_content.unwrap_or("").to_string(),
        );
    }
    if let Some(tb) = thinking_blocks {
        msg["thinking_blocks"] = Value::Array(tb);
    }
    msg
}

// ── tool result persistence ───────────────────────────────────────────────────

const TOOL_RESULT_PREVIEW_CHARS: usize = 1200;
const TOOL_RESULTS_DIR: &str = ".nanobot/tool-results";
const TOOL_RESULT_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
const TOOL_RESULT_MAX_BUCKETS: usize = 32;

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn bucket_mtime(path: &Path) -> f64 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn cleanup_tool_result_buckets(root: &Path, current_bucket: &Path) {
    let cutoff = unix_now() - TOOL_RESULT_RETENTION_SECS as f64;
    let mut siblings: Vec<PathBuf> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p != current_bucket)
        .collect();

    // Remove expired buckets
    siblings.retain(|p| {
        if bucket_mtime(p) < cutoff {
            let _ = fs::remove_dir_all(p);
            false
        } else {
            true
        }
    });

    // Enforce max bucket count
    let keep = TOOL_RESULT_MAX_BUCKETS.saturating_sub(1);
    if siblings.len() <= keep {
        return;
    }
    siblings.sort_by(|a, b| bucket_mtime(b).partial_cmp(&bucket_mtime(a)).unwrap_or(std::cmp::Ordering::Equal));
    for path in &siblings[keep..] {
        let _ = fs::remove_dir_all(path);
    }
}

fn write_text_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp_name = format!(".{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), Uuid::new_v4().simple());
    let tmp = path.with_file_name(tmp_name);
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e
    })
}

fn render_tool_result_reference(
    filepath: &Path,
    original_size: usize,
    preview: &str,
    truncated_preview: bool,
) -> String {
    let mut result = format!(
        "[tool output persisted]\nFull output saved to: {}\nOriginal size: {} chars\nPreview:\n{}",
        filepath.display(),
        original_size,
        preview
    );
    if truncated_preview {
        result.push_str("\n...\n(Read the saved file if you need the full output.)");
    }
    result
}

/// Persist oversized tool output to disk and replace it with a stable reference string.
///
/// Returns the original `content` unchanged when:
/// - `workspace` is `None`
/// - `max_chars` is 0
/// - content is not a string or a text-block array
/// - the text payload is within `max_chars`
pub fn maybe_persist_tool_result(
    workspace: Option<&Path>,
    session_key: Option<&str>,
    tool_call_id: &str,
    content: Value,
    max_chars: usize,
) -> Value {
    let workspace = match workspace {
        Some(w) if max_chars > 0 => w,
        _ => return content,
    };

    let (text_payload, is_json) = match &content {
        Value::String(s) => (s.clone(), false),
        Value::Array(blocks) => {
            match stringify_text_blocks(blocks) {
                Some(text) => (text, true),
                None => return content,
            }
        }
        _ => return content,
    };

    if text_payload.len() <= max_chars {
        return content;
    }

    let root = ensure_dir(workspace.join(TOOL_RESULTS_DIR));
    let bucket = ensure_dir(root.join(safe_filename(session_key.unwrap_or("default"))));

    if let Err(e) = std::panic::catch_unwind(|| cleanup_tool_result_buckets(&root, &bucket)) {
        log::warn!("Failed to clean stale tool result buckets in {:?}: {:?}", root, e);
    }

    let ext = if is_json { "json" } else { "txt" };
    let path = bucket.join(format!("{}.{}", safe_filename(tool_call_id), ext));

    if !path.exists() {
        let body = if is_json {
            serde_json::to_string_pretty(&content).unwrap_or_else(|_| text_payload.clone())
        } else {
            text_payload.clone()
        };
        if let Err(e) = write_text_atomic(&path, &body) {
            log::warn!("Failed to write tool result to {:?}: {}", path, e);
        }
    }

    let preview = &text_payload[..text_payload.len().min(TOOL_RESULT_PREVIEW_CHARS)];
    Value::String(render_tool_result_reference(
        &path,
        text_payload.len(),
        preview,
        text_payload.len() > TOOL_RESULT_PREVIEW_CHARS,
    ))
}

// ── token estimation ──────────────────────────────────────────────────────────

/// Estimate tokens contributed by a single message (character ÷ 4 + overhead).
///
/// Counts content, tool_calls, reasoning_content, name, and tool_call_id —
/// same fields as the Python tiktoken implementation. The `+4` per-message
/// overhead matches the OpenAI framing cost.
pub fn estimate_message_tokens(message: &Value) -> usize {
    let mut parts: Vec<String> = Vec::new();

    match message.get("content") {
        Some(Value::String(s)) => parts.push(s.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        if !t.is_empty() {
                            parts.push(t.to_string());
                        }
                    }
                } else {
                    parts.push(block.to_string());
                }
            }
        }
        Some(other) => parts.push(other.to_string()),
        None => {}
    }

    for key in &["name", "tool_call_id"] {
        if let Some(v) = message.get(*key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                parts.push(v.to_string());
            }
        }
    }
    if let Some(tc) = message.get("tool_calls") {
        parts.push(tc.to_string());
    }
    if let Some(Value::String(rc)) = message.get("reasoning_content") {
        if !rc.is_empty() {
            parts.push(rc.clone());
        }
    }

    let payload = parts.join("\n");
    if payload.is_empty() {
        return 4;
    }
    (payload.len() / 4 + 4).max(4)
}

pub fn estimate_prompt_tokens_chain(
    provider: &impl LLMProvider,
    model: &str,
    messages: &[Value],
    tools: Option<&[Value]>,
) -> (usize, String) {
    
    let estimated = estimate_prompt_tokens(messages, tools);
    if estimated > 0 {
        return (estimated, "tiktoken".to_string());
    }
    return (0, "none".to_string());
}

/// Estimate total prompt tokens for a list of messages and optional tools.
///
/// Uses a character ÷ 4 approximation with a 4-token-per-message overhead,
/// matching the tiktoken fallback in the Python implementation.
pub fn estimate_prompt_tokens(
    messages: &[Value],
    tools: Option<&[Value]>,
) -> usize {
    use std::sync::OnceLock;
    use tiktoken_rs::{CoreBPE, cl100k_base};

    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();

    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        match msg.get("content") {
            Some(Value::String(s)) => parts.push(s.clone()),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                parts.push(t.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(tc) = msg.get("tool_calls") {
            parts.push(tc.to_string());
        }
        if let Some(Value::String(rc)) = msg.get("reasoning_content") {
            if !rc.is_empty() {
                parts.push(rc.clone());
            }
        }
        for key in &["name", "tool_call_id"] {
            if let Some(v) = msg.get(*key).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    parts.push(v.to_string());
                }
            }
        }
    }

    if let Some(tools) = tools {
        parts.push(Value::Array(tools.to_vec()).to_string());
    }

    let joined_str = parts.join("\n");
    let overhead = messages.len() * 4;

    let bpe = BPE.get_or_init(|| cl100k_base().ok());
    match bpe {
        Some(bpe) => bpe.encode_with_special_tokens(&joined_str).len() + overhead,
        // Fallback: byte-length / 4 heuristic if BPE failed to initialise
        None => joined_str.len() / 4 + overhead,
    }
}

// ── status ────────────────────────────────────────────────────────────────────

/// Build a human-readable runtime status snapshot.
pub fn build_status_content(
    version: &str,
    model: &str,
    start_time_secs: f64,
    last_usage: &HashMap<String, i64>,
    context_window_tokens: i64,
    session_msg_count: usize,
    context_tokens_estimate: i64,
    search_usage_text: Option<&str>,
) -> String {
    let uptime_s = (unix_now() - start_time_secs).max(0.0) as u64;
    let uptime = if uptime_s >= 3600 {
        format!("{}h {}m", uptime_s / 3600, (uptime_s % 3600) / 60)
    } else {
        format!("{}m {}s", uptime_s / 60, uptime_s % 60)
    };

    let last_in = last_usage.get("prompt_tokens").copied().unwrap_or(0);
    let last_out = last_usage.get("completion_tokens").copied().unwrap_or(0);
    let cached = last_usage.get("cached_tokens").copied().unwrap_or(0);
    let ctx_total = context_window_tokens.max(0);
    let ctx_pct = if ctx_total > 0 {
        (context_tokens_estimate * 100 / ctx_total) as u64
    } else {
        0
    };
    let ctx_used_str = if context_tokens_estimate >= 1000 {
        format!("{}k", context_tokens_estimate / 1000)
    } else {
        context_tokens_estimate.to_string()
    };
    let ctx_total_str = if ctx_total > 0 {
        format!("{}k", ctx_total / 1024)
    } else {
        "n/a".to_string()
    };

    let mut token_line = format!("📊 Tokens: {} in / {} out", last_in, last_out);
    if cached > 0 && last_in > 0 {
        token_line.push_str(&format!(" ({}% cached)", cached * 100 / last_in));
    }

    let mut lines = vec![
        format!("🐈 nanobot v{}", version),
        format!("🧠 Model: {}", model),
        token_line,
        format!("📚 Context: {}/{} ({}%)", ctx_used_str, ctx_total_str, ctx_pct),
        format!("💬 Session: {} messages", session_msg_count),
        format!("⏱ Uptime: {}", uptime),
    ];
    if let Some(s) = search_usage_text {
        lines.push(s.to_string());
    }
    lines.join("\n")
}

// ── workspace template sync ───────────────────────────────────────────────────

/// Sync bundled templates to the workspace. Only creates missing files.
///
/// Returns the list of relative paths that were created.
pub fn sync_workspace_templates(workspace: &Path, silent: bool) -> Vec<String> {
    let templates_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("templates");
    if !templates_root.is_dir() {
        return vec![];
    }

    let mut added: Vec<String> = Vec::new();

    let write_if_missing = |src: &Path, dest: &Path, added: &mut Vec<String>| {
        if dest.exists() {
            return;
        }
        if let Some(parent) = dest.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::read_to_string(src) {
            Ok(content) => {
                if fs::write(dest, content).is_ok() {
                    if let Ok(rel) = dest.strip_prefix(workspace) {
                        added.push(rel.to_string_lossy().into_owned());
                    }
                }
            }
            Err(_) => {}
        }
    };

    // Copy top-level .md files
    if let Ok(entries) = fs::read_dir(&templates_root) {
        for entry in entries.filter_map(|e| e.ok()) {
            let src = entry.path();
            let name = src.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if name.ends_with(".md") && !name.starts_with('.') {
                write_if_missing(&src, &workspace.join(&name), &mut added);
            }
        }
    }

    // Copy memory/MEMORY.md
    let memory_src = templates_root.join("memory").join("MEMORY.md");
    if memory_src.exists() {
        write_if_missing(&memory_src, &workspace.join("memory").join("MEMORY.md"), &mut added);
    }

    // Touch memory/history.jsonl
    let history = workspace.join("memory").join("history.jsonl");
    if !history.exists() {
        let _ = fs::create_dir_all(history.parent().unwrap());
        let _ = fs::File::create(&history);
        if let Ok(rel) = history.strip_prefix(workspace) {
            added.push(rel.to_string_lossy().into_owned());
        }
    }

    // Ensure skills/ dir exists
    let _ = fs::create_dir_all(workspace.join("skills"));

    if !added.is_empty() && !silent {
        for name in &added {
            log::info!("Created {}", name);
        }
    }

    added
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ── strip_think ───────────────────────────────────────────────────────────

    #[test]
    fn test_strip_think_closed_block() {
        let input = "Hello <think>secret reasoning</think> world";
        assert_eq!(strip_think(input), "Hello  world");
    }

    #[test]
    fn test_strip_think_multiline_block() {
        let input = "Before\n<think>\nline1\nline2\n</think>\nAfter";
        assert_eq!(strip_think(input), "Before\n\nAfter");
    }

    #[test]
    fn test_strip_think_unclosed_block() {
        let input = "Hello <think>incomplete reasoning";
        assert_eq!(strip_think(input), "Hello");
    }

    #[test]
    fn test_strip_think_no_block() {
        let input = "  plain text  ";
        assert_eq!(strip_think(input), "plain text");
    }

    #[test]
    fn test_strip_think_both() {
        let input = "<think>closed</think> middle <think>unclosed";
        assert_eq!(strip_think(input), "middle");
    }

    // ── detect_image_mime ─────────────────────────────────────────────────────

    #[test]
    fn test_detect_png() {
        let data = b"\x89PNG\r\n\x1a\nrest";
        assert_eq!(detect_image_mime(data), Some("image/png"));
    }

    #[test]
    fn test_detect_jpeg() {
        let data = b"\xff\xd8\xffrest";
        assert_eq!(detect_image_mime(data), Some("image/jpeg"));
    }

    #[test]
    fn test_detect_gif87() {
        assert_eq!(detect_image_mime(b"GIF87amore"), Some("image/gif"));
    }

    #[test]
    fn test_detect_gif89() {
        assert_eq!(detect_image_mime(b"GIF89amore"), Some("image/gif"));
    }

    #[test]
    fn test_detect_webp() {
        let mut data = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        data.extend_from_slice(b"more");
        assert_eq!(detect_image_mime(&data), Some("image/webp"));
    }

    #[test]
    fn test_detect_unknown() {
        assert_eq!(detect_image_mime(b"\x00\x01\x02"), None);
    }

    // ── build_image_content_blocks ────────────────────────────────────────────

    #[test]
    fn test_build_image_content_blocks_structure() {
        let blocks = build_image_content_blocks(b"fake", "image/png", "/img.png", "a photo");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "image_url");
        assert!(blocks[0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert_eq!(blocks[0]["_meta"]["path"], "/img.png");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "a photo");
    }

    // ── timestamp ─────────────────────────────────────────────────────────────

    #[test]
    fn test_timestamp_is_rfc3339() {
        let ts = timestamp();
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok(), "not RFC3339: {}", ts);
    }

    // ── current_time_str ──────────────────────────────────────────────────────

    #[test]
    fn test_current_time_str_local() {
        let s = current_time_str(None);
        println!("s: {}", s);
        assert!(s.contains("UTC"), "expected UTC offset: {}", s);
        assert_eq!(s.rfind("(").unwrap(), 29, "expected at least one ( in the string: {}", s);
    }

    #[test]
    fn test_current_time_str_with_valid_timezone() {
        let s = current_time_str(Some("Europe/Berlin"));
        assert!(s.contains("Europe/Berlin"), "expected tz name: {}", s);
        assert!(s.contains("UTC"), "expected UTC offset: {}", s);
    }

    #[test]
    fn test_current_time_str_with_invalid_timezone_falls_back() {
        // Should not panic; falls back to local time
        let s = current_time_str(Some("Not/ATimezone"));
        assert!(s.contains("UTC"), "expected fallback with UTC offset: {}", s);
    }

    // ── safe_filename ─────────────────────────────────────────────────────────

    #[test]
    fn test_safe_filename_replaces_unsafe_chars() {
        assert_eq!(safe_filename("foo/bar:baz"), "foo_bar_baz");
        assert_eq!(safe_filename(r#"a<b>c"d"#), "a_b_c_d");
    }

    #[test]
    fn test_safe_filename_trims_whitespace() {
        assert_eq!(safe_filename("  hello  "), "hello");
    }



    #[test]
    fn test_safe_filename_clean() {
        assert_eq!(safe_filename("clean-name_123"), "clean-name_123");
    }

    // ── image_placeholder_text ────────────────────────────────────────────────

    #[test]
    fn test_image_placeholder_with_path() {
        assert_eq!(image_placeholder_text(Some("/img.png"), "[image]"), "[image: /img.png]");
    }

    #[test]
    fn test_image_placeholder_without_path() {
        assert_eq!(image_placeholder_text(None, "[no image]"), "[no image]");
    }

    // ── truncate_text ─────────────────────────────────────────────────────────

    #[test]
    fn test_truncate_short_text_unchanged() {
        assert_eq!(truncate_text("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exactly_max_unchanged() {
        assert_eq!(truncate_text("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long_text() {
        let result = truncate_text("0123456789", 5);
        assert!(result.starts_with("01234"));
        assert!(result.contains("(truncated)"));
    }

    #[test]
    fn test_truncate_zero_max() {
        assert_eq!(truncate_text("any", 0), "any");
    }

    // ── find_legal_message_start ──────────────────────────────────────────────

    #[test]
    fn test_find_legal_start_all_valid() {
        let messages = vec![
            serde_json::json!({ "role": "assistant", "tool_calls": [{"id": "c1"}] }),
            serde_json::json!({ "role": "tool", "tool_call_id": "c1" }),
        ];
        assert_eq!(find_legal_message_start(&messages), 0);
    }

    #[test]
    fn test_find_legal_start_orphaned_tool_result() {
        let messages = vec![
            serde_json::json!({ "role": "tool", "tool_call_id": "orphan" }),
            serde_json::json!({ "role": "user", "content": "hi" }),
        ];
        // orphan has no prior assistant call → start moves past it
        assert_eq!(find_legal_message_start(&messages), 1);
    }

    #[test]
    fn test_find_legal_start_empty() {
        assert_eq!(find_legal_message_start(&[]), 0);
    }

    // ── stringify_text_blocks ─────────────────────────────────────────────────

    #[test]
    fn test_stringify_text_blocks_ok() {
        let blocks = vec![
            serde_json::json!({ "type": "text", "text": "hello" }),
            serde_json::json!({ "type": "text", "text": "world" }),
        ];
        assert_eq!(stringify_text_blocks(&blocks), Some("hello\nworld".to_string()));
    }

    #[test]
    fn test_stringify_text_blocks_non_text_returns_none() {
        let blocks = vec![
            serde_json::json!({ "type": "image_url", "url": "..." }),
        ];
        assert_eq!(stringify_text_blocks(&blocks), None);
    }

    #[test]
    fn test_stringify_text_blocks_empty() {
        assert_eq!(stringify_text_blocks(&[]), Some(String::new()));
    }

    // ── split_message ─────────────────────────────────────────────────────────

    #[test]
    fn test_split_message_empty() {
        assert_eq!(split_message("", 100), Vec::<String>::new());
    }

    #[test]
    fn test_split_message_short() {
        assert_eq!(split_message("hi", 100), vec!["hi"]);
    }

    #[test]
    fn test_split_message_breaks_at_newline() {
        let text = "line1\nline2\nline3";
        let chunks = split_message(text, 12);
        assert!(chunks.iter().all(|c| c.len() <= 12), "chunk too long: {:?}", chunks);
        assert_eq!(chunks.join("\n"), text);
    }

    #[test]
    fn test_split_message_breaks_at_space() {
        let text = "word1 word2 word3";
        let chunks = split_message(text, 12);
        assert!(chunks.iter().all(|c| c.len() <= 12));
    }

    #[test]
    fn test_split_message_hard_cut_no_whitespace() {
        let text = "abcdefghij";
        let chunks = split_message(text, 4);
        assert!(chunks.iter().all(|c| c.len() <= 4));
    }

    // ── build_assistant_message ───────────────────────────────────────────────

    #[test]
    fn test_build_assistant_message_minimal() {
        let msg = build_assistant_message(Some("hello"), None, None, None);
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "hello");
        assert!(msg.get("tool_calls").is_none());
        assert!(msg.get("reasoning_content").is_none());
    }

    #[test]
    fn test_build_assistant_message_with_tool_calls() {
        let tc = vec![serde_json::json!({"id": "c1", "function": {"name": "f"}})];
        let msg = build_assistant_message(None, Some(tc), None, None);
        assert!(msg["tool_calls"].is_array());
    }

    #[test]
    fn test_build_assistant_message_with_reasoning() {
        let msg = build_assistant_message(Some("hi"), None, Some("thought"), None);
        assert_eq!(msg["reasoning_content"], "thought");
    }

    // ── estimate_message_tokens ───────────────────────────────────────────────

    #[test]
    fn test_estimate_message_tokens_string_content() {
        let msg = serde_json::json!({ "role": "user", "content": "hello world" });
        let tokens = estimate_message_tokens(&msg);
        assert!(tokens >= 4, "expected at least 4: {}", tokens);
    }

    #[test]
    fn test_estimate_message_tokens_empty_returns_4() {
        let msg = serde_json::json!({ "role": "user" });
        assert_eq!(estimate_message_tokens(&msg), 4);
    }

    // ── estimate_prompt_tokens ────────────────────────────────────────────────

    #[test]
    fn test_estimate_prompt_tokens_grows_with_content() {
        let short = vec![serde_json::json!({ "role": "user", "content": "hi" })];
        let long = vec![serde_json::json!({ "role": "user", "content": "a".repeat(400) })];
        println!("long: {}", estimate_prompt_tokens(&long, None));
        assert!(estimate_prompt_tokens(&long, None) > estimate_prompt_tokens(&short, None));
    }

    #[test]
    fn test_estimate_prompt_tokens_tools_add_cost() {
        let msgs = vec![serde_json::json!({ "role": "user", "content": "hi" })];
        let tools = vec![serde_json::json!({"type": "function", "function": {"name": "f"}})];
        assert!(estimate_prompt_tokens(&msgs, Some(&tools)) > estimate_prompt_tokens(&msgs, None));
    }

    // ── build_status_content ──────────────────────────────────────────────────

    #[test]
    fn test_build_status_content_structure() {
        let mut usage = HashMap::new();
        usage.insert("prompt_tokens".to_string(), 100_i64);
        usage.insert("completion_tokens".to_string(), 50_i64);
        let status = build_status_content(
            "1.0", "gpt-4o",
            unix_now() - 130.0,
            &usage,
            128_000,
            10,
            5000,
            None,
        );
        assert!(status.contains("gpt-4o"));
        assert!(status.contains("100 in"));
        assert!(status.contains("50 out"));
        assert!(status.contains("2m"));  // ~130s uptime
        assert!(status.contains("10 messages"));
    }

    #[test]
    fn test_build_status_content_hours_uptime() {
        let usage = HashMap::new();
        let status = build_status_content(
            "1.0", "model",
            unix_now() - 7200.0, // 2 hours
            &usage, 0, 0, 0, None,
        );
        assert!(status.contains("2h"), "expected hours in uptime: {}", status);
    }

    #[test]
    fn test_build_status_content_search_usage_appended() {
        let usage = HashMap::new();
        let status = build_status_content(
            "1.0", "m", unix_now(), &usage, 0, 0, 0,
            Some("🔍 Web search: 3 queries"),
        );
        assert!(status.contains("Web search"), "search line missing: {}", status);
    }

    // ── maybe_persist_tool_result ─────────────────────────────────────────────

    #[test]
    fn test_persist_tool_result_short_content_unchanged() {
        let tmp = tempdir().unwrap();
        let content = Value::String("short".to_string());
        let result = maybe_persist_tool_result(Some(tmp.path()), None, "id1", content.clone(), 100);
        assert_eq!(result, content);
    }

    #[test]
    fn test_persist_tool_result_long_content_replaced() {
        let tmp = tempdir().unwrap();
        let long = "x".repeat(2000);
        let content = Value::String(long);
        let result = maybe_persist_tool_result(Some(tmp.path()), Some("sess"), "id2", content, 100);
        let s = result.as_str().unwrap();
        assert!(s.contains("[tool output persisted]"));
        assert!(s.contains("2000 chars"));
    }

    #[test]
    fn test_persist_tool_result_no_workspace_unchanged() {
        let content = Value::String("x".repeat(2000));
        let result = maybe_persist_tool_result(None, None, "id", content.clone(), 10);
        assert_eq!(result, content);
    }

    // ── sync_workspace_templates ──────────────────────────────────────────────

    #[test]
    fn test_sync_workspace_templates_creates_files() {
        let tmp = tempdir().unwrap();
        let added = sync_workspace_templates(tmp.path(), true);
        assert!(!added.is_empty(), "expected files to be created");
        assert!(tmp.path().join("memory").join("history.jsonl").exists());
        assert!(tmp.path().join("skills").is_dir());
    }

    #[test]
    fn test_sync_workspace_templates_idempotent() {
        let tmp = tempdir().unwrap();
        let first = sync_workspace_templates(tmp.path(), true);
        let second = sync_workspace_templates(tmp.path(), true);
        assert!(!first.is_empty());
        assert!(second.is_empty(), "second sync should add nothing: {:?}", second);
    }
}
