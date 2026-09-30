//! `rust-bot acp`: serve rust-bot as an Agent Client Protocol agent over stdio.
//!
//! Startup order matters. stdout is the protocol, so it is claimed before
//! anything can print, logging is forced away from stdout, and only then is the
//! config loaded. A config or provider problem is reported to the client as a
//! JSON-RPC error on `initialize`, not as a silently dead process.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::ByteStreams;
use clap::Parser;

use crate::agent::acp::HEADLESS_DISABLED_TOOLS;
use crate::agent::acp::agent_mode::{serve, serve_startup_failure};
use crate::agent::acp::link::{AcpLink, ConnectionSlot};
use crate::agent::acp::registry::SessionRegistry;
use crate::agent::acp::session_hook::{AcpSessionHook, DEFAULT_PERMISSION_TIMEOUT};
use crate::agent::agent_loop::AgentLoop;
use crate::agent::hook::AgentHook;
use crate::cli::commands::{
    CliError, DEFAULT_CONFIG_PATH, eprint_error, init_agent_loop_with_provider,
    try_create_provider, try_prepare_workspace,
};
use crate::config::log::init_protocol_logging;
use crate::config::schema::Config;
use crate::providers::base::LLMProviderDyn;
use crate::utils::exit_codes::{self, GENERAL_ERROR};
use crate::utils::stdio_redirect::claim_protocol_stdout;

/// Longest the process waits, after the client has gone, for background memory
/// work and Dream to finish before exiting anyway.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(180);

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
}

/// Everything the agent role needs, built from the config.
pub struct AcpRuntime {
    pub agent_loop: Arc<AgentLoop>,
    pub registry: Arc<SessionRegistry>,
    pub slot: Arc<ConnectionSlot>,
}

/// Make `--config` absolute and require that the file exists.
///
/// The client launches rust-bot from an arbitrary working directory, so a
/// relative path would silently load a different file (or none, which the
/// loader would turn into the default config).
pub fn resolve_acp_config_path(config: PathBuf) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(&config)
        .map_err(|e| format!("Invalid config path {}: {e}", config.display()))?;
    if !absolute.is_file() {
        return Err(format!("Config file not found: {}", absolute.display()));
    }
    Ok(absolute)
}

/// Text of a caught panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown error".to_string())
}

/// Load the config and build the ACP runtime from it.
pub fn build_acp_runtime(args: &AcpArgs) -> Result<AcpRuntime, String> {
    let config_path = resolve_acp_config_path(args.config.clone())?;
    // The config loader panics on malformed JSON; turn that into an error.
    let (config, workspace) = std::panic::catch_unwind(AssertUnwindSafe(|| {
        try_prepare_workspace(config_path, args.workspace.clone())
    }))
    .map_err(|payload| {
        format!(
            "Config file could not be loaded: {}",
            panic_message(payload.as_ref())
        )
    })??;

    let provider = try_create_provider(&config)?;
    Ok(assemble_acp_runtime(&config, workspace, provider))
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
    let registry = Arc::new(SessionRegistry::new());
    let slot = Arc::new(ConnectionSlot::new());
    let link: Arc<dyn AcpLink> = slot.clone();
    let hook: Arc<dyn AgentHook> = Arc::new(AcpSessionHook::new(
        link,
        Arc::clone(&registry),
        config.tools.confirm_before_execute,
        DEFAULT_PERMISSION_TIMEOUT,
    ));

    let agent_loop = init_agent_loop_with_provider(config, workspace, provider, Some(vec![hook]));
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

    let transport = ByteStreams::new(
        blocking::Unblock::new(protocol_stdout),
        blocking::Unblock::new(std::io::stdin()),
    );

    match build_acp_runtime(&args) {
        Err(message) => {
            eprint_error(&message);
            let _ = serve_startup_failure(message, transport).await;
            exit_codes::exit(GENERAL_ERROR);
        }
        Ok(runtime) => {
            let served = serve(
                Arc::clone(&runtime.agent_loop),
                Arc::clone(&runtime.registry),
                Arc::clone(&runtime.slot),
                transport,
            )
            .await;
            drain_and_dream(&runtime).await;
            served.map_err(|error| CliError::Other(format!("ACP connection failed: {error}")))
        }
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
    }
}
