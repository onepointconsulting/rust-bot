//! The ACP **agent** role: what `rust-bot acp` serves to an ACP client.
//!
//! One connection, many sessions. Each `session/new` creates a rust-bot
//! session (`acp:<sessionId>`) whose project folder is the request's `cwd`,
//! confined by the existing per-session workspace scope. Each `session/prompt`
//! runs one agent turn in its own task, so `session/cancel` can abort it while
//! the connection's dispatch loop stays free.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, Error, Responder, on_receive_notification,
    on_receive_request,
};
use futures::future::abortable;

use super::ACP_CHANNEL;
use super::link::{AcpLink, ConnectionSlot};
use super::registry::{SessionRegistry, TurnError};
use crate::agent::agent_loop::{AgentLoop, ProgressCallback, StreamCallback};
use crate::bus::outbound_events::ProgressKind;
use crate::security::workspace_access::WorkspaceAccessMode;

/// Name the agent reports in `initialize`.
const AGENT_NAME: &str = "rust-bot";

/// Session key under which an ACP session is stored by the session manager.
pub fn session_key(session_id: &str) -> String {
    format!("acp:{session_id}")
}

/// Answer to `initialize`: the client's protocol version if we speak it, else ours.
///
/// Capabilities are deliberately minimal for this milestone: no `session/load`,
/// text prompts only, no authentication.
pub fn initialize_response(request: &InitializeRequest) -> InitializeResponse {
    let protocol_version = request.protocol_version.min(ProtocolVersion::LATEST);
    InitializeResponse::new(protocol_version)
        .agent_capabilities(AgentCapabilities::new().load_session(false))
        .auth_methods(vec![])
        .agent_info(Implementation::new(AGENT_NAME, env!("CARGO_PKG_VERSION")))
}

/// Create the rust-bot session behind a new ACP session and return its id.
///
/// `cwd` becomes the session's project folder in `restricted` mode; it must be
/// an absolute, existing directory, otherwise the request is rejected.
pub fn create_session(
    agent_loop: &AgentLoop,
    registry: &SessionRegistry,
    cwd: &Path,
) -> Result<String, Error> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let scope = {
        let mut manager = agent_loop
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        agent_loop.set_session_workspace_scope(
            &mut manager,
            &session_key(&session_id),
            cwd,
            WorkspaceAccessMode::Restricted,
        )
    }
    .map_err(|error| Error::invalid_params().data(error.to_string()))?;
    registry.insert(session_id.clone(), scope.project_path);
    Ok(session_id)
}

/// Flatten prompt content blocks into the text the agent loop takes.
///
/// Only text and resource links are advertised, so only those are expected;
/// anything else is skipped with a log line.
pub fn prompt_text(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::ResourceLink(link) => parts.push(format!("{} ({})", link.name, link.uri)),
            _ => log::warn!("ACP: ignoring a prompt content block of an unsupported type"),
        }
    }
    parts.join("\n")
}

/// JSON-RPC error for a prompt that cannot start.
fn turn_error(error: TurnError) -> Error {
    match error {
        TurnError::UnknownSession => Error::invalid_params().data("unknown sessionId"),
        TurnError::Busy => {
            Error::invalid_request().data("the session already has a prompt in progress")
        }
    }
}

fn agent_text_update(text: String) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text,
    ))))
}

fn agent_thought_update(text: String) -> SessionUpdate {
    SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text,
    ))))
}

/// Stream callback: every text delta becomes an `agent_message_chunk`.
fn stream_callback(
    link: Arc<dyn AcpLink>,
    session_id: String,
    streamed_any: Arc<AtomicBool>,
) -> StreamCallback {
    Arc::new(
        move |delta: String| -> Pin<Box<dyn Future<Output = ()> + Send>> {
            streamed_any.store(true, Ordering::Relaxed);
            let link = Arc::clone(&link);
            let session_id = session_id.clone();
            Box::pin(async move {
                if let Err(error) = link
                    .send_update(&session_id, agent_text_update(delta))
                    .await
                {
                    log::warn!("ACP: could not stream text: {error}");
                }
            })
        },
    )
}

/// Progress callback: reasoning deltas become `agent_thought_chunk`; tool hints
/// are ignored because the hook reports structured tool calls instead.
fn progress_callback(link: Arc<dyn AcpLink>, session_id: String) -> ProgressCallback {
    Arc::new(
        move |text: String, kind: ProgressKind| -> Pin<Box<dyn Future<Output = ()> + Send>> {
            let link = Arc::clone(&link);
            let session_id = session_id.clone();
            Box::pin(async move {
                if kind == ProgressKind::ReasoningDelta && !text.is_empty() {
                    let _ = link
                        .send_update(&session_id, agent_thought_update(text))
                        .await;
                }
            })
        },
    )
}

/// Run one agent turn for an ACP session and report how it ended.
async fn run_turn(
    agent_loop: Arc<AgentLoop>,
    link: Arc<dyn AcpLink>,
    session_id: String,
    prompt: String,
) -> StopReason {
    let streamed_any = Arc::new(AtomicBool::new(false));
    let reply = agent_loop
        .process_direct(
            &prompt,
            Some(&session_key(&session_id)),
            Some(ACP_CHANNEL),
            Some(&session_id),
            None,
            Some(progress_callback(Arc::clone(&link), session_id.clone())),
            Some(stream_callback(
                Arc::clone(&link),
                session_id.clone(),
                Arc::clone(&streamed_any),
            )),
            None,
        )
        .await;

    // Providers that do not stream hand back the whole answer at the end.
    if !streamed_any.load(Ordering::Relaxed)
        && let Some(reply) = reply
        && !reply.content.is_empty()
    {
        let _ = link
            .send_update(&session_id, agent_text_update(reply.content))
            .await;
    }
    StopReason::EndTurn
}

/// Start a prompt turn in its own task and answer the request from there.
fn start_prompt(
    agent_loop: &Arc<AgentLoop>,
    registry: &Arc<SessionRegistry>,
    slot: &Arc<ConnectionSlot>,
    request: PromptRequest,
    responder: Responder<PromptResponse>,
    connection: &ConnectionTo<Client>,
) -> Result<(), Error> {
    let session_id = request.session_id.0.to_string();
    let prompt = prompt_text(&request.prompt);
    let link: Arc<dyn AcpLink> = slot.clone();

    let (turn, abort_handle) = abortable(run_turn(
        Arc::clone(agent_loop),
        link,
        session_id.clone(),
        prompt,
    ));
    if let Err(error) = registry.begin_turn(&session_id, abort_handle) {
        return responder.respond_with_error(turn_error(error));
    }

    let registry = Arc::clone(registry);
    connection.spawn(async move {
        let stop_reason = match turn.await {
            Ok(stop_reason) => stop_reason,
            Err(_aborted) => StopReason::Cancelled,
        };
        registry.end_turn(&session_id);
        responder.respond(PromptResponse::new(stop_reason))
    })
}

/// Serve the ACP agent role over `transport` until the client disconnects.
///
/// A clean disconnect (EOF on the client's stdin) is a normal end, not an error.
pub async fn serve(
    agent_loop: Arc<AgentLoop>,
    registry: Arc<SessionRegistry>,
    slot: Arc<ConnectionSlot>,
    transport: impl ConnectTo<Agent> + 'static,
) -> Result<(), Error> {
    let (initialize_slot, new_session_slot) = (Arc::clone(&slot), Arc::clone(&slot));
    let (new_session_loop, new_session_registry) = (Arc::clone(&agent_loop), Arc::clone(&registry));
    let (prompt_loop, prompt_registry, prompt_slot) =
        (Arc::clone(&agent_loop), Arc::clone(&registry), slot);
    let cancel_registry = Arc::clone(&registry);

    let result = Agent
        .builder()
        .name(AGENT_NAME)
        .on_receive_request(
            async move |request: InitializeRequest, responder, connection| {
                initialize_slot.bind(&connection);
                responder.respond(initialize_response(&request))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, connection| {
                new_session_slot.bind(&connection);
                match create_session(&new_session_loop, &new_session_registry, &request.cwd) {
                    Ok(session_id) => responder.respond(NewSessionResponse::new(session_id)),
                    Err(error) => responder.respond_with_error(error),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                start_prompt(
                    &prompt_loop,
                    &prompt_registry,
                    &prompt_slot,
                    request,
                    responder,
                    &connection,
                )
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _connection| {
                cancel_registry.cancel(&notification.session_id.0);
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(transport)
        .await;

    match result {
        Err(error) if !agent_client_protocol::is_incoming_transport_closed(&error) => Err(error),
        _ => Ok(()),
    }
}

/// Serve a connection whose startup failed: answer `initialize` with a
/// JSON-RPC error carrying `message`, then end.
///
/// This is how a bad `--config` reaches the client as a readable error instead
/// of a silently dead process.
pub async fn serve_startup_failure(
    message: String,
    transport: impl ConnectTo<Agent> + 'static,
) -> Result<(), Error> {
    let (answered_tx, answered_rx) = futures::channel::oneshot::channel::<()>();
    let answered_tx = std::sync::Mutex::new(Some(answered_tx));

    Agent
        .builder()
        .name(AGENT_NAME)
        .on_receive_request(
            async move |_request: InitializeRequest, responder, _connection| {
                let outcome =
                    responder.respond_with_error(Error::internal_error().data(message.clone()));
                if let Some(sender) = answered_tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = sender.send(());
                }
                outcome
            },
            on_receive_request!(),
        )
        .connect_with(transport, async move |_connection| {
            let _ = answered_rx.await;
            // Give the error response a moment to flush before the connection closes.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Ok(())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_is_namespaced() {
        assert_eq!(session_key("abc"), "acp:abc");
    }

    #[test]
    fn initialize_echoes_a_supported_version_and_caps_a_newer_one() {
        let supported = initialize_response(&InitializeRequest::new(ProtocolVersion::V1));
        assert_eq!(supported.protocol_version, ProtocolVersion::V1);
        let newer = initialize_response(&InitializeRequest::new(ProtocolVersion::from(2u16)));
        assert_eq!(newer.protocol_version, ProtocolVersion::LATEST);
    }

    #[test]
    fn initialize_advertises_no_load_session_and_no_auth() {
        let response = initialize_response(&InitializeRequest::new(ProtocolVersion::V1));
        assert!(!response.agent_capabilities.load_session);
        assert!(response.auth_methods.is_empty());
        assert_eq!(response.agent_info.unwrap().name, "rust-bot");
    }

    #[test]
    fn prompt_text_joins_text_blocks() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("first")),
            ContentBlock::Text(TextContent::new("second")),
        ];
        assert_eq!(prompt_text(&blocks), "first\nsecond");
        assert_eq!(prompt_text(&[]), "");
    }

    #[test]
    fn turn_errors_map_to_distinct_json_rpc_errors() {
        let unknown = turn_error(TurnError::UnknownSession);
        let busy = turn_error(TurnError::Busy);
        assert_ne!(unknown.code, busy.code);
    }
}
