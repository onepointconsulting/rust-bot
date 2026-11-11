//! Raw OpenAI-compatible SSE parsing for [`super::openai_compat_provider`].
//!
//! Chunks are `serde_json::Value` so gateway extras (`reasoning_content`,
//! unknown `service_tier`, etc.) are preserved.

use std::collections::BTreeMap;
use std::time::Duration;

use futures::StreamExt;
use tokio::time::timeout;

use crate::providers::base::{LLMResponse, LLMUsage};
use crate::providers::openai_compat_provider::OpenAICompatProvider;

const MAX_SSE_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) enum SseData {
    Done,
    Json(serde_json::Value),
}

#[derive(Debug, Default)]
pub(crate) struct StreamChunkDeltas {
    pub content: Option<String>,
    pub reasoning: Option<String>,
}

#[derive(Debug)]
pub(crate) struct StreamAccumulator {
    pub content: String,
    pub reasoning: String,
    pub finish_reason: String,
    pub usage: LLMUsage,
    pub tool_call_acc: BTreeMap<u32, (Option<String>, String, String)>,
}

impl StreamAccumulator {
    pub(crate) fn new() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            finish_reason: "stop".to_string(),
            usage: LLMUsage::new(),
            tool_call_acc: BTreeMap::new(),
        }
    }

    pub(crate) fn apply_chunk(
        &mut self,
        chunk: &serde_json::Value,
    ) -> Result<StreamChunkDeltas, String> {
        if let Some(message) = OpenAICompatProvider::error_message_from_value(chunk) {
            return Err(message);
        }
        if let Some(usage) = chunk.get("usage") {
            self.usage = OpenAICompatProvider::parse_usage(usage);
        }

        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            return Ok(StreamChunkDeltas::default());
        };

        let mut deltas = StreamChunkDeltas::default();
        for choice in choices {
            let empty = serde_json::Value::Null;
            let delta = choice.get("delta").unwrap_or(&empty);

            if let Some(content_val) = delta.get("content")
                && let Some(text) = OpenAICompatProvider::extract_text_content(content_val)
            {
                let normalized =
                    OpenAICompatProvider::non_overlapping_suffix(&self.content, text.as_str());
                if !normalized.is_empty() {
                    self.content.push_str(normalized);
                    match &mut deltas.content {
                        Some(existing) => existing.push_str(normalized),
                        None => deltas.content = Some(normalized.to_string()),
                    }
                }
            }

            let reasoning_val = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"));
            if let Some(reasoning_val) = reasoning_val
                && let Some(text) = OpenAICompatProvider::extract_text_content(reasoning_val)
                    .filter(|s| !s.is_empty())
            {
                self.reasoning.push_str(&text);
                match &mut deltas.reasoning {
                    Some(existing) => existing.push_str(&text),
                    None => deltas.reasoning = Some(text),
                }
            }

            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    let entry = self
                        .tool_call_acc
                        .entry(index)
                        .or_insert_with(|| (None, String::new(), String::new()));
                    if let Some(id) = tc
                        .get("id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        entry.0 = Some(id.to_string());
                    }
                    if let Some(func) = tc.get("function") {
                        if let Some(name) = func
                            .get("name")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            entry.1 = name.to_string();
                        }
                        if let Some(args) = func.get("arguments") {
                            match args {
                                serde_json::Value::String(s) => entry.2.push_str(s),
                                other => {
                                    entry.2.push_str(&other.to_string());
                                }
                            }
                        }
                    }
                }
            }

            if let Some(obj) = choice.as_object()
                && let Some(reason) = OpenAICompatProvider::json_finish_reason(obj)
            {
                self.finish_reason = reason;
            }
        }
        Ok(deltas)
    }

    pub(crate) fn into_response(self) -> LLMResponse {
        OpenAICompatProvider::parse_stream_response(
            self.content,
            self.finish_reason,
            self.tool_call_acc,
            self.usage,
            if self.reasoning.is_empty() {
                None
            } else {
                Some(self.reasoning)
            },
        )
    }
}

pub(crate) fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((pos, 4));
    }
    buffer
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|pos| (pos, 2))
}

pub(crate) fn parse_sse_data(raw: &str) -> Option<Result<SseData, String>> {
    let mut data_parts = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            data_parts.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data_parts.is_empty() {
        return None;
    }
    let data = data_parts.join("\n");
    if data.trim() == "[DONE]" {
        return Some(Ok(SseData::Done));
    }
    match serde_json::from_str::<serde_json::Value>(&data) {
        Ok(value) => {
            if let Some(message) = OpenAICompatProvider::error_message_from_value(&value) {
                Some(Err(message))
            } else {
                Some(Ok(SseData::Json(value)))
            }
        }
        Err(e) => Some(Err(format!("failed to parse SSE chunk: {e}"))),
    }
}

pub(crate) fn take_sse_event(buffer: &mut Vec<u8>) -> Option<Result<SseData, String>> {
    let (split_pos, delim_len) = find_event_boundary(buffer)?;
    let raw = String::from_utf8_lossy(&buffer[..split_pos]).into_owned();
    buffer.drain(..split_pos + delim_len);
    parse_sse_data(&raw)
}

/// A stream protocol's state machine: folds decoded SSE `data:` payloads into a final
/// [`LLMResponse`]. [`consume_sse_events`] owns the framing, timeouts and callbacks;
/// implementors only interpret events.
pub(crate) trait SseEventAccumulator {
    /// Apply one decoded JSON event. Returns the text to forward to the callbacks, or
    /// the error message that ends the stream.
    fn apply_event(&mut self, data: &serde_json::Value) -> Result<StreamChunkDeltas, String>;

    /// Build the final response once the stream is over (`[DONE]` or end of body).
    fn finish(self) -> LLMResponse;
}

impl SseEventAccumulator for StreamAccumulator {
    fn apply_event(&mut self, data: &serde_json::Value) -> Result<StreamChunkDeltas, String> {
        self.apply_chunk(data)
    }

    fn finish(self) -> LLMResponse {
        self.into_response()
    }
}

/// Consume an OpenAI Chat Completions SSE byte stream. See [`consume_sse_events`].
pub(crate) async fn consume_sse_byte_stream<S, B, F, Fut>(
    byte_stream: S,
    idle_timeout: Duration,
    idle_timeout_s: u64,
    on_content_delta: &Option<F>,
    on_progress: &Option<crate::providers::base::BoxedProgressCallback>,
) -> LLMResponse
where
    S: futures::Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
    F: Fn(String) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = ()> + Send,
{
    consume_sse_events(
        byte_stream,
        StreamAccumulator::new(),
        idle_timeout,
        idle_timeout_s,
        on_content_delta,
        on_progress,
    )
    .await
}

/// Read SSE events from `byte_stream` into `accumulator` until `[DONE]` or the end of
/// the body, forwarding content deltas to `on_content_delta` and reasoning deltas to
/// `on_progress` (followed by one `ReasoningEnd` when any reasoning was streamed).
///
/// The stream fails with an error response when no bytes arrive for `idle_timeout`,
/// when the buffer grows past [`MAX_SSE_BUFFER_BYTES`] without a complete event, or
/// when the accumulator rejects an event.
pub(crate) async fn consume_sse_events<S, B, A, F, Fut>(
    mut byte_stream: S,
    mut accumulator: A,
    idle_timeout: Duration,
    idle_timeout_s: u64,
    on_content_delta: &Option<F>,
    on_progress: &Option<crate::providers::base::BoxedProgressCallback>,
) -> LLMResponse
where
    S: futures::Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
    A: SseEventAccumulator,
    F: Fn(String) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = ()> + Send,
{
    let mut buffer = Vec::new();
    let mut streamed_reasoning = false;

    loop {
        while let Some(event) = take_sse_event(&mut buffer) {
            match event {
                Ok(SseData::Done) => {
                    return finish_stream(accumulator, streamed_reasoning, on_progress).await;
                }
                Ok(SseData::Json(data)) => match accumulator.apply_event(&data) {
                    Ok(deltas) => {
                        streamed_reasoning |=
                            forward_deltas(deltas, on_content_delta, on_progress).await;
                    }
                    Err(message) => return stream_error(message),
                },
                Err(message) => return stream_error(message),
            }
        }

        if buffer.len() > MAX_SSE_BUFFER_BYTES {
            return stream_error(format!(
                "SSE buffer exceeded {MAX_SSE_BUFFER_BYTES} bytes without a complete event"
            ));
        }

        match timeout(idle_timeout, byte_stream.next()).await {
            Err(_) => {
                return stream_error(format!(
                    "Error calling LLM: stream stalled for more than {idle_timeout_s} seconds"
                ));
            }
            Ok(None) => {
                // A last event without its blank-line terminator is still an event.
                if !buffer.iter().all(u8::is_ascii_whitespace) {
                    buffer.extend_from_slice(
                        b"

",
                    );
                    match take_sse_event(&mut buffer) {
                        Some(Ok(SseData::Json(data))) => match accumulator.apply_event(&data) {
                            Ok(deltas) => {
                                streamed_reasoning |=
                                    forward_deltas(deltas, on_content_delta, on_progress).await;
                            }
                            Err(message) => return stream_error(message),
                        },
                        Some(Err(message)) => return stream_error(message),
                        Some(Ok(SseData::Done)) | None => {}
                    }
                }
                return finish_stream(accumulator, streamed_reasoning, on_progress).await;
            }
            Ok(Some(Ok(chunk))) => buffer.extend_from_slice(chunk.as_ref()),
            Ok(Some(Err(e))) => return stream_error(format!("Error in HTTP stream: {e}")),
        }
    }
}

/// Send one event's deltas to the callbacks. Returns whether reasoning text was sent.
async fn forward_deltas<F, Fut>(
    deltas: StreamChunkDeltas,
    on_content_delta: &Option<F>,
    on_progress: &Option<crate::providers::base::BoxedProgressCallback>,
) -> bool
where
    F: Fn(String) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = ()> + Send,
{
    if let (Some(callback), Some(content)) = (on_content_delta.as_ref(), deltas.content) {
        callback(content).await;
    }
    let Some(reasoning) = deltas.reasoning.filter(|text| !text.is_empty()) else {
        return false;
    };
    if let Some(progress) = on_progress.as_ref() {
        progress(
            reasoning,
            crate::bus::outbound_events::ProgressKind::ReasoningDelta,
        )
        .await;
    }
    true
}

/// Close the reasoning progress stream (when reasoning was sent) and build the response.
async fn finish_stream<A: SseEventAccumulator>(
    accumulator: A,
    streamed_reasoning: bool,
    on_progress: &Option<crate::providers::base::BoxedProgressCallback>,
) -> LLMResponse {
    if streamed_reasoning && let Some(progress) = on_progress.as_ref() {
        progress(
            String::new(),
            crate::bus::outbound_events::ProgressKind::ReasoningEnd,
        )
        .await;
    }
    accumulator.finish()
}

fn stream_error(message: impl Into<String>) -> LLMResponse {
    LLMResponse::error(message)
}

pub(crate) fn stream_idle_timeout() -> (Duration, u64) {
    let idle_timeout_s: u64 = std::env::var("RUSTBOT_STREAM_IDLE_TIMEOUT_S")
        .unwrap_or_else(|_| "90".to_string())
        .parse()
        .unwrap_or(90);
    (Duration::from_secs(idle_timeout_s), idle_timeout_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_data_done() {
        let parsed = parse_sse_data("data: [DONE]\n").unwrap().unwrap();
        assert!(matches!(parsed, SseData::Done));
    }

    #[test]
    fn parse_sse_data_json_chunk() {
        let parsed = parse_sse_data(r#"data: {"id":"x","choices":[]}"#)
            .unwrap()
            .unwrap();
        match parsed {
            SseData::Json(value) => assert_eq!(value["id"], "x"),
            SseData::Done => panic!("expected json"),
        }
    }

    #[test]
    fn parse_sse_data_error_event() {
        let err = parse_sse_data(
            r#"data: {"error":{"message":"No endpoints found that support image input","code":404}}"#,
        )
        .unwrap()
        .unwrap_err();
        assert!(err.contains("No endpoints found"));
    }

    #[test]
    fn take_sse_event_handles_split_crlf_frames() {
        let mut buffer = b"data: {\"id\":\"a\"}\r\n\r\ndata: [DONE]\r\n\r\n".to_vec();
        match take_sse_event(&mut buffer).unwrap().unwrap() {
            SseData::Json(value) => assert_eq!(value["id"], "a"),
            SseData::Done => panic!("expected json first"),
        }
        assert!(matches!(
            take_sse_event(&mut buffer).unwrap().unwrap(),
            SseData::Done
        ));
        assert!(take_sse_event(&mut buffer).is_none());
    }

    #[test]
    fn apply_chunk_accumulates_content_reasoning_tools_and_usage() {
        let mut acc = StreamAccumulator::new();
        acc.apply_chunk(&serde_json::json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "think ",
                    "content": "Hel"
                }
            }]
        }))
        .unwrap();
        acc.apply_chunk(&serde_json::json!({
            "choices": [{
                "delta": {
                    "reasoning": "hard",
                    "content": "lo",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": { "name": "lookup", "arguments": "{\"q\":" }
                    }]
                }
            }]
        }))
        .unwrap();
        acc.apply_chunk(&serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "\"rust\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 4, "completion_tokens": 2 },
            "service_tier": "standard"
        }))
        .unwrap();

        assert_eq!(acc.content, "Hello");
        assert_eq!(acc.reasoning, "think hard");
        assert_eq!(acc.finish_reason, "tool_calls");
        assert_eq!(acc.usage.input_tokens, Some(4));
        assert_eq!(acc.usage.output_tokens, Some(2));
        let (_, name, args) = acc.tool_call_acc.get(&0).unwrap();
        assert_eq!(name, "lookup");
        assert_eq!(args, r#"{"q":"rust"}"#);

        let response = acc.into_response();
        assert_eq!(response.content.as_deref(), Some("Hello"));
        assert_eq!(response.reasoning_content.as_deref(), Some("think hard"));
        assert_eq!(response.finish_reason, "tool_calls");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "lookup");
    }

    #[tokio::test]
    async fn consume_sse_emits_content_and_reasoning_progress() {
        use std::sync::{Arc, Mutex};

        use crate::bus::outbound_events::ProgressKind;
        use crate::providers::base::BoxedProgressCallback;

        let sse = concat!(
            r#"data: {"choices":[{"delta":{"reasoning_content":"think ","content":"He"}}],"service_tier":"flex"}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"reasoning":"hard","content":"llo"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let (head, tail) = sse.split_at(48);
        let stream = futures::stream::iter([
            Ok::<_, reqwest::Error>(head.as_bytes().to_vec()),
            Ok(tail.as_bytes().to_vec()),
        ]);

        let contents = Arc::new(Mutex::new(Vec::new()));
        let contents_cb = Arc::clone(&contents);
        let on_content = Some(move |delta: String| {
            contents_cb.lock().unwrap().push(delta);
            async {}
        });

        let progress_events = Arc::new(Mutex::new(Vec::new()));
        let progress_cb = Arc::clone(&progress_events);
        let on_progress: BoxedProgressCallback = Box::new(move |content, kind| {
            progress_cb.lock().unwrap().push((content, kind));
            Box::pin(async {})
        });

        let response = consume_sse_byte_stream(
            stream,
            Duration::from_secs(5),
            5,
            &on_content,
            &Some(on_progress),
        )
        .await;

        assert_eq!(response.content.as_deref(), Some("Hello"));
        assert_eq!(response.reasoning_content.as_deref(), Some("think hard"));
        assert_eq!(response.finish_reason, "stop");
        assert_eq!(response.usage.input_tokens, Some(3));
        assert_eq!(*contents.lock().unwrap(), vec!["He", "llo"]);
        assert_eq!(
            *progress_events.lock().unwrap(),
            vec![
                ("think ".into(), ProgressKind::ReasoningDelta),
                ("hard".into(), ProgressKind::ReasoningDelta),
                (String::new(), ProgressKind::ReasoningEnd),
            ]
        );
    }
}
