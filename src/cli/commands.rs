use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use crate::agent::cron_context::with_cron_context_stack;
use crate::agent::tools::cron::CronTool;
use crate::agent::tools::message::MessageTool;
use crate::api::login::{GatewayApiDoc, LoginState, jwt_auth_state_from_config, login};
use crate::api::rest::ApiServer;
use crate::api::rest::build_cors_layer;
use crate::api::rest::create_api_server;
use crate::api::user_registry::{
    JsonUserRegistry, User, UserRegistry, UserRegistryError, hash_password,
};
use crate::bus::events::{InboundMessage, OutboundMessage};
use crate::channels::base::BaseChannel;
use crate::channels::manager::ChannelManager;
use crate::channels::websocket::runtime::WebSocketChannel;
use crate::channels::websocket::types::WebSocketConfig;
use crate::channels::whatsapp::{WhatsAppChannel, WhatsAppConfig};
use crate::cli::cancel::wait_for_escape_cancel;
use crate::cli::onboard::run_onboard;
use crate::cli::wizard::resolve_onboard_config_path;
use crate::cron::CronJobState;
use crate::cron::CronPayload;
use crate::cron::CronPayloadKind;
use crate::cron::compute_next_run;
use crate::cron::service::now_ms;
use crate::heartbeat::service::HeartbeatService;
use crate::security::jwt::{
    DEFAULT_EXPIRES_IN_MONTHS, JwtError, generate_jwt_keypair, generate_jwt_token,
};
use crate::security::workspace_requests::WorkspaceRequestHandler;
use crate::session::manager::SessionManager;
use crate::utils::cli::{is_all_interfaces_host, print_markdown, print_warning};
use crate::utils::evaluator::evaluate_response;

use anstyle::{AnsiColor, Color, Style};
use axum::Router;
use axum::routing::post;
use clap::{Parser, Subcommand};
use futures::lock::Mutex;
use reedline::{
    EditCommand, FileBackedHistory, KeyCode, KeyModifiers, Keybindings, Prompt, PromptEditMode,
    PromptHistorySearch, PromptHistorySearchStatus, Reedline, ReedlineEvent, Signal,
    default_emacs_keybindings,
};
use serde_json::Value;
use termimad::MadSkin;
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::agent::agent_loop::{AgentLoop, ProgressCallback};
use crate::bus::queue::MessageBus;
use crate::cli::paste_edit_mode::{
    PasteCapturingEmacs, prepare_image_paste_insert, prepare_text_paste_insert,
};
use crate::cli::progress::{ProgressType, create_on_progress, print_cli_progress_line};
use crate::cli::stream::{StreamRenderer, stream_callbacks};
use crate::config::loader::{load_config, resolve_config_env_vars, save_config, set_config_path};
use crate::config::log::init_runtime_logging;
use crate::config::paths::get_cli_history_path;
use crate::config::schema::{ChannelsConfig, Config};
use crate::cron::{CronJob, CronService};
use crate::providers::base::LLMProviderDyn;
use crate::providers::factory::create_provider_for;
use crate::utils::clipboard::IMAGE_PASTE_COMMAND_REGEX;
use crate::utils::clipboard::try_get_clipboard_text;
use crate::utils::clipboard::{IMAGE_PASTE_COMMAND, try_get_clipboard_image};
use crate::utils::clipboard::{TEXT_PASTE_COMMAND, TEXT_PASTE_SENTINEL_REGEX};
use crate::utils::exit_codes::{self, GENERAL_ERROR, INVALID_PROVIDER};
use crate::utils::helpers::{ensure_dir, sync_workspace_templates};
use crate::utils::logo::LOGO;
use crate::utils::restart::{
    consume_restart_notice_from_env, format_restart_completed_message,
    should_show_cli_restart_notice,
};

#[derive(Debug, Parser)]
#[command(
    name = "rust-bot",
    version = env!("CARGO_PKG_VERSION"),
    about = "Personal AI agent with workspace tools, interactive console, API, and gateway",
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

    /// Run the API server
    Api(ApiArgs),

    /// Run the gateway server
    Gateway(GatewayArgs),

    /// Perform interactive channel login (e.g. WhatsApp QR pairing)
    Login(LoginArgs),

    /// Initialize configuration and workspace
    Onboard(OnboardArgs),

    /// Generate an Ed25519 keypair and update api.jwt key paths in the config file
    GenerateJwtKeypair(GenerateJwtKeypairArgs),

    /// Mint an EdDSA JWT using the private key path from the config file
    GenerateJwtToken(GenerateJwtTokenArgs),
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

    /// JSON configuration file path
    #[arg(short, long)]
    pub config: PathBuf,

    /// Render assistant output as Markdown
    #[arg(long, default_value_t = true, action = clap::ArgAction::SetTrue)]
    #[arg(
        long = "no-markdown",
        action = clap::ArgAction::SetFalse,
        overrides_with = "markdown"
    )]
    pub markdown: bool,

    /// Show rust-bot runtime logs during chat
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    #[arg(
        long = "no-logs",
        action = clap::ArgAction::SetFalse,
        overrides_with = "logs"
    )]
    pub logs: bool,
}

#[derive(Debug, Parser)]
pub struct ApiArgs {
    /// Port to listen on
    #[arg(short, long, default_value = "8900")]
    pub port: Option<u16>,

    /// Bind address to listen on
    #[arg(long, default_value = "0.0.0.0")]
    pub host: Option<String>,

    /// Timeout for API requests
    #[arg(short, long, default_value = "60")]
    pub timeout: u64,

    #[arg(short, long, default_value = "api:default")]
    pub session: String,

    /// Directory of pre-built web-chat static assets (index.html, *.js, *.wasm)
    /// to serve alongside the API. Falls back to `api.webRoot` in the config
    /// file, then to `./web` if that directory exists.
    #[arg(long = "web-root")]
    pub web_root: Option<PathBuf>,

    /// JSON configuration file path (workspace is taken from `agents.workspace`)
    #[arg(short, long)]
    pub config: PathBuf,
}

#[derive(Debug, Parser)]
pub struct GatewayArgs {
    /// Workspace directory
    #[arg(short, long)]
    pub workspace: Option<PathBuf>,

    /// Set logging to debug during runtime.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub verbose: bool,

    /// JSON configuration file path
    #[arg(short, long)]
    pub config: PathBuf,

    /// Bind address for the combined login + WebSocket gateway server.
    /// Overrides `gateway.host`. Only relevant when a `"websocket"` channel
    /// is declared in the config — otherwise nothing binds this port.
    #[arg(long)]
    pub host: Option<String>,

    /// Port for the combined login + WebSocket gateway server. Overrides
    /// `gateway.port`. Only relevant when a `"websocket"` channel is
    /// declared in the config — otherwise nothing binds this port.
    #[arg(long)]
    pub port: Option<u16>,

    /// Directory of pre-built web-chat static assets (index.html, *.js, *.wasm)
    /// to serve alongside the API. Falls back to `gateway.webRoot` in the config
    /// file, then to `./web` if that directory exists.
    #[arg(long = "web-root")]
    pub web_root: Option<PathBuf>,
}

#[derive(Debug, Parser)]
pub struct OnboardArgs {
    /// Workspace directory
    #[arg(short, long, default_value = "./.rust-bot/workspace")]
    pub workspace: PathBuf,

    /// JSON configuration file path
    #[arg(short, long, default_value = "./.rust-bot/config.json")]
    pub config: PathBuf,

    /// Use interactive setup wizard
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub wizard: bool,
}

#[derive(Debug, Parser)]
pub struct LoginArgs {
    /// Channel to log in to (e.g. whatsapp)
    #[arg(long)]
    pub channel: String,

    /// Ignore existing credentials and force re-authentication
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub force: bool,

    /// JSON configuration file path
    #[arg(short, long, default_value = "./rust-bot/config.json")]
    pub config: PathBuf,
}

#[derive(Debug, Parser)]
pub struct GenerateJwtKeypairArgs {
    /// Path to the rust-bot JSON configuration file
    #[arg(short, long)]
    pub config: PathBuf,

    /// Directory where private_key.pem and public_key.pem are written
    #[arg(long, default_value = "./.rust-bot/credentials")]
    pub credentials_dir: PathBuf,

    /// Overwrite existing key files
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub force: bool,
}

#[derive(Debug, Parser)]
pub struct GenerateJwtTokenArgs {
    /// Path to the rust-bot JSON configuration file
    #[arg(short, long)]
    pub config: PathBuf,

    /// Override issuer (defaults to api.jwt.iss from config)
    #[arg(long)]
    pub iss: Option<String>,

    /// Override audience (defaults to api.jwt.aud from config; empty omits claim)
    #[arg(long)]
    pub aud: Option<String>,

    /// Purpose claim marking what this token was minted for (e.g. "webui"
    /// for a WebSocket-connecting WebUI frontend); omitted when unset
    #[arg(long)]
    pub purpose: Option<String>,

    /// Token lifetime in months (default: 6)
    #[arg(long, default_value_t = DEFAULT_EXPIRES_IN_MONTHS)]
    pub expires_in_months: u32,

    /// The email of the user for whom the token is being generated
    #[arg(long, required = true)]
    pub user_email: String,

    /// Optional password; stored as an Argon2id hash in the users file
    #[arg(long)]
    pub password: Option<String>,

    /// Path to the JSON user registry file (email -> token map)
    #[arg(long, required = true)]
    pub users_file: PathBuf,
}

#[derive(Debug)]
pub enum CliError {
    FailedToCreateWebRootDirectory(std::io::Error),
    InteractiveNotImplemented,
    Inquire(inquire::InquireError),
    Jwt(JwtError),
    UserRegistry(UserRegistryError),
    Other(String),
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
            Self::Inquire(err) => {
                write!(f, "Inquire error: {err}")
            }
            Self::FailedToCreateWebRootDirectory(err) => {
                write!(f, "Failed to create web root directory: {err}")
            }
            Self::Jwt(err) => write!(f, "{err}"),
            Self::UserRegistry(err) => write!(f, "{err}"),
            Self::Other(err) => write!(f, "{err}"),
        }
    }
}

impl From<inquire::InquireError> for CliError {
    fn from(err: inquire::InquireError) -> Self {
        Self::Inquire(err)
    }
}

impl From<JwtError> for CliError {
    fn from(err: JwtError) -> Self {
        Self::Jwt(err)
    }
}

impl From<UserRegistryError> for CliError {
    fn from(err: UserRegistryError) -> Self {
        Self::UserRegistry(err)
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
        Commands::Api(args) => run_api(args).await,
        Commands::Gateway(args) => run_gateway(args).await,
        Commands::Login(args) => run_login(args).await,
        Commands::Onboard(args) => run_onboard(args),
        Commands::GenerateJwtKeypair(args) => run_generate_keypair(args),
        Commands::GenerateJwtToken(args) => run_generate_token(args),
    }
}

fn path_for_config(path: PathBuf) -> PathBuf {
    let canonical = path.canonicalize().unwrap_or(path);
    #[cfg(windows)]
    {
        let as_str = canonical.to_string_lossy();
        if let Some(stripped) = as_str.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    canonical
}

fn run_generate_keypair(args: GenerateJwtKeypairArgs) -> Result<(), CliError> {
    let mut config = load_config(Some(args.config.clone()));
    run_generate_keypair_with_config(&mut config, args.credentials_dir, args.force)?;

    save_config(&config, Some(args.config.clone()))?;

    eprintln!("Updated api.jwt key paths in {}", args.config.display());
    Ok(())
}

pub fn run_generate_keypair_with_config(
    config: &mut Config,
    credentials_dir: PathBuf,
    force: bool,
) -> Result<(), CliError> {
    let keys = generate_jwt_keypair(credentials_dir, force)?;

    let private_key_path = path_for_config(keys.private_key_path);
    let public_key_path = path_for_config(keys.public_key_path);

    config.api.jwt.private_key_path = private_key_path.display().to_string();
    config.api.jwt.public_key_path = public_key_path.display().to_string();

    eprintln!("Wrote private key: {}", private_key_path.display());
    eprintln!("Wrote public key:  {}", public_key_path.display());
    Ok(())
}

/// Resolves the `aud` claim for a minted token.
///
/// Precedence, checked in order:
/// 1. An explicit `--aud` override always wins — the caller knows best.
/// 2. `purpose == "webui"` mints a token meant to authenticate a WebUI
///    frontend's *WebSocket* connection, not a REST API call — so it must
///    carry the WebSocket channel's own audience, or
///    `validate_jwt_aud_matches_path` (`channels::websocket::types`) will
///    401 it at WS upgrade since that channel checks incoming tokens'
///    `aud` against its own `path`, not `api.jwt.aud`. That audience is:
///    - `existing_websocket_config.jwt.aud` if a `websocket` entry already
///      exists in `channels.extra` and its `jwt.aud` is non-empty; else
///    - that same existing entry's `path`; else, if no `websocket` entry
///      exists yet,
///    - `WebSocketConfig::default().path` — the value `run_generate_token`
///      is about to write into a freshly created entry, so the minted token
///      and the config it's paired with always agree.
/// 3. Any other purpose (or none) keeps the REST API's own audience,
///    `api_jwt_aud` (i.e. `api.jwt.aud`) — unchanged from prior behavior.
fn resolve_aud(
    aud_override: Option<&str>,
    purpose: Option<&str>,
    api_jwt_aud: &str,
    existing_websocket_config: Option<&WebSocketConfig>,
) -> String {
    if let Some(explicit) = aud_override {
        return explicit.to_string();
    }

    if purpose == Some("webui") {
        return match existing_websocket_config {
            Some(cfg) if !cfg.jwt.aud.trim().is_empty() => cfg.jwt.aud.clone(),
            Some(cfg) => cfg.path.clone(),
            None => WebSocketConfig::default().path,
        };
    }

    api_jwt_aud.to_string()
}

fn run_generate_token(args: GenerateJwtTokenArgs) -> Result<(), CliError> {
    let mut config = load_config(Some(args.config.clone()));
    let jwt = &config.api.jwt;

    let iss = args.iss.unwrap_or_else(|| jwt.iss.clone());

    // Deserialize the existing `websocket` entry (if any) so `resolve_aud`
    // can mirror the audience the gateway will actually validate against,
    // rather than always falling back to the REST API's own `api.jwt.aud`.
    // A malformed existing entry (fails to deserialize as `WebSocketConfig`)
    // is treated as absent — `resolve_aud` then falls back to the default
    // WebSocket path, the same value used when writing a fresh entry below.
    let existing_websocket_config: Option<WebSocketConfig> = config
        .channels
        .extra
        .get("websocket")
        .and_then(|value| serde_json::from_value(value.clone()).ok());

    let aud = resolve_aud(
        args.aud.as_deref(),
        args.purpose.as_deref(),
        &jwt.aud,
        existing_websocket_config.as_ref(),
    );
    let purpose = args.purpose.unwrap_or_default();

    let minted = generate_jwt_token(
        &jwt.private_key_path,
        iss,
        aud,
        purpose,
        args.expires_in_months,
    )?;

    let mut registry = JsonUserRegistry::open(args.users_file.clone())?;
    registry
        .register_user(&User {
            email: args.user_email,
            password_hash: hash_password(args.password)?,
            token: minted.token.clone(),
        })
        .map_err(|e| CliError::Other(e.to_string()))?;
    // Canonicalize after register_user so the file exists on disk.
    config.api.users_file = path_for_config(args.users_file).display().to_string();

    let websocket_config = WebSocketConfig::default();
    if !config.channels.extra.contains_key("websocket") {
        config.channels.extra.insert(
            "websocket".to_string(),
            serde_json::json!({
                "enabled": websocket_config.enabled,
                "host": websocket_config.host,
                "port": websocket_config.port,
                "path": websocket_config.path,
                "jwt": serde_json::json!({
                    "enabled": true,
                    "privateKeyPath": config.api.jwt.private_key_path,
                    "publicKeyPath": config.api.jwt.public_key_path,
                    "iss": config.api.jwt.iss,
                    "aud": websocket_config.path,
                }),
                "allowFrom": websocket_config.allow_from,
                "streaming": websocket_config.streaming,
                "maxMessageBytes": websocket_config.max_message_bytes,
                "pingIntervalS": websocket_config.ping_interval_s,
                "pingTimeoutS": websocket_config.ping_timeout_s,
                "sslCertfile": websocket_config.ssl_certfile,
            }),
        );
    }

    eprintln!("Updated websocket config in {}", args.config.display());

    save_config(&config, Some(args.config.clone()))?;

    eprintln!("sub: {}", minted.claims.sub);
    eprintln!("exp: {} (unix)", minted.claims.exp);
    if let Some(aud) = &minted.claims.aud {
        eprintln!("aud: {aud}");
    } else {
        eprintln!("aud: (omitted)");
    }
    if let Some(purpose) = &minted.claims.purpose {
        eprintln!("purpose: {purpose}");
    } else {
        eprintln!("purpose: (omitted)");
    }
    println!("{}", minted.token);
    Ok(())
}

fn prepare_workspace(config: PathBuf, workspace: Option<PathBuf>) -> (Config, PathBuf) {
    let config = load_runtime_config(config, workspace);
    let workspace = config.workspace_path();
    ensure_dir(&workspace);
    sync_workspace_templates(&workspace, false);
    (config, workspace)
}

fn init_agent_loop(config: &Config, workspace: PathBuf) -> AgentLoop {
    let bus = MessageBus::new();
    let provider = create_provider(&config);
    log::info!("provider api base: {:?}", provider.api_base());

    let cron_store_path = config.workspace_path().join("cron").join("jobs.json");
    let cron_service = CronService::new(cron_store_path, None);

    let agent_loop = AgentLoop::new(
        Arc::new(bus),
        provider,
        workspace,
        config.clone(),
        Some(cron_service),
        None,
        None,
    );
    agent_loop
}

async fn run_agent(args: AgentArgs) -> Result<(), CliError> {
    let session_id = args.session.clone();
    let markdown = args.markdown;
    let logs = args.logs;
    init_runtime_logging(logs, None);

    let (config, workspace) = prepare_workspace(args.config, args.workspace);
    let agent_loop = init_agent_loop(&config, workspace);

    if let Some(restart_notice) = consume_restart_notice_from_env() {
        if should_show_cli_restart_notice(restart_notice.clone(), args.session.as_str()) {
            print_agent_response(
                &format_restart_completed_message(&restart_notice.started_at_raw),
                false,
                None,
            );
        }
    }

    let agent_loop = Arc::new(agent_loop);
    // Subagent completions publish system-channel messages to the inbound bus.
    // The gateway handles those in `AgentLoop::run()`; CLI uses `process_direct`
    // instead, so we need a background listener to deliver async results.
    let system_listener = spawn_system_message_listener(Arc::clone(&agent_loop), markdown);
    let outbound_listener = spawn_outbound_message_listener(Arc::clone(&agent_loop), markdown);
    let result = match args.message {
        Some(message) if !message.is_empty() => {
            message_session(
                &message,
                vec![],
                markdown,
                &config.channels,
                &session_id,
                Arc::clone(&agent_loop),
                true,
            )
            .await
        }
        Some(_) => Ok(()),
        None => {
            interactive_session(
                Arc::clone(&agent_loop),
                markdown,
                &config.channels,
                &session_id,
            )
            .await
        }
    };

    system_listener.abort();
    outbound_listener.abort();
    // Drain title generation / consolidation before exit. One-shot `-m`
    // otherwise kills those background tasks before they persist.
    agent_loop.close_mcp().await;
    result
}

/// Lightweight non-interactive config bootstrap (wizard mode not yet implemented).
async fn run_login(args: LoginArgs) -> Result<(), CliError> {
    init_runtime_logging(true, None);
    let config_path = resolve_onboard_config_path(args.config);
    if !config_path.exists() {
        eprint_error(format!("Config file not found: {}", config_path.display()));
        exit_codes::exit(GENERAL_ERROR);
    }
    let config = match resolve_config_env_vars(&load_config(Some(config_path.clone()))) {
        Ok(config) => config,
        Err(e) => {
            eprint_error(e);
            exit_codes::exit(GENERAL_ERROR);
        }
    };

    let channel_name = args.channel.trim().to_ascii_lowercase();
    let bus = Arc::new(MessageBus::new());
    // Login never touches sessions or workspace scope — it only calls
    // `channel.login(force)` — so these throwaway values exist solely to
    // satisfy the (now-uniform) channel constructor signature.
    let session_manager = Arc::new(StdMutex::new(SessionManager::new(config.workspace_path())));
    let workspace_request_handler =
        WorkspaceRequestHandler::new(config.workspace_path(), config.tools.restrict_to_workspace);
    let channel: Arc<dyn BaseChannel> = match channel_name.as_str() {
        "whatsapp" => {
            let whatsapp_cfg = config
                .channels
                .extra
                .get("whatsapp")
                .cloned()
                .and_then(|v| serde_json::from_value::<WhatsAppConfig>(v).ok())
                .unwrap_or_default();
            Arc::new(WhatsAppChannel::new(
                whatsapp_cfg,
                Arc::clone(&bus),
                config.channels.clone(),
                session_manager,
                workspace_request_handler,
            ))
        }
        other => {
            eprint_error(format!(
                "Unsupported channel for login: '{other}'. Currently supported: whatsapp"
            ));
            exit_codes::exit(GENERAL_ERROR);
        }
    };

    println!(
        "Logging in to {}{}...",
        channel.display_name(),
        if args.force { " (force)" } else { "" }
    );
    let ok = channel.login(args.force).await;
    if ok {
        println!("{} login succeeded.", channel.display_name());
        Ok(())
    } else {
        eprint_error(format!("{} login failed.", channel.display_name()));
        exit_codes::exit(GENERAL_ERROR);
    }
}

/// Resolve the web-chat static assets directory: CLI `--web-root` takes
/// priority, then `api.webRoot` from the config file, then `./web` if that
/// directory happens to exist. Returns `None` when nothing is configured
/// and no default directory is present (the API then runs without a UI).
fn resolve_web_root(
    cli_web_root: Option<PathBuf>,
    config_web_root: Option<String>,
) -> Option<PathBuf> {
    if let Some(path) = cli_web_root {
        return Some(path);
    }
    if let Some(path) = config_web_root {
        return Some(PathBuf::from(path));
    }
    let default_path = PathBuf::from("./web");
    default_path.exists().then_some(default_path)
}

/// True when `web_root` is a directory that contains the static bundle's
/// `index.html`. An empty directory (including one just created because the
/// configured path was missing) is treated as "no UI".
fn web_root_has_ui(web_root: &std::path::Path) -> bool {
    web_root.is_dir() && web_root.join("index.html").is_file()
}

async fn run_api(args: ApiArgs) -> Result<(), CliError> {
    init_runtime_logging(true, None);
    let (config, workspace) = prepare_workspace(args.config, None);
    let agent_loop = init_agent_loop(&config, workspace.clone());
    let host = args.host.unwrap_or_else(|| config.api.host.clone());
    let port = args.port.unwrap_or_else(|| config.api.port);
    let model_name = config.agents.model.clone();
    let session_id = args.session.clone();
    let timeout = args.timeout;
    let web_root = resolve_web_root(args.web_root, config.api.web_root.clone());
    let users_file: PathBuf = config.api.users_file.clone().into();
    let user_registry = if users_file.exists() {
        JsonUserRegistry::open(users_file).unwrap()
    } else {
        JsonUserRegistry::empty()
    };
    render_api_startup_message(
        &host,
        port,
        &model_name,
        &workspace,
        &session_id,
        &timeout,
        web_root.as_deref(),
    );
    if let Err(err) = create_api_server(ApiServer {
        agent_loop: Arc::new(agent_loop),
        host,
        port,
        session_id,
        model_name,
        timeout,
        jwt: config.api.jwt.clone(),
        cors: config.api.cors.clone(),
        web_root,
        user_registry: Arc::new(StdMutex::new(user_registry)),
    })
    .await
    {
        eprintln!("Failed to start API server: {err}");
        exit_codes::exit(GENERAL_ERROR);
    }
    Ok(())
}

/// `None` when `channels.websocket` is absent, malformed, or `enabled: false`
/// — [`run_gateway`]'s combined login+WebSocket server only starts when this
/// returns `Some`. `WebSocketConfig::default().enabled` is `true`, so the
/// check has to be "is the `websocket` key present in `extra` at all" (mirrors
/// `registry::discover_all`'s own `.get(name)` pattern for email/whatsapp),
/// not merely "is it disabled" — a config that never mentions `websocket`
/// must not silently start a server nobody asked for.
///
/// Not routed through `discover_all`/`BUILTIN_CHANNELS`: that function
/// returns type-erased `Box<dyn BaseChannel>`, but `run_gateway` needs the
/// *concrete* `WebSocketChannel` to call its inherent `.router()` method.
fn resolve_websocket_channel(
    config: &Config,
    bus: Arc<MessageBus>,
    session_manager: Arc<StdMutex<SessionManager>>,
    workspace_request_handler: WorkspaceRequestHandler,
) -> Option<Arc<WebSocketChannel>> {
    let raw = config.channels.extra.get("websocket")?.clone();
    let cfg: WebSocketConfig = serde_json::from_value(raw)
        .inspect_err(|err| log::error!("Invalid \"websocket\" channel config: {err}"))
        .ok()?;
    if !cfg.enabled {
        return None;
    }
    Some(Arc::new(WebSocketChannel::new(
        cfg,
        bus,
        config.channels.clone(),
        session_manager,
        workspace_request_handler,
    )))
}

/// Wait for the given channel's shutdown signal — the same `Arc<Notify>`
/// [`BaseChannel::stop`] fires — so the combined server's `axum::serve(...)`
/// can shut down gracefully alongside the rest of the gateway.
async fn wait_for_websocket_shutdown(ws_channel: Arc<WebSocketChannel>) {
    ws_channel.shutdown_signal().notified().await;
}

/// Build and serve the combined login + WebSocket gateway server on one
/// port: `POST /v1/login` (documented via its own minimal Swagger doc),
/// `ws_channel`'s upgrade route, and — when `web_root` points at a real
/// directory — the websockets-chat static bundle as a fallback service,
/// sharing one `axum::serve` call. Every minted token carries
/// `purpose: "webui"` unconditionally — this server's only real client is
/// the websockets-chat UI.
///
/// Reuses `ws_channel`'s own `WebSocketConfig.jwt` for minting (already
/// validated at config-load time to have `aud == path`, so a token minted
/// here satisfies the same channel's `authorize()` check with no new JWT
/// config section) and `config.api.users_file` for credentials, so
/// websockets-chat and web-chat share one user base.
async fn serve_combined_login_and_gateway(
    config: &Config,
    ws_channel: &Arc<WebSocketChannel>,
    host: &str,
    port: u16,
    web_root: Option<&std::path::Path>,
) -> std::io::Result<()> {
    let login_state = Arc::new(LoginState {
        jwt_auth: jwt_auth_state_from_config(&ws_channel_jwt(ws_channel)),
        user_registry: Arc::new(StdMutex::new(open_or_empty_user_registry(
            &config.api.users_file,
        ))),
        token_purpose: "webui".to_string(),
    });
    let login_router = Router::new()
        .route("/v1/login", post(login))
        .with_state(login_state);

    let mut app: Router = login_router
        .merge(
            SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", GatewayApiDoc::openapi()),
        )
        .layer(build_cors_layer(&config.api.cors))
        .merge(ws_channel.router());

    // Mirrors `create_api_server`'s own web-root fallback wiring exactly —
    // `ServeDir` serves the bundle, falling back to `index.html` for
    // client-side routed paths, without shadowing `/v1/login`, `/swagger-ui`,
    // or the WebSocket upgrade route above (all matched before the fallback).
    let web_ui_status = match web_root {
        Some(root) if web_root_has_ui(root) => {
            let index_html = root.join("index.html");
            app = app.fallback_service(
                ServeDir::new(root).not_found_service(ServeFile::new(index_html)),
            );
            Some(format!("serving `{}`", root.display()))
        }
        Some(root) if root.is_dir() => {
            log::warn!(
                "gateway.webRoot / --web-root points at '{}', which has no index.html; web UI serving is disabled",
                root.display()
            );
            None
        }
        Some(root) => {
            log::warn!(
                "gateway.webRoot / --web-root points at '{}', which is not a directory; web UI serving is disabled",
                root.display()
            );
            None
        }
        None => None,
    };

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    log::info!("Gateway login + WebSocket server listening on http://{addr}");
    log::info!("Swagger UI available at http://{addr}/swagger-ui");
    match web_ui_status {
        Some(status) => log::info!("Web UI available at http://{addr}/ ({status})"),
        None => log::info!("Web UI disabled (no valid --web-root / gateway.webRoot configured)"),
    }

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_websocket_shutdown(Arc::clone(ws_channel)))
    .await
}

/// `ws_channel`'s `WebSocketConfig.jwt` isn't exposed directly (only via the
/// `WsShared` snapshot built per-router-call) — reading it through `shared()`
/// keeps this file from needing a dedicated accessor on `WebSocketChannel`
/// just for this one field.
fn ws_channel_jwt(ws_channel: &Arc<WebSocketChannel>) -> crate::config::schema::JwtConfig {
    ws_channel.shared().jwt
}

/// Mirrors `run_api`'s own open-or-empty fallback so the combined gateway
/// server's login shares the exact same credential-file semantics as the
/// REST API's `/v1/login`.
fn open_or_empty_user_registry(users_file: &str) -> JsonUserRegistry {
    let path: PathBuf = users_file.into();
    if path.exists() {
        JsonUserRegistry::open(path).unwrap()
    } else {
        JsonUserRegistry::empty()
    }
}

async fn run_gateway(args: GatewayArgs) -> Result<(), CliError> {
    init_runtime_logging(true, Some(args.verbose));
    let host_override = args.host.clone();
    let port_override = args.port;
    let web_root_override = args.web_root.clone();
    let (config, workspace) = prepare_workspace(args.config, args.workspace);
    let agent_loop = Arc::new(init_agent_loop(&config, workspace.clone()));
    let session_manager = agent_loop.session_manager.clone();
    let cron = agent_loop.cron_service.clone();

    // Execute a cron job through the agent.
    let on_cron_job = {
        let agent_loop = Arc::clone(&agent_loop);
        move |job: CronJob| {
            let agent_loop = Arc::clone(&agent_loop);
            Box::pin(async move {
                with_cron_context_stack(|| async move {
                    // Dream is an internal job — run directly, not through the agent loop.
                    if job.name == "dream" {
                        let _ = agent_loop.dream.run().await;
                        return Ok(());
                    }
                    let reminder_note = [
                        "[Scheduled Task] Timer finished.\n",
                        format!("Task '{}' has been triggered.", job.name).as_str(),
                        format!("Scheduled instruction: {}", job.payload.message).as_str(),
                    ]
                    .join("\n");
                    let session_key = format!("cron:{}", job.id);
                    let cron_tool = {
                        let guard = agent_loop.tools.lock().unwrap_or_else(|e| e.into_inner());
                        guard.get("cron")
                    };
                    let cron_token = cron_tool.as_ref().and_then(|tool| {
                        (tool.as_ref() as &dyn std::any::Any)
                            .downcast_ref::<CronTool>()
                            .map(|cron_tool| cron_tool.set_cron_context(true))
                    });
                    let channel = job
                        .payload
                        .channel
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("cli");
                    let chat_id = job
                        .payload
                        .to
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("direct");
                    let resp = Arc::clone(&agent_loop)
                        .process_direct(
                            &reminder_note,
                            Some(session_key.as_str()),
                            Some(channel),
                            Some(chat_id),
                            None,
                            None,
                            None,
                            None,
                        )
                        .await;
                    if let Some(token) = cron_token {
                        if let Some(tool) = cron_tool.as_ref() {
                            if let Some(cron_tool) =
                                (tool.as_ref() as &dyn std::any::Any).downcast_ref::<CronTool>()
                            {
                                cron_tool.reset_cron_context(token);
                            }
                        }
                    }

                    // If the message tool already delivered the reply, we're done.
                    let already_sent = {
                        let message_tool = agent_loop
                            .tools
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get("message");
                        message_tool
                            .as_ref()
                            .and_then(|tool| {
                                (tool.as_ref() as &dyn std::any::Any).downcast_ref::<MessageTool>()
                            })
                            .map(|message_tool| {
                                *message_tool
                                    .sent_in_turn
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                            })
                            .unwrap_or(false)
                    };
                    if let Some(response) = resp {
                        if already_sent {
                            return Ok(());
                        }
                        if job.payload.deliver
                            && let Some(to) = job.payload.to
                            && !to.is_empty()
                            && !response.content.is_empty()
                        {
                            let should_notify = evaluate_response(
                                &response.content,
                                &reminder_note,
                                agent_loop.provider(),
                                &agent_loop.model(),
                            )
                            .await;
                            if should_notify {
                                let outbound = OutboundMessage {
                                    channel: channel.to_string(),
                                    chat_id: to,
                                    content: response.content.clone(),
                                    reply_to: None,
                                    media: vec![],
                                    metadata: HashMap::new(),
                                    event: None,
                                };
                                let bus = agent_loop.bus();
                                if let Err(e) = bus.publish_outbound(outbound) {
                                    log::error!("Failed to publish cron outbound message: {e}");
                                }
                            }
                        }
                    }
                    Ok(())
                })
                .await
            }) as Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
        }
    };

    if let Some(cron) = &cron {
        cron.set_on_job(Arc::new(on_cron_job)).await;
    }

    // Create the channel manager
    let config = Arc::new(config);
    // `None` unless a `"websocket"` section is actually declared in config —
    // see `resolve_websocket_channel`'s doc comment.
    let ws_channel = resolve_websocket_channel(
        &config,
        Arc::clone(&agent_loop.bus()),
        Arc::clone(&session_manager),
        agent_loop.workspace_request_handler(),
    );
    let mut channel_manager = ChannelManager::new(
        Arc::clone(&config),
        Arc::clone(&agent_loop.bus()),
        Arc::clone(&session_manager),
        agent_loop.workspace_request_handler(),
    );
    if let Some(ws_channel) = &ws_channel {
        channel_manager = channel_manager
            .register_channel("websocket", Arc::clone(ws_channel) as Arc<dyn BaseChannel>);
    }
    let channels = Arc::new(channel_manager);

    /// Pick a routable channel/chat target for heartbeat-triggered messages.
    async fn pick_heartbeat_target(
        channels: Arc<ChannelManager>,
        session_manager: Arc<StdMutex<SessionManager>>,
    ) -> (String, String) {
        let enabled = channels.get_enabled_channels();
        // Prefer the most recently updated non-internal session on an enabled channel.
        for item in session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .list_sessions()
        {
            let key = item
                .get("key")
                .and_then(|v| v.as_str())
                .filter(|k| !k.is_empty())
                .unwrap_or("");
            if !key.contains(':') {
                continue;
            }
            let splits = key.splitn(2, ':').collect::<Vec<&str>>();
            let channel = splits[0];
            let chat_id = splits[1];
            if channel == "cli" || channel == "system" {
                continue;
            }
            if enabled.contains(&channel.to_string()) && !chat_id.is_empty() {
                return (channel.to_string(), chat_id.to_string());
            }
        }
        ("cli".to_string(), "direct".to_string())
    }

    // Create heartbeat service
    let on_heartbeat_execute = {
        let agent_loop = Arc::clone(&agent_loop);
        let session_manager = Arc::clone(&session_manager);
        let channels = Arc::clone(&channels);
        let config = Arc::clone(&config);
        move |tasks: &str| {
            let tasks = tasks.to_string();
            let agent_loop = Arc::clone(&agent_loop);
            let session_manager = Arc::clone(&session_manager);
            let channels = Arc::clone(&channels);
            let config = Arc::clone(&config);
            Box::pin(async move {
                let (channel, chat_id) =
                    pick_heartbeat_target(Arc::clone(&channels), Arc::clone(&session_manager))
                        .await;
                // Suppress progress publishing during heartbeat (matches Python `_silent`).
                let silent: ProgressCallback = Arc::new(|_message, _kind| Box::pin(async {}));
                let resp = Arc::clone(&agent_loop)
                    .process_direct(
                        &tasks,
                        Some("heartbeat"),
                        Some(&channel),
                        Some(&chat_id),
                        None,
                        Some(silent),
                        None,
                        None,
                    )
                    .await;
                // Keep a small tail of heartbeat history so the loop stays bounded
                // without losing all short-term context between runs.
                let mut manager = session_manager.lock().unwrap_or_else(|e| e.into_inner());
                let session = {
                    let session = manager.get_or_create_session("heartbeat");
                    session.retain_recent_legal_suffix(
                        config.gateway.heartbeat.keep_recent_messages as usize,
                    );
                    session.clone()
                };
                let _ = manager.save(session);
                resp.map(|r| r.content).unwrap_or_default()
            }) as Pin<Box<dyn Future<Output = String> + Send>>
        }
    };

    let on_heartbeat_notify = {
        let bus = Arc::clone(&agent_loop).bus();
        let session_manager = Arc::clone(&session_manager);
        let channels = Arc::clone(&channels);
        move |response: &str| {
            let response = response.to_string();
            let bus = Arc::clone(&bus);
            let session_manager = Arc::clone(&session_manager);
            let channels = Arc::clone(&channels);
            Box::pin(async move {
                let (channel, chat_id) = pick_heartbeat_target(channels, session_manager).await;
                if channel == "cli" {
                    return Ok(());
                }
                let outbound = OutboundMessage {
                    channel,
                    chat_id,
                    content: response,
                    reply_to: None,
                    media: vec![],
                    metadata: HashMap::new(),
                    event: None,
                };
                bus.publish_outbound(outbound).map_err(|e| e.to_string())
            }) as Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
        }
    };

    let hb_cfg = &config.gateway.heartbeat;
    let heartbeat = Arc::new(HeartbeatService::new(
        workspace.clone(),
        agent_loop.provider(),
        agent_loop.model(),
        Some(Arc::new(on_heartbeat_execute)),
        Some(Arc::new(on_heartbeat_notify)),
        hb_cfg.interval_s,
        hb_cfg.enabled,
        Some(config.agents.timezone.clone()),
    ));

    let green = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
    let yellow = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));

    let gateway_host = host_override.unwrap_or_else(|| config.gateway.host.clone());
    let gateway_port = port_override.unwrap_or(config.gateway.port);
    let web_root = resolve_web_root(web_root_override, config.gateway.web_root.clone());
    if let Some(web_root) = &web_root {
        if !web_root.exists() {
            std::fs::create_dir_all(web_root).map_err(CliError::FailedToCreateWebRootDirectory)?;
        }
    }

    let enabled = channels.get_enabled_channels();
    if !enabled.is_empty() {
        println!(
            "{}✓{} Channels enabled: {}",
            green.render(),
            green.render_reset(),
            enabled.join(", ")
        );
    } else {
        println!(
            "{}Warning: No channels enabled{}",
            yellow.render(),
            yellow.render_reset()
        );
    }

    // Python: console.print(f"[green]✓[/green] Cron: {cron_status['jobs']} scheduled jobs")
    if let Some(cron) = &cron {
        let cron_status = cron.status().await;
        if cron_status.jobs > 0 {
            println!(
                "{}✓{} Cron: {} scheduled jobs",
                green.render(),
                green.render_reset(),
                cron_status.jobs
            );
        }
    }

    if hb_cfg.enabled {
        println!(
            "{}✓{} Heartbeat: every {}s",
            green.render(),
            green.render_reset(),
            hb_cfg.interval_s
        );
    } else {
        println!(
            "{}✗{} Heartbeat: disabled",
            yellow.render(),
            yellow.render_reset()
        );
    }

    // Register Dream system job (always-on, idempotent on restart)
    // Note: dream.model_override is already applied in AgentLoop::new.

    // Register Cron system job (always-on, idempotent on restart)
    let mut cron_option: Option<Arc<CronService>> = None;
    if let Some(cron) = &cron {
        cron_option = Some(Arc::clone(cron));
        let dream_cfg = agent_loop.config.agents.dream.clone();
        let timezone = agent_loop.config.agents.timezone.clone();
        let now = now_ms();
        let schedule = dream_cfg.build_schedule(&timezone);
        cron.register_system_job(crate::cron::types::CronJob {
            id: "dream".to_string(),
            name: "dream".to_string(),
            enabled: true,
            schedule: schedule.clone(),
            payload: CronPayload {
                kind: CronPayloadKind::SystemEvent,
                ..Default::default()
            },
            created_at_ms: now,
            updated_at_ms: now,
            delete_after_run: false,
            state: CronJobState {
                next_run_at_ms: compute_next_run(&schedule, now),
                ..Default::default()
            },
        })
        .await;
        println!(
            "{}✓{} Dream: {}",
            green.render(),
            green.render_reset(),
            dream_cfg.describe_schedule()
        );
    }

    // Combined login + WebSocket REST server (only when channels.websocket is present).
    if ws_channel.is_some() {
        let display_host = replace_host(&gateway_host);
        let base_url = format!("http://{display_host}:{gateway_port}");
        println!(
            "{}✓{} Gateway: {}",
            green.render(),
            green.render_reset(),
            base_url
        );
        println!(
            "{}✓{} Swagger UI: {}/swagger-ui",
            green.render(),
            green.render_reset(),
            base_url
        );
        match &web_root {
            Some(root) if web_root_has_ui(root) => println!(
                "{}✓{} Web UI: {}/ (serving `{}`)",
                green.render(),
                green.render_reset(),
                base_url,
                root.display()
            ),
            Some(root) if root.is_dir() => println!(
                "{}Warning: gateway.webRoot / --web-root '{}' has no index.html; web UI disabled{}",
                yellow.render(),
                root.display(),
                yellow.render_reset()
            ),
            Some(root) => println!(
                "{}Warning: gateway.webRoot / --web-root '{}' is not a directory; web UI disabled{}",
                yellow.render(),
                root.display(),
                yellow.render_reset()
            ),
            None => println!(
                "{}✗{} Web UI: disabled (pass --web-root <dir> or set gateway.webRoot)",
                yellow.render(),
                yellow.render_reset()
            ),
        }
    }

    // Python: async def run() / try / gather / finally shutdown
    let red = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));

    if let Some(cron) = &cron_option {
        cron.start().await;
    }
    heartbeat.start().await;

    let mut gather = tokio::spawn({
        let agent_loop = Arc::clone(&agent_loop);
        let channels = Arc::clone(&channels);
        let config = Arc::clone(&config);
        let ws_channel = ws_channel.clone();
        let web_root = web_root.clone();
        async move {
            // A no-op, never-resolving branch when no `"websocket"` channel
            // was declared — keeps this a fixed 3-way join (so the
            // select!/abort shutdown handling below doesn't change shape)
            // without binding a port nobody asked for.
            let maybe_serve = async move {
                match &ws_channel {
                    Some(ws_channel) => {
                        if let Err(err) = serve_combined_login_and_gateway(
                            &config,
                            ws_channel,
                            &gateway_host,
                            gateway_port,
                            web_root.as_deref(),
                        )
                        .await
                        {
                            log::error!("Combined login + WebSocket gateway server failed: {err}");
                        }
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::join!(agent_loop.run(), channels.start_all(), maybe_serve);
        }
    });

    tokio::select! {
        result = &mut gather => {
            if let Err(join_err) = result {
                println!(
                    "\n{}Error: Gateway crashed unexpectedly{}",
                    red.render(),
                    red.render_reset()
                );
                println!("{join_err}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down...");
            gather.abort();
            let _ = gather.await;
        }
    }

    agent_loop.close_mcp().await;
    heartbeat.stop().await;
    if let Some(cron) = &cron_option {
        cron.stop().await;
    }
    agent_loop.stop();
    channels.stop_all().await;

    Ok(())
}

fn replace_host(host: &str) -> String {
    let mut host = if host.is_empty() {
        "0.0.0.0".to_string()
    } else {
        host.to_string()
    };
    if host == "0.0.0.0" {
        host = "localhost".to_string();
    }
    host
}

fn render_api_startup_message(
    host: &str,
    port: u16,
    model_name: &str,
    workspace: &PathBuf,
    session_id: &str,
    timeout: &u64,
    web_root: Option<&std::path::Path>,
) {
    print_markdown(&format!("{} Starting OpenAI-compatible API server", LOGO));
    println!();
    let host = replace_host(host);
    print_markdown(&format!(
        "Endpoint: **http://{host}:{port}/v1/chat/completions**"
    ));
    print_markdown(&format!("Swagger UI: **http://{host}:{port}/swagger-ui**"));
    match web_root {
        Some(path) => print_markdown(&format!(
            "Web UI: **http://{host}:{port}/** (serving `{}`)",
            path.display()
        )),
        None => print_markdown(
            "Web UI: **disabled** (pass `--web-root <dir>` or set `api.webRoot` to serve web-chat)",
        ),
    }
    print_markdown(&format!("Model: **{}**", model_name));
    print_markdown(&format!("Workspace: **{}**", workspace.display()));
    print_markdown(&format!("Session: **{}**", session_id));
    print_markdown(&format!("Timeout: **{timeout} seconds**"));
    if is_all_interfaces_host(&host) {
        print_warning(
            "API is bound to all interfaces. Only do this behind a trusted network boundary, firewall, or reverse proxy.",
        );
    }
}

/// Consume inbound system messages (e.g. subagent announcements) and print responses.
fn spawn_system_message_listener(
    agent_loop: Arc<AgentLoop>,
    markdown: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let bus = agent_loop.bus();
        while let Some(msg) = bus.consume_inbound().await {
            if let Some(response) = handle_cli_system_message(Arc::clone(&agent_loop), msg).await {
                print_agent_response_with_header(
                    &response.content,
                    markdown,
                    Some(&response.metadata),
                    true,
                );
            }
        }
    })
}

/// Consume async outbound messages (e.g. `/dream` completion) and print them.
fn spawn_outbound_message_listener(
    agent_loop: Arc<AgentLoop>,
    markdown: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let bus = agent_loop.bus();
        while let Some(msg) = bus.consume_outbound().await {
            if !msg.channel.eq_ignore_ascii_case("cli") {
                continue;
            }
            if is_internal_outbound(&msg) {
                continue;
            }
            print_agent_response_with_header(&msg.content, markdown, Some(&msg.metadata), true);
        }
    })
}

/// Skip progress/stream control messages that gateway mode routes via outbound.
fn is_internal_outbound(msg: &OutboundMessage) -> bool {
    let meta = &msg.metadata;
    meta.get("_progress").and_then(|v| v.as_bool()) == Some(true)
        || meta.get("_stream_delta").and_then(|v| v.as_bool()) == Some(true)
        || meta.get("_stream_end").and_then(|v| v.as_bool()) == Some(true)
        || (msg.content.is_empty()
            && (meta.contains_key("_stream_end") || meta.contains_key("_stream_delta")))
}

async fn handle_cli_system_message(
    agent_loop: Arc<AgentLoop>,
    msg: InboundMessage,
) -> Option<OutboundMessage> {
    if !msg.channel.eq_ignore_ascii_case("system") {
        log::warn!(
            "Ignoring non-system inbound message in CLI listener: channel={}",
            msg.channel
        );
        return None;
    }
    agent_loop.process_system_message(msg).await
}

async fn message_session(
    message: &str,
    media: Vec<String>,
    markdown: bool,
    channels_config: &ChannelsConfig,
    session_id: &str,
    agent_loop: Arc<AgentLoop>,
    stream: bool,
) -> Result<(), CliError> {
    log::info!("message={message}");
    for media_path in &media {
        log::info!("media={media_path}");
    }
    let renderer: Arc<Mutex<StreamRenderer>> =
        Arc::new(Mutex::new(StreamRenderer::new(markdown, true)));
    let on_progress = create_on_progress(channels_config.clone(), Arc::clone(&renderer));
    let (on_stream, on_stream_end) = stream_callbacks(Arc::clone(&renderer));
    // Esc cancels the in-flight turn instead of exiting the process (unlike
    // Ctrl+C, which `cmd.exe` intercepts as a batch-job kill when launched via
    // scripts/start_rust_bot.bat). Losing the race just drops `process_direct`,
    // which is already the agent loop's intended cancellation path.
    let response = tokio::select! {
        response = agent_loop.process_direct(
            &message,
            Some(&session_id),
            None,
            None,
            Some(media),
            Some(on_progress),
            if stream { Some(on_stream) } else { None },
            if stream { Some(on_stream_end) } else { None },
        ) => response,
        _ = wait_for_escape_cancel() => {
            renderer.lock().await.close().await;
            println!("Cancelled.");
            return Ok(());
        }
    };
    let (streamed, header_printed) = {
        let locked_renderer = renderer.lock().await;
        (locked_renderer.streamed, locked_renderer.header_printed)
    }; // guard dropped here — lock is free before the next acquire
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
fn load_runtime_config(config: PathBuf, workspace: Option<PathBuf>) -> Config {
    if !config.exists() {
        eprint_error(format!("Config file not found: {}", config.display()));
        exit_codes::exit(GENERAL_ERROR);
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
            exit_codes::exit(GENERAL_ERROR);
        }
    }
}

/// Build the process-wide startup provider from `config.agents.model`/`.provider`.
///
/// Thin wrapper around [`create_provider_for`], which holds the actual
/// provider-selection logic shared with [`ModelRuntimeResolver`]
/// (`agent::model_runtime`) so named model presets resolve identically.
fn create_provider(config: &Config) -> Arc<dyn LLMProviderDyn> {
    let model = config.agents.model.clone();
    let provider_name = config.agents.provider.clone();
    log::info!("Provider Name: {:?}", provider_name);
    log::info!("Model: {:?}", model);
    create_provider_for(config, &model, &provider_name).unwrap_or_else(|e| {
        eprint_error(e);
        exit_codes::exit(INVALID_PROVIDER);
    })
}

fn extract_images(
    line: &str,
    renderer: &mut StreamRenderer,
    image_captures: &[String],
) -> (String, Vec<String>) {
    let mut media: Vec<String> = Vec::new();

    for caps in IMAGE_PASTE_COMMAND_REGEX.captures_iter(line) {
        let idx = caps[1].parse::<usize>().unwrap_or(usize::MAX);
        if let Some(path) = image_captures.get(idx) {
            media.push(path.clone());
            let filename = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path.as_str());
            print_cli_progress_line(
                renderer,
                &format!("Image attached from clipboard ({filename})"),
                ProgressType::Image,
            );
        }
    }

    let text = IMAGE_PASTE_COMMAND_REGEX
        .replace_all(line, "")
        .trim()
        .to_string();
    (text, media)
}

fn replace_text_sentinels(line: &str, captures: &[String]) -> String {
    TEXT_PASTE_SENTINEL_REGEX
        .replace_all(line, |caps: &regex::Captures<'_>| {
            let idx = caps[1].parse::<usize>().unwrap_or(usize::MAX);
            captures.get(idx).map(String::as_str).unwrap_or("")
        })
        .trim()
        .to_string()
}

/// Interactive CLI prompt: `rust-bot$ `.
struct CliPrompt;

impl Prompt for CliPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let path = std::env::current_dir();
        match path {
            Ok(path) => Cow::Owned(format!("{}", path.to_string_lossy())),
            Err(_) => Cow::Borrowed("rust-bot"),
        }
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("$ ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("::: ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({}reverse-search: {}) ",
            prefix, history_search.term
        ))
    }
}

async fn interactive_session(
    agent_loop: Arc<AgentLoop>,
    markdown: bool,
    channels_config: &ChannelsConfig,
    session_id: &str,
) -> Result<(), CliError> {
    let welcome = interactive_welcome_text(markdown);
    print_agent_response_with_header(&welcome, markdown, None, true);
    let text_captures: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let image_captures: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let mut line_editor = init_prompt_session(text_captures.clone());
    let prompt = CliPrompt;
    loop {
        let sig = tokio::task::block_in_place(|| line_editor.read_line(&prompt))
            .map_err(|_| CliError::InteractiveNotImplemented)?;
        match sig {
            Signal::HostCommand(cmd) if cmd == TEXT_PASTE_COMMAND => {
                let captured = try_get_clipboard_text().unwrap_or_default();
                let insert = prepare_text_paste_insert(
                    &mut text_captures.lock().expect("text captures lock"),
                    captured,
                );
                line_editor.run_edit_commands(&[EditCommand::InsertString(insert)]);
                continue;
            }
            Signal::HostCommand(cmd) if cmd == IMAGE_PASTE_COMMAND => {
                log::info!("IMAGE_PASTE_COMMAND");
                if let Some(captured) = try_get_clipboard_image() {
                    let insert = prepare_image_paste_insert(
                        &mut image_captures.lock().expect("image captures lock"),
                        captured.path.to_string_lossy().into_owned(),
                    );
                    line_editor.run_edit_commands(&[EditCommand::InsertString(insert)]);
                } else {
                    let mut renderer = StreamRenderer::new(markdown, false);
                    print_cli_progress_line(
                        &mut renderer,
                        "No image found in clipboard",
                        ProgressType::Image,
                    );
                }
                continue;
            }
            Signal::Success(line) => {
                // No spinner here: this renderer only prints clipboard-attach
                // progress lines. message_session owns the "thinking" spinner,
                // so starting one here would duplicate it.
                let mut renderer = StreamRenderer::new(markdown, false);
                let (text, media) = extract_images(
                    line.trim_end(),
                    &mut renderer,
                    &image_captures.lock().expect("image captures lock"),
                );
                let text = replace_text_sentinels(
                    text.as_str(),
                    &text_captures.lock().expect("text captures lock"),
                );
                text_captures.lock().expect("text captures lock").clear();
                image_captures.lock().expect("image captures lock").clear();
                if text.trim().is_empty() && media.is_empty() {
                    continue;
                }
                if text.trim().eq_ignore_ascii_case("exit")
                    || text.trim().eq_ignore_ascii_case("quit")
                {
                    break;
                }
                let send_result: Result<(), CliError> = message_session(
                    text.as_str(),
                    media.clone(),
                    markdown,
                    channels_config,
                    session_id,
                    Arc::clone(&agent_loop),
                    !markdown,
                )
                .await;
                for media_path in media {
                    if let Err(err) = fs::remove_file(&media_path) {
                        log::debug!(
                            "Failed to delete temporary clipboard image {media_path}: {err}"
                        );
                    }
                }
                send_result?;
            }
            Signal::CtrlC => {
                continue;
            }
            Signal::CtrlD => break,
            _ => continue,
        }
    }

    Ok(())
}

fn interactive_welcome_text(markdown: bool) -> String {
    if markdown {
        format!(
            "{LOGO} Interactive mode \n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            "type **exit** or **Ctrl+D** to quit",
            "**Esc** to cancel the current turn",
            "**Ctrl+O** for a new line",
            "**Ctrl+W** or **Alt+Backspace** to delete word",
            "**Alt+I** or **Ctrl+Tab** to paste image",
            "**Ctrl+V** or **Alt+V** to paste text",
            "Type **/help** for available commands",
        )
    } else {
        format!(
            "{LOGO} Interactive mode \n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            "type exit or Ctrl+D to quit",
            "Esc to cancel the current turn",
            "Ctrl+O for a new line",
            "Ctrl+W or Alt+Backspace to delete word",
            "Alt+I or Ctrl+Tab to paste image",
            "Ctrl+V or Alt+V to paste text",
            "Type /help for available commands",
        )
    }
}

fn interactive_keybindings() -> Keybindings {
    let mut kb = default_emacs_keybindings();
    // Ctrl+I is ASCII Tab (0x09); terminals emit KeyCode::Tab, not Control+Char('i').
    kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Tab,
        ReedlineEvent::ExecuteHostCommand(IMAGE_PASTE_COMMAND.to_string()),
    );
    kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Char('i'),
        ReedlineEvent::ExecuteHostCommand(IMAGE_PASTE_COMMAND.to_string()),
    );
    kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Char('v'),
        ReedlineEvent::ExecuteHostCommand(TEXT_PASTE_COMMAND.to_string()),
    );
    // On Windows, conhost/Windows Terminal often inject clipboard text directly
    // instead of sending Event::Paste (crossterm 0.29 lacks Windows bracketed-paste
    // parsing). This binding handles Ctrl+V when the terminal forwards the key event.
    kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('v'),
        ReedlineEvent::ExecuteHostCommand(TEXT_PASTE_COMMAND.to_string()),
    );
    // Newline without submitting. Ctrl/Shift/Alt+Enter cannot be used reliably:
    // with ENABLE_VIRTUAL_TERMINAL_INPUT (needed for bracketed paste), Windows
    // Terminal reports every modifier+Enter as a bare `\r`, so the modifier is
    // lost before reedline sees it. A Ctrl+<letter> arrives as a real control
    // byte (0x0F for Ctrl+O), which survives VT input and is parsed as
    // Char('o') + CONTROL on every platform.
    kb.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('o'),
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    // Also honor Alt+Enter / Shift+Enter (reedline defaults) for terminals that
    // do disambiguate them (e.g. kitty-protocol-capable emulators on Unix).
    kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    kb.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    kb
}

fn build_reedline(
    history: Option<FileBackedHistory>,
    text_captures: Arc<StdMutex<Vec<String>>>,
) -> Reedline {
    let mut editor = Reedline::create()
        .use_bracketed_paste(true)
        .with_edit_mode(Box::new(PasteCapturingEmacs::new(
            interactive_keybindings(),
            text_captures,
        )));
    if let Some(history) = history {
        editor = editor.with_history(Box::new(history));
    }
    editor
}

fn init_prompt_session(text_captures: Arc<StdMutex<Vec<String>>>) -> Reedline {
    let history_file = get_cli_history_path();
    if let Some(parent) = history_file.parent() {
        ensure_dir(parent);
    }

    let history_result = FileBackedHistory::with_file(100, history_file);

    match history_result {
        Ok(history) => {
            build_reedline(Some(history), text_captures).use_kitty_keyboard_enhancement(true)
        }
        Err(e) => {
            log::warn!("Failed to read history file: {}", e);
            build_reedline(None, text_captures)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::clipboard::{format_image_paste_sentinel, format_text_paste_sentinel};

    #[test]
    fn extract_images_resolves_captures_by_index_and_strips_sentinels() {
        let img0 = "/tmp/paste-0.png".to_string();
        let img1 = "/tmp/paste-1.png".to_string();
        let captures = vec![img0.clone(), img1.clone()];
        let line = format!(
            "look at {} and {}",
            format_image_paste_sentinel(1),
            format_image_paste_sentinel(0),
        );
        let mut renderer = StreamRenderer::new(false, true);
        let (text, media) = extract_images(&line, &mut renderer, &captures);
        assert_eq!(text, "look at  and");
        assert_eq!(media, vec![img1, img0]);
    }

    #[test]
    fn extract_images_ignores_unknown_index() {
        let captures = vec!["/tmp/paste-0.png".to_string()];
        let line = format!("a {} b", format_image_paste_sentinel(3));
        let mut renderer = StreamRenderer::new(false, true);
        let (text, media) = extract_images(&line, &mut renderer, &captures);
        assert_eq!(text, "a  b");
        assert!(media.is_empty());
    }

    #[test]
    fn replace_text_sentinels_substitutes_captures_by_index() {
        let line = format!(
            "first {} then {} end",
            format_text_paste_sentinel(1, 1),
            format_text_paste_sentinel(0, 1),
        );
        let captures = vec!["alpha".to_string(), "beta".to_string()];
        let text = replace_text_sentinels(&line, &captures);
        assert_eq!(text, "first beta then alpha end");
    }

    #[test]
    fn replace_text_sentinels_drops_unknown_index_without_capture() {
        let line = format!(
            "a {} b {}",
            format_text_paste_sentinel(0, 1),
            format_text_paste_sentinel(3, 5),
        );
        let captures = vec!["one".to_string()];
        let text = replace_text_sentinels(&line, &captures);
        assert_eq!(text, "a one b");
    }

    #[test]
    fn replace_text_sentinels_ignores_line_count_metadata() {
        let line = format!("a {} b", format_text_paste_sentinel(0, 99));
        let captures = vec!["content".to_string()];
        let text = replace_text_sentinels(&line, &captures);
        assert_eq!(text, "a content b");
    }

    // --- resolve_websocket_channel ---
    // Covers the three declaration states this plan turn was specifically
    // about: key absent (never start), key present but disabled (never
    // start), key present and enabled (start).

    fn test_bus() -> Arc<MessageBus> {
        Arc::new(MessageBus::new())
    }

    fn test_session_manager() -> Arc<StdMutex<SessionManager>> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(StdMutex::new(SessionManager::new(dir.keep())))
    }

    fn test_workspace_request_handler() -> WorkspaceRequestHandler {
        WorkspaceRequestHandler::new(tempfile::tempdir().unwrap().keep(), true)
    }

    #[test]
    fn resolve_websocket_channel_is_none_when_key_absent() {
        let config = Config::default();
        assert!(config.channels.extra.get("websocket").is_none());

        let resolved = resolve_websocket_channel(
            &config,
            test_bus(),
            test_session_manager(),
            test_workspace_request_handler(),
        );

        assert!(
            resolved.is_none(),
            "a config that never mentions \"websocket\" must not start the combined server, \
             even though WebSocketConfig::default().enabled is true"
        );
    }

    #[test]
    fn resolve_websocket_channel_is_none_when_present_but_disabled() {
        let mut config = Config::default();
        config.channels.extra.insert(
            "websocket".to_string(),
            serde_json::json!({"enabled": false}),
        );

        let resolved = resolve_websocket_channel(
            &config,
            test_bus(),
            test_session_manager(),
            test_workspace_request_handler(),
        );

        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_websocket_channel_is_some_when_present_and_enabled() {
        let mut config = Config::default();
        config.channels.extra.insert(
            "websocket".to_string(),
            serde_json::json!({"enabled": true}),
        );

        let resolved = resolve_websocket_channel(
            &config,
            test_bus(),
            test_session_manager(),
            test_workspace_request_handler(),
        );

        assert!(resolved.is_some());
    }

    #[test]
    fn resolve_websocket_channel_is_none_when_malformed() {
        let mut config = Config::default();
        config.channels.extra.insert(
            "websocket".to_string(),
            serde_json::json!({"port": "not-a-number"}),
        );

        let resolved = resolve_websocket_channel(
            &config,
            test_bus(),
            test_session_manager(),
            test_workspace_request_handler(),
        );

        assert!(resolved.is_none());
    }

    // --- resolve_web_root ---
    // Shared by `run_api` (CLI `--web-root` / `api.webRoot`) and `run_gateway`
    // (CLI `--web-root` / `gateway.webRoot`) — generic over the source config
    // field, so these tests exercise the function directly rather than
    // duplicating them per caller.

    #[test]
    fn resolve_web_root_cli_override_wins_over_config() {
        let resolved = resolve_web_root(
            Some(PathBuf::from("/from/cli")),
            Some("/from/config".to_string()),
        );
        assert_eq!(resolved, Some(PathBuf::from("/from/cli")));
    }

    #[test]
    fn resolve_web_root_falls_back_to_config_when_no_cli_override() {
        let resolved = resolve_web_root(None, Some("/from/config".to_string()));
        assert_eq!(resolved, Some(PathBuf::from("/from/config")));
    }

    #[test]
    fn resolve_web_root_is_none_when_neither_given_and_default_dir_absent() {
        // `./web` relative to the test process's cwd (the crate root) is not
        // expected to exist; if it ever does, this test's assumption (and
        // the "no UI" fallback behavior it guards) would need revisiting.
        let default_dir_exists = PathBuf::from("./web").exists();
        let resolved = resolve_web_root(None, None);
        if default_dir_exists {
            assert_eq!(resolved, Some(PathBuf::from("./web")));
        } else {
            assert_eq!(resolved, None);
        }
    }

    #[test]
    fn gateway_args_parses_web_root_flag() {
        let args = GatewayArgs::try_parse_from([
            "rust-bot",
            "--config",
            "config.json",
            "--web-root",
            "./websockets-chat-web",
        ])
        .unwrap();
        assert_eq!(args.web_root, Some(PathBuf::from("./websockets-chat-web")));
    }

    #[test]
    fn gateway_args_web_root_defaults_to_none() {
        let args = GatewayArgs::try_parse_from(["rust-bot", "--config", "config.json"]).unwrap();
        assert_eq!(args.web_root, None);
    }

    // --- web_root_has_ui ---

    #[test]
    fn web_root_has_ui_is_false_for_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!web_root_has_ui(dir.path()));
    }

    #[test]
    fn web_root_has_ui_is_false_when_other_files_exist_but_index_html_does_not() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("readme.txt"), "not a bundle").unwrap();
        assert!(!web_root_has_ui(dir.path()));
    }

    #[test]
    fn web_root_has_ui_is_true_when_index_html_is_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.html"), "<html></html>").unwrap();
        assert!(web_root_has_ui(dir.path()));
    }

    #[test]
    fn web_root_has_ui_is_false_when_path_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, "x").unwrap();
        assert!(!web_root_has_ui(&file));
    }

    /// `purpose = "webui"`, no `--aud` override, no existing `websocket`
    /// entry in `channels.extra` yet: falls back to
    /// `WebSocketConfig::default().path`, matching what `run_generate_token`
    /// is about to write into a freshly created entry.
    #[test]
    fn webui_purpose_with_no_existing_websocket_config_uses_default_path() {
        let aud = resolve_aud(None, Some("webui"), "https://api.example.com", None);
        assert_eq!(aud, WebSocketConfig::default().path);
    }

    /// `purpose = "webui"`, no `--aud` override, an existing `websocket`
    /// entry whose `jwt.aud` is already set: that existing `jwt.aud` wins,
    /// not the default path.
    #[test]
    fn webui_purpose_with_existing_jwt_aud_uses_that_aud() {
        let existing = WebSocketConfig {
            path: "/ws".to_string(),
            jwt: crate::config::schema::JwtConfig {
                aud: "/ws".to_string(),
                ..Default::default()
            },
            ..WebSocketConfig::default()
        };

        let aud = resolve_aud(
            None,
            Some("webui"),
            "https://api.example.com",
            Some(&existing),
        );
        assert_eq!(aud, "/ws");
    }

    /// `purpose = "webui"`, no `--aud` override, an existing `websocket`
    /// entry whose `path` was customized but `jwt.aud` is left empty: falls
    /// back to that existing entry's `path`, not the global default.
    #[test]
    fn webui_purpose_with_existing_config_and_empty_jwt_aud_uses_existing_path() {
        let existing = WebSocketConfig {
            path: "/custom-ws".to_string(),
            jwt: crate::config::schema::JwtConfig {
                aud: String::new(),
                ..Default::default()
            },
            ..WebSocketConfig::default()
        };
        assert!(existing.jwt.aud.trim().is_empty(), "test assumes empty aud");

        let aud = resolve_aud(
            None,
            Some("webui"),
            "https://api.example.com",
            Some(&existing),
        );
        assert_eq!(aud, "/custom-ws");
    }

    /// An explicit `--aud` override always wins, even for `purpose = "webui"`
    /// and even when an existing `websocket` config entry would otherwise
    /// resolve to a different audience.
    #[test]
    fn explicit_override_wins_even_for_webui_purpose() {
        let existing = WebSocketConfig {
            path: "/ws".to_string(),
            ..WebSocketConfig::default()
        };

        let aud = resolve_aud(
            Some("explicit-aud"),
            Some("webui"),
            "https://api.example.com",
            Some(&existing),
        );
        assert_eq!(aud, "explicit-aud");
    }

    /// No purpose (or some other purpose) keeps the prior behavior: fall
    /// back to `api.jwt.aud`, regardless of any existing `websocket` config.
    #[test]
    fn non_webui_purpose_falls_back_to_api_jwt_aud() {
        let existing = WebSocketConfig {
            path: "/ws".to_string(),
            ..WebSocketConfig::default()
        };

        assert_eq!(
            resolve_aud(None, None, "https://api.example.com", Some(&existing)),
            "https://api.example.com"
        );
        assert_eq!(
            resolve_aud(
                None,
                Some("some-other-purpose"),
                "https://api.example.com",
                Some(&existing)
            ),
            "https://api.example.com"
        );
    }
}
