//! The ACP **client** role: a parent rust-bot driving one turn of a child agent.
//!
//! [`run_turn`] does everything between "a connection to an agent exists" and
//! "the agent's reply is in hand":
//!
//! 1. `initialize`;
//! 2. `session/load` of the conversation this parent session had with the child
//!    before, falling back to `session/new` when there is none or the child no
//!    longer knows it. The updates a child replays during `session/load` are
//!    discarded, so old history never ends up in the new reply;
//! 3. `session/prompt`, aggregating `session/update` into the final reply and a
//!    list of what the child did, and reporting each tool call as progress;
//! 4. on timeout, `session/cancel`, and a short wait for the child to confirm.
//!
//! Permission requests from the child are answered by a [`PermissionResponder`]
//! (the parent's `permissionPolicy`). The transport is generic: a real child
//! process, or an in-process channel in tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, LoadSessionRequest, NewSessionRequest,
    PromptRequest, RequestPermissionRequest, RequestPermissionResponse, SessionNotification,
    SessionUpdate, StopReason, TextContent, ToolCallStatus,
};
use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, Error, on_receive_notification, on_receive_request,
};

use crate::agent::acp::permission::{PermissionResponder, ask_from_request, outcome_for};

/// How the parent hears about the child's progress while it waits.
pub trait ProgressSink: Send + Sync {
    /// The child started a tool call; `title` is its one-line description.
    fn tool_started(&self, title: &str);
    /// The child has been working for `elapsed` without a new tool call.
    fn still_working(&self, elapsed: Duration);
}

/// Everything one turn needs.
pub struct TurnRequest {
    /// The folder the child works on (its session's project scope).
    pub cwd: PathBuf,
    pub prompt: String,
    /// The child session this parent session used before, if any.
    pub previous_session: Option<String>,
    /// How long `initialize` and session setup may take (a child may wait for its
    /// workspace lock first).
    pub startup_timeout: Duration,
    /// How long the prompt may take before the turn is cancelled.
    pub turn_timeout: Duration,
    /// How long a cancelled child gets to confirm.
    pub cancel_grace: Duration,
    /// How often `still_working` fires while no tool call starts.
    pub heartbeat_every: Duration,
    pub responder: Arc<dyn PermissionResponder>,
    pub progress: Option<Arc<dyn ProgressSink>>,
}

/// How the child's turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd {
    Finished,
    /// The child stopped because the turn was cancelled by the parent.
    Cancelled,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    /// The turn exceeded its timeout. `cancel_confirmed` says whether the child
    /// answered the cancel in time; when it did not, the caller should kill it.
    TimedOut {
        cancel_confirmed: bool,
    },
}

/// Progress of one of the child's tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Completed,
    Failed,
}

/// One tool call the child made, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub title: String,
    pub status: ActivityStatus,
}

/// What a turn produced.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// The child's ACP session id, to be remembered for the next turn.
    pub session_id: String,
    /// The earlier session was loaded (its history was replayed and discarded).
    pub resumed: bool,
    /// An earlier session was given but the child could not load it, so a new one
    /// was created; the caller should replace its stored id.
    pub session_replaced: bool,
    /// The child's reply text.
    pub reply: String,
    pub activity: Vec<ActivityEntry>,
    pub end: TurnEnd,
}

/// Why a turn could not be run at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// `initialize` / session setup failed or took too long.
    Startup(String),
    /// The connection broke or the child answered with an error.
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Startup(message) => write!(f, "the agent did not start: {message}"),
            ClientError::Protocol(message) => write!(f, "the agent connection failed: {message}"),
        }
    }
}

impl std::error::Error for ClientError {}

// ── aggregating updates ─────────────────────────────────────────────────────

/// Collects the child's `session/update` stream. Pure state, no I/O.
#[derive(Debug, Default)]
struct Aggregator {
    /// Updates are replayed history right now: ignore them.
    replaying: bool,
    reply: String,
    order: Vec<String>,
    tools: HashMap<String, ActivityEntry>,
}

impl Aggregator {
    /// Fold one update in. Returns the title of a tool call seen for the first time.
    fn apply(&mut self, update: &SessionUpdate) -> Option<String> {
        if self.replaying {
            return None;
        }
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = &chunk.content {
                    self.reply.push_str(&text.text);
                }
                None
            }
            SessionUpdate::ToolCall(call) => {
                let id = call.tool_call_id.0.to_string();
                let status = activity_status(call.status).unwrap_or(ActivityStatus::Running);
                match self.tools.get_mut(&id) {
                    Some(known) => {
                        known.title = call.title.clone();
                        known.status = status;
                        None
                    }
                    None => {
                        self.order.push(id.clone());
                        self.tools.insert(
                            id,
                            ActivityEntry {
                                title: call.title.clone(),
                                status,
                            },
                        );
                        Some(call.title.clone())
                    }
                }
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.0.to_string();
                let is_new = !self.tools.contains_key(&id);
                if is_new {
                    self.order.push(id.clone());
                }
                let entry = self
                    .tools
                    .entry(id.clone())
                    .or_insert_with(|| ActivityEntry {
                        title: update.fields.title.clone().unwrap_or_else(|| id.clone()),
                        status: ActivityStatus::Running,
                    });
                if let Some(title) = &update.fields.title {
                    entry.title = title.clone();
                }
                if let Some(status) = update.fields.status.and_then(activity_status) {
                    entry.status = status;
                }
                is_new.then(|| entry.title.clone())
            }
            _ => None,
        }
    }

    fn activity(&self) -> Vec<ActivityEntry> {
        self.order
            .iter()
            .filter_map(|id| self.tools.get(id).cloned())
            .collect()
    }
}

/// `None` for statuses that do not move an entry (pending is "not started yet").
fn activity_status(status: ToolCallStatus) -> Option<ActivityStatus> {
    match status {
        ToolCallStatus::Completed => Some(ActivityStatus::Completed),
        ToolCallStatus::Failed => Some(ActivityStatus::Failed),
        ToolCallStatus::InProgress => Some(ActivityStatus::Running),
        _ => None,
    }
}

type SharedAggregator = Arc<Mutex<Aggregator>>;

fn lock(aggregator: &SharedAggregator) -> std::sync::MutexGuard<'_, Aggregator> {
    aggregator.lock().unwrap_or_else(|e| e.into_inner())
}

// ── the turn ────────────────────────────────────────────────────────────────

/// Run one turn against the agent on `transport`.
pub async fn run_turn(
    transport: impl ConnectTo<Client> + 'static,
    request: TurnRequest,
) -> Result<TurnOutcome, ClientError> {
    let aggregator: SharedAggregator = Arc::new(Mutex::new(Aggregator::default()));
    let progress = request.progress.clone();
    let responder = Arc::clone(&request.responder);
    let notification_state = Arc::clone(&aggregator);
    let notification_progress = progress.clone();

    let result = Client
        .builder()
        .name("rust-bot-parent")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                // Synchronous on purpose: an update is applied before the next
                // message (a `session/load` response, say) is looked at.
                let started = lock(&notification_state).apply(&notification.update);
                if let (Some(title), Some(progress)) = (started, &notification_progress) {
                    progress.tool_started(&title);
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |permission: RequestPermissionRequest, responder_handle, _connection| {
                let ask = ask_from_request(&permission);
                let allow = responder.decide(&ask).await;
                responder_handle.respond(RequestPermissionResponse::new(outcome_for(
                    &permission,
                    allow,
                )))
            },
            on_receive_request!(),
        )
        .connect_with(transport, async |connection| {
            drive_turn(&connection, &request, &aggregator).await
        })
        .await;

    match result {
        Ok(outcome) => outcome,
        Err(error) => Err(ClientError::Protocol(describe(&error))),
    }
}

/// The body of the connection: setup, prompt, wait.
async fn drive_turn(
    connection: &ConnectionTo<Agent>,
    request: &TurnRequest,
    aggregator: &SharedAggregator,
) -> Result<Result<TurnOutcome, ClientError>, Error> {
    let setup = tokio::time::timeout(request.startup_timeout, async {
        let initialized = connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await
            .map_err(|error| ClientError::Startup(describe(&error)))?;
        open_session(
            connection,
            request,
            aggregator,
            initialized.agent_capabilities.load_session,
        )
        .await
    })
    .await;
    let session = match setup {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => return Ok(Err(error)),
        Err(_elapsed) => {
            return Ok(Err(ClientError::Startup(format!(
                "no answer within {}s",
                request.startup_timeout.as_secs()
            ))));
        }
    };

    let end = prompt_and_wait(connection, request, &session.id).await?;
    let (reply, activity) = {
        let state = lock(aggregator);
        (state.reply.clone(), state.activity())
    };
    Ok(Ok(TurnOutcome {
        session_id: session.id,
        resumed: session.resumed,
        session_replaced: session.replaced,
        reply,
        activity,
        end,
    }))
}

/// The session a turn runs in.
struct OpenedSession {
    id: String,
    resumed: bool,
    replaced: bool,
}

/// `session/load` the earlier session when there is one and the child supports
/// it; otherwise `session/new`.
async fn open_session(
    connection: &ConnectionTo<Agent>,
    request: &TurnRequest,
    aggregator: &SharedAggregator,
    supports_load: bool,
) -> Result<OpenedSession, ClientError> {
    let mut replaced = false;
    if let (Some(previous), true) = (&request.previous_session, supports_load) {
        lock(aggregator).replaying = true;
        let loaded = connection
            .send_request(LoadSessionRequest::new(previous.clone(), &request.cwd))
            .block_task()
            .await;
        // Whatever arrived before the answer was history; drop it for good.
        {
            let mut state = lock(aggregator);
            state.replaying = false;
            *state = Aggregator::default();
        }
        match loaded {
            Ok(_) => {
                return Ok(OpenedSession {
                    id: previous.clone(),
                    resumed: true,
                    replaced: false,
                });
            }
            Err(error) => {
                log::warn!(
                    "ACP child cannot load session {previous}: {}; starting a new one",
                    describe(&error)
                );
                replaced = true;
            }
        }
    } else if request.previous_session.is_some() {
        replaced = true;
    }

    let created = connection
        .send_request(NewSessionRequest::new(&request.cwd))
        .block_task()
        .await
        .map_err(|error| ClientError::Startup(describe(&error)))?;
    Ok(OpenedSession {
        id: created.session_id.0.to_string(),
        resumed: false,
        replaced,
    })
}

/// Send the prompt and wait for its answer, with the timeout, the heartbeat and
/// the cancel handshake.
async fn prompt_and_wait(
    connection: &ConnectionTo<Agent>,
    request: &TurnRequest,
    session_id: &str,
) -> Result<TurnEnd, Error> {
    let pending = connection.send_request(PromptRequest::new(
        session_id.to_string(),
        vec![ContentBlock::Text(TextContent::new(request.prompt.clone()))],
    ));
    let mut answer = pin!(pending.block_task());
    let deadline = tokio::time::sleep(request.turn_timeout);
    let mut deadline = pin!(deadline);
    let started = tokio::time::Instant::now();
    let mut heartbeat =
        tokio::time::interval_at(started + request.heartbeat_every, request.heartbeat_every);

    loop {
        tokio::select! {
            response = &mut answer => return Ok(end_of(response?.stop_reason)),
            _ = &mut deadline => break,
            _ = heartbeat.tick() => {
                if let Some(progress) = &request.progress {
                    progress.still_working(started.elapsed());
                }
            }
        }
    }

    // Timed out: ask the child to stop, and give it a moment to confirm.
    let _ = connection.send_notification(CancelNotification::new(session_id.to_string()));
    let confirmed = matches!(
        tokio::time::timeout(request.cancel_grace, &mut answer).await,
        Ok(Ok(_))
    );
    Ok(TurnEnd::TimedOut {
        cancel_confirmed: confirmed,
    })
}

fn end_of(stop_reason: StopReason) -> TurnEnd {
    match stop_reason {
        StopReason::EndTurn => TurnEnd::Finished,
        StopReason::Cancelled => TurnEnd::Cancelled,
        StopReason::MaxTokens => TurnEnd::MaxTokens,
        StopReason::MaxTurnRequests => TurnEnd::MaxTurnRequests,
        StopReason::Refusal => TurnEnd::Refusal,
        // A reason a newer protocol adds: the agent did stop, so it finished.
        _ => TurnEnd::Finished,
    }
}

/// An error as the operator should read it.
fn describe(error: &Error) -> String {
    let text = error.to_string();
    match &error.data {
        Some(data) => {
            let detail = data
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| data.to_string());
            if text.contains(&detail) {
                text
            } else {
                format!("{text}: {detail}")
            }
        }
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentChunk, ToolCall, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn message(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        ))))
    }

    fn call(id: &str, title: &str) -> SessionUpdate {
        SessionUpdate::ToolCall(ToolCall::new(id.to_string(), title))
    }

    fn update(id: &str, status: ToolCallStatus) -> SessionUpdate {
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id.to_string(),
            ToolCallUpdateFields::new().status(status),
        ))
    }

    #[test]
    fn message_chunks_are_joined_into_the_reply() {
        let mut aggregator = Aggregator::default();
        aggregator.apply(&message("Hello, "));
        aggregator.apply(&message("world."));
        assert_eq!(aggregator.reply, "Hello, world.");
    }

    #[test]
    fn thoughts_and_user_chunks_are_not_part_of_the_reply() {
        let mut aggregator = Aggregator::default();
        let thought = SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(
            TextContent::new("thinking"),
        )));
        let user = SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
            TextContent::new("question"),
        )));
        aggregator.apply(&thought);
        aggregator.apply(&user);
        aggregator.apply(&message("answer"));
        assert_eq!(aggregator.reply, "answer");
    }

    #[test]
    fn a_tool_call_is_reported_once_and_completes_in_place() {
        let mut aggregator = Aggregator::default();
        assert_eq!(
            aggregator.apply(&call("t1", "read src/main.rs")).as_deref(),
            Some("read src/main.rs")
        );
        // The same call announced again, or updated, is not new.
        assert_eq!(aggregator.apply(&call("t1", "read src/main.rs")), None);
        assert_eq!(
            aggregator.apply(&update("t1", ToolCallStatus::InProgress)),
            None
        );
        assert_eq!(
            aggregator.apply(&update("t1", ToolCallStatus::Completed)),
            None
        );

        assert_eq!(
            aggregator.activity(),
            vec![ActivityEntry {
                title: "read src/main.rs".to_string(),
                status: ActivityStatus::Completed
            }]
        );
    }

    #[test]
    fn activity_keeps_the_order_of_the_calls_and_marks_failures() {
        let mut aggregator = Aggregator::default();
        aggregator.apply(&call("b", "second-announced-first"));
        aggregator.apply(&call("a", "announced-second"));
        aggregator.apply(&update("a", ToolCallStatus::Failed));
        aggregator.apply(&update("b", ToolCallStatus::Completed));

        let titles: Vec<_> = aggregator
            .activity()
            .iter()
            .map(|e| e.title.clone())
            .collect();
        assert_eq!(titles, vec!["second-announced-first", "announced-second"]);
        assert_eq!(aggregator.activity()[1].status, ActivityStatus::Failed);
    }

    #[test]
    fn an_update_for_an_unannounced_call_still_counts() {
        let mut aggregator = Aggregator::default();
        let started = aggregator.apply(&update("lonely", ToolCallStatus::Completed));
        assert_eq!(started.as_deref(), Some("lonely"));
        assert_eq!(aggregator.activity()[0].status, ActivityStatus::Completed);
    }

    #[test]
    fn a_pending_status_does_not_move_a_running_call() {
        let mut aggregator = Aggregator::default();
        aggregator.apply(&call("t", "shell ls"));
        aggregator.apply(&update("t", ToolCallStatus::Pending));
        assert_eq!(aggregator.activity()[0].status, ActivityStatus::Running);
    }

    #[test]
    fn nothing_is_collected_while_replaying() {
        let mut aggregator = Aggregator {
            replaying: true,
            ..Aggregator::default()
        };
        assert_eq!(aggregator.apply(&message("old history")), None);
        assert_eq!(aggregator.apply(&call("old", "read old.txt")), None);
        assert!(aggregator.reply.is_empty());
        assert!(aggregator.activity().is_empty());
    }

    #[test]
    fn stop_reasons_map_to_turn_ends() {
        assert_eq!(end_of(StopReason::EndTurn), TurnEnd::Finished);
        assert_eq!(end_of(StopReason::Cancelled), TurnEnd::Cancelled);
        assert_eq!(end_of(StopReason::MaxTokens), TurnEnd::MaxTokens);
        assert_eq!(
            end_of(StopReason::MaxTurnRequests),
            TurnEnd::MaxTurnRequests
        );
        assert_eq!(end_of(StopReason::Refusal), TurnEnd::Refusal);
    }
}
