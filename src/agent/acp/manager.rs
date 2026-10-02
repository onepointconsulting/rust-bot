//! Running a child agent for one turn: the parent's side of plan steps 2–4 and 7–8.
//!
//! [`AcpManager::run`] takes a prompt for a named child and does, in order:
//! check the child and the folder it will work on, derive its environment, take
//! the per-child lock, launch `rust-bot acp`, run the turn over ACP, remember the
//! child's session, and start the child's graceful stop in the background.
//!
//! One live process per child at a time (plan decision 10). The in-process lock
//! taken here is held until the child process has really exited, so a second run
//! for the same child waits instead of starting a second process on the same
//! files. (Across processes the child's own `.acp.lock` does the same.)
//!
//! If the future returned by `run` is dropped before it finishes (the operator
//! stopped the parent's turn), the child process is dropped with it, and
//! [`RunningChild`]'s drop stops it gracefully in the background: it is never
//! left running unattended (plan decision 22).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::agent::acp::client::{
    ActivityEntry, ClientError, ProgressSink, TurnEnd, TurnRequest, run_turn,
};
use crate::agent::acp::launch::{
    LaunchError, RUSTBOT_PRESET, RustBotLaunch, build_rustbot_spec, rustbot_command,
};
use crate::agent::acp::permission::PermissionResponder;
use crate::agent::acp::project_folder::{
    ProjectFolderError, ProjectRequest, choose_project_folder,
};
use crate::agent::acp::session_index::ChildSessionIndex;
use crate::agent::acp::store::{AgentMeta, ChildAgentStore, StoreError};
use crate::agent::acp::transport::{ExitReport, RunningChild};
use crate::config::loader::load_config;
use crate::config::overlay::{
    OverlayError, effective_config, effective_config_chain, read_overlay,
};
use crate::config::schema::{AcpConfig, AcpSessionScope, Config};

/// The waits the manager applies around a turn. The defaults suit real use;
/// tests shorten them.
#[derive(Debug, Clone, Copy)]
pub struct ManagerTimings {
    /// Added to the child's lock wait, once for the child's own wait and once for
    /// the parent's wait for `initialize` (process start, config load, provider setup).
    pub startup_slack: Duration,
    /// How long a timed-out child gets to confirm `session/cancel`.
    pub cancel_grace: Duration,
    /// How often `still working` is reported while the child shows no tool call.
    pub heartbeat_every: Duration,
}

impl Default for ManagerTimings {
    fn default() -> Self {
        Self {
            startup_slack: Duration::from_secs(60),
            cancel_grace: Duration::from_secs(10),
            heartbeat_every: Duration::from_secs(30),
        }
    }
}

/// Where the manager lives and what it runs on.
#[derive(Debug, Clone)]
pub struct AcpManagerSettings {
    /// The parent's own home; children live under `<home>/acp/agents/`.
    pub parent_workspace: PathBuf,
    /// The root config file. Children read it live, as the parent does.
    pub config_path: PathBuf,
    /// The overlays this process itself runs on, outermost first (empty in the
    /// root process). A child's children inherit through all of them, so no
    /// grandchild can have more than the child above it.
    pub inherited_overlays: Vec<PathBuf>,
    /// This process's depth in the chain (`0` for the root).
    pub depth: u32,
    /// Replaces `current_exe` when the `rustbot` preset says `self`.
    pub executable_override: Option<PathBuf>,
    /// The parent's environment, as a snapshot (children get a derived subset).
    pub parent_env: HashMap<String, String>,
    pub timings: ManagerTimings,
}

/// One request to run a child.
pub struct RunRequest {
    pub agent: String,
    pub prompt: String,
    /// The `cwd` the model passed, if any.
    pub cwd: Option<String>,
    /// The parent session this run belongs to (keys the child session).
    pub parent_session_key: String,
    /// The parent session's project folder and whether it is confined to it.
    pub parent_project: PathBuf,
    pub parent_restricted: bool,
    /// The parent's `tools.acp` settings.
    pub acp: AcpConfig,
    /// Overrides `acp.default_timeout_secs` for this run.
    pub timeout: Option<Duration>,
    pub responder: Arc<dyn PermissionResponder>,
    pub progress: Option<Arc<dyn ProgressSink>>,
}

/// What a run produced.
pub struct RunResult {
    pub agent: String,
    pub reply: String,
    pub activity: Vec<ActivityEntry>,
    pub end: TurnEnd,
    pub session_id: String,
    /// The child continued an earlier conversation.
    pub resumed: bool,
    /// The folder the child worked on.
    pub cwd: PathBuf,
    /// Things the operator should hear about.
    pub warnings: Vec<String>,
    /// How the child's process ended; resolves after the graceful stop.
    pub exit: JoinHandle<ExitReport>,
}

/// Why a run did not produce a turn.
#[derive(Debug)]
pub enum RunError {
    UnknownAgent {
        name: String,
        known: Vec<String>,
    },
    /// Only the `rustbot` preset can be launched in this version.
    UnsupportedPreset {
        agent: String,
        preset: String,
    },
    Store(StoreError),
    Overlay(OverlayError),
    ParentConfig(String),
    Project(ProjectFolderError),
    Launch(LaunchError),
    /// The process could not be started.
    Spawn(String),
    /// The child started but the turn could not be run.
    Child {
        error: ClientError,
        stderr_tail: String,
    },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::UnknownAgent { name, known } if known.is_empty() => write!(
                f,
                "there is no agent named '{name}' and no agents exist yet; create one with acp_create_agent"
            ),
            RunError::UnknownAgent { name, known } => write!(
                f,
                "there is no agent named '{name}'. Existing agents: {}",
                known.join(", ")
            ),
            RunError::UnsupportedPreset { agent, preset } => write!(
                f,
                "agent '{agent}' uses the launch preset '{preset}', which this version cannot launch (only '{RUSTBOT_PRESET}')"
            ),
            RunError::Store(error) => write!(f, "{error}"),
            RunError::Overlay(error) => write!(f, "{error}"),
            RunError::ParentConfig(reason) => write!(f, "cannot read the parent config: {reason}"),
            RunError::Project(error) => write!(f, "{error}"),
            RunError::Launch(error) => write!(f, "cannot launch the agent: {error}"),
            RunError::Spawn(reason) => write!(f, "cannot start the agent process: {reason}"),
            RunError::Child { error, stderr_tail } if stderr_tail.trim().is_empty() => {
                write!(f, "{error}")
            }
            RunError::Child { error, stderr_tail } => {
                write!(f, "{error}. The agent's stderr: {}", tail_of(stderr_tail))
            }
        }
    }
}

impl std::error::Error for RunError {}

/// The last few lines of a stderr capture, for an error message.
fn tail_of(text: &str) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    let start = lines.len().saturating_sub(8);
    lines[start..].join(" | ")
}

/// Parent-side coordinator of child agents.
pub struct AcpManager {
    settings: AcpManagerSettings,
    store: ChildAgentStore,
    index: ChildSessionIndex,
    /// One lock per child name: held until that child's process has exited.
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl AcpManager {
    pub fn new(settings: AcpManagerSettings) -> Self {
        let store = ChildAgentStore::new(&settings.parent_workspace);
        let index = ChildSessionIndex::new(&settings.parent_workspace);
        Self {
            settings,
            store,
            index,
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn settings(&self) -> &AcpManagerSettings {
        &self.settings
    }

    pub fn store(&self) -> &ChildAgentStore {
        &self.store
    }

    pub fn index(&self) -> &ChildSessionIndex {
        &self.index
    }

    /// The config file as stored on disk, **not** expanded and **not** narrowed
    /// by this process's own overlays.
    fn load_root_config(&self) -> Result<Config, String> {
        let path = self.settings.config_path.clone();
        std::panic::catch_unwind(move || load_config(Some(path))).map_err(|payload| {
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "the config file is not valid JSON".to_string())
        })
    }

    /// What a child started now would inherit: the config on disk with this
    /// process's own overlays applied (none in the root process), **not**
    /// expanded. New overlays are checked against it, and children's environments
    /// are derived from it.
    pub fn load_parent_config(&self) -> Result<Config, String> {
        let root = self.load_root_config()?;
        let overlays = self
            .settings
            .inherited_overlays
            .iter()
            .map(|path| read_overlay(path))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        effective_config_chain(&root, &overlays).map_err(|e| e.to_string())
    }

    /// The lock of one child, created on first use.
    fn lock_for(&self, agent: &str) -> Arc<AsyncMutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(agent.to_string())
            .or_default()
            .clone()
    }

    /// Run one turn of `request.agent`.
    pub async fn run(&self, request: RunRequest) -> Result<RunResult, RunError> {
        let meta = self.load_agent(&request.agent)?;
        let parent_config = self.load_parent_config().map_err(RunError::ParentConfig)?;
        let overlay = self
            .store
            .read_overlay(&meta.name)
            .map_err(RunError::Store)?;
        // The child's effective config, still with `${VAR}` placeholders.
        let child_config = effective_config(&parent_config, &overlay).map_err(RunError::Overlay)?;

        let project = choose_project_folder(&ProjectRequest {
            requested: request.cwd.as_deref(),
            agent_default: meta.cwd.as_deref(),
            parent_project: &request.parent_project,
            parent_restricted: request.parent_restricted,
            parent_workspace: &self.settings.parent_workspace,
            child_shell_enabled: shell_is_enabled(&child_config),
        })
        .map_err(RunError::Project)?;

        let (program, leading_args) = rustbot_command(
            &parent_config.tools.acp,
            self.settings.executable_override.as_deref(),
        )
        .map_err(RunError::Launch)?;
        let grace = Duration::from_secs(request.acp.shutdown_grace_secs);
        let timings = self.settings.timings;
        let lock_wait = grace + timings.startup_slack;
        let home = self.store.agent_dir(&meta.name);
        // The chain the child runs on: this process's own overlays, then its own.
        let mut overlays = self.settings.inherited_overlays.clone();
        overlays.push(self.store.overlay_path(&meta.name));
        let spec = build_rustbot_spec(&RustBotLaunch {
            program,
            leading_args,
            parent_config: &self.settings.config_path,
            overlays: &overlays,
            home: &home,
            denied_roots: &project.denied_roots,
            lock_wait,
            depth: self.settings.depth + 1,
            child_config: &child_config,
            parent_env: &self.settings.parent_env,
        })
        .map_err(RunError::Launch)?;

        let session_key = session_key_for(request.acp.session_scope, &request.parent_session_key);
        let previous_session = self.remembered_session(&meta.name, &session_key).await;

        // Wait for any earlier run of this child to be gone, then hold the lock
        // until *this* child's process is gone too.
        let guard = self.lock_for(&meta.name).lock_owned().await;
        let mut child = RunningChild::spawn(&spec, grace, Box::new(guard))
            .map_err(|e| RunError::Spawn(e.to_string()))?;
        let transport = child
            .take_transport()
            .ok_or_else(|| RunError::Spawn("the child's pipes are unavailable".to_string()))?;

        let turn = TurnRequest {
            cwd: project.cwd.clone(),
            prompt: request.prompt,
            previous_session,
            startup_timeout: lock_wait + timings.startup_slack,
            turn_timeout: request
                .timeout
                .unwrap_or_else(|| Duration::from_secs(request.acp.default_timeout_secs)),
            cancel_grace: timings.cancel_grace,
            heartbeat_every: timings.heartbeat_every,
            responder: request.responder,
            progress: request.progress,
        };
        let outcome = run_turn(transport, turn).await;

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // A child that never started, or broke, is not worth waiting for.
                let stderr_tail = child.stderr_tail();
                kill_in_background(&child);
                return Err(RunError::Child { error, stderr_tail });
            }
        };
        if outcome.end
            == (TurnEnd::TimedOut {
                cancel_confirmed: false,
            })
        {
            // It ignored the cancel: there is nothing to wait for.
            kill_in_background(&child);
        }

        self.remember_session(&meta.name, &session_key, &outcome.session_id)
            .await;
        let exit = child.shutdown();

        Ok(RunResult {
            agent: meta.name,
            reply: outcome.reply,
            activity: outcome.activity,
            end: outcome.end,
            session_id: outcome.session_id,
            resumed: outcome.resumed,
            cwd: project.cwd,
            warnings: project.warnings,
            exit,
        })
    }

    /// An agent's metadata, or an error that lists the agents that do exist.
    fn load_agent(&self, name: &str) -> Result<AgentMeta, RunError> {
        let meta = match self.store.get(name) {
            Ok(meta) => meta,
            Err(StoreError::NotFound(_)) | Err(StoreError::InvalidName(_)) => {
                return Err(RunError::UnknownAgent {
                    name: name.to_string(),
                    known: self
                        .store
                        .list()
                        .into_iter()
                        .map(|summary| summary.meta.name)
                        .collect(),
                });
            }
            Err(error) => return Err(RunError::Store(error)),
        };
        if meta.preset != RUSTBOT_PRESET {
            return Err(RunError::UnsupportedPreset {
                agent: meta.name,
                preset: meta.preset,
            });
        }
        Ok(meta)
    }

    /// The child session remembered for this parent session. A damaged or
    /// unreadable index only costs a fresh session, so it is not an error.
    async fn remembered_session(&self, agent: &str, session_key: &str) -> Option<String> {
        let index = self.index.clone();
        let (agent, key) = (agent.to_string(), session_key.to_string());
        tokio::task::spawn_blocking(move || index.get(&agent, &key))
            .await
            .ok()
            .and_then(|result| {
                result
                    .map_err(|e| log::warn!("cannot read the ACP session index: {e}"))
                    .ok()
            })
            .flatten()
    }

    async fn remember_session(&self, agent: &str, session_key: &str, session_id: &str) {
        let index = self.index.clone();
        let (agent, key, session_id) = (
            agent.to_string(),
            session_key.to_string(),
            session_id.to_string(),
        );
        let saved = tokio::task::spawn_blocking(move || index.set(&agent, &key, &session_id)).await;
        if let Ok(Err(error)) | Err(error) = saved.map_err(std::io::Error::other) {
            log::warn!("cannot save the ACP session index: {error}");
        }
    }
}

/// The key under which a parent session's child session is remembered.
fn session_key_for(scope: AcpSessionScope, parent_session_key: &str) -> String {
    match scope {
        AcpSessionScope::PerParentSession => parent_session_key.to_string(),
        AcpSessionScope::Shared => String::new(),
    }
}

/// Whether the child will have the (unconfined) shell tool.
fn shell_is_enabled(child_config: &Config) -> bool {
    child_config.tools.exec.enable
        && !child_config
            .tools
            .disabled_tools
            .iter()
            .any(|tool| tool == "shell")
}

/// Kill the child's process tree without blocking the async runtime.
fn kill_in_background(child: &RunningChild) {
    let pid = child.pid();
    tokio::task::spawn_blocking(move || crate::utils::process::kill_process_tree_sync(pid));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_scope_uses_one_key_for_every_parent_session() {
        assert_eq!(session_key_for(AcpSessionScope::Shared, "ws:alice"), "");
        assert_eq!(session_key_for(AcpSessionScope::Shared, "ws:bob"), "");
        assert_eq!(
            session_key_for(AcpSessionScope::PerParentSession, "ws:alice"),
            "ws:alice"
        );
    }

    #[test]
    fn shell_counts_as_enabled_unless_switched_off_or_disabled() {
        let mut config = Config::default();
        assert!(shell_is_enabled(&config));
        config.tools.disabled_tools = vec!["shell".to_string()];
        assert!(!shell_is_enabled(&config));
        config.tools.disabled_tools.clear();
        config.tools.exec.enable = false;
        assert!(!shell_is_enabled(&config));
    }

    #[test]
    fn the_error_for_an_unknown_agent_lists_the_known_ones() {
        let error = RunError::UnknownAgent {
            name: "reviewr".to_string(),
            known: vec!["code-review".to_string(), "docs".to_string()],
        };
        let text = error.to_string();
        assert!(
            text.contains("reviewr") && text.contains("code-review, docs"),
            "{text}"
        );
        let none = RunError::UnknownAgent {
            name: "x".to_string(),
            known: Vec::new(),
        };
        assert!(none.to_string().contains("acp_create_agent"));
    }

    #[test]
    fn a_child_error_carries_the_end_of_its_stderr() {
        let error = RunError::Child {
            error: ClientError::Startup("no answer within 5s".to_string()),
            stderr_tail: (1..=20)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let text = error.to_string();
        assert!(
            text.contains("line 20") && !text.contains("line 5 "),
            "{text}"
        );
        assert!(text.contains("no answer within 5s"));
    }
}
