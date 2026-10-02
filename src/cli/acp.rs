//! `rust-bot acp`: serve rust-bot as an Agent Client Protocol agent over stdio.
//!
//! Startup order matters:
//!
//! 1. stdout is the protocol, so it is claimed before anything can print, and
//!    logging is forced away from it.
//! 2. stdin is read from the start, so a client that disconnects while we wait
//!    is noticed.
//! 3. The config is loaded read-only to learn the workspace.
//! 4. The workspace lock is taken. Only the lock holder may touch the workspace;
//!    waiting for it never writes anything, and EOF while waiting exits quietly.
//! 5. Only now is the runtime built (which creates workspace files) and served.
//!
//! A config, lock or provider problem is reported to the client as a JSON-RPC
//! error on `initialize`, not as a silently dead process.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::ByteStreams;
use clap::Parser;

use crate::agent::acp::HEADLESS_DISABLED_TOOLS;
use crate::agent::acp::agent_mode::{serve, serve_startup_failure};
use crate::agent::acp::link::{AcpLink, ConnectionSlot};
use crate::agent::acp::registry::SessionRegistry;
use crate::agent::acp::session_hook::{AcpSessionHook, DEFAULT_PERMISSION_TIMEOUT};
use crate::agent::acp::stdin_pump::{EofSignal, StdinPump, spawn_pump};
use crate::agent::acp::workspace_lock::{DEFAULT_POLL_INTERVAL, WorkspaceLock, acquire};
use crate::agent::agent_loop::AgentLoop;
use crate::agent::hook::AgentHook;
use crate::cli::commands::{
    CliError, DEFAULT_CONFIG_PATH, eprint_error, init_agent_loop_with_provider,
    try_create_provider, try_load_runtime_config,
};
use crate::config::loader::set_config_path;
use crate::config::log::init_protocol_logging;
use crate::config::overlay::{load_config_with_overlays, mark_overlay_active};
use crate::config::schema::Config;
use crate::providers::base::LLMProviderDyn;
use crate::utils::exit_codes::{self, GENERAL_ERROR};
use crate::utils::helpers::{ensure_dir, sync_workspace_templates};
use crate::utils::stdio_redirect::claim_protocol_stdout;

/// Longest the process waits, after the client has gone, for background memory
/// work and Dream to finish before exiting anyway.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(180);

/// Default for `--lock-wait-secs`.
pub const DEFAULT_LOCK_WAIT_SECS: u64 = 30;

#[derive(Debug, Parser)]
pub struct AcpArgs {
    /// JSON configuration file path. Use an absolute path when a client launches rust-bot.
    #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,

    /// Workspace directory (overrides the config)
    #[arg(short, long)]
    pub workspace: Option<PathBuf>,

    /// Write rust-bot runtime logs to stderr (or to `RUST_LOG_FILE`). Never to stdout.
    #[arg(long, default_value_t = false)]
    pub logs: bool,

    /// Run as a child of another rust-bot: `--config` is the parent's config and
    /// this file is a JSON merge patch applied on top of it. Only a short
    /// allowlist of settings may be overridden, and only towards less power (see
    /// `config::overlay`). Repeat it for a child of a child: the overlays apply in
    /// the order given, outermost first, each checked against the one before.
    /// Needs `--workspace`: a child has its own home, it must never share its
    /// parent's memory and sessions.
    #[arg(long, requires = "workspace")]
    pub overlay: Vec<PathBuf>,

    /// How long to wait for another `rust-bot acp` process to release the workspace
    /// before failing `initialize` with an error that names it.
    #[arg(long, default_value_t = DEFAULT_LOCK_WAIT_SECS)]
    pub lock_wait_secs: u64,

    /// A folder the file tools must refuse, e.g. the parent's own home when the
    /// project a child works on contains it. Repeatable. Set by a parent rust-bot.
    #[arg(long = "deny-path")]
    pub deny_path: Vec<PathBuf>,
}

/// Everything the agent role needs, built from the config.
pub struct AcpRuntime {
    pub agent_loop: Arc<AgentLoop>,
    pub registry: Arc<SessionRegistry>,
    pub slot: Arc<ConnectionSlot>,
}

/// Make `path` absolute and require that the file exists. `what` names the
/// file in the error ("Config file", "Overlay file").
///
/// The client launches rust-bot from an arbitrary working directory, so a
/// relative path would silently load a different file (or none, which the
/// config loader would turn into the default config).
fn resolve_existing_file(path: PathBuf, what: &str) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(&path)
        .map_err(|e| format!("Invalid {what} path {}: {e}", path.display()))?;
    if !absolute.is_file() {
        return Err(format!("{what} not found: {}", absolute.display()));
    }
    Ok(absolute)
}

/// Make `--config` absolute and require that the file exists.
pub fn resolve_acp_config_path(config: PathBuf) -> Result<PathBuf, String> {
    resolve_existing_file(config, "Config file")
}

/// Text of a caught panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown error".to_string())
}

/// A loaded config and the workspace it names. Nothing has been written yet.
pub struct AcpStartup {
    pub config: Config,
    pub workspace: PathBuf,
    /// Folders the file tools refuse (from `--deny-path`).
    pub denied_roots: Vec<PathBuf>,
}

/// Load the config read-only to learn the workspace.
///
/// Does not create or change any file: the workspace lock has to be taken
/// before anything in the workspace is touched.
pub fn load_acp_config(args: &AcpArgs) -> Result<AcpStartup, String> {
    let config_path = resolve_acp_config_path(args.config.clone())?;
    // The config loader panics on malformed JSON; turn that into an error.
    let loaded = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if args.overlay.is_empty() {
            try_load_runtime_config(config_path.clone(), args.workspace.clone())
        } else {
            load_child_config(&config_path, &args.overlay, args.workspace.as_deref())
        }
    }))
    .map_err(|payload| {
        format!(
            "Config file could not be loaded: {}",
            panic_message(payload.as_ref())
        )
    })?;
    let config = loaded?;
    let workspace = config.workspace_path();
    Ok(AcpStartup {
        config,
        workspace,
        denied_roots: args.deny_path.clone(),
    })
}

/// The parent's config with the overlays applied and the child's own workspace.
///
/// The overlays are validated against the allowlist and merged *before* `${VAR}`
/// placeholders are expanded (see `config::overlay`). Marks the process as a
/// child so commands that would rewrite the parent's config refuse to run, and
/// remembers the chain so this child's own children inherit through it.
fn load_child_config(
    config_path: &Path,
    overlays: &[PathBuf],
    workspace: Option<&Path>,
) -> Result<Config, String> {
    let overlay_paths = overlays
        .iter()
        .map(|overlay| resolve_existing_file(overlay.clone(), "Overlay file"))
        .collect::<Result<Vec<_>, _>>()?;
    // The data folder (logs, media) follows the parent's config path.
    set_config_path(config_path.to_path_buf());
    let mut config =
        load_config_with_overlays(config_path, &overlay_paths).map_err(|e| e.to_string())?;
    let workspace = workspace.ok_or("--overlay needs --workspace: a child has its own home")?;
    config.agents.workspace = workspace.to_string_lossy().into_owned();
    mark_overlay_active(&overlay_paths);
    Ok(config)
}

/// Create the workspace files and build the ACP runtime.
///
/// Call this only while holding the workspace lock.
pub fn build_acp_runtime(startup: AcpStartup) -> Result<AcpRuntime, String> {
    let AcpStartup {
        config,
        workspace,
        denied_roots,
    } = startup;
    ensure_dir(&workspace);
    sync_workspace_templates(&workspace, false);
    let provider = try_create_provider(&config)?;
    Ok(assemble_acp_runtime_denying(
        &config,
        workspace,
        provider,
        denied_roots,
    ))
}

/// Build the ACP runtime around an existing provider: the ACP hook installed,
/// the tools that cannot work headless removed.
///
/// `CliAskHook` is never installed here: stdin is the protocol pipe, never a
/// terminal, so the CLI hook would run every gated tool without asking. The
/// ACP hook asks the client instead and fails closed.
pub fn assemble_acp_runtime(
    config: &Config,
    workspace: PathBuf,
    provider: Arc<dyn LLMProviderDyn>,
) -> AcpRuntime {
    assemble_acp_runtime_denying(config, workspace, provider, Vec::new())
}

/// [`assemble_acp_runtime`] with folders the file tools must refuse.
pub fn assemble_acp_runtime_denying(
    config: &Config,
    workspace: PathBuf,
    provider: Arc<dyn LLMProviderDyn>,
    denied_roots: Vec<PathBuf>,
) -> AcpRuntime {
    let registry = Arc::new(SessionRegistry::new());
    let slot = Arc::new(ConnectionSlot::new());
    let link: Arc<dyn AcpLink> = slot.clone();
    let hook: Arc<dyn AgentHook> = Arc::new(AcpSessionHook::new(
        link,
        Arc::clone(&registry),
        config.tools.confirm_before_execute,
        DEFAULT_PERMISSION_TIMEOUT,
    ));

    let agent_loop = init_agent_loop_with_provider(config, workspace, provider, Some(vec![hook]))
        .with_denied_roots(denied_roots);
    agent_loop.unregister_tools(&HEADLESS_DISABLED_TOOLS);
    AcpRuntime {
        agent_loop: Arc::new(agent_loop),
        registry,
        slot,
    }
}

/// Clean stop once the client has gone: abort running turns, let background
/// memory work finish, run Dream, then return so the process can exit 0.
///
/// Bounded by [`SHUTDOWN_DRAIN_TIMEOUT`] so a stuck LLM call cannot keep the
/// process alive forever.
async fn drain_and_dream(runtime: &AcpRuntime) {
    runtime.registry.cancel_all();
    let agent_loop = Arc::clone(&runtime.agent_loop);
    let drained = tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, async move {
        agent_loop.close_mcp().await;
        let _ = agent_loop.dream.run().await;
    })
    .await;
    if drained.is_err() {
        log::warn!("ACP: shutdown work did not finish within {SHUTDOWN_DRAIN_TIMEOUT:?}; exiting");
    }
}

/// Entry point of `rust-bot acp`.
pub async fn run_acp(args: AcpArgs) -> Result<(), CliError> {
    // Before anything else can print: take the real stdout for the protocol.
    let protocol_stdout = match claim_protocol_stdout() {
        Ok(stdout) => stdout,
        Err(error) => {
            eprint_error(format!("cannot take over standard output: {error}"));
            exit_codes::exit(GENERAL_ERROR);
        }
    };
    init_protocol_logging(args.logs, None);

    // Read stdin from the start so a disconnect during startup is noticed.
    let StdinPump { reader, mut eof } = spawn_pump(std::io::stdin());
    let transport = ByteStreams::new(blocking::Unblock::new(protocol_stdout), reader);

    match start(&args, &mut eof).await {
        Startup::ClientGone => Ok(()),
        Startup::Failed(message) => {
            eprint_error(&message);
            let _ = serve_startup_failure(message, transport).await;
            exit_codes::exit(GENERAL_ERROR);
        }
        Startup::Ready { runtime, lock } => {
            let served = serve(
                Arc::clone(&runtime.agent_loop),
                Arc::clone(&runtime.registry),
                Arc::clone(&runtime.slot),
                transport,
            )
            .await;
            drain_and_dream(&runtime).await;
            drop(lock);
            served.map_err(|error| CliError::Other(format!("ACP connection failed: {error}")))
        }
    }
}

/// How startup ended.
pub enum Startup {
    /// The workspace is locked by us and the runtime is built.
    Ready {
        runtime: AcpRuntime,
        lock: WorkspaceLock,
    },
    /// The client closed stdin while we were waiting for the lock. Nothing in
    /// the workspace was touched.
    ClientGone,
    /// Startup failed; the message is what the client should be told.
    Failed(String),
}

/// Load the config, take the workspace lock (waiting up to `--lock-wait-secs`,
/// and giving up at once if the client disconnects), then build the runtime.
pub async fn start(args: &AcpArgs, eof: &mut EofSignal) -> Startup {
    let startup = match load_acp_config(args) {
        Ok(startup) => startup,
        Err(message) => return Startup::Failed(message),
    };

    let wait = Duration::from_secs(args.lock_wait_secs);
    let locked = tokio::select! {
        // A free lock is taken even if stdin has already closed, so a client
        // that sends its requests and closes stdin at once is still served.
        biased;
        result = acquire(&startup.workspace, wait, DEFAULT_POLL_INTERVAL) => result,
        _ = eof.reached() => return Startup::ClientGone,
    };
    let lock = match locked {
        Ok(lock) => lock,
        Err(error) => return Startup::Failed(error.to_string()),
    };

    match build_acp_runtime(startup) {
        Ok(runtime) => Startup::Ready { runtime, lock },
        Err(message) => Startup::Failed(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_config_path_becomes_absolute() {
        // `cargo test` runs with the package root as working directory, where
        // Cargo.toml always exists; this avoids changing the process-wide cwd.
        let resolved = resolve_acp_config_path(PathBuf::from("Cargo.toml")).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("Cargo.toml"));
    }

    #[test]
    fn missing_config_is_an_error_not_a_silent_default() {
        let error = resolve_acp_config_path(PathBuf::from("definitely-not-here.json")).unwrap_err();
        assert!(error.contains("Config file not found"), "{error}");
    }

    #[test]
    fn a_directory_is_not_a_config_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_acp_config_path(dir.path().to_path_buf()).is_err());
    }

    #[test]
    fn panic_payloads_of_both_kinds_are_readable() {
        let from_str = std::panic::catch_unwind(|| panic!("static message")).unwrap_err();
        assert_eq!(panic_message(from_str.as_ref()), "static message");
        let from_string = std::panic::catch_unwind(|| panic!("formatted {}", 7)).unwrap_err();
        assert_eq!(panic_message(from_string.as_ref()), "formatted 7");
    }

    #[test]
    fn acp_args_default_to_the_standard_config_path() {
        let args = AcpArgs::try_parse_from(["acp"]).unwrap();
        assert_eq!(args.config, PathBuf::from(DEFAULT_CONFIG_PATH));
        assert!(!args.logs);
        assert!(args.workspace.is_none());
        assert_eq!(args.lock_wait_secs, DEFAULT_LOCK_WAIT_SECS);
        assert!(args.deny_path.is_empty());
    }

    #[test]
    fn deny_path_can_be_repeated() {
        let args =
            AcpArgs::try_parse_from(["acp", "--deny-path", "/a", "--deny-path", "/b"]).unwrap();
        assert_eq!(args.deny_path, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }
}
