use std::fmt;
use std::path::PathBuf;

use anstyle::{AnsiColor, Color, Style};
use clap::{Parser, Subcommand};
use log::LevelFilter;

use crate::bus::queue::MessageBus;
use crate::config::loader::{load_config, resolve_config_env_vars, set_config_path};
use crate::config::schema::Config;
use crate::utils::helpers::{ensure_dir, sync_workspace_templates, TemplatesSyncError};

#[derive(Debug, Parser)]
#[command(
    name = "rust-bot",
    version = env!("CARGO_PKG_VERSION"),
    about = "Rust port of the Nanobot agent",
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run the agent from the command line
    Agent(AgentArgs),
}

#[derive(Debug, Parser)]
pub struct AgentArgs {
    /// Message to send to the agent
    #[arg(short, long)]
    pub message: Option<String>,

    /// Session ID
    #[arg(short, long, default_value = "cli:direct")]
    pub session: String,

    /// Workspace directory
    #[arg(short, long)]
    pub workspace: Option<PathBuf>,

    /// Config file path
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Render assistant output as Markdown
    #[arg(long, default_value_t = true, action = clap::ArgAction::SetTrue)]
    pub markdown: bool,

    #[arg(long = "no-markdown", action = clap::ArgAction::SetTrue, hide = true)]
    no_markdown: bool,

    /// Show rust-bot runtime logs during chat
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub logs: bool,

    #[arg(long = "no-logs", action = clap::ArgAction::SetTrue, hide = true)]
    no_logs: bool,
}

impl AgentArgs {
    pub fn markdown_enabled(&self) -> bool {
        self.markdown && !self.no_markdown
    }

    pub fn logs_enabled(&self) -> bool {
        self.logs && !self.no_logs
    }
}

#[derive(Debug)]
pub enum CliError {
    InteractiveNotImplemented,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InteractiveNotImplemented => {
                write!(f, "Interactive mode is not yet implemented; use -m/--message")
            }
        }
    }
}

impl std::error::Error for CliError {}

/// Print an error line to stderr (red when the terminal supports color).
pub fn eprint_error(message: impl fmt::Display) {
    let style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));
    eprintln!(
        "{}Error: {message}{}",
        style.render(),
        style.render_reset()
    );
}

fn init_runtime_logging(logs: bool) {
    let has_rust_log = std::env::var_os("RUST_LOG").is_some();
    if !logs && !has_rust_log {
        return;
    }

    let mut builder = env_logger::Builder::from_default_env();
    // `--logs` without RUST_LOG: default to info. RUST_LOG wins when set.
    if logs && !has_rust_log {
        builder.filter_level(LevelFilter::Info);
    }
    let _ = builder.try_init();
}

/// Parse and dispatch CLI commands.
pub async fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Commands::Agent(args) => run_agent(args).await,
    }
}

async fn run_agent(args: AgentArgs) -> Result<(), CliError> {
    let markdown = args.markdown_enabled();
    let logs = args.logs_enabled();
    init_runtime_logging(logs);

    let config = load_runtime_config(args.config, args.workspace);
    let workspace = config.workspace_path();
    ensure_dir(&workspace);
    if let Err(err @ TemplatesSyncError::TemplatesUnavailable { .. }) =
        sync_workspace_templates(&workspace, false)
    {
        eprint_error(err);
        std::process::exit(2);
    }
    let bus = MessageBus::new();

    match args.message {
        Some(message) => {
            log::info!("message={message}");
            // Agent loop wiring is a follow-up; parsing validated here.
            eprintln!("(agent runtime not yet wired; received message: {message})");
            Ok(())
        }
        None => Err(CliError::InteractiveNotImplemented),
    }
}

/// Load config and optionally override the active workspace.
fn load_runtime_config(config: Option<PathBuf>, workspace: Option<PathBuf>) -> Config {

    if let Some(config) = config {
        if !config.exists() {
            eprint_error(format!("Config file not found: {}", config.display()));
            std::process::exit(1);
        }
        set_config_path(config.clone());
        let loaded = resolve_config_env_vars(&load_config(Some(config.clone())));
        println!("Using config: {}", config.display());
        match loaded {
            Ok(mut loaded) => {
                if let Some(workspace) = workspace {
                    loaded.agents.workspace = workspace.clone().to_string_lossy().into_owned();
                }
                loaded
            },
            Err(e) => {
                eprint_error(e);
                std::process::exit(1);
            }
        }
    } else {
        eprint_error("No config file provided");
        std::process::exit(1);
    }
}