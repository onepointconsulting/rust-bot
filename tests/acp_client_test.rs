//! The ACP **client** role (`rust-bot`'s parent side) against a child agent.
//!
//! Most tests drive the real `serve` (the child's agent role, with a scripted LLM)
//! over an in-process channel, so the whole turn is real: tool calls, replayed
//! history, permission requests. The negative cases (a child that cannot load
//! sessions, one that ignores `session/cancel`) use a small mock agent, because
//! the real one always behaves.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
};
use agent_client_protocol::{Agent, Channel, ConnectTo, Error, on_receive_request};
use async_trait::async_trait;
use rust_bot::agent::acp::agent_mode::serve;
use rust_bot::agent::acp::client::{
    ActivityStatus, ClientError, ProgressSink, TurnEnd, TurnOutcome, TurnRequest, run_turn,
};
use rust_bot::agent::acp::permission::{
    Escalator, PermissionAsk, PermissionResponder, PolicyResponder,
};
use rust_bot::cli::acp::AcpRuntime;
use rust_bot::config::schema::AcpPermissionPolicy;
use serde_json::json;

mod support;

use support::{
    Fixture, fixture, long_running_command, text_reply, tool_call_reply, write_project_file,
};

// ── helpers ─────────────────────────────────────────────────────────────────

fn policy(policy: AcpPermissionPolicy) -> Arc<dyn PermissionResponder> {
    Arc::new(PolicyResponder::new(policy, None))
}

fn request(
    cwd: &Path,
    prompt: &str,
    previous_session: Option<&str>,
    responder: Arc<dyn PermissionResponder>,
) -> TurnRequest {
    TurnRequest {
        cwd: cwd.to_path_buf(),
        prompt: prompt.to_string(),
        previous_session: previous_session.map(str::to_string),
        startup_timeout: Duration::from_secs(20),
        turn_timeout: Duration::from_secs(30),
        cancel_grace: Duration::from_secs(5),
        heartbeat_every: Duration::from_secs(60),
        responder,
        progress: None,
    }
}

/// Run `turn` against the real child agent of `runtime`.
async fn run_against(runtime: &AcpRuntime, turn: TurnRequest) -> Result<TurnOutcome, ClientError> {
    let (agent_side, client_side) = Channel::duplex();
    let agent = serve(
        Arc::clone(&runtime.agent_loop),
        Arc::clone(&runtime.registry),
        Arc::clone(&runtime.slot),
        agent_side,
    );
    let (agent_result, outcome) = tokio::join!(agent, run_turn(client_side, turn));
    agent_result.expect("the child agent served cleanly");
    outcome
}

/// Records what the parent reported while it waited.
#[derive(Default)]
struct RecordingProgress {
    tools: Mutex<Vec<String>>,
    heartbeats: Mutex<usize>,
}

impl ProgressSink for RecordingProgress {
    fn tool_started(&self, title: &str) {
        self.tools.lock().unwrap().push(title.to_string());
    }

    fn still_working(&self, _elapsed: Duration) {
        *self.heartbeats.lock().unwrap() += 1;
    }
}

/// Answers every escalation with a fixed decision and remembers what it was asked.
struct FixedEscalator {
    answer: bool,
    asked: Mutex<Vec<PermissionAsk>>,
}

#[async_trait]
impl Escalator for FixedEscalator {
    async fn ask(&self, ask: &PermissionAsk) -> bool {
        self.asked.lock().unwrap().push(ask.clone());
        self.answer
    }
}

fn with_notes(fixture: &Fixture) {
    write_project_file(
        fixture.project.path(),
        "notes.txt",
        "hello from the project",
    );
}

// ── a run ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_run_returns_the_reply_and_what_the_child_did() {
    let fixture = fixture(vec![
        tool_call_reply("call-1", "read_file", json!({"path": "notes.txt"})),
        text_reply("The file says: hello from the project"),
    ]);
    with_notes(&fixture);

    let outcome = run_against(
        &fixture.runtime,
        request(
            fixture.project.path(),
            "what is in notes.txt?",
            None,
            policy(AcpPermissionPolicy::AutoApproveRead),
        ),
    )
    .await
    .unwrap();

    assert_eq!(outcome.reply, "The file says: hello from the project");
    assert_eq!(outcome.end, TurnEnd::Finished);
    assert!(!outcome.session_id.is_empty());
    assert!(!outcome.resumed && !outcome.session_replaced);
    assert_eq!(outcome.activity.len(), 1, "{:?}", outcome.activity);
    assert!(
        outcome.activity[0].title.contains("notes.txt"),
        "{:?}",
        outcome.activity
    );
    assert_eq!(outcome.activity[0].status, ActivityStatus::Completed);
}

#[tokio::test]
async fn each_tool_call_is_reported_once_as_progress() {
    let fixture = fixture(vec![
        tool_call_reply("call-1", "read_file", json!({"path": "notes.txt"})),
        text_reply("done"),
    ]);
    with_notes(&fixture);
    let progress = Arc::new(RecordingProgress::default());
    let mut turn = request(
        fixture.project.path(),
        "read it",
        None,
        policy(AcpPermissionPolicy::AutoApproveRead),
    );
    turn.progress = Some(progress.clone());

    run_against(&fixture.runtime, turn).await.unwrap();

    let tools = progress.tools.lock().unwrap().clone();
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert!(tools[0].contains("notes.txt"), "{tools:?}");
}

// ── session continuity ──────────────────────────────────────────────────────

#[tokio::test]
async fn the_second_run_loads_the_session_and_the_child_remembers_without_replaying_it() {
    let first = fixture(vec![
        tool_call_reply("call-1", "read_file", json!({"path": "notes.txt"})),
        text_reply("first answer"),
    ]);
    with_notes(&first);
    let first_outcome = run_against(
        &first.runtime,
        request(
            first.project.path(),
            "what is in notes.txt?",
            None,
            policy(AcpPermissionPolicy::AutoApproveRead),
        ),
    )
    .await
    .unwrap();

    // A new process on the same workspace, like the next launch of the child.
    let (runtime, provider) = first.restarted_runtime(vec![text_reply("second answer")]);
    let second_outcome = run_against(
        &runtime,
        request(
            first.project.path(),
            "and what did I ask before?",
            Some(&first_outcome.session_id),
            policy(AcpPermissionPolicy::AutoApproveRead),
        ),
    )
    .await
    .unwrap();

    assert!(second_outcome.resumed);
    assert!(!second_outcome.session_replaced);
    assert_eq!(second_outcome.session_id, first_outcome.session_id);
    // The replayed history (first answer, first tool call) is not in the new reply.
    assert_eq!(second_outcome.reply, "second answer");
    assert!(
        second_outcome.activity.is_empty(),
        "{:?}",
        second_outcome.activity
    );
    // But the model did see the earlier turn.
    assert!(provider.everything_seen().contains("what is in notes.txt?"));
}

#[tokio::test]
async fn a_session_the_child_does_not_know_is_replaced_by_a_new_one() {
    let fixture = fixture(vec![text_reply("fresh start")]);

    let outcome = run_against(
        &fixture.runtime,
        request(
            fixture.project.path(),
            "hello",
            Some("session-that-never-existed"),
            policy(AcpPermissionPolicy::AutoApproveRead),
        ),
    )
    .await
    .unwrap();

    assert!(outcome.session_replaced);
    assert!(!outcome.resumed);
    assert_ne!(outcome.session_id, "session-that-never-existed");
    assert_eq!(outcome.reply, "fresh start");
}

// ── permission policy ───────────────────────────────────────────────────────

/// Script for a turn in which the model runs one shell command, then answers.
fn shell_turn_script() -> Vec<rust_bot::providers::base::LLMResponse> {
    vec![
        tool_call_reply("shell-1", "shell", json!({"command": "echo marker-4711"})),
        text_reply("shell turn finished"),
    ]
}

#[tokio::test]
async fn auto_approve_read_denies_a_shell_call_and_the_child_sees_the_denial() {
    let fixture = fixture(shell_turn_script());

    let outcome = run_against(
        &fixture.runtime,
        request(
            fixture.project.path(),
            "run echo",
            None,
            policy(AcpPermissionPolicy::AutoApproveRead),
        ),
    )
    .await
    .unwrap();

    assert_eq!(outcome.end, TurnEnd::Finished);
    let seen = fixture.provider.everything_seen();
    assert!(seen.contains("denied"), "{seen}");
    assert!(
        !seen.contains("marker-4711\\r") && !seen.contains("marker-4711\\n"),
        "{seen}"
    );
    assert_eq!(
        outcome.activity[0].status,
        ActivityStatus::Failed,
        "{:?}",
        outcome.activity
    );
}

#[tokio::test]
async fn allow_all_lets_the_shell_call_run() {
    let fixture = fixture(shell_turn_script());

    let outcome = run_against(
        &fixture.runtime,
        request(
            fixture.project.path(),
            "run echo",
            None,
            policy(AcpPermissionPolicy::AllowAll),
        ),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.activity[0].status,
        ActivityStatus::Completed,
        "{:?}",
        outcome.activity
    );
    let seen = fixture.provider.everything_seen();
    assert!(seen.contains("marker-4711"), "{seen}");
}

#[tokio::test]
async fn escalate_asks_the_human_with_the_childs_request_and_obeys_the_answer() {
    let escalator = Arc::new(FixedEscalator {
        answer: true,
        asked: Mutex::new(Vec::new()),
    });
    let responder: Arc<dyn PermissionResponder> = Arc::new(PolicyResponder::new(
        AcpPermissionPolicy::Escalate,
        Some(escalator.clone()),
    ));
    let fixture = fixture(shell_turn_script());

    let outcome = run_against(
        &fixture.runtime,
        request(fixture.project.path(), "run echo", None, responder),
    )
    .await
    .unwrap();

    assert_eq!(outcome.activity[0].status, ActivityStatus::Completed);
    let asked = escalator.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].title.contains("echo marker-4711"), "{asked:?}");
    assert_eq!(
        asked[0].raw_input,
        Some(json!({"command": "echo marker-4711"}))
    );
}

#[tokio::test]
async fn escalate_with_nobody_to_ask_denies() {
    let fixture = fixture(shell_turn_script());

    let outcome = run_against(
        &fixture.runtime,
        request(
            fixture.project.path(),
            "run echo",
            None,
            policy(AcpPermissionPolicy::Escalate),
        ),
    )
    .await
    .unwrap();

    assert_eq!(outcome.activity[0].status, ActivityStatus::Failed);
    assert!(fixture.provider.everything_seen().contains("denied"));
}

// ── timeouts ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_slow_turn_is_cancelled_the_cancel_is_confirmed_and_a_heartbeat_was_sent() {
    let fixture = fixture(vec![tool_call_reply(
        "long-1",
        "shell",
        json!({"command": long_running_command()}),
    )]);
    let progress = Arc::new(RecordingProgress::default());
    let mut turn = request(
        fixture.project.path(),
        "run the long command",
        None,
        policy(AcpPermissionPolicy::AllowAll),
    );
    turn.turn_timeout = Duration::from_millis(1500);
    turn.heartbeat_every = Duration::from_millis(300);
    turn.progress = Some(progress.clone());

    let outcome = run_against(&fixture.runtime, turn).await.unwrap();

    assert_eq!(
        outcome.end,
        TurnEnd::TimedOut {
            cancel_confirmed: true
        }
    );
    assert!(
        *progress.heartbeats.lock().unwrap() >= 1,
        "no heartbeat while the child ran a long command"
    );
    assert_eq!(progress.tools.lock().unwrap().len(), 1);
}

// ── a mock agent for what the real one never does ───────────────────────────

/// What the mock agent saw and how it behaves.
#[derive(Default)]
struct MockAgent {
    advertises_load_session: bool,
    new_sessions: Mutex<usize>,
    loads: Mutex<usize>,
}

/// Serve a mock agent that never answers a prompt and ignores `session/cancel`.
async fn serve_silent_mock(
    mock: Arc<MockAgent>,
    transport: impl ConnectTo<Agent> + 'static,
) -> Result<(), Error> {
    let (init_mock, new_mock, load_mock) = (mock.clone(), mock.clone(), mock);
    let result = Agent
        .builder()
        .name("mock-agent")
        .on_receive_request(
            async move |_request: InitializeRequest, responder, _connection| {
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                        AgentCapabilities::new().load_session(init_mock.advertises_load_session),
                    ),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: NewSessionRequest, responder, _connection| {
                *new_mock.new_sessions.lock().unwrap() += 1;
                responder.respond(NewSessionResponse::new("mock-session"))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: LoadSessionRequest, responder, _connection| {
                *load_mock.loads.lock().unwrap() += 1;
                responder.respond(LoadSessionResponse::new())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: PromptRequest, responder, _connection| {
                // Never answer, and keep the responder alive so the request stays pending.
                std::mem::forget(responder);
                Ok(())
            },
            on_receive_request!(),
        )
        .connect_to(transport)
        .await;
    match result {
        Err(error) if !agent_client_protocol::is_incoming_transport_closed(&error) => Err(error),
        _ => Ok(()),
    }
}

async fn run_against_mock(
    mock: Arc<MockAgent>,
    turn: TurnRequest,
) -> Result<TurnOutcome, ClientError> {
    let (agent_side, client_side) = Channel::duplex();
    let agent = serve_silent_mock(mock, agent_side);
    let (agent_result, outcome) = tokio::join!(agent, run_turn(client_side, turn));
    agent_result.expect("the mock agent served cleanly");
    outcome
}

#[tokio::test]
async fn a_child_that_ignores_the_cancel_is_reported_as_not_confirming() {
    let mock = Arc::new(MockAgent {
        advertises_load_session: true,
        ..MockAgent::default()
    });
    let project = tempfile::tempdir().unwrap();
    let mut turn = request(
        project.path(),
        "hello?",
        None,
        policy(AcpPermissionPolicy::AutoApproveRead),
    );
    turn.turn_timeout = Duration::from_millis(300);
    turn.cancel_grace = Duration::from_millis(300);

    let outcome = run_against_mock(mock, turn).await.unwrap();

    assert_eq!(
        outcome.end,
        TurnEnd::TimedOut {
            cancel_confirmed: false
        }
    );
}

#[tokio::test]
async fn session_load_is_never_called_on_a_child_that_does_not_advertise_it() {
    let mock = Arc::new(MockAgent {
        advertises_load_session: false,
        ..MockAgent::default()
    });
    let project = tempfile::tempdir().unwrap();
    let mut turn = request(
        project.path(),
        "hello?",
        Some("an-earlier-session"),
        policy(AcpPermissionPolicy::AutoApproveRead),
    );
    turn.turn_timeout = Duration::from_millis(200);
    turn.cancel_grace = Duration::from_millis(100);

    let outcome = run_against_mock(Arc::clone(&mock), turn).await.unwrap();

    assert_eq!(
        *mock.loads.lock().unwrap(),
        0,
        "session/load must not be called"
    );
    assert_eq!(*mock.new_sessions.lock().unwrap(), 1);
    assert!(outcome.session_replaced);
    assert_eq!(outcome.session_id, "mock-session");
}

#[tokio::test]
async fn a_child_that_advertises_session_load_is_asked_to_load() {
    let mock = Arc::new(MockAgent {
        advertises_load_session: true,
        ..MockAgent::default()
    });
    let project = tempfile::tempdir().unwrap();
    let mut turn = request(
        project.path(),
        "hello?",
        Some("an-earlier-session"),
        policy(AcpPermissionPolicy::AutoApproveRead),
    );
    turn.turn_timeout = Duration::from_millis(200);
    turn.cancel_grace = Duration::from_millis(100);

    let outcome = run_against_mock(Arc::clone(&mock), turn).await.unwrap();

    assert_eq!(*mock.loads.lock().unwrap(), 1);
    assert_eq!(*mock.new_sessions.lock().unwrap(), 0);
    assert!(outcome.resumed);
    assert_eq!(outcome.session_id, "an-earlier-session");
}

#[tokio::test]
async fn a_child_that_never_answers_initialize_fails_startup_with_a_clear_error() {
    // A transport whose other end never speaks.
    let (silent_side, client_side) = Channel::duplex();
    let project = tempfile::tempdir().unwrap();
    let mut turn = request(
        project.path(),
        "hello?",
        None,
        policy(AcpPermissionPolicy::AutoApproveRead),
    );
    turn.startup_timeout = Duration::from_millis(300);

    let outcome = run_turn(client_side, turn).await;
    drop(silent_side);

    match outcome {
        Err(ClientError::Startup(message)) => assert!(message.contains("no answer"), "{message}"),
        other => panic!("expected a startup error, got {other:?}"),
    }
}
