//! A tiny OpenAI-compatible chat server for tests that run the real binary.
//!
//! The child process is configured with `apiBase` pointing here, so a whole
//! parent → child → LLM round trip runs with no network and no API key.
//! Streaming requests (the agent's turns) take the next scripted reply; every
//! other request (session title, memory consolidation) gets a fixed answer so it
//! cannot consume a scripted reply. Every request body is recorded.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// One scripted model reply.
#[derive(Debug, Clone)]
pub enum MockReply {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

impl MockReply {
    pub fn text(text: &str) -> Self {
        MockReply::Text(text.to_string())
    }

    pub fn tool_call(id: &str, name: &str, arguments: Value) -> Self {
        MockReply::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        }
    }
}

/// A running mock server. It stops accepting when dropped.
pub struct MockLlm {
    port: u16,
    script: Arc<Mutex<VecDeque<MockReply>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl MockLlm {
    /// Start a server that answers streaming requests from `script`.
    pub fn start(script: Vec<MockReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the mock LLM server");
        let port = listener.local_addr().unwrap().port();
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (accept_script, accept_requests) = (Arc::clone(&script), Arc::clone(&requests));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (script, requests) = (Arc::clone(&accept_script), Arc::clone(&accept_requests));
                std::thread::spawn(move || handle(stream, &script, &requests));
            }
        });
        Self {
            port,
            script,
            requests,
        }
    }

    /// The `apiBase` to put in a provider's config.
    pub fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// Add replies for later turns.
    pub fn push(&self, replies: Vec<MockReply>) {
        self.script.lock().unwrap().extend(replies);
    }

    /// Every request body received so far.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    /// Everything the model was shown, as one string (for `contains` checks).
    pub fn everything_seen(&self) -> String {
        serde_json::to_string(&self.requests()).unwrap()
    }
}

fn handle(
    mut stream: TcpStream,
    script: &Mutex<VecDeque<MockReply>>,
    requests: &Mutex<Vec<Value>>,
) {
    let Some((path, body)) = read_request(&mut stream) else {
        return;
    };
    if !path.ends_with("/chat/completions") {
        let _ = respond(&mut stream, "404 Not Found", "application/json", "{}");
        return;
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    requests.lock().unwrap().push(body.clone());

    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        let reply = script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| MockReply::text("(script exhausted)"));
        let _ = respond(
            &mut stream,
            "200 OK",
            "text/event-stream",
            &sse_body(&reply),
        );
    } else {
        let completion = json!({
            "id": "mock",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Mock title"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        let _ = respond(
            &mut stream,
            "200 OK",
            "application/json",
            &completion.to_string(),
        );
    }
}

/// The path and body of one HTTP request.
fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some((path, body))
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// The SSE body that streams `reply`.
fn sse_body(reply: &MockReply) -> String {
    let (delta, finish_reason) = match reply {
        MockReply::Text(text) => (json!({"role": "assistant", "content": text}), "stop"),
        MockReply::ToolCall {
            id,
            name,
            arguments,
        } => (
            json!({"role": "assistant", "tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            }]}),
            "tool_calls",
        ),
    };
    let content =
        json!({"id": "mock", "choices": [{"index": 0, "delta": delta, "finish_reason": null}]});
    let finish = json!({
        "id": "mock",
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
    });
    format!("data: {content}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}
