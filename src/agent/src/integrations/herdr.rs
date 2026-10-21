//! Client for Herdr's pane agent-status protocol.
//!
//! [Herdr](https://herdr.dev) is a terminal-pane manager that can host an
//! arbitrary CLI process in a pane and render a live status indicator for it.
//! When it does, it sets `HERDR_ENV`, `HERDR_SOCKET_PATH`, and `HERDR_PANE_ID`
//! on the hosted process, which may then push `working` / `blocked` / `idle`
//! updates over a local Unix domain socket or Windows named pipe.
//!
//! Herdr ships this integration for another, unrelated coding-agent CLI, but
//! has no reason to ever build one for rust-bot. This module implements the
//! client side of the same protocol (reverse-engineered from that other
//! integration) so rust-bot's status shows up in Herdr panes too. When
//! rust-bot isn't Herdr-hosted, every public entry point here is a no-op.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
#[cfg(test)]
use serde_json::Value;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::config::paths::get_legacy_sessions_dir;
use crate::utils::helpers::safe_filename;

/// Identifies this integration to Herdr, distinct from its bundled integration for other agents.
const SOURCE: &str = "herdr:rust-bot";
const AGENT: &str = "rust-bot";

/// First delivery attempt's timeout before a single, longer retry.
const SHORT_TIMEOUT: Duration = Duration::from_millis(500);
/// Retry timeout used when the short attempt doesn't get an ack in time.
const LONG_TIMEOUT: Duration = Duration::from_millis(1500);
/// Bound on how long [`HerdrReporter::shutdown`] waits for queued events to flush.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(2_500);

// ── wire protocol ───────────────────────────────────────────────────────────

/// Live agent status reported to Herdr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentState {
    Working,
    Blocked,
    Idle,
}

impl AgentState {
    fn as_str(self) -> &'static str {
        match self {
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Idle => "idle",
        }
    }
}

/// One outbound event, queued in the order it was requested.
enum HerdrEvent {
    Session {
        start_source: &'static str,
    },
    State {
        state: AgentState,
        message: Option<String>,
    },
    /// Hands back lifecycle authority for this pane on clean exit. Without
    /// this, Herdr keeps showing the last-reported agent/state forever: it
    /// only clears a self-reported pane's agent identity in response to an
    /// explicit release, never on its own (there is no process-exit
    /// detection for self-reporting integrations like this one, unlike
    /// agents Herdr itself launched via `agent start`).
    Release,
}

fn random_id_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn session_envelope(
    seq: i64,
    pane_id: &str,
    session_path: &str,
    start_source: &'static str,
) -> String {
    json!({
        "id": format!("{SOURCE}:session:{}:{}", chrono::Utc::now().timestamp_millis(), random_id_suffix()),
        "method": "pane.report_agent_session",
        "params": {
            "pane_id": pane_id,
            "source": SOURCE,
            "agent": AGENT,
            "seq": seq,
            "session_start_source": start_source,
            "agent_session_path": session_path,
        },
    })
    .to_string()
}

fn state_envelope(
    seq: i64,
    pane_id: &str,
    session_path: &str,
    state: AgentState,
    message: Option<&str>,
) -> String {
    let mut params = json!({
        "pane_id": pane_id,
        "source": SOURCE,
        "agent": AGENT,
        "state": state.as_str(),
        "seq": seq,
        "agent_session_path": session_path,
    });
    if let Some(message) = message {
        params["message"] = json!(message);
    }
    json!({
        "id": format!("{SOURCE}:{}:{}", chrono::Utc::now().timestamp_millis(), random_id_suffix()),
        "method": "pane.report_agent",
        "params": params,
    })
    .to_string()
}

fn release_envelope(seq: i64, pane_id: &str) -> String {
    json!({
        "id": format!("{SOURCE}:release:{}:{}", chrono::Utc::now().timestamp_millis(), random_id_suffix()),
        "method": "pane.release_agent",
        "params": {
            "pane_id": pane_id,
            "source": SOURCE,
            "agent": AGENT,
            "seq": seq,
        },
    })
    .to_string()
}

/// Mutable state owned by the single background drain task (see
/// [`HerdrReporter::with_transport`]) — deliberately plain fields rather than
/// atomics/mutexes, since only that one task ever touches it.
struct DrainState {
    seq: i64,
    last_sent: Option<(AgentState, Option<String>)>,
}

impl DrainState {
    fn new() -> Self {
        // Mirrors the reference integration's own seed, keeping `seq`
        // roughly time-correlated and comfortably unique across process runs.
        let seed = chrono::Utc::now().timestamp_millis().saturating_mul(1000);
        Self {
            seq: seed,
            last_sent: None,
        }
    }
}

/// Builds the next JSON line to send for `event`, or `None` if it's a state
/// update identical (state and message) to the last one actually sent.
/// Advances `state.seq` only when a line is actually produced.
fn build_envelope(
    state: &mut DrainState,
    event: HerdrEvent,
    pane_id: &str,
    session_path: &str,
) -> Option<String> {
    match event {
        HerdrEvent::Session { start_source } => {
            state.seq += 1;
            Some(session_envelope(
                state.seq,
                pane_id,
                session_path,
                start_source,
            ))
        }
        HerdrEvent::State {
            state: agent_state,
            message,
        } => {
            let key = (agent_state, message.clone());
            if state.last_sent.as_ref() == Some(&key) {
                return None;
            }
            state.seq += 1;
            let line = state_envelope(
                state.seq,
                pane_id,
                session_path,
                agent_state,
                message.as_deref(),
            );
            state.last_sent = Some(key);
            Some(line)
        }
        HerdrEvent::Release => {
            state.seq += 1;
            Some(release_envelope(state.seq, pane_id))
        }
    }
}

// ── env-var hosting detection ────────────────────────────────────────────────

/// Env-var-derived facts about being hosted in a Herdr pane.
struct HerdrHostInfo {
    socket_path: String,
    pane_id: String,
}

/// Parses Herdr's three hosting env vars into [`HerdrHostInfo`], or `None` if
/// rust-bot isn't running inside a Herdr pane (any var missing/empty, or
/// `HERDR_ENV` isn't exactly `"1"`). Takes an injectable lookup so tests never
/// touch real process environment — `HERDR_*` names are fixed, and `cargo
/// test` runs tests in parallel, so mutating real env vars would race.
fn parse_herdr_env(lookup: impl Fn(&str) -> Option<String>) -> Option<HerdrHostInfo> {
    if lookup("HERDR_ENV").as_deref() != Some("1") {
        return None;
    }
    let socket_path = lookup("HERDR_SOCKET_PATH").filter(|v| !v.is_empty())?;
    let pane_id = lookup("HERDR_PANE_ID").filter(|v| !v.is_empty())?;
    Some(HerdrHostInfo {
        socket_path,
        pane_id,
    })
}

/// Where `SessionManager` would read/write `session_id`'s on-disk transcript,
/// without requiring a constructed `SessionManager`. Mirrors
/// `JSONLSessionStore::session_path`'s sanitization by reusing the same
/// public [`safe_filename`] helper, falling back to `legacy_dir` when only a
/// pre-migration copy exists there. The file need not exist yet at either
/// location — Herdr is just told where the session lives, not asked to read it.
fn resolve_session_file_path(workspace: &Path, session_id: &str, legacy_dir: &Path) -> PathBuf {
    let filename = format!("{}.jsonl", safe_filename(session_id));
    let candidate = workspace.join("sessions").join(&filename);
    if candidate.exists() {
        return candidate;
    }
    let legacy = legacy_dir.join(&filename);
    if legacy.exists() { legacy } else { candidate }
}

// ── transport ────────────────────────────────────────────────────────────────

/// Minimal transport seam so the wire protocol can be unit-tested without a
/// real socket/pipe. `try_deliver` reports whether any response byte arrived
/// within `budget` — Herdr's protocol never requires parsing the body.
#[async_trait]
trait HerdrTransport: Send + Sync {
    async fn try_deliver(&self, line: &str, budget: Duration) -> bool;
}

enum HerdrEndpoint {
    #[cfg(windows)]
    NamedPipe(String),
    #[cfg(unix)]
    UnixSocket(PathBuf),
}

#[cfg(windows)]
fn build_endpoint(raw_socket_path: &str) -> HerdrEndpoint {
    HerdrEndpoint::NamedPipe(format!(r"\\.\pipe\{raw_socket_path}"))
}

#[cfg(unix)]
fn build_endpoint(raw_socket_path: &str) -> HerdrEndpoint {
    HerdrEndpoint::UnixSocket(PathBuf::from(raw_socket_path))
}

/// Writes `payload` and waits for a single acknowledgement byte. Generic over
/// the concrete stream type so the Windows named-pipe and Unix-socket paths
/// share one implementation instead of duplicating the write/read logic.
async fn write_and_wait_for_ack<S>(stream: &mut S, payload: &str) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if stream.write_all(payload.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 1];
    matches!(stream.read(&mut buf).await, Ok(n) if n > 0)
}

#[cfg(windows)]
async fn connect_write_ack(endpoint: &HerdrEndpoint, payload: &str) -> bool {
    let HerdrEndpoint::NamedPipe(path) = endpoint;
    // `ClientOptions::open` does not retry on `ERROR_PIPE_BUSY`; a busy pipe
    // simply fails this attempt, which the short/long retry in `try_deliver`
    // (via `RealHerdrTransport`) covers well enough for a best-effort integration.
    let Ok(mut client) = tokio::net::windows::named_pipe::ClientOptions::new().open(path) else {
        return false;
    };
    write_and_wait_for_ack(&mut client, payload).await
}

#[cfg(unix)]
async fn connect_write_ack(endpoint: &HerdrEndpoint, payload: &str) -> bool {
    let HerdrEndpoint::UnixSocket(path) = endpoint;
    let Ok(mut stream) = tokio::net::UnixStream::connect(path).await else {
        return false;
    };
    write_and_wait_for_ack(&mut stream, payload).await
}

/// Real Herdr transport: connects fresh for each attempt (matching the
/// reference integration, which never keeps the socket open between updates).
struct RealHerdrTransport {
    endpoint: HerdrEndpoint,
}

impl RealHerdrTransport {
    /// Builds the transport for `raw_socket_path` (the raw `HERDR_SOCKET_PATH`
    /// value), applying Herdr's Windows-only `\\.\pipe\<name>` convention;
    /// Unix paths are used as-is.
    fn new(raw_socket_path: &str) -> Self {
        Self {
            endpoint: build_endpoint(raw_socket_path),
        }
    }
}

#[async_trait]
impl HerdrTransport for RealHerdrTransport {
    async fn try_deliver(&self, line: &str, budget: Duration) -> bool {
        let payload = format!("{line}\n");
        matches!(
            timeout(budget, connect_write_ack(&self.endpoint, &payload)).await,
            Ok(true)
        )
    }
}

// ── reporter ─────────────────────────────────────────────────────────────────

/// Reports rust-bot's live agent status to Herdr when hosted in a Herdr pane;
/// a complete, zero-overhead no-op otherwise. Every `report_*` method is a
/// cheap, non-blocking channel send, with a single background task
/// serializing actual wire delivery so state updates never race or reorder.
pub struct HerdrReporter {
    sender: StdMutex<Option<mpsc::UnboundedSender<HerdrEvent>>>,
    drain_task: StdMutex<Option<JoinHandle<()>>>,
}

impl HerdrReporter {
    /// Detects Herdr hosting from the real process environment and resolves
    /// `session_id`'s on-disk session file under `workspace`. Safe to call
    /// unconditionally at startup: when not Herdr-hosted, returns a reporter
    /// whose every `report_*` call is a true no-op (no background task spawned).
    pub fn from_env(session_id: &str, workspace: &Path) -> Arc<Self> {
        let Some(host) = parse_herdr_env(|key| std::env::var(key).ok()) else {
            return Arc::new(Self::disabled());
        };
        let transport = Arc::new(RealHerdrTransport::new(&host.socket_path));
        let session_path =
            resolve_session_file_path(workspace, session_id, &get_legacy_sessions_dir());
        Self::with_transport(
            transport,
            host.pane_id,
            session_path.to_string_lossy().into_owned(),
            SHORT_TIMEOUT,
            LONG_TIMEOUT,
        )
    }

    /// A reporter with no transport at all: every `report_*` call is a true
    /// no-op and `shutdown` returns immediately. Used when Herdr isn't
    /// hosting this process.
    fn disabled() -> Self {
        Self {
            sender: StdMutex::new(None),
            drain_task: StdMutex::new(None),
        }
    }

    /// Test double for code that requires a `HerdrReporter` but isn't
    /// exercising this integration (e.g. `CliAskHook`'s own unit tests).
    #[cfg(test)]
    pub(crate) fn disabled_for_test() -> Arc<Self> {
        Arc::new(Self::disabled())
    }

    /// Test/DI seam identical to [`Self::from_env`] but with an injected
    /// transport, pane id, session path, and retry timeouts, so tests never
    /// touch real env vars, sockets, or real-time sleeps.
    fn with_transport(
        transport: Arc<dyn HerdrTransport>,
        pane_id: String,
        session_path: String,
        short_timeout: Duration,
        long_timeout: Duration,
    ) -> Arc<Self> {
        let (sender, mut receiver) = mpsc::unbounded_channel::<HerdrEvent>();
        let drain_task = tokio::spawn(async move {
            let mut state = DrainState::new();
            while let Some(event) = receiver.recv().await {
                let Some(line) = build_envelope(&mut state, event, &pane_id, &session_path) else {
                    continue;
                };
                if !transport.try_deliver(&line, short_timeout).await {
                    transport.try_deliver(&line, long_timeout).await;
                }
            }
        });
        Arc::new(Self {
            sender: StdMutex::new(Some(sender)),
            drain_task: StdMutex::new(Some(drain_task)),
        })
    }

    fn send(&self, event: HerdrEvent) {
        let guard = self.sender.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sender) = guard.as_ref() {
            // Delivery is best-effort; a closed/lagging channel must never
            // propagate as an error into the agent's own control flow.
            let _ = sender.send(event);
        }
    }

    /// Reports that this session has (re)started. Send once, right after the
    /// session's identity is known.
    pub fn report_session(&self, start_source: &'static str) {
        self.send(HerdrEvent::Session { start_source });
    }

    /// Reports that the agent is actively working on a turn.
    pub fn report_working(&self) {
        self.send(HerdrEvent::State {
            state: AgentState::Working,
            message: None,
        });
    }

    /// Reports that the agent is blocked waiting on something (e.g. a
    /// tool-approval prompt), with an optional human-readable reason.
    pub fn report_blocked(&self, message: Option<String>) {
        self.send(HerdrEvent::State {
            state: AgentState::Blocked,
            message,
        });
    }

    /// Reports that the agent has settled and is waiting for the next turn.
    pub fn report_idle(&self) {
        self.send(HerdrEvent::State {
            state: AgentState::Idle,
            message: None,
        });
    }

    /// Releases lifecycle authority for this pane, closes the event queue,
    /// and waits (bounded) for the drain task to flush whatever was already
    /// queued. `#[tokio::main]` drops the runtime the instant `main()`
    /// returns, so a one-shot run that skips this can exit before its final
    /// updates are actually delivered. Without the release, Herdr has no way
    /// to detect that a self-reporting agent like this one has exited, and
    /// keeps showing the last-reported agent/state indefinitely. No-op on a
    /// disabled reporter.
    pub async fn shutdown(&self) {
        self.send(HerdrEvent::Release);
        {
            let mut guard = self.sender.lock().unwrap_or_else(|e| e.into_inner());
            // Dropping the sender closes the channel, letting the drain
            // task's `recv()` return `None` once the queue is empty.
            guard.take();
        }
        let handle = self
            .drain_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = timeout(SHUTDOWN_TIMEOUT, handle).await;
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};

    fn parse_seq(line: &str) -> i64 {
        let value: Value = serde_json::from_str(line).unwrap();
        value["params"]["seq"].as_i64().unwrap()
    }

    // ── parse_herdr_env ───────────────────────────────────────────────────

    #[test]
    fn parse_herdr_env_accepts_all_three_vars_present() {
        let vars = HashMap::from([
            ("HERDR_ENV".to_string(), "1".to_string()),
            ("HERDR_SOCKET_PATH".to_string(), "pipe-name".to_string()),
            ("HERDR_PANE_ID".to_string(), "pane-1".to_string()),
        ]);
        let host = parse_herdr_env(|key| vars.get(key).cloned());
        assert!(host.is_some());
        let host = host.unwrap();
        assert_eq!(host.socket_path, "pipe-name");
        assert_eq!(host.pane_id, "pane-1");
    }

    #[test]
    fn parse_herdr_env_rejects_missing_or_empty_vars() {
        let cases: Vec<HashMap<String, String>> = vec![
            HashMap::new(),
            HashMap::from([
                ("HERDR_ENV".into(), "0".into()),
                ("HERDR_SOCKET_PATH".into(), "x".into()),
                ("HERDR_PANE_ID".into(), "y".into()),
            ]),
            HashMap::from([
                ("HERDR_ENV".into(), "1".into()),
                ("HERDR_SOCKET_PATH".into(), "".into()),
                ("HERDR_PANE_ID".into(), "y".into()),
            ]),
            HashMap::from([
                ("HERDR_ENV".into(), "1".into()),
                ("HERDR_SOCKET_PATH".into(), "x".into()),
            ]),
        ];
        for vars in cases {
            assert!(parse_herdr_env(|key| vars.get(key).cloned()).is_none());
        }
    }

    // ── resolve_session_file_path ─────────────────────────────────────────

    #[test]
    fn resolve_session_file_path_uses_new_location_when_neither_exists() {
        let workspace = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let path = resolve_session_file_path(workspace.path(), "cli:direct", legacy.path());
        assert_eq!(
            path,
            workspace.path().join("sessions").join("cli_direct.jsonl")
        );
    }

    #[test]
    fn resolve_session_file_path_falls_back_to_legacy_when_only_legacy_exists() {
        let workspace = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let legacy_file = legacy.path().join("cli_direct.jsonl");
        std::fs::write(&legacy_file, "").unwrap();
        let path = resolve_session_file_path(workspace.path(), "cli:direct", legacy.path());
        assert_eq!(path, legacy_file);
    }

    #[test]
    fn resolve_session_file_path_prefers_new_location_when_both_exist() {
        let workspace = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let new_dir = workspace.path().join("sessions");
        std::fs::create_dir_all(&new_dir).unwrap();
        let new_file = new_dir.join("cli_direct.jsonl");
        std::fs::write(&new_file, "").unwrap();
        std::fs::write(legacy.path().join("cli_direct.jsonl"), "").unwrap();
        let path = resolve_session_file_path(workspace.path(), "cli:direct", legacy.path());
        assert_eq!(path, new_file);
    }

    // ── build_envelope ────────────────────────────────────────────────────

    #[test]
    fn build_envelope_dedups_identical_consecutive_state_updates() {
        let mut state = DrainState::new();
        let session = "s1".to_string();
        let first = build_envelope(
            &mut state,
            HerdrEvent::State {
                state: AgentState::Working,
                message: None,
            },
            "pane-1",
            &session,
        );
        assert!(first.is_some());
        let second = build_envelope(
            &mut state,
            HerdrEvent::State {
                state: AgentState::Working,
                message: None,
            },
            "pane-1",
            &session,
        );
        assert!(
            second.is_none(),
            "identical consecutive state must be deduped"
        );
    }

    #[test]
    fn build_envelope_does_not_dedup_message_change_on_same_state() {
        let mut state = DrainState::new();
        let session = "s1".to_string();
        build_envelope(
            &mut state,
            HerdrEvent::State {
                state: AgentState::Blocked,
                message: Some("a".into()),
            },
            "pane-1",
            &session,
        );
        let second = build_envelope(
            &mut state,
            HerdrEvent::State {
                state: AgentState::Blocked,
                message: Some("b".into()),
            },
            "pane-1",
            &session,
        );
        assert!(
            second.is_some(),
            "a message change must not be deduped even for the same state"
        );
    }

    #[test]
    fn build_envelope_seq_strictly_increases_across_events() {
        let mut state = DrainState::new();
        let session = "s1".to_string();
        let a = build_envelope(
            &mut state,
            HerdrEvent::State {
                state: AgentState::Working,
                message: None,
            },
            "pane-1",
            &session,
        )
        .unwrap();
        let b = build_envelope(
            &mut state,
            HerdrEvent::State {
                state: AgentState::Idle,
                message: None,
            },
            "pane-1",
            &session,
        )
        .unwrap();
        assert!(parse_seq(&b) > parse_seq(&a));
    }

    #[test]
    fn session_envelope_has_expected_shape() {
        let mut state = DrainState::new();
        let session = "/workspace/sessions/cli_direct.jsonl".to_string();
        let line = build_envelope(
            &mut state,
            HerdrEvent::Session {
                start_source: "cli-interactive",
            },
            "pane-1",
            &session,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["method"], "pane.report_agent_session");
        assert_eq!(value["params"]["pane_id"], "pane-1");
        assert_eq!(value["params"]["source"], SOURCE);
        assert_eq!(value["params"]["agent"], AGENT);
        assert_eq!(value["params"]["session_start_source"], "cli-interactive");
        assert_eq!(
            value["params"]["agent_session_path"],
            "/workspace/sessions/cli_direct.jsonl"
        );
    }

    #[test]
    fn state_envelope_includes_message_only_when_present() {
        let mut state = DrainState::new();
        let session = "s1".to_string();
        let without = build_envelope(
            &mut state,
            HerdrEvent::State {
                state: AgentState::Idle,
                message: None,
            },
            "pane-1",
            &session,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&without).unwrap();
        assert_eq!(value["method"], "pane.report_agent");
        assert_eq!(value["params"]["state"], "idle");
        assert!(value["params"].get("message").is_none());

        let with = build_envelope(
            &mut state,
            HerdrEvent::State {
                state: AgentState::Blocked,
                message: Some("waiting".into()),
            },
            "pane-1",
            &session,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&with).unwrap();
        assert_eq!(value["params"]["state"], "blocked");
        assert_eq!(value["params"]["message"], "waiting");
    }

    #[test]
    fn release_envelope_has_expected_shape_and_is_never_deduped() {
        let mut state = DrainState::new();
        let session = "s1".to_string();
        // A release is never suppressed by state dedup, even with no prior events.
        let line = build_envelope(&mut state, HerdrEvent::Release, "pane-1", &session).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["method"], "pane.release_agent");
        assert_eq!(value["params"]["pane_id"], "pane-1");
        assert_eq!(value["params"]["source"], SOURCE);
        assert_eq!(value["params"]["agent"], AGENT);
        assert!(value["params"].get("state").is_none());
        assert!(value["params"].get("agent_session_path").is_none());
    }

    // ── endpoint construction ─────────────────────────────────────────────

    #[cfg(windows)]
    #[test]
    fn build_endpoint_uses_named_pipe_convention_on_windows() {
        match build_endpoint("my-socket") {
            HerdrEndpoint::NamedPipe(path) => assert_eq!(path, r"\\.\pipe\my-socket"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn build_endpoint_uses_raw_path_on_unix() {
        match build_endpoint("/tmp/my.sock") {
            HerdrEndpoint::UnixSocket(path) => assert_eq!(path, PathBuf::from("/tmp/my.sock")),
        }
    }

    // ── HerdrReporter against a fake transport ─────────────────────────────

    struct FakeHerdrTransport {
        delivered: StdMutex<Vec<String>>,
        responses: StdMutex<VecDeque<bool>>,
    }

    impl FakeHerdrTransport {
        fn always_ack() -> Arc<Self> {
            Arc::new(Self {
                delivered: StdMutex::new(Vec::new()),
                responses: StdMutex::new(VecDeque::new()),
            })
        }

        /// Scripts a fixed sequence of ack outcomes, one per `try_deliver`
        /// call; once exhausted, further calls default to acking (`true`).
        fn scripted(responses: Vec<bool>) -> Arc<Self> {
            Arc::new(Self {
                delivered: StdMutex::new(Vec::new()),
                responses: StdMutex::new(responses.into()),
            })
        }

        fn delivered_lines(&self) -> Vec<String> {
            self.delivered.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HerdrTransport for FakeHerdrTransport {
        async fn try_deliver(&self, line: &str, _budget: Duration) -> bool {
            self.delivered.lock().unwrap().push(line.to_string());
            self.responses.lock().unwrap().pop_front().unwrap_or(true)
        }
    }

    fn tiny_timeouts() -> (Duration, Duration) {
        (Duration::from_millis(5), Duration::from_millis(5))
    }

    #[tokio::test]
    async fn reporter_delivers_events_in_order_and_dedups() {
        let fake = FakeHerdrTransport::always_ack();
        let (short, long) = tiny_timeouts();
        let reporter =
            HerdrReporter::with_transport(fake.clone(), "pane-1".into(), "s1".into(), short, long);

        reporter.report_working();
        reporter.report_blocked(Some("waiting".into()));
        reporter.report_working();
        reporter.report_working(); // duplicate — must be deduped
        reporter.report_idle();
        reporter.shutdown().await;

        let delivered = fake.delivered_lines();
        let parsed: Vec<Value> = delivered
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let states: Vec<String> = parsed
            .iter()
            .filter(|v| v["method"] == "pane.report_agent")
            .map(|v| v["params"]["state"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(states, vec!["working", "blocked", "working", "idle"]);
        // `shutdown` must release lifecycle authority last, after every queued state.
        assert_eq!(parsed.last().unwrap()["method"], "pane.release_agent");
    }

    #[tokio::test]
    async fn reporter_retries_once_on_failed_short_attempt() {
        let fake = FakeHerdrTransport::scripted(vec![false, true]);
        let (short, long) = tiny_timeouts();
        let reporter =
            HerdrReporter::with_transport(fake.clone(), "pane-1".into(), "s1".into(), short, long);

        reporter.report_working();
        reporter.shutdown().await;

        // 2 attempts for the working report (fails short, succeeds on retry)
        // plus 1 for shutdown's release (scripted responses exhausted by then,
        // so `FakeHerdrTransport` defaults to acking it on the first attempt).
        assert_eq!(fake.delivered_lines().len(), 3);
    }

    #[tokio::test]
    async fn disabled_reporter_is_a_complete_no_op() {
        let reporter = HerdrReporter::disabled_for_test();
        reporter.report_session("cli-interactive");
        reporter.report_working();
        reporter.report_blocked(Some("x".into()));
        reporter.report_idle();
        reporter.shutdown().await; // must return promptly, no drain task exists
    }
}
