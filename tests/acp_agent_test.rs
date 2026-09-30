//! End-to-end tests of `rust-bot acp`'s agent role.
//!
//! A real ACP client (the `agent-client-protocol` crate's client role) talks to
//! the real `serve` over an in-process channel. The LLM is a scripted mock, so
//! no network or API key is needed. The agent loop is assembled with the same
//! `assemble_acp_runtime` the binary uses, so the ACP hook, the tool list and
//! the session scope are the production ones.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCallStatus, ToolKind,
};
use agent_client_protocol::{
    Agent, Channel, Client, ConnectionTo, Error, on_receive_notification, on_receive_request,
};
use rust_bot::agent::acp::agent_mode::serve;
use rust_bot::cli::acp::{AcpRuntime, assemble_acp_runtime};
use rust_bot::config::schema::Config;
use rust_bot::providers::base::{
    BoxedProgressCallback, GenerationSettings, LLMProvider, LLMProviderDyn, LLMResponse, LLMUsage,
    ToolCallRequest,
};
use rust_bot::providers::registry::ProviderSpec;
use rust_bot::utils::helpers::sync_workspace_templates;
use serde_json::{Value, json};

// ── scripted LLM ────────────────────────────────────────────────────────────

/// Replies from a fixed script (one per streamed agent-turn call); records every
/// message list a turn was given.
struct ScriptedProvider {
    script: Mutex<VecDeque<LLMResponse>>,
    seen_messages: Mutex<Vec<Vec<Value>>>,
    generation: GenerationSettings,
}

impl ScriptedProvider {
    fn new(script: Vec<LLMResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            seen_messages: Mutex::new(Vec::new()),
            generation: GenerationSettings::new(),
        })
    }

    fn next_response(&self, messages: Vec<Value>) -> LLMResponse {
        self.seen_messages.lock().unwrap().push(messages);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_reply("(script exhausted)"))
    }

    /// Text of every message the model was shown, across all calls.
    fn everything_seen(&self) -> String {
        serde_json::to_string(&*self.seen_messages.lock().unwrap()).unwrap()
    }
}

fn text_reply(text: &str) -> LLMResponse {
    LLMResponse {
        content: Some(text.to_string()),
        tool_calls: Vec::new(),
        finish_reason: "stop".to_string(),
        usage: LLMUsage::new(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

fn tool_call_reply(id: &str, name: &str, arguments: Value) -> LLMResponse {
    LLMResponse {
        content: None,
        tool_calls: vec![ToolCallRequest {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments
                .as_object()
                .unwrap()
                .clone()
                .into_iter()
                .collect::<HashMap<_, _>>(),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }],
        finish_reason: "tool_calls".to_string(),
        usage: LLMUsage::new(),
        reasoning_content: None,
        thinking_blocks: None,
    }
}

/// `Arc<ScriptedProvider>` is what the test keeps; this newtype is what the agent owns.
struct SharedProvider(Arc<ScriptedProvider>);

impl LLMProvider for SharedProvider {
    fn new(
        _api_key: Option<String>,
        _api_base: Option<String>,
        _default_model: Option<String>,
        _extra_headers: Option<HashMap<String, String>>,
        _spec: Option<ProviderSpec>,
    ) -> Self {
        SharedProvider(ScriptedProvider::new(Vec::new()))
    }

    fn api_key(&self) -> Option<String> {
        None
    }

    fn api_base(&self) -> Option<String> {
        None
    }

    fn extra_headers(&self) -> Option<HashMap<String, String>> {
        None
    }

    fn generation_settings(&self) -> &GenerationSettings {
        &self.0.generation
    }

    fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
        // Only reachable before the provider is shared; the tests never call it.
        unimplemented!("generation settings are fixed for the scripted provider")
    }

    fn spec(&self) -> Option<&ProviderSpec> {
        None
    }

    fn get_default_model(&self) -> String {
        "scripted".to_string()
    }

    async fn chat(
        &self,
        messages: Vec<Value>,
        _tools: Option<Vec<Value>>,
        _model: Option<String>,
        _max_tokens: usize,
        _temperature: Option<f32>,
        _reasoning_effort: Option<String>,
        _tool_choice: Option<Value>,
    ) -> LLMResponse {
        // Non-streaming calls are background utilities (session title, memory
        // consolidation): agent turns always stream. They must not consume the
        // replies scripted for the turns.
        let _ = messages;
        text_reply("Scripted title")
    }

    async fn chat_stream<F, Fut>(
        &self,
        messages: Vec<Value>,
        _tools: Option<Vec<Value>>,
        _model: Option<String>,
        _max_tokens: usize,
        _temperature: Option<f32>,
        _reasoning_effort: Option<String>,
        _tool_choice: Option<Value>,
        on_content_delta: &Option<F>,
        _on_progress: &Option<BoxedProgressCallback>,
    ) -> LLMResponse
    where
        F: Fn(String) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let response = self.0.next_response(messages);
        if let (Some(callback), Some(text)) = (on_content_delta, &response.content) {
            callback(text.clone()).await;
        }
        response
    }
}

// ── test client ─────────────────────────────────────────────────────────────

/// What the test client answers when the agent asks for permission.
#[derive(Clone, Copy)]
enum Permission {
    Allow,
    Reject,
    Cancelled,
}

/// Everything the test client received from the agent.
#[derive(Default)]
struct ClientLog {
    updates: Mutex<Vec<SessionUpdate>>,
    permission_requests: Mutex<Vec<RequestPermissionRequest>>,
}

impl ClientLog {
    fn updates(&self) -> Vec<SessionUpdate> {
        self.updates.lock().unwrap().clone()
    }

    fn permission_request_count(&self) -> usize {
        self.permission_requests.lock().unwrap().len()
    }

    /// Concatenated `agent_message_chunk` text.
    fn agent_text(&self) -> String {
        self.updates()
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// `(toolCallId, status)` of every `tool_call_update` that carries a status.
    fn tool_statuses(&self) -> Vec<(String, ToolCallStatus)> {
        self.updates()
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::ToolCallUpdate(update) => update
                    .fields
                    .status
                    .map(|status| (update.tool_call_id.0.to_string(), status)),
                _ => None,
            })
            .collect()
    }
}

/// Run a client against `runtime`'s agent and wait for both sides to finish.
async fn run_client<F>(runtime: &AcpRuntime, permission: Permission, log: &Arc<ClientLog>, body: F)
where
    F: AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), Error>,
{
    let (agent_side, client_side) = Channel::duplex();
    let agent = serve(
        Arc::clone(&runtime.agent_loop),
        Arc::clone(&runtime.registry),
        Arc::clone(&runtime.slot),
        agent_side,
    );

    let (updates_log, permission_log) = (Arc::clone(log), Arc::clone(log));
    let client = Client
        .builder()
        .name("test-client")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                updates_log
                    .updates
                    .lock()
                    .unwrap()
                    .push(notification.update);
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                permission_log
                    .permission_requests
                    .lock()
                    .unwrap()
                    .push(request.clone());
                let outcome = match permission {
                    Permission::Allow => {
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow"))
                    }
                    Permission::Reject => {
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("reject"))
                    }
                    Permission::Cancelled => RequestPermissionOutcome::Cancelled,
                };
                responder.respond(RequestPermissionResponse::new(outcome))
            },
            on_receive_request!(),
        )
        .connect_with(client_side, body);

    let (agent_result, client_result) = tokio::join!(agent, client);
    client_result.expect("client finished cleanly");
    agent_result.expect("agent served cleanly");
}

async fn initialize(connection: &ConnectionTo<Agent>) -> Result<(), Error> {
    connection
        .send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await?;
    Ok(())
}

async fn initialize_with_response(
    connection: &ConnectionTo<Agent>,
) -> Result<InitializeResponse, Error> {
    connection
        .send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await
}

async fn load_session(
    connection: &ConnectionTo<Agent>,
    session_id: &str,
    cwd: &Path,
) -> Result<LoadSessionResponse, Error> {
    connection
        .send_request(LoadSessionRequest::new(session_id.to_string(), cwd))
        .block_task()
        .await
}

async fn new_session(connection: &ConnectionTo<Agent>, cwd: &Path) -> Result<String, Error> {
    let response = connection
        .send_request(NewSessionRequest::new(cwd))
        .block_task()
        .await?;
    Ok(response.session_id.0.to_string())
}

async fn prompt(
    connection: &ConnectionTo<Agent>,
    session_id: &str,
    text: &str,
) -> Result<StopReason, Error> {
    let response = connection
        .send_request(PromptRequest::new(
            session_id.to_string(),
            vec![ContentBlock::Text(TextContent::new(text))],
        ))
        .block_task()
        .await?;
    Ok(response.stop_reason)
}

// ── fixtures ────────────────────────────────────────────────────────────────

struct Fixture {
    runtime: AcpRuntime,
    provider: Arc<ScriptedProvider>,
    project: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Fixture {
    /// A fresh runtime on the same workspace, like a restarted `rust-bot acp`
    /// process: new agent loop, new registry, new connection slot.
    fn restarted_runtime(&self, script: Vec<LLMResponse>) -> (AcpRuntime, Arc<ScriptedProvider>) {
        let mut config = Config::default();
        config.agents.workspace = self.workspace.path().to_string_lossy().into_owned();
        let provider = ScriptedProvider::new(script);
        let for_agent: Arc<dyn LLMProviderDyn> = Arc::new(SharedProvider(Arc::clone(&provider)));
        let runtime = assemble_acp_runtime(&config, self.workspace.path().to_path_buf(), for_agent);
        (runtime, provider)
    }

    /// Stored message lines (everything after the metadata line) of a session.
    fn stored_messages(&self, session_id: &str) -> Vec<String> {
        let path = self
            .workspace
            .path()
            .join("sessions")
            .join(format!("acp_{session_id}.jsonl"));
        std::fs::read_to_string(path)
            .expect("the session file exists")
            .lines()
            .skip(1)
            .map(str::to_string)
            .collect()
    }
}

fn fixture(script: Vec<LLMResponse>) -> Fixture {
    fixture_with(script, |_| {})
}

/// [`fixture`] with a chance to adjust the config before the agent is built.
fn fixture_with(script: Vec<LLMResponse>, adjust: impl FnOnce(&mut Config)) -> Fixture {
    let workspace = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    sync_workspace_templates(workspace.path(), false);

    let mut config = Config::default();
    config.agents.workspace = workspace.path().to_string_lossy().into_owned();
    adjust(&mut config);

    let provider = ScriptedProvider::new(script);
    let provider_for_agent: Arc<dyn LLMProviderDyn> =
        Arc::new(SharedProvider(Arc::clone(&provider)));
    let runtime = assemble_acp_runtime(&config, workspace.path().to_path_buf(), provider_for_agent);
    Fixture {
        runtime,
        provider,
        project,
        workspace,
    }
}

fn write_project_file(project: &Path, name: &str, contents: &str) -> PathBuf {
    let path = project.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn conversation_streams_text_and_tool_events_and_keeps_context() {
    let fixture = fixture(vec![
        tool_call_reply("call-1", "read_file", json!({"path": "notes.txt"})),
        text_reply("The file says: hello from the project"),
        text_reply("Earlier you asked about notes.txt"),
    ]);
    write_project_file(
        fixture.project.path(),
        "notes.txt",
        "hello from the project",
    );
    let log = Arc::new(ClientLog::default());

    run_client(
        &fixture.runtime,
        Permission::Reject,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let session = new_session(&connection, fixture.project.path()).await?;

            let first = prompt(&connection, &session, "what is in notes.txt?").await?;
            assert_eq!(first, StopReason::EndTurn);
            let second = prompt(&connection, &session, "what did I ask before?").await?;
            assert_eq!(second, StopReason::EndTurn);
            Ok(())
        },
    )
    .await;

    // Text is streamed back.
    let text = log.agent_text();
    assert!(
        text.contains("The file says: hello from the project"),
        "{text}"
    );
    assert!(text.contains("Earlier you asked about notes.txt"), "{text}");

    // The tool call is announced with id, kind, raw input and location ...
    let announced = log.updates().into_iter().find_map(|update| match update {
        SessionUpdate::ToolCall(call) => Some(call),
        _ => None,
    });
    let announced = announced.expect("a tool_call was sent");
    assert_eq!(announced.tool_call_id.0.as_ref(), "call-1");
    assert_eq!(announced.kind, ToolKind::Read);
    assert_eq!(announced.raw_input, Some(json!({"path": "notes.txt"})));
    assert_eq!(announced.locations.len(), 1);
    assert!(announced.locations[0].path.ends_with("notes.txt"));
    assert!(announced.locations[0].path.is_absolute());

    // ... then moves to in-progress and completed, with the file's text.
    assert_eq!(
        log.tool_statuses(),
        vec![
            ("call-1".to_string(), ToolCallStatus::InProgress),
            ("call-1".to_string(), ToolCallStatus::Completed),
        ]
    );
    let completion_text = log
        .updates()
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::ToolCallUpdate(update) => update.fields.content.clone(),
            _ => None,
        })
        .flatten()
        .map(|content| format!("{content:?}"))
        .collect::<String>();
    assert!(
        completion_text.contains("hello from the project"),
        "{completion_text}"
    );

    // read_file never asks for permission.
    assert_eq!(log.permission_request_count(), 0);

    // The second prompt saw the first turn (context is kept per session).
    let seen = fixture.provider.everything_seen();
    assert!(seen.contains("what is in notes.txt?"));
    assert!(seen.contains("what did I ask before?"));
    assert!(
        seen.matches("The file says: hello from the project")
            .count()
            >= 1,
        "the first answer must be part of the second turn's context"
    );
}

#[tokio::test]
async fn shell_asks_for_permission_and_runs_when_approved() {
    let fixture = fixture(vec![
        tool_call_reply("sh-1", "shell", json!({"command": "echo approved-output"})),
        text_reply("done"),
    ]);
    let log = Arc::new(ClientLog::default());

    run_client(
        &fixture.runtime,
        Permission::Allow,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let session = new_session(&connection, fixture.project.path()).await?;
            assert_eq!(
                prompt(&connection, &session, "run it").await?,
                StopReason::EndTurn
            );
            Ok(())
        },
    )
    .await;

    assert_eq!(log.permission_request_count(), 1);
    let request = log.permission_requests.lock().unwrap()[0].clone();
    assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "sh-1");
    assert_eq!(request.tool_call.fields.kind, Some(ToolKind::Execute));
    assert_eq!(
        log.tool_statuses().last(),
        Some(&("sh-1".to_string(), ToolCallStatus::Completed))
    );
    assert!(
        fixture
            .provider
            .everything_seen()
            .contains("approved-output")
    );
}

#[tokio::test]
async fn rejected_shell_is_denied_and_the_model_is_told() {
    let fixture = fixture(vec![
        tool_call_reply("sh-1", "shell", json!({"command": "echo should-not-run"})),
        text_reply("understood, I will not run it"),
    ]);
    let log = Arc::new(ClientLog::default());

    run_client(
        &fixture.runtime,
        Permission::Reject,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let session = new_session(&connection, fixture.project.path()).await?;
            assert_eq!(
                prompt(&connection, &session, "run it").await?,
                StopReason::EndTurn
            );
            Ok(())
        },
    )
    .await;

    assert_eq!(log.permission_request_count(), 1);
    assert_eq!(
        log.tool_statuses().last(),
        Some(&("sh-1".to_string(), ToolCallStatus::Failed))
    );
    // The command never ran, and the model received the reason, with the transcript intact.
    let seen = fixture.provider.everything_seen();
    assert!(seen.contains("denied by the ACP client"), "{seen}");
    assert!(log.agent_text().contains("understood, I will not run it"));
}

#[tokio::test]
async fn a_cancelled_permission_request_also_denies() {
    let fixture = fixture(vec![
        tool_call_reply("sh-1", "shell", json!({"command": "echo nope"})),
        text_reply("ok"),
    ]);
    let log = Arc::new(ClientLog::default());

    run_client(
        &fixture.runtime,
        Permission::Cancelled,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let session = new_session(&connection, fixture.project.path()).await?;
            prompt(&connection, &session, "run it").await?;
            Ok(())
        },
    )
    .await;

    assert_eq!(
        log.tool_statuses().last(),
        Some(&("sh-1".to_string(), ToolCallStatus::Failed))
    );
    assert!(
        fixture
            .provider
            .everything_seen()
            .contains("denied by the ACP client")
    );
}

#[tokio::test]
async fn file_tools_are_confined_to_the_session_project_folder() {
    let outside = tempfile::tempdir().unwrap();
    let secret = write_project_file(outside.path(), "secret.txt", "TOP-SECRET-CONTENT");
    let fixture = fixture(vec![
        tool_call_reply("r-1", "read_file", json!({"path": secret})),
        text_reply("could not read it"),
    ]);
    write_project_file(fixture.project.path(), "inside.txt", "inside content");
    let log = Arc::new(ClientLog::default());

    run_client(
        &fixture.runtime,
        Permission::Allow,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let session = new_session(&connection, fixture.project.path()).await?;
            prompt(&connection, &session, "read the secret").await?;
            Ok(())
        },
    )
    .await;

    let seen = fixture.provider.everything_seen();
    assert!(
        !seen.contains("TOP-SECRET-CONTENT"),
        "a file outside the project folder must not be readable"
    );
    assert_eq!(
        log.tool_statuses().last(),
        Some(&("r-1".to_string(), ToolCallStatus::Failed))
    );
}

#[tokio::test]
async fn session_new_rejects_relative_and_missing_project_folders() {
    let fixture = fixture(vec![]);
    let log = Arc::new(ClientLog::default());

    run_client(
        &fixture.runtime,
        Permission::Allow,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let relative = new_session(&connection, Path::new("relative/dir")).await;
            assert!(relative.is_err(), "a relative cwd must be rejected");
            let missing = new_session(&connection, &fixture.project.path().join("nope")).await;
            assert!(missing.is_err(), "a missing cwd must be rejected");
            // A valid one still works on the same connection.
            assert!(
                new_session(&connection, fixture.project.path())
                    .await
                    .is_ok()
            );
            Ok(())
        },
    )
    .await;
}

#[tokio::test]
async fn prompting_an_unknown_session_is_an_error() {
    let fixture = fixture(vec![]);
    let log = Arc::new(ClientLog::default());

    run_client(
        &fixture.runtime,
        Permission::Allow,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let result = prompt(&connection, "no-such-session", "hello").await;
            assert!(result.is_err());
            Ok(())
        },
    )
    .await;
}

#[tokio::test]
async fn sessions_keep_separate_context() {
    let fixture = fixture(vec![
        text_reply("first-answer"),
        text_reply("second-answer"),
    ]);
    let other_project = tempfile::tempdir().unwrap();
    let log = Arc::new(ClientLog::default());

    run_client(
        &fixture.runtime,
        Permission::Allow,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let session_a = new_session(&connection, fixture.project.path()).await?;
            let session_b = new_session(&connection, other_project.path()).await?;
            assert_ne!(session_a, session_b);
            prompt(&connection, &session_a, "question-for-A").await?;
            prompt(&connection, &session_b, "question-for-B").await?;
            Ok(())
        },
    )
    .await;

    let calls = fixture.provider.seen_messages.lock().unwrap().clone();
    let last_call = serde_json::to_string(calls.last().unwrap()).unwrap();
    assert!(last_call.contains("question-for-B"));
    assert!(
        !last_call.contains("question-for-A"),
        "session B must not see session A's conversation"
    );
}

#[tokio::test]
async fn headless_tool_list_has_no_spawn_question_or_message() {
    let fixture = fixture(vec![]);
    let names = fixture
        .runtime
        .agent_loop
        .tools_for_session(None)
        .tool_names();
    for removed in ["spawn", "question", "message"] {
        assert!(
            !names.iter().any(|name| name == removed),
            "{removed} must be absent: {names:?}"
        );
    }
    for kept in ["read_file", "shell"] {
        assert!(
            names.iter().any(|name| name == kept),
            "{kept} must stay: {names:?}"
        );
    }
}

#[tokio::test]
async fn disabled_tools_are_removed_from_the_acp_agent() {
    let fixture = fixture_with(vec![], |config| {
        config.tools.disabled_tools = vec!["write_file".to_string(), "edit_file".to_string()];
    });
    let names = fixture
        .runtime
        .agent_loop
        .tools_for_session(None)
        .tool_names();
    for gone in ["write_file", "edit_file"] {
        assert!(
            !names.iter().any(|name| name == gone),
            "{gone} must be absent: {names:?}"
        );
    }
    for kept in ["read_file", "shell", "grep"] {
        assert!(
            names.iter().any(|name| name == kept),
            "{kept} must stay: {names:?}"
        );
    }
}

#[tokio::test]
async fn cli_mode_tool_list_still_has_spawn() {
    // Same assembly minus the ACP-specific removal: what `rust-bot agent` builds.
    let workspace = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.agents.workspace = workspace.path().to_string_lossy().into_owned();
    let provider: Arc<dyn LLMProviderDyn> = Arc::new(SharedProvider(ScriptedProvider::new(vec![])));
    let agent_loop = rust_bot::agent::agent_loop::AgentLoop::new(
        Arc::new(rust_bot::bus::queue::MessageBus::new()),
        provider,
        workspace.path().to_path_buf(),
        config,
        None,
        None,
        None,
    );
    let names = agent_loop.tools_for_session(None).tool_names();
    assert!(names.iter().any(|name| name == "spawn"), "{names:?}");
}

// ── cancellation ────────────────────────────────────────────────────────────

/// Number of running processes whose command line contains `needle`.
fn running_process_count(needle: &str) -> usize {
    if cfg!(windows) {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {needle}"), "/NH"])
            .output()
            .expect("run tasklist");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| {
                line.to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase())
            })
            .count()
    } else {
        let output = std::process::Command::new("pgrep")
            .args(["-c", "-f", needle])
            .output()
            .expect("run pgrep");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }
}

/// A shell command that runs for a long time and is easy to recognise.
fn long_running_command() -> &'static str {
    if cfg!(windows) {
        "ping -n 41 127.0.0.1 > nul"
    } else {
        "sleep 41"
    }
}

/// Name to look for in the process list while the long command runs.
fn long_running_process_name() -> &'static str {
    if cfg!(windows) {
        "PING.EXE"
    } else {
        "sleep 41"
    }
}

async fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test]
async fn cancel_stops_the_turn_kills_the_shell_tree_and_the_session_still_works() {
    let fixture = fixture(vec![
        tool_call_reply(
            "long-1",
            "shell",
            json!({"command": long_running_command()}),
        ),
        // Never reached for the cancelled turn; consumed by the next prompt.
        text_reply("still alive after the cancel"),
    ]);
    let log = Arc::new(ClientLog::default());
    let baseline = running_process_count(long_running_process_name());

    run_client(
        &fixture.runtime,
        Permission::Allow,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let session = new_session(&connection, fixture.project.path()).await?;

            // Start the long turn without waiting for it.
            let turn = {
                let connection = connection.clone();
                let session = session.clone();
                tokio::spawn(
                    async move { prompt(&connection, &session, "run the long command").await },
                )
            };

            // Wait until the shell process is really running, then cancel.
            wait_until(
                || running_process_count(long_running_process_name()) > baseline,
                "the long shell command to start",
            )
            .await;
            connection.send_notification(CancelNotification::new(session.clone()))?;

            let stop_reason = turn.await.expect("turn task joined")?;
            assert_eq!(stop_reason, StopReason::Cancelled);

            // The whole process tree is gone, not just the top shell.
            wait_until(
                || running_process_count(long_running_process_name()) <= baseline,
                "the shell process tree to be killed",
            )
            .await;

            // The session is usable again.
            let next = prompt(&connection, &session, "are you there?").await?;
            assert_eq!(next, StopReason::EndTurn);
            Ok(())
        },
    )
    .await;

    assert!(log.agent_text().contains("still alive after the cancel"));
}

// ── session/load ────────────────────────────────────────────────────────────

/// Create a session that has had one conversation, then disconnect.
/// Returns the session id.
async fn conversation_to_resume(fixture: &Fixture) -> String {
    let created = Arc::new(Mutex::new(String::new()));
    let log = Arc::new(ClientLog::default());
    write_project_file(
        fixture.project.path(),
        "notes.txt",
        "hello from the project",
    );
    let slot = Arc::clone(&created);
    run_client(
        &fixture.runtime,
        Permission::Allow,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let session = new_session(&connection, fixture.project.path()).await?;
            prompt(&connection, &session, "what is in notes.txt?").await?;
            *slot.lock().unwrap() = session;
            Ok(())
        },
    )
    .await;
    created.lock().unwrap().clone()
}

fn first_conversation_script() -> Vec<LLMResponse> {
    vec![
        tool_call_reply("call-1", "read_file", json!({"path": "notes.txt"})),
        text_reply("The file says: hello from the project"),
    ]
}

#[tokio::test]
async fn initialize_advertises_load_session() {
    let fixture = fixture(vec![]);
    let log = Arc::new(ClientLog::default());
    let advertised = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&advertised);

    run_client(
        &fixture.runtime,
        Permission::Allow,
        &log,
        async |connection| {
            let response = initialize_with_response(&connection).await?;
            *flag.lock().unwrap() = response.agent_capabilities.load_session;
            Ok(())
        },
    )
    .await;

    assert!(
        *advertised.lock().unwrap(),
        "loadSession must be advertised"
    );
}

#[tokio::test]
async fn a_stored_session_is_replayed_and_the_conversation_continues() {
    let fixture = fixture(first_conversation_script());
    let session_id = conversation_to_resume(&fixture).await;
    let stored_before = fixture.stored_messages(&session_id);
    assert!(!stored_before.is_empty());

    // A restarted process: nothing in memory, only what is on disk.
    let (runtime, provider) =
        fixture.restarted_runtime(vec![text_reply("Earlier you asked what is in notes.txt")]);
    let log = Arc::new(ClientLog::default());
    let replayed_before_follow_up = Arc::new(Mutex::new(Vec::new()));
    let snapshot = Arc::clone(&replayed_before_follow_up);
    let stored_after_load = Arc::new(Mutex::new(Vec::new()));
    let after_load = Arc::clone(&stored_after_load);

    run_client(&runtime, Permission::Allow, &log, async |connection| {
        initialize(&connection).await?;
        load_session(&connection, &session_id, fixture.project.path()).await?;
        // Everything replayed so far, captured before the follow-up runs.
        *snapshot.lock().unwrap() = log.updates();
        *after_load.lock().unwrap() = fixture.stored_messages(&session_id);
        let stop = prompt(&connection, &session_id, "what did I ask before?").await?;
        assert_eq!(stop, StopReason::EndTurn);
        Ok(())
    })
    .await;

    // The replay shows the first conversation, in order.
    let replayed = replayed_before_follow_up.lock().unwrap().clone();
    let summary: Vec<&str> = replayed
        .iter()
        .map(|update| match update {
            SessionUpdate::UserMessageChunk(_) => "user",
            SessionUpdate::AgentMessageChunk(_) => "agent",
            SessionUpdate::ToolCall(_) => "tool_call",
            SessionUpdate::ToolCallUpdate(_) => "tool_update",
            _ => "other",
        })
        .collect();
    assert_eq!(summary, vec!["user", "tool_call", "tool_update", "agent"]);
    match &replayed[1] {
        SessionUpdate::ToolCall(call) => {
            assert_eq!(call.tool_call_id.0.as_ref(), "call-1");
            assert_eq!(call.kind, ToolKind::Read);
        }
        other => panic!("expected the tool call, got {other:?}"),
    }

    // Replaying changed nothing in the stored history.
    assert_eq!(*stored_after_load.lock().unwrap(), stored_before);

    // The follow-up sees the earlier conversation.
    let seen = provider.everything_seen();
    assert!(seen.contains("what is in notes.txt?"), "{seen}");
    assert!(
        seen.contains("The file says: hello from the project"),
        "{seen}"
    );
    assert!(seen.contains("what did I ask before?"), "{seen}");
}

#[tokio::test]
async fn loading_reapplies_the_project_folder_of_the_new_request() {
    let fixture = fixture(first_conversation_script());
    let session_id = conversation_to_resume(&fixture).await;

    // Reopen the same conversation with a different project folder. A file of the
    // original project must no longer be readable.
    let other_project = tempfile::tempdir().unwrap();
    let old_file = fixture.project.path().join("notes.txt");
    let (runtime, _provider) = fixture.restarted_runtime(vec![
        tool_call_reply("r-1", "read_file", json!({"path": old_file})),
        text_reply("done"),
    ]);
    let log = Arc::new(ClientLog::default());

    run_client(&runtime, Permission::Allow, &log, async |connection| {
        initialize(&connection).await?;
        load_session(&connection, &session_id, other_project.path()).await?;
        prompt(&connection, &session_id, "read the old file").await?;
        Ok(())
    })
    .await;

    assert_eq!(
        log.tool_statuses().last(),
        Some(&("r-1".to_string(), ToolCallStatus::Failed)),
        "the old project folder must be out of scope"
    );
}

#[tokio::test]
async fn loading_an_unknown_session_is_an_error() {
    let fixture = fixture(vec![]);
    let log = Arc::new(ClientLog::default());

    run_client(
        &fixture.runtime,
        Permission::Allow,
        &log,
        async |connection| {
            initialize(&connection).await?;
            let result = load_session(&connection, "no-such-session", fixture.project.path()).await;
            assert!(result.is_err());
            // Nothing was replayed and the connection is still usable.
            assert!(log.updates().is_empty());
            assert!(
                new_session(&connection, fixture.project.path())
                    .await
                    .is_ok()
            );
            Ok(())
        },
    )
    .await;
}

#[tokio::test]
async fn loading_rejects_a_relative_or_missing_project_folder() {
    let fixture = fixture(first_conversation_script());
    let session_id = conversation_to_resume(&fixture).await;
    let (runtime, _provider) = fixture.restarted_runtime(vec![]);
    let log = Arc::new(ClientLog::default());

    run_client(&runtime, Permission::Allow, &log, async |connection| {
        initialize(&connection).await?;
        assert!(
            load_session(&connection, &session_id, Path::new("relative/dir"))
                .await
                .is_err()
        );
        assert!(
            load_session(
                &connection,
                &session_id,
                &fixture.project.path().join("gone")
            )
            .await
            .is_err()
        );
        assert!(
            load_session(&connection, &session_id, fixture.project.path())
                .await
                .is_ok(),
            "a valid folder still works after the rejected attempts"
        );
        Ok(())
    })
    .await;
}
