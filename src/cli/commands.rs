use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use anstyle::{AnsiColor, Color, Style};
use clap::{Parser, Subcommand};
use futures::lock::Mutex;
use reedline::{
    default_emacs_keybindings, EditCommand, Emacs, FileBackedHistory, Keybindings, KeyCode,
    KeyModifiers, Reedline, ReedlineEvent, Signal, DefaultPrompt,
};
use serde_json::Value;
use termimad::MadSkin;

use crate::agent::agent_loop::{AgentLoop, ProgressCallback};
use crate::bus::queue::MessageBus;
use crate::cli::stream::{StreamRenderer, stream_callbacks};
use crate::config::loader::{load_config, resolve_config_env_vars, set_config_path};
use crate::config::log::init_runtime_logging;
use crate::config::paths::get_cli_history_path;
use crate::config::schema::{ChannelsConfig, Config};
use crate::cron::CronService;
use crate::providers::anthropic_provider::AnthropicProvider;
use crate::providers::base::{LLMProvider, LLMProviderDyn};
use crate::providers::openai_compat_provider::OpenAICompatProvider;
use crate::utils::helpers::{TemplatesSyncError, ensure_dir, sync_workspace_templates};
use crate::utils::logo::LOGO;
use crate::utils::restart::{
    consume_restart_notice_from_env, format_restart_completed_message,
    should_show_cli_restart_notice,
};

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
                write!(
                    f,
                    "Interactive mode is not yet implemented; use -m/--message"
                )
            }
        }
    }
}

impl std::error::Error for CliError {}

/// Print an error line to stderr (red when the terminal supports color).
pub fn eprint_error(message: impl fmt::Display) {
    let style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));
    eprintln!("{}Error: {message}{}", style.render(), style.render_reset());
}

fn render_as_text(metadata: Option<&HashMap<String, Value>>) -> bool {
    metadata
        .and_then(|m| m.get("render_as"))
        .and_then(Value::as_str)
        == Some("text")
}

fn response_body(
    content: &str,
    render_markdown: bool,
    metadata: Option<&HashMap<String, Value>>,
) -> String {
    if !render_markdown || render_as_text(metadata) {
        return content.to_string();
    }
    format!("{}", MadSkin::default().term_text(content))
}

/// Render assistant output with consistent terminal styling.
pub fn print_agent_response(
    response: &str,
    render_markdown: bool,
    metadata: Option<&HashMap<String, Value>>,
) {
    print_agent_response_with_header(response, render_markdown, metadata, true);
}

pub fn print_agent_response_with_header(
    response: &str,
    render_markdown: bool,
    metadata: Option<&HashMap<String, Value>>,
    show_header: bool,
) {
    if show_header {
        let header = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan)));
        println!();
        println!("{}rust-bot{}", header.render(), header.render_reset());
    }
    print!("{}", response_body(response, render_markdown, metadata));
    println!();
    if show_header {
        println!();
    }
}

/// Parse and dispatch CLI commands.
pub async fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Commands::Agent(args) => run_agent(args).await,
    }
}

async fn run_agent(args: AgentArgs) -> Result<(), CliError> {
    let session_id = args.session.clone();
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
    let provider = create_provider(&config);

    let cron_store_path = config.workspace_path().join("cron").join("jobs.json");
    let cron_service = CronService::new(cron_store_path, None);
 
    let agent_loop = AgentLoop::new(
        Arc::new(bus),
        provider,
        workspace,
        Some(config.agents.model.clone()),
        Some(config.agents.max_tool_iterations),
        Some(config.agents.max_tokens),
        Some(config.agents.context_window_tokens),
        config.agents.context_block_limit,
        Some(config.agents.max_tool_result_chars),
        Some(config.agents.provider_retry_mode),
        Some(config.tools.web.clone()),
        Some(config.tools.exec),
        Some(cron_service),
        Some(config.tools.restrict_to_workspace),
        None,
        Some(config.tools.mcp_servers),
        Some(config.channels.clone()),
        Some(config.agents.timezone.clone()),
        None,
    );
    if let Some(restart_notice) = consume_restart_notice_from_env() {
        if should_show_cli_restart_notice(restart_notice.clone(), args.session.as_str()) {
            print_agent_response(
                &format_restart_completed_message(&restart_notice.started_at_raw),
                false,
                None,
            );
        }
    }

    match args.message {
        Some(message) if !message.is_empty() => {
            message_session(&message, markdown, &config.channels, &session_id, Arc::new(agent_loop)).await
        }
        Some(_) => Ok(()),
        None => interactive_session(Arc::new(agent_loop), markdown, &config.channels, &session_id).await,
    }
}

async fn message_session(
    message: &str,
    markdown: bool,
    channels_config: &ChannelsConfig,
    session_id: &str,
    agent_loop: Arc<AgentLoop>,
) -> Result<(), CliError> {
    log::info!("message={message}");
    let renderer: Arc<Mutex<StreamRenderer>> = Arc::new(Mutex::new(StreamRenderer::new(markdown, true)));
    let on_progress = create_on_progress(channels_config.clone(), Arc::clone(&renderer));
    let (on_stream, on_stream_end) = stream_callbacks(Arc::clone(&renderer));
    let response = agent_loop
        .process_direct(
            &message,
            Some(&session_id),
            None,
            None,
            Some(on_progress),
            Some(on_stream),
            Some(on_stream_end),
        )
        .await;
    let locked_renderer = renderer.lock().await;
    let streamed = locked_renderer.streamed;
    let header_printed = locked_renderer.header_printed;
    if !streamed {
        renderer.lock().await.close().await;
    }
    if !streamed {
        print_agent_response_with_header(
            &response.as_ref().map(|r| r.content.as_str()).unwrap_or(""),
            markdown,
            response.as_ref().map(|r| &r.metadata),
            !header_printed,
        );
    }
    Ok(())
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
            }
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

fn create_provider(config: &Config) -> Arc<dyn LLMProviderDyn> {
    let model = config.agents.model.clone();
    let provider_name = config.agents.provider.clone();
    match provider_name.as_str() {
        "openai" | "openai_compat" | "openrouter" => Arc::new(OpenAICompatProvider::new(
            Some(
                config.providers.custom.api_key.clone()),
            config.providers.custom.api_base.clone(),
            Some(model),
            None,
            None,
        )),
        "anthropic" => Arc::new(AnthropicProvider::new(
            Some(config.providers.custom.api_key.clone()),
            config.providers.custom.api_base.clone(),
            Some(model),
            None,
            None,
        )),
        _ => {
            eprint_error(format!("Invalid provider: {provider_name}"));
            std::process::exit(3);
        }
    }
}

fn print_cli_progress_line(renderer: &mut StreamRenderer, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    renderer.pause_spinner(|renderer| {
        renderer.ensure_header();
        let dim = Style::new().dimmed();
        println!("  {}↳ {text}{}", dim.render(), dim.render_reset());
    });
}

fn create_on_progress(channels: ChannelsConfig, renderer: Arc<Mutex<StreamRenderer>>) -> ProgressCallback {
    Arc::new(move |content, tool_hint| {
        let renderer = Arc::clone(&renderer);
        Box::pin(async move {
            if tool_hint {
                if !channels.send_tool_hints {
                    return;
                }
            } else if !channels.send_progress {
                return;
            }
            let mut renderer_guard = renderer.lock().await;
            print_cli_progress_line(&mut renderer_guard, &content);
        })
    })
}

async fn interactive_session(
    agent_loop: Arc<AgentLoop>,
    markdown: bool,
    channels_config: &ChannelsConfig,
    session_id: &str,
) -> Result<(), CliError> {
    let welcome = if markdown {
        format!("{LOGO} Interactive mode (type **exit** or **Ctrl+D** to quit; **Ctrl+Enter** for a new line)\n")
    } else {
        format!("{LOGO} Interactive mode (type exit or Ctrl+D to quit; Ctrl+Enter for a new line)\n")
    };
    print_agent_response_with_header(&welcome, markdown, None, true);
    let mut line_editor = init_prompt_session();
    let prompt = DefaultPrompt::default();
    loop {
        let sig = tokio::task::block_in_place(|| line_editor.read_line(&prompt))
            .map_err(|_| CliError::InteractiveNotImplemented)?;
        match sig {
            Signal::Success(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                if line.trim().eq_ignore_ascii_case("exit") || line.trim().eq_ignore_ascii_case("quit") {
                    break;
                }
                message_session(
                    line.trim_end(),
                    markdown,
                    channels_config,
                    session_id,
                    Arc::clone(&agent_loop),
                ).await?;
            }
            Signal::CtrlC => {
                continue;
            }
            Signal::CtrlD => break,
            _ => continue,
        }
    };

    Ok(())
}

fn interactive_keybindings() -> Keybindings {
    let mut kb = default_emacs_keybindings();
    kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    kb
}

fn build_reedline(history: Option<FileBackedHistory>) -> Reedline {
    let mut editor = Reedline::create()
        .use_bracketed_paste(true)
        .with_edit_mode(Box::new(Emacs::new(interactive_keybindings())));
    if let Some(history) = history {
        editor = editor.with_history(Box::new(history));
    }
    editor
}

fn init_prompt_session() -> Reedline {
    let history_file = get_cli_history_path();
    if let Some(parent) = history_file.parent() {
        ensure_dir(parent);
    }

    let history_result = FileBackedHistory::with_file(100, history_file);

    match history_result {
        Ok(history) => build_reedline(Some(history)),
        Err(e) => {
            log::warn!("Failed to read history file: {}", e);
            build_reedline(None)
        }
    }
}