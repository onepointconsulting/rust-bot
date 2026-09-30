//! ACP peer: run an external coding agent (Grok Build, Claude Code, Codex, Pi, ...)
//! as a child process and talk to it over the Agent Client Protocol.
//!
//! Same shape as grok-build's `xai-acp-lib` gateway:
//! the `!Send` ACP connection lives on its own thread (current_thread runtime + LocalSet),
//! and the rest of the app talks to it through a `Send` mpsc channel whose
//! messages carry a oneshot for the reply.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use agent_client_protocol::{self as acp, Agent as _};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// How to launch one agent. Lives in config, e.g.
/// `{ name = "grok", program = "grok", args = ["agent", "stdio"] }`.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// What rust-bot does when the agent asks to run a tool.
#[derive(Debug, Clone, Copy)]
pub enum PermissionPolicy {
    AllowOnce,
    Reject,
}

/// Progress streamed out of a peer while a turn runs (for the bus / UI).
#[derive(Debug, Clone)]
pub enum PeerEvent {
    Text { peer: String, chunk: String },
    Thought { peer: String, chunk: String },
    ToolCall { peer: String, title: String },
    PermissionDecided { peer: String, title: String, allowed: bool },
}

/// Final result of one `session/prompt` turn.
#[derive(Debug, Clone)]
pub struct PeerReply {
    pub text: String,
    pub stop_reason: acp::StopReason,
}

enum PeerCmd {
    Prompt {
        text: String,
        reply: oneshot::Sender<anyhow::Result<PeerReply>>,
    },
    Cancel,
}

/// `Send + Clone` handle. Safe to keep in an `Arc`, a tool, or the subagent manager.
#[derive(Clone)]
pub struct AcpPeer {
    pub name: String,
    cmd_tx: mpsc::UnboundedSender<PeerCmd>,
}

impl AcpPeer {
    /// Spawn the agent process on a dedicated thread and do the ACP handshake.
    /// Returns once `initialize` + `session/new` have succeeded.
    pub async fn spawn(
        spec: AgentSpec,
        policy: PermissionPolicy,
        events: mpsc::UnboundedSender<PeerEvent>,
    ) -> anyhow::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let name = spec.name.clone();

        std::thread::Builder::new()
            .name(format!("acp-{name}"))
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.into()));
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, peer_main(spec, policy, events, cmd_rx, ready_tx));
            })?;

        ready_rx.await??;
        Ok(Self { name, cmd_tx })
    }

    /// Send a prompt and wait for the whole turn. Returns a `Send` future.
    pub async fn prompt(&self, text: impl Into<String>) -> anyhow::Result<PeerReply> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(PeerCmd::Prompt { text: text.into(), reply })
            .map_err(|_| anyhow::anyhow!("agent '{}' has exited", self.name))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("agent '{}' dropped the turn", self.name))?
    }

    /// Ask the agent to stop the current turn (`session/cancel`).
    pub fn cancel(&self) {
        let _ = self.cmd_tx.send(PeerCmd::Cancel);
    }
}

/// Everything below runs on the peer's own thread, inside the LocalSet.
async fn peer_main(
    spec: AgentSpec,
    policy: PermissionPolicy,
    events: mpsc::UnboundedSender<PeerEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<PeerCmd>,
    ready_tx: oneshot::Sender<anyhow::Result<()>>,
) {
    let mut child = match tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .envs(spec.env.iter().cloned())
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!("spawn {}: {e}", spec.program)));
            return;
        }
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = ready_tx.send(Err(anyhow::anyhow!("no stdio pipes")));
        return;
    };

    let client = Rc::new(PeerClient {
        name: spec.name.clone(),
        policy,
        events,
        turn_text: RefCell::new(String::new()),
    });
    let (conn, io) = acp::ClientSideConnection::new(
        client.clone(),
        stdin.compat_write(),
        stdout.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    tokio::task::spawn_local(io);
    let conn = Rc::new(conn);

    // Handshake: initialize, then one session per peer.
    let handshake = async {
        conn.initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                .client_info(acp::Implementation::new("rust-bot", env!("CARGO_PKG_VERSION"))),
        )
        .await?;
        let session = conn.new_session(acp::NewSessionRequest::new(spec.cwd.clone())).await?;
        Ok::<_, acp::Error>(session.session_id)
    };
    let session_id = match handshake.await {
        Ok(id) => {
            let _ = ready_tx.send(Ok(()));
            id
        }
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!("ACP handshake failed: {e}")));
            return;
        }
    };

    // Command loop: the Send side of the gateway.
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            PeerCmd::Prompt { text, reply } => {
                // Run each turn as its own local task so Cancel can still be received.
                let conn = conn.clone();
                let client = client.clone();
                let session_id = session_id.clone();
                tokio::task::spawn_local(async move {
                    client.turn_text.borrow_mut().clear();
                    let result = conn
                        .prompt(acp::PromptRequest::new(session_id, vec![text.into()]))
                        .await
                        .map(|resp| PeerReply {
                            text: client.turn_text.borrow().clone(),
                            stop_reason: resp.stop_reason,
                        })
                        .map_err(|e| anyhow::anyhow!("prompt failed: {e}"));
                    let _ = reply.send(result);
                });
            }
            PeerCmd::Cancel => {
                let _ = conn.cancel(acp::CancelNotification::new(session_id.clone())).await;
            }
        }
    }
    drop(child); // kill_on_drop
}

/// The `acp::Client` side: what the agent can call back into rust-bot.
struct PeerClient {
    name: String,
    policy: PermissionPolicy,
    events: mpsc::UnboundedSender<PeerEvent>,
    turn_text: RefCell<String>,
}

fn chunk_text(content: &acp::ContentBlock) -> Option<&str> {
    match content {
        acp::ContentBlock::Text(t) => Some(&t.text),
        _ => None,
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for PeerClient {
    async fn session_notification(&self, n: acp::SessionNotification) -> acp::Result<()> {
        let peer = self.name.clone();
        match &n.update {
            acp::SessionUpdate::AgentMessageChunk(c) => {
                if let Some(t) = chunk_text(&c.content) {
                    self.turn_text.borrow_mut().push_str(t);
                    let _ = self.events.send(PeerEvent::Text { peer, chunk: t.to_owned() });
                }
            }
            acp::SessionUpdate::AgentThoughtChunk(c) => {
                if let Some(t) = chunk_text(&c.content) {
                    let _ = self.events.send(PeerEvent::Thought { peer, chunk: t.to_owned() });
                }
            }
            acp::SessionUpdate::ToolCall(tc) => {
                let _ = self.events.send(PeerEvent::ToolCall { peer, title: tc.title.clone() });
            }
            _ => {}
        }
        Ok(())
    }

    async fn request_permission(
        &self,
        req: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let wanted = match self.policy {
            PermissionPolicy::AllowOnce => acp::PermissionOptionKind::AllowOnce,
            PermissionPolicy::Reject => acp::PermissionOptionKind::RejectOnce,
        };
        let title = req.tool_call.fields.title.clone().unwrap_or_default();
        let outcome = match req.options.iter().find(|o| o.kind == wanted) {
            Some(opt) => {
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    opt.option_id.clone(),
                ))
            }
            None => acp::RequestPermissionOutcome::Cancelled,
        };
        let allowed = matches!(self.policy, PermissionPolicy::AllowOnce);
        let _ = self.events.send(PeerEvent::PermissionDecided {
            peer: self.name.clone(),
            title,
            allowed,
        });
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    // We don't advertise fs/terminal capabilities in `initialize`, so agents use their
    // own tools and should never call these. Add them later to sandbox agents inside
    // rust-bot's workspace rules.
    async fn write_text_file(
        &self,
        _: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        Err(acp::Error::method_not_found())
    }
    async fn read_text_file(
        &self,
        _: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        Err(acp::Error::method_not_found())
    }
    async fn create_terminal(
        &self,
        _: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        Err(acp::Error::method_not_found())
    }
    async fn terminal_output(
        &self,
        _: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        Err(acp::Error::method_not_found())
    }
    async fn release_terminal(
        &self,
        _: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        Err(acp::Error::method_not_found())
    }
    async fn wait_for_terminal_exit(
        &self,
        _: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        Err(acp::Error::method_not_found())
    }
    async fn kill_terminal(
        &self,
        _: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        Err(acp::Error::method_not_found())
    }
    async fn ext_method(&self, _: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        Err(acp::Error::method_not_found())
    }
    async fn ext_notification(&self, _: acp::ExtNotification) -> acp::Result<()> {
        Ok(()) // ignore vendor extras such as x.ai/session_notification
    }
}
