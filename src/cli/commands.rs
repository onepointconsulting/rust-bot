use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use crate::bus::events::{InboundMessage, OutboundMessage};

use anstyle::{AnsiColor, Color, Style};
use clap::{Parser, Subcommand};
use futures::lock::Mutex;
use reedline::{
    DefaultPrompt, EditCommand, FileBackedHistory, KeyCode, KeyModifiers, Keybindings,
    Reedline, ReedlineEvent, Signal, default_emacs_keybindings,
};
use serde_json::Value;
use termimad::MadSkin;

use crate::agent::agent_loop::{AgentLoop, ProgressCallback};
use crate::bus::queue::MessageBus;
use crate::cli::paste_edit_mode::{prepare_image_paste_insert, PasteCapturingEmacs, prepare_text_paste_insert};
use crate::cli::stream::{StreamRenderer, stream_callbacks};
use crate::config::loader::{load_config, resolve_config_env_vars, set_config_path};
use crate::config::log::init_runtime_logging;
use crate::config::paths::get_cli_history_path;
use crate::config::schema::{ChannelsConfig, Config};
use crate::cron::CronService;
use crate::providers::anthropic_provider::AnthropicProvider;
use crate::providers::base::{LLMProvider, LLMProviderDyn};
use crate::providers::openai_compat_provider::OpenAICompatProvider;
use crate::utils::clipboard::ClipboardImage;
use crate::utils::clipboard::IMAGE_PASTE_COMMAND_REGEX;
use crate::utils::clipboard::try_get_clipboard_text;
use crate::utils::clipboard::{IMAGE_PASTE_COMMAND, try_get_clipboard_image};
use crate::utils::clipboard::{
    TEXT_PASTE_COMMAND, TEXT_PASTE_SENTINEL_REGEX,
};
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

    let agent_loop = Arc::new(agent_loop);
    // Subagent completions publish system-channel messages to the inbound bus.
    // The gateway handles those in `AgentLoop::run()`; CLI uses `process_direct`
    // instead, so we need a background listener to deliver async results.
    let system_listener = spawn_system_message_listener(Arc::clone(&agent_loop), markdown);

    let result = match args.message {
        Some(message) if !message.is_empty() => {
            message_session(
                &message,
                vec![],
                markdown,
                &config.channels,
                &session_id,
                Arc::clone(&agent_loop),
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
    result
}

/// Consume inbound system messages (e.g. subagent announcements) and print responses.
fn spawn_system_message_listener(
    agent_loop: Arc<AgentLoop>,
    markdown: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let bus = agent_loop.bus();
        while let Some(msg) = bus.consume_inbound().await {
            if let Some(response) =
                handle_cli_system_message(Arc::clone(&agent_loop), msg).await
            {
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
) -> Result<(), CliError> {
    log::info!("message={message}");
    for media_path in &media {
        log::info!("media={media_path}");
    }
    let renderer: Arc<Mutex<StreamRenderer>> =
        Arc::new(Mutex::new(StreamRenderer::new(markdown, true)));
    let on_progress = create_on_progress(channels_config.clone(), Arc::clone(&renderer));
    let (on_stream, on_stream_end) = stream_callbacks(Arc::clone(&renderer));
    let response = agent_loop
        .process_direct(
            &message,
            Some(&session_id),
            None,
            None,
            Some(media),
            Some(on_progress),
            Some(on_stream),
            Some(on_stream_end),
        )
        .await;
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
            Some(config.providers.custom.api_key.clone()),
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

fn create_on_progress(
    channels: ChannelsConfig,
    renderer: Arc<Mutex<StreamRenderer>>,
) -> ProgressCallback {
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

fn extract_paste_sentinel_with<F>(
    line: &str,
    renderer: &mut StreamRenderer,
    mut read_clipboard_image: F,
) -> (String, Vec<String>)
where
    F: FnMut() -> Option<ClipboardImage>,
{
    if !line.contains(IMAGE_PASTE_COMMAND) {
        return (line.to_string(), vec![]);
    }

    let mut media: Vec<String> = Vec::new();
    for _ in 0..line.matches(IMAGE_PASTE_COMMAND).count() {
        if let Some(image) = read_clipboard_image() {
            let filename = image
                .path
                .file_name()
                .and_then(|n: &std::ffi::OsStr| n.to_str())
                .unwrap_or("clipboard-image.png");
            print_cli_progress_line(
                renderer,
                &format!(
                    "Image attached from clipboard ({filename}, {}x{})",
                    image.width, image.height
                ),
            );
            media.push(image.path.to_string_lossy().into_owned());
        } else {
            print_cli_progress_line(renderer, "No image found in clipboard");
        }
    }

    let text = line.replace(IMAGE_PASTE_COMMAND, "").trim().to_string();
    log::info!("text={text}");
    log::info!("Number of images: {}", media.len());
    (text, media)
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
    let prompt = DefaultPrompt::default();
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
                    print_cli_progress_line(&mut renderer, "No image found in clipboard");
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
            "{LOGO} Interactive mode ({}; {}; {}; {})\n",
            "type **exit** or **Ctrl+D** to quit",
            "**Ctrl+Enter** for a new line",
            "**Alt+I** or **Ctrl+Tab** to paste image",
            "**Ctrl+V** or **Alt+V** to paste text",
        )
    } else {
        format!(
            "{LOGO} Interactive mode ({}; {}; {}; {})\n",
            "type exit or Ctrl+D to quit",
            "Ctrl+Enter for a new line",
            "Alt+I or Ctrl+Tab to paste image",
            "Ctrl+V or Alt+V to paste text",
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
    kb.add_binding(
        KeyModifiers::CONTROL,
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
        Ok(history) => build_reedline(Some(history), text_captures),
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
    fn extract_paste_sentinel_strips_sentinel_and_collects_media() {
        let temp = tempfile::tempdir().expect("tempdir");
        let img_path = temp.path().join("paste.png");
        let mut renderer = StreamRenderer::new(false, true);
        let line = format!("describe{}", IMAGE_PASTE_COMMAND);
        let mut responses = vec![Some(ClipboardImage {
            path: img_path.clone(),
            width: 640,
            height: 480,
        })]
        .into_iter();
        let (text, media) =
            extract_paste_sentinel_with(&line, &mut renderer, || responses.next().unwrap_or(None));

        assert_eq!(text, "describe");
        assert_eq!(media, vec![img_path.to_string_lossy().into_owned()]);
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
}
