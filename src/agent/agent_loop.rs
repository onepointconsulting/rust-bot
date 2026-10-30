use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use tera::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::agent::autocompact::Autocompact;
use crate::agent::background_activity::ActivityKind;
use crate::agent::checkpoint_payload;
use crate::agent::circuit_breaker::CIRCUIT_BREAKER_STOP_REASON;
use crate::agent::context::{ContextBuilder, DEFAULT_CURRENT_ROLE};
use crate::agent::crash_points::crash_point;
use crate::agent::hook::{AgentHook, AgentHookContext, LoopHookChain, ToolHookDecision};
use crate::agent::memory::MessageBuilder;
use crate::agent::memory::{Consolidator, Dream};
use crate::agent::model_runtime::{
    ModelRuntime, ModelRuntimeResolver, SESSION_MODEL_PRESET_METADATA_KEY,
};
use crate::agent::modes::{AgentMode, RESERVED_AGENT_MODE_NAME, SESSION_AGENT_MODE_METADATA_KEY};
use crate::agent::recovery::{self, ToolRecovery, UnfinishedTurn};
use crate::agent::resume::{self, ResumeAction, ResumePolicy};
use crate::agent::runner::{AgentRunResult, AgentRunSpec, AgentRunner};
use crate::agent::subagent::SubagentManager;
use crate::agent::tool_call_context::{ToolCallIdentity, with_tool_call};
use crate::agent::tool_progress::with_tool_progress;
use crate::agent::tools::acp::{AcpToolsContext, register_acp_tools};
use crate::agent::tools::cron::CronTool;
use crate::agent::tools::filesystem::FsToolConfig;
use crate::agent::tools::goal::UpdateGoalTool;
use crate::agent::tools::mcp::mcp_file_ref::FileRefResolver;
use crate::agent::tools::mcp::{
    LoadedMcpTools, McpConnectBatch, connect_mcp_servers_independently, is_mcp_tool_enabled,
    unmatched_enabled_tools,
};
use crate::agent::tools::message::MessageTool;
use crate::agent::tools::registry::ToolRegistry;
use crate::agent::tools::shell::ShellTool;
use crate::agent::tools::spawn::SpawnTool;
use crate::agent::tools::tool_search::{ToolSearchTool, deferred_tools_prompt_section};
use crate::agent::turn_journal::TurnJournal;
use crate::agent::workspace_context::{
    bind_workspace_scope, reset_workspace_scope, with_workspace_scope_stack,
};
use crate::bus::events::{InboundMessage, OutboundMessage};
use crate::bus::outbound_events::{
    OutboundEvent, ProgressEvent, ProgressKind, StreamDeltaEvent, StreamEndEvent,
    StreamedResponseEvent, TurnEndEvent, outbound_message_for_event,
};
use crate::bus::queue::MessageBus;
use crate::channels::websocket::webui::metadata::WEBUI_SYSTEM_TURN_PREFIX;
use crate::command::CommandContext;
use crate::command::types::ChatCommand;
use crate::command::{CommandRouter, builtin::register_builtin_commands};
use crate::config::schema::{
    ChannelsConfig, Config, DocxToolConfig, DurabilityConfig, ExecToolConfig, GmailToolConfig,
    ImageGenerationToolConfig, MCP_ALL_TOOLS, McpServerConfig, OcrToolConfig, QuestionToolConfig,
    RESERVED_MODEL_PRESET_NAME, WebToolsConfig,
};
use crate::cron::CronService;
use crate::providers::base::{LLMProviderDyn, LLMUsage};
use crate::runtime_context::{RUNTIME_CONTEXT_TAG, strip_leaked_runtime_context};
use crate::security::workspace_access::{
    WORKSPACE_SCOPE_METADATA_KEY, WorkspaceAccessMode, WorkspaceScope, WorkspaceScopeError,
    WorkspaceScopeResolver, validate_workspace_scope_payload,
};
use crate::security::workspace_requests::WorkspaceRequestHandler;
use crate::session::goal_state;
use crate::session::inbox::{self, InboxLedger, LedgerGuard, RedriveAction};
use crate::session::journal::{
    JournalRecord, JournalStore, RunJournal, ToolCallStatus, TurnOutcome,
};
use crate::session::keys::{
    COMMAND_KEY, HIDDEN_HISTORY_KEY, RUNTIME_CHECKPOINT_KEY, SESSION_WEBUI_METADATA_KEY,
};
use crate::session::manager::{Session, SessionManager};
use crate::utils::helpers::{image_placeholder_text, strip_think, truncate_text};
use crate::utils::registry_helper::{
    filesystem_tool_scope, register_conversion_tools, register_filesystem_tools,
    register_gmail_tools, register_image_generation_tools, register_ocr_tools,
    register_question_tool, register_web_tools,
};
use crate::utils::runtime::EMPTY_FINAL_RESPONSE_MESSAGE;
use crate::utils::tool_hints::format_tool_hints;

const CONTEXT_AWARE_TOOLS: &[&str] = &["message", "spawn", "cron", "update_goal", "question"];

// Match Python's optional async callbacks (`tool_hint=True` keyword in
// Python). The `ProgressKind` discriminant tells sinks what kind of update
// this is — today only `Plain`/`ToolHint` are produced here, but the type
// already covers `Reasoning*` for when that gets wired through this same
// callback.
pub type ProgressCallback =
    Arc<dyn Fn(String, ProgressKind) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub type StreamCallback =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub type StreamEndCallback = Arc<
    dyn Fn(bool) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
    // or: dyn Fn(&AgentHookContext, bool) -> ... if you need ctx like AgentHook
>;

pub struct LoopHook {
    agent_loop: Arc<AgentLoop>,
    on_progress: Option<ProgressCallback>,
    on_stream: Option<StreamCallback>,
    on_stream_end: Option<StreamEndCallback>,
    channel: String,
    chat_id: String,
    message_id: Option<String>,
    stream_buf: Mutex<String>,
}

impl LoopHook {
    pub fn new(
        agent_loop: Arc<AgentLoop>,
        on_progress: Option<ProgressCallback>,
        on_stream: Option<StreamCallback>,
        on_stream_end: Option<StreamEndCallback>,
    ) -> Self {
        Self::with_context(
            agent_loop,
            on_progress,
            on_stream,
            on_stream_end,
            "cli",
            "direct",
            None,
        )
    }

    // Keyword-only Python args (`channel`, `chat_id`, `message_id`) use this constructor.
    pub fn with_context(
        agent_loop: Arc<AgentLoop>,
        on_progress: Option<ProgressCallback>,
        on_stream: Option<StreamCallback>,
        on_stream_end: Option<StreamEndCallback>,
        channel: impl Into<String>,
        chat_id: impl Into<String>,
        message_id: Option<String>,
    ) -> Self {
        Self {
            agent_loop,
            on_progress,
            on_stream,
            on_stream_end,
            channel: channel.into(),
            chat_id: chat_id.into(),
            message_id,
            stream_buf: Mutex::new(String::new()),
        }
    }
}

/// Remove <think>…</think> blocks that some models embed in content.
fn safe_strip_think(text: Option<&str>) -> Option<String> {
    text?;
    let text = strip_think(text.unwrap());
    if text.is_empty() { None } else { Some(text) }
}

/// A final reply as it may be shown and saved: without `<think>` blocks and
/// without a runtime-context block the model copied from its input. `None`
/// when nothing is left, so the runner treats the reply as blank.
fn clean_final_content(text: Option<&str>) -> Option<String> {
    let without_think = safe_strip_think(text)?;
    let without_leak = strip_leaked_runtime_context(&without_think);
    (!without_leak.is_empty()).then(|| without_leak.to_string())
}

/// The role of a sub-agent result in the prompt: always `user`. As an
/// `assistant` turn it was merged into the previous reply, so the model took
/// the result and its instructions for its own words and copied them into
/// later replies.
const SUBAGENT_ANNOUNCE_ROLE: &str = "user";

/// Mark the sub-agent result at `announce_index` of a finished system turn
/// hidden, so it is saved for the model's history but never shown as a chat
/// turn. A no-op when that message is not the `user` result (e.g. it was
/// merged into an unanswered user message).
fn mark_announcement_hidden(messages: &mut [Value], announce_index: usize) {
    if let Some(announcement) = messages.get_mut(announce_index)
        && announcement.get("role").and_then(Value::as_str) == Some(SUBAGENT_ANNOUNCE_ROLE)
        && let Some(fields) = announcement.as_object_mut()
    {
        fields.insert(HIDDEN_HISTORY_KEY.to_string(), Value::Bool(true));
    }
}

/// How long to wait before trying again to connect MCP servers that failed, so a broken
/// server does not make every chat message wait for another failed attempt.
const MCP_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// A session's turn in progress: the per-session lock and the global
/// concurrency permit. Dropping it lets the next turn start.
struct TurnSlot {
    _session_guard: tokio::sync::OwnedMutexGuard<()>,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// Outbound metadata flag that makes the websocket channel record a delivery
/// in the chat's WebUI transcript (the same key `MessageTool` copies).
const WEBUI_METADATA_KEY: &str = "webui";
/// Outbound metadata key naming the WebUI turn a recorded row belongs to.
const WEBUI_TURN_ID_METADATA_KEY: &str = "webui_turn_id";

/// Split a system message's `chat_id` (`channel:chat_id`) into its parts. An id
/// with no channel belongs to the CLI.
fn split_system_chat_id(chat_id: &str) -> (&str, &str) {
    chat_id.split_once(':').unwrap_or(("cli", chat_id))
}

/// Whether `kind` belongs to the model's reasoning stream.
fn is_reasoning_progress(kind: ProgressKind) -> bool {
    matches!(
        kind,
        ProgressKind::Reasoning | ProgressKind::ReasoningDelta | ProgressKind::ReasoningEnd
    )
}

/// The WebUI markers for a turn run for a system message (e.g. a sub-agent's
/// result) on `session`: `webui` and a fresh turn id when the session is a
/// WebUI websocket chat, nothing otherwise. Attached to the turn's replies so
/// the websocket channel persists them, exactly as it does for a user turn.
fn webui_system_turn_metadata(channel: &str, session: &Session) -> HashMap<String, Value> {
    let is_webui_chat = channel == crate::channels::websocket::CHANNEL_NAME
        && session
            .metadata
            .get(SESSION_WEBUI_METADATA_KEY)
            .and_then(Value::as_bool)
            == Some(true);
    if !is_webui_chat {
        return HashMap::new();
    }
    HashMap::from([
        (WEBUI_METADATA_KEY.to_string(), Value::Bool(true)),
        (
            WEBUI_TURN_ID_METADATA_KEY.to_string(),
            Value::String(format!(
                "{WEBUI_SYSTEM_TURN_PREFIX}{}",
                uuid::Uuid::new_v4().simple()
            )),
        ),
    ])
}

/// Which MCP servers are up, which failed (and why), and when we last tried.
#[derive(Default)]
struct McpConnectionState {
    /// Servers with a live session.
    connected_names: HashSet<String>,
    /// Servers whose last connection attempt failed, with the error text.
    failures: BTreeMap<String, String>,
    last_attempt: Option<Instant>,
}

/// The configured MCP servers whose `enabled` flag is on; disabled ones are never
/// connected, counted or reported.
pub(crate) fn enabled_mcp_servers(
    configured: &HashMap<String, McpServerConfig>,
) -> HashMap<String, McpServerConfig> {
    configured
        .iter()
        .filter(|(_, config)| config.enabled)
        .map(|(name, config)| (name.clone(), config.clone()))
        .collect()
}

/// The tools `session` sees in `mode`: `registry` restricted to the mode, with its own
/// loaded-tools handle seeded from the session's `loaded_tools`. Names the view cannot
/// load (hidden by the mode, not registered, no longer deferred) are skipped but stay
/// in the session. Shared by the agent loop and the tool lists, so both agree.
pub(crate) fn session_tool_view(
    registry: &ToolRegistry,
    mode: AgentMode,
    session: Option<&Session>,
) -> ToolRegistry {
    let view = mode.restrict_tools(registry);
    if let Some(session) = session {
        view.load_tools(&session.loaded_tools());
    }
    view
}

/// `mcpServers.<name>.description` of every server that has one, keyed by server name.
fn mcp_server_descriptions(servers: &HashMap<String, McpServerConfig>) -> HashMap<String, String> {
    servers
        .iter()
        .filter(|(_, config)| !config.description.trim().is_empty())
        .map(|(name, config)| (name.clone(), config.description.clone()))
        .collect()
}

/// The names in `tools.deferredTools` that can be deferred: registered built-ins that
/// are not also disabled. Logs a warning for every name it drops; disabled wins over
/// deferred. (`tool_search` is rejected earlier, by config validation.)
pub(crate) fn deferrable_builtins(
    deferred_tools: &[String],
    disabled_tools: &[String],
    builtins: &ToolRegistry,
) -> Vec<String> {
    deferred_tools
        .iter()
        .filter(|name| {
            if disabled_tools.contains(name) {
                log::warn!(
                    "tools.deferredTools: '{name}' is also in tools.disabledTools; it stays disabled"
                );
                false
            } else if !builtins.has(name) {
                log::warn!(
                    "tools.deferredTools: '{name}' is not a built-in tool and is ignored \
                     (defer MCP tools with the server's \"exposure\" setting)"
                );
                false
            } else {
                true
            }
        })
        .cloned()
        .collect()
}

/// The configured servers that still need connecting, sorted by name.
fn pending_mcp_servers(
    configured: &HashMap<String, McpServerConfig>,
    connected: &HashSet<String>,
) -> Vec<(String, McpServerConfig)> {
    let mut pending: Vec<(String, McpServerConfig)> = configured
        .iter()
        .filter(|(name, _)| !connected.contains(*name))
        .map(|(name, config)| (name.clone(), config.clone()))
        .collect();
    pending.sort_by(|a, b| a.0.cmp(&b.0));
    pending
}

/// Whether enough time has passed since `last_attempt` to try connecting again. Always
/// true before the first attempt.
fn mcp_retry_due(last_attempt: Option<Instant>, now: Instant, interval: Duration) -> bool {
    last_attempt.is_none_or(|last| now.saturating_duration_since(last) >= interval)
}

fn mcp_server_endpoint(config: &McpServerConfig) -> String {
    if !config.url.is_empty() {
        config.url.clone()
    } else if !config.command.is_empty() {
        let mut endpoint = config.command.clone();
        if !config.args.is_empty() {
            endpoint.push(' ');
            endpoint.push_str(&config.args.join(" "));
        }
        endpoint
    } else {
        "(unknown)".to_string()
    }
}

#[async_trait]
impl AgentHook for LoopHook {
    fn wants_streaming(&self) -> bool {
        self.on_stream.is_some()
    }

    async fn on_stream(&self, _ctx: &mut AgentHookContext, delta: &str) {
        let incremental = {
            let mut buf = self.stream_buf.lock().unwrap_or_else(|e| e.into_inner());
            let prev_clean = strip_think(&buf);
            buf.push_str(delta);
            let new_clean = strip_think(&buf);
            new_clean.get(prev_clean.len()..).unwrap_or("").to_string()
        };

        if !incremental.is_empty()
            && let Some(on_stream) = &self.on_stream
        {
            on_stream(incremental).await;
        }
    }

    async fn on_stream_end(&self, _ctx: &mut AgentHookContext, resuming: bool) {
        if let Some(on_stream_end) = &self.on_stream_end {
            on_stream_end(resuming).await;
        }
        let mut buf = self.stream_buf.lock().unwrap_or_else(|e| e.into_inner());
        buf.clear();
    }

    async fn on_reasoning_delta(&self, _ctx: &mut AgentHookContext, delta: &str) {
        if let Some(on_progress) = &self.on_progress {
            on_progress(delta.to_string(), ProgressKind::ReasoningDelta).await;
        }
    }

    async fn on_reasoning_end(&self, _ctx: &mut AgentHookContext) {
        if let Some(on_progress) = &self.on_progress {
            on_progress(String::new(), ProgressKind::ReasoningEnd).await;
        }
    }

    async fn before_execute_tools(&self, context: &mut AgentHookContext) -> ToolHookDecision {
        if let Some(on_progress) = &self.on_progress {
            if self.on_stream.is_none() {
                let content: Option<String> = if let Some(response) = context.response.clone()
                    && let Some(response_content) = response.content
                {
                    Some(response_content.clone())
                } else {
                    None
                };
                let thought = safe_strip_think(content.as_deref());
                if let Some(thought) = thought {
                    on_progress(thought, ProgressKind::Plain).await;
                }
            }
            let tool_hint =
                safe_strip_think(Some(format_tool_hints(context.tool_calls.clone()).as_str()));
            if let Some(tool_hint) = tool_hint {
                on_progress(tool_hint, ProgressKind::ToolHint).await;
            }
        }
        for tc in &context.tool_calls {
            let args_str =
                serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".to_string());
            log::info!(
                "Tool call: {}({})",
                tc.name,
                args_str.get(..200).unwrap_or(&args_str)
            );
        }
        self.agent_loop
            .set_tool_context(&self.channel, &self.chat_id, self.message_id.as_deref());
        ToolHookDecision::Continue
    }

    async fn after_iteration(&self, context: &mut AgentHookContext) {
        let prompt = context.usage.prompt_tokens().unwrap_or(0);
        let completion = context.usage.output_tokens.unwrap_or(0);
        let cached = context.usage.cache_read_input_tokens.unwrap_or(0);
        log::debug!("LLM usage: prompt={prompt} completion={completion} cached={cached}");
    }

    fn finalize_content(&self, _ctx: &AgentHookContext, content: Option<String>) -> Option<String> {
        clean_final_content(content.as_deref())
    }
}

///
/// The agent loop is the core processing engine.
/// 1. Receives messages from the bus
/// 2. Builds context with history, memory, skills
/// 3. Calls the LLM
/// 4. Executes tool calls
/// 5. Sends responses back
///
/// Join handles of in-flight tasks, keyed by their unique task id.
type TaskJoinHandles = HashMap<u64, JoinHandle<()>>;

pub struct AgentLoop {
    pub config: Config,
    bus: Arc<MessageBus>,
    workspace: PathBuf,
    /// Owns model-preset selection and resolves the runtime (provider, model,
    /// generation settings) used for each turn. There is no fixed
    /// provider/model captured once at startup: the main loop, subagents,
    /// and Consolidator/Dream all resolve through this shared resolver,
    /// keyed by session (see [`Self::model`]/[`Self::provider`] for the
    /// process-wide default, and `run_agent_loop` for the per-session path).
    pub runtime_resolver: Arc<ModelRuntimeResolver>,
    max_iterations: u32,
    context_block_limit: Option<u32>,
    max_tool_result_chars: u32,
    provider_retry_mode: String,
    pub web_config: WebToolsConfig,
    exec_config: ExecToolConfig,
    pub cron_service: Option<Arc<CronService>>,
    restrict_to_workspace: bool,
    /// Tool names that are never registered (`tools.disabledTools`), including
    /// MCP tools that connect later.
    disabled_tools: Vec<String>,
    /// Resolves the effective per-turn workspace scope (see
    /// `security::workspace_access`), from a session's persisted override
    /// if any, else this loop's fixed `workspace`/`restrict_to_workspace`.
    workspace_scopes: WorkspaceScopeResolver,
    pub session_manager: Arc<Mutex<SessionManager>>,
    mcp_servers: HashMap<String, McpServerConfig>,
    mcp_connected: AtomicBool,
    mcp_connecting: AtomicBool,
    /// Live MCP sessions. Holding these keeps the connections open; dropping
    /// them closes the connections (RAII equivalent of Python's AsyncExitStack).
    mcp_sessions: Mutex<Vec<LoadedMcpTools>>,
    /// Per-server connection outcome. One failing server never affects the others.
    mcp_state: Mutex<McpConnectionState>,
    _channels_config: Option<ChannelsConfig>,
    _timezone: Option<String>,
    pub start_time: SystemTime,
    pub last_usage: Mutex<LLMUsage>,
    extra_hooks: Vec<Arc<dyn AgentHook>>,
    context: Arc<ContextBuilder>,
    pub(crate) tools: Arc<Mutex<ToolRegistry>>,
    pub subagents: Arc<SubagentManager>,
    /// The `acp_*` tools' shared state; `None` when ACP is off or this process
    /// is already as deep as `tools.acp.maxDepth` allows.
    acp_tools: Option<Arc<AcpToolsContext>>,
    /// In-flight per-session tasks, keyed by session then by a unique task id so
    /// each task can remove itself on completion (the `add_done_callback` analog).
    pub active_tasks: Arc<AsyncMutex<HashMap<String, TaskJoinHandles>>>,
    /// Monotonic source of task ids for `active_tasks`.
    next_task_id: AtomicU64,
    background_tasks: Arc<AsyncMutex<TaskJoinHandles>>,
    /// Monotonic source of task ids for `background_tasks`.
    next_background_task_id: AtomicU64,
    session_locks: Arc<AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    /// Crash-recovery settings (`durability` in the config).
    durability: DurabilityConfig,
    /// When this process started (Unix millis). Journal records older than
    /// this belong to a previous process and are what recovery acts on.
    process_started_ms: AtomicI64,
    _max: usize,
    running: AtomicBool,
    concurrency_gate: Option<Arc<Semaphore>>,
    pub consolidator: Arc<Consolidator>,
    pub dream: Arc<Dream>,
    pub auto_compact: Arc<Autocompact>,
    /// Minimum interval between `Autocompact::check_expired` scans, matching
    /// nanobot's `_idle_compact_check_interval_s` (`loop.py:1099-1104`).
    idle_compact_check_interval: Duration,
    commands: CommandRouter,
}

/// Counts of what one startup resume pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResumeSummary {
    /// Turns handed to the resume driver.
    pub resumed: usize,
    /// Turns closed because they ran out of resume attempts (or were superseded).
    pub abandoned: usize,
    /// Turns left alone because they were too old.
    pub stale: usize,
}

impl AgentLoop {
    pub fn new(
        bus: Arc<MessageBus>,
        provider: Arc<dyn LLMProviderDyn>,
        workspace: PathBuf,
        config: Config,
        cron_service: Option<Arc<CronService>>,
        session_manager: Option<Arc<Mutex<SessionManager>>>,
        hooks: Option<Vec<Arc<dyn AgentHook>>>,
    ) -> Self {
        let agents_cfg = config.agents.clone();
        let tools_cfg = config.tools.clone();
        let subagent_config = config.subagent.clone();
        let channels_config = Some(config.channels.clone());
        let timezone = Some(agents_cfg.timezone.clone());

        let web_config = tools_cfg.web.clone();
        let exec_config = tools_cfg.exec.clone();
        let gmail_config = tools_cfg.gmail.clone();
        let ocr_config = tools_cfg.ocr.clone();
        let docx_config = tools_cfg.docx.clone();
        let image_generation_config = tools_cfg.image_generation.clone();
        let question_config = tools_cfg.question.clone();
        let restrict_to_workspace = tools_cfg.restrict_to_workspace;
        let workspace_scopes =
            WorkspaceScopeResolver::new(workspace.clone(), restrict_to_workspace);
        let mcp_servers = enabled_mcp_servers(&tools_cfg.mcp_servers);

        let context_window_tokens = agents_cfg.context_window_tokens;
        let max_tool_result_chars = agents_cfg.max_tool_result_chars;
        let max_iterations = agents_cfg.max_tool_iterations;
        let context_block_limit = agents_cfg.context_block_limit;
        let provider_retry_mode = agents_cfg.provider_retry_mode;

        let max = std::env::var("RUST_BOT_MAX_CONCURRENT_REQUESTS")
            .unwrap_or_else(|_| "3".to_string())
            .parse()
            .unwrap_or(3);

        let concurrency_gate = if max > 0 {
            Some(Arc::new(Semaphore::new(max)))
        } else {
            None
        };
        let session_manager = session_manager.unwrap_or_else(|| {
            Arc::new(Mutex::new(SessionManager::from_agents_config(
                workspace.clone(),
                &agents_cfg,
            )))
        });
        // Record every published inbound message durably and drop redeliveries.
        let journal_store = session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .journal_store();
        bus.set_inbound_guard(Arc::new(LedgerGuard::new(journal_store)));
        let runtime_resolver =
            Arc::new(ModelRuntimeResolver::new(config.clone(), provider.clone()));
        let subagents = Arc::new(
            SubagentManager::new(
                runtime_resolver.clone(),
                session_manager.clone(),
                workspace.clone(),
                bus.clone(),
                max_tool_result_chars as usize,
                Some(web_config.clone()),
                Some(exec_config.clone()),
                Some(gmail_config.clone()),
                Some(ocr_config.clone()),
                Some(docx_config.clone()),
                Some(image_generation_config.clone()),
                Some(subagent_config.clone()),
                Some(restrict_to_workspace),
            )
            .with_disabled_tools(tools_cfg.disabled_tools.clone()),
        );
        let mut tools = ToolRegistry::new();
        AgentLoop::register_default_tools(
            &mut tools,
            restrict_to_workspace,
            &exec_config,
            &web_config,
            &gmail_config,
            &ocr_config,
            &docx_config,
            &image_generation_config,
            &question_config,
            bus.clone(),
            &cron_service,
            &timezone,
            &workspace,
            session_manager.clone(),
        );
        tools.register(Box::new(SpawnTool::new(subagents.clone())));
        // The acp_* tools exist only when enabled and not already at maxDepth.
        let acp_tools =
            AcpToolsContext::for_process(&tools_cfg.acp, workspace.clone(), restrict_to_workspace);
        if let Some(acp_tools) = &acp_tools {
            register_acp_tools(&mut tools, acp_tools);
        }
        for name in &tools_cfg.disabled_tools {
            tools.disable(name);
        }
        tools.defer_builtins(deferrable_builtins(
            &tools_cfg.deferred_tools,
            &tools_cfg.disabled_tools,
            &tools,
        ));
        let tools = Arc::new(Mutex::new(tools));
        let mut context_builder = ContextBuilder::with_default_mode(
            workspace.clone(),
            timezone.clone(),
            tools.clone(),
            agents_cfg.mode,
        );
        if let Some(acp_tools) = &acp_tools {
            let acp_tools = Arc::clone(acp_tools);
            context_builder = context_builder
                .with_prompt_section(Arc::new(move |_mode| acp_tools.prompt_section()));
        }
        // Names the deferred servers and tools, so the model knows `tool_search` can load
        // them. Built from the mode's view, so `no_mcp` lists no MCP server.
        let server_descriptions = mcp_server_descriptions(&mcp_servers);
        let section_tools = Arc::clone(&tools);
        context_builder = context_builder.with_prompt_section(Arc::new(move |mode| {
            let registry = section_tools.lock().unwrap_or_else(|e| e.into_inner());
            deferred_tools_prompt_section(&mode.restrict_tools(&registry), &server_descriptions)
        }));
        let context = Arc::new(context_builder);
        let consolidator = Arc::new(Consolidator::new(
            Arc::clone(&context.memory),
            runtime_resolver.clone(),
            session_manager.clone(),
            context_window_tokens,
            Box::new(Arc::clone(&context)),
            max_tool_result_chars as usize,
        ));
        let auto_compact = Arc::new(Autocompact::new(
            session_manager.clone(),
            Arc::clone(&consolidator),
            agents_cfg.session_ttl_minutes as i64,
        ));
        let idle_compact_check_interval =
            Duration::from_secs(agents_cfg.idle_compact_check_interval_seconds as u64);

        let dream_cfg = agents_cfg.dream.clone();

        Self {
            bus: bus.clone(),
            _channels_config: channels_config,
            runtime_resolver: runtime_resolver.clone(),
            workspace: workspace.clone(),
            max_iterations,
            context_block_limit,
            max_tool_result_chars,
            provider_retry_mode: provider_retry_mode.to_string(),
            web_config: web_config.clone(),
            exec_config: exec_config.clone(),
            cron_service,
            restrict_to_workspace,
            disabled_tools: tools_cfg.disabled_tools.clone(),
            workspace_scopes,
            _timezone: timezone,
            start_time: SystemTime::now(),
            last_usage: Mutex::new(LLMUsage::new()),
            extra_hooks: hooks.unwrap_or_default(),
            context: context.clone(),
            session_manager: session_manager.clone(),
            tools,
            subagents,
            acp_tools,
            running: AtomicBool::new(false),
            mcp_servers,
            mcp_connected: AtomicBool::new(false),
            mcp_connecting: AtomicBool::new(false),
            mcp_sessions: Mutex::new(Vec::new()),
            mcp_state: Mutex::new(McpConnectionState::default()),
            active_tasks: Arc::new(AsyncMutex::new(HashMap::new())),
            next_task_id: AtomicU64::new(0),
            background_tasks: Arc::new(AsyncMutex::new(HashMap::new())),
            next_background_task_id: AtomicU64::new(0),
            session_locks: Arc::new(AsyncMutex::new(HashMap::new())),
            durability: config.durability.clone(),
            process_started_ms: AtomicI64::new(Utc::now().timestamp_millis()),
            _max: max,
            concurrency_gate,
            consolidator,
            dream: Arc::new(Dream::new(
                Arc::clone(&context.memory),
                runtime_resolver.clone(),
                dream_cfg.dream_model_preset.clone(),
                dream_cfg.model_override.clone(),
                dream_cfg.max_batch_size as usize,
                dream_cfg.max_iterations as usize,
                max_tool_result_chars as usize,
            )),
            auto_compact,
            idle_compact_check_interval,
            commands: {
                let mut router = CommandRouter::new();
                register_builtin_commands(&mut router);
                router
            },
            config,
        }
    }

    /// The process-wide default model (read-through onto the resolver's
    /// current default; does not reflect any particular session's override).
    pub fn model(&self) -> String {
        self.runtime_resolver.get_model()
    }

    /// The process-wide default provider (read-through onto the resolver's
    /// current default; does not reflect any particular session's override).
    pub fn provider(&self) -> Arc<dyn LLMProviderDyn> {
        self.runtime_resolver.current_default().provider
    }

    /// The process-wide default context window (read-through onto the
    /// resolver's current default).
    pub fn context_window_tokens(&self) -> u64 {
        self.runtime_resolver
            .current_default()
            .context_window_tokens
    }

    /// Change the process-wide default model without reconstructing any
    /// downstream consumer (subagents, Dream, Consolidator all read through
    /// the shared resolver). Delegates to
    /// [`ModelRuntimeResolver::select_model`]; see its doc comment for why
    /// this resets `preset_name` to `"default"`.
    pub fn set_runtime_model(&self, model: &str) -> Result<ModelRuntime, String> {
        self.runtime_resolver.select_model(model)
    }

    /// Change the process-wide default context-window budget. Delegates to
    /// [`ModelRuntimeResolver::select_context_window`].
    pub fn set_runtime_context_window(&self, tokens: u64) -> ModelRuntime {
        self.runtime_resolver.select_context_window(tokens)
    }

    /// Names available for `/model <name>`: `"default"` plus every configured preset.
    pub fn available_model_presets(&self) -> Vec<String> {
        self.runtime_resolver.available_preset_names()
    }

    /// Resolve the runtime a session would use right now (its stored preset
    /// override, if any, else the process-wide default) without mutating
    /// anything — used by `/model` with no arguments to report the active
    /// model/preset for the calling session.
    pub fn runtime_for_session(&self, session: Option<&Session>) -> ModelRuntime {
        self.runtime_resolver.runtime_for_session(session)
    }

    /// Validate and persist one session's model-preset override.
    ///
    /// `name == "default"` clears the override, reverting the session to the
    /// process-wide default on its next turn. This never mutates the
    /// process-wide default itself — the switch is isolated to this session,
    /// matching nanobot's `/model <preset>` semantics.
    pub fn set_session_model_preset(
        &self,
        session_manager: &mut SessionManager,
        session_key: &str,
        name: &str,
    ) -> Result<ModelRuntime, String> {
        let runtime = if name == RESERVED_MODEL_PRESET_NAME {
            self.runtime_resolver.current_default()
        } else {
            self.runtime_resolver.resolve_preset(name)?
        };

        let session = session_manager.get_or_create_session(session_key);
        if name == RESERVED_MODEL_PRESET_NAME {
            session.metadata.remove(SESSION_MODEL_PRESET_METADATA_KEY);
        } else {
            session.metadata.insert(
                SESSION_MODEL_PRESET_METADATA_KEY.to_string(),
                Value::String(name.to_string()),
            );
        }
        let snapshot = session.clone();
        session_manager
            .save(snapshot)
            .map_err(|e| format!("Failed to save session: {e}"))?;
        Ok(runtime)
    }

    /// Resolve the agent mode a session would use right now (its stored
    /// override, if any and valid, else the process-wide `agents.mode`).
    pub fn mode_for_session(&self, session: Option<&Session>) -> AgentMode {
        AgentMode::resolve(self.config.agents.mode, session.map(|s| &s.metadata))
    }

    /// Validate and persist one session's agent-mode override.
    ///
    /// `name == "default"` clears the override. Unknown names error without
    /// mutating the session.
    pub fn set_session_mode(
        &self,
        session_manager: &mut SessionManager,
        session_key: &str,
        name: &str,
    ) -> Result<AgentMode, String> {
        let session = session_manager.get_or_create_session(session_key);
        if name.eq_ignore_ascii_case(RESERVED_AGENT_MODE_NAME) {
            session.metadata.remove(SESSION_AGENT_MODE_METADATA_KEY);
            let snapshot = session.clone();
            session_manager
                .save(snapshot)
                .map_err(|e| format!("Failed to save session: {e}"))?;
            return Ok(self.config.agents.mode);
        }
        let mode = AgentMode::parse(name).ok_or_else(|| {
            format!(
                "Unknown agent mode '{name}'. Available modes: {}",
                AgentMode::available_names()
            )
        })?;
        session.metadata.insert(
            SESSION_AGENT_MODE_METADATA_KEY.to_string(),
            Value::String(mode.as_str().to_string()),
        );
        let snapshot = session.clone();
        session_manager
            .save(snapshot)
            .map_err(|e| format!("Failed to save session: {e}"))?;
        Ok(mode)
    }

    /// The `acp_*` tools' shared state, to wire escalation to the web-socket chat.
    pub fn acp_tools(&self) -> Option<&Arc<AcpToolsContext>> {
        self.acp_tools.as_ref()
    }

    /// Deny `denied_roots` to the file tools in every session's scope (another
    /// rust-bot's home that lies inside the project a child works on).
    pub fn with_denied_roots(mut self, denied_roots: Vec<PathBuf>) -> Self {
        self.workspace_scopes = self.workspace_scopes.with_denied_roots(denied_roots);
        self
    }

    /// Remove tools by name from the shared registry.
    ///
    /// Used by headless entry points (e.g. `rust-bot acp`) to drop tools that
    /// cannot work there. Unknown names are ignored.
    pub fn unregister_tools(&self, names: &[&str]) {
        let mut tools = self.tools.lock().unwrap_or_else(|e| e.into_inner());
        for name in names {
            tools.unregister(name);
        }
    }

    /// Tools visible to this session after applying its agent mode, with its loads
    /// (see [`session_tool_view`]).
    pub fn tools_for_session(&self, session: Option<&Session>) -> ToolRegistry {
        let mode = self.mode_for_session(session);
        let registry = self.tools.lock().unwrap_or_else(|e| e.into_inner());
        session_tool_view(&registry, mode, session)
    }

    /// Resolve the workspace scope a session would use right now (its
    /// stored override, if any, else the process-wide default) without
    /// mutating anything — used by `/workspace` with no arguments.
    pub fn workspace_scope_for_session(&self, session: Option<&Session>) -> WorkspaceScope {
        self.workspace_scopes
            .for_session(session.map(|s| &s.metadata))
    }

    /// Builds a fresh `WorkspaceRequestHandler` for this loop's default workspace/restriction —
    /// cheap (two fields, no shared state), so callers just call this once at the point they
    /// need one rather than storing a long-lived reference back into `AgentLoop`.
    pub fn workspace_request_handler(&self) -> WorkspaceRequestHandler {
        WorkspaceRequestHandler::new(self.workspace.clone(), self.restrict_to_workspace)
    }

    /// Validate and persist one session's workspace-scope override.
    ///
    /// Never mutates the process-wide default workspace — the switch is
    /// isolated to this session's tool calls on subsequent turns, matching
    /// nanobot's per-session `workspace_scope` semantics and mirroring
    /// [`Self::set_session_model_preset`]'s shape.
    pub fn set_session_workspace_scope(
        &self,
        session_manager: &mut SessionManager,
        session_key: &str,
        project_path: &std::path::Path,
        access_mode: WorkspaceAccessMode,
    ) -> Result<WorkspaceScope, WorkspaceScopeError> {
        Self::set_session_workspace_scope_from_payload(
            session_manager,
            session_key,
            project_path,
            access_mode,
            &self.workspace,
            self.restrict_to_workspace,
        )
    }

    pub fn set_session_workspace_scope_from_payload(
        session_manager: &mut SessionManager,
        session_key: &str,
        project_path: &std::path::Path,
        access_mode: WorkspaceAccessMode,
        workspace: &Path,
        restrict_to_workspace: bool,
    ) -> Result<WorkspaceScope, WorkspaceScopeError> {
        let scope = validate_workspace_scope_payload(
            &serde_json::json!({
                "project_path": project_path.display().to_string(),
                "access_mode": access_mode.as_str(),
            }),
            workspace,
            restrict_to_workspace,
            None,
        )?;

        let session = session_manager.get_or_create_session(session_key);
        session
            .metadata
            .insert(WORKSPACE_SCOPE_METADATA_KEY.to_string(), scope.metadata());
        let snapshot = session.clone();
        session_manager
            .save(snapshot)
            .map_err(|e| WorkspaceScopeError::new(500, format!("Failed to save session: {e}")))?;
        Ok(scope)
    }

    /// Clear a session's workspace-scope override, reverting to the
    /// process-wide default on its next turn.
    pub fn clear_session_workspace_scope(session_manager: &mut SessionManager, session_key: &str) {
        let session = session_manager.get_or_create_session(session_key);
        session.metadata.remove(WORKSPACE_SCOPE_METADATA_KEY);
        let snapshot = session.clone();
        if let Err(e) = session_manager.save(snapshot) {
            log::error!("Failed to save session after clearing workspace scope: {e}");
        }
    }

    /// Start a new sustained goal for this session. Thin delegator to
    /// [`goal_state::create_session_goal`] — kept as an `AgentLoop` method so
    /// command handlers find it alongside [`Self::set_session_workspace_scope`].
    pub fn create_session_goal(
        &self,
        session_manager: &mut SessionManager,
        session_key: &str,
        objective: &str,
        ui_summary: Option<&str>,
    ) -> Result<(), goal_state::GoalError> {
        goal_state::create_session_goal(session_manager, session_key, objective, ui_summary)
    }

    /// Complete/cancel/block/replace this session's active goal. Thin
    /// delegator to [`goal_state::update_session_goal`].
    pub fn update_session_goal(
        &self,
        session_manager: &mut SessionManager,
        session_key: &str,
        action: goal_state::GoalUpdateAction,
        recap: Option<&str>,
        objective: Option<&str>,
        ui_summary: Option<&str>,
    ) -> Result<String, goal_state::GoalError> {
        goal_state::update_session_goal(
            session_manager,
            session_key,
            action,
            recap,
            objective,
            ui_summary,
        )
    }

    /// Register the default set of tools.
    #[allow(clippy::too_many_arguments)]
    fn register_default_tools(
        tools: &mut ToolRegistry,
        restrict_to_workspace: bool,
        exec_config: &ExecToolConfig,
        web_config: &WebToolsConfig,
        gmail_config: &GmailToolConfig,
        ocr_config: &OcrToolConfig,
        docx_config: &DocxToolConfig,
        image_generation_config: &ImageGenerationToolConfig,
        question_config: &QuestionToolConfig,
        bus: Arc<MessageBus>,
        cron_service: &Option<Arc<CronService>>,
        timezone: &Option<String>,
        workspace: &Path,
        session_manager: Arc<Mutex<SessionManager>>,
    ) {
        log::info!("Registering default tools");
        let (allowed_dir, extra_read) =
            filesystem_tool_scope(workspace, restrict_to_workspace, &exec_config.sandbox);
        register_filesystem_tools(tools, workspace, allowed_dir.clone(), extra_read.clone());
        if exec_config.enable {
            log::debug!("Registering exec tool");
            tools.register(Box::new(ShellTool::new(
                exec_config.timeout as u64,
                Some(workspace.to_path_buf()),
                None,
                None,
                restrict_to_workspace,
                None,
                Some(exec_config.path_append.clone()),
            )));
        }
        register_web_tools(web_config, tools);
        register_gmail_tools(gmail_config, workspace, tools);
        register_ocr_tools(
            ocr_config,
            workspace,
            allowed_dir.clone(),
            extra_read.clone(),
            tools,
        );
        register_conversion_tools(
            docx_config,
            &FsToolConfig::new(Some(workspace.to_path_buf()), allowed_dir, Some(extra_read)),
            tools,
        );
        register_image_generation_tools(image_generation_config, workspace, tools);
        register_question_tool(question_config, bus.clone(), tools);
        tools.register(Box::new(MessageTool::new(
            Some(MessageTool::create_send_callback(bus)),
            "",
            "",
            None,
        )));
        if let Some(cron_service) = cron_service {
            tools.register(Box::new(CronTool::new(
                cron_service.clone(),
                timezone.clone().unwrap_or("UTC".to_string()),
            )));
        }
        tools.register(Box::new(UpdateGoalTool::new(session_manager)));
    }

    /// Connect to configured MCP servers (lazy, on the first message).
    ///
    /// Each server is connected on its own: the ones that come up get their tools
    /// registered even if another server fails. Servers that failed are retried on a
    /// later message, at most once per [`MCP_RETRY_INTERVAL`]; servers that are already
    /// up are left alone.
    ///
    /// Takes `&self` (state is held in atomics / a mutex) so it can be called
    /// from a shared `Arc<Self>` in the run loop.
    pub async fn connect_mcp(&self) {
        if self.mcp_connected.load(Ordering::Relaxed) || self.mcp_servers.is_empty() {
            return;
        }
        let pending = {
            let state = self.mcp_state.lock().unwrap_or_else(|e| e.into_inner());
            if !mcp_retry_due(state.last_attempt, Instant::now(), MCP_RETRY_INTERVAL) {
                return;
            }
            pending_mcp_servers(&self.mcp_servers, &state.connected_names)
        };
        if self.mcp_connecting.swap(true, Ordering::Relaxed) {
            return;
        }

        // Rust uses RAII instead of an AsyncExitStack: each established session
        // closes its connection when dropped, so the sessions are kept alive by
        // storing them on `self`.
        let batch = connect_mcp_servers_independently(pending, self.mcp_file_ref_resolver()).await;
        self.record_mcp_batch(batch);

        // `finally`: always release the connecting flag.
        self.mcp_connecting.store(false, Ordering::Relaxed);
    }

    /// Register the tools of the servers that connected, remember the ones that did
    /// not (and why), and update whether every configured server is up. A tool is
    /// registered when the server's `enabledTools` enables it and `tools.disabledTools`
    /// does not name it; `enabledTools` entries that match no tool are logged.
    fn record_mcp_batch(&self, batch: McpConnectBatch) {
        let McpConnectBatch { connected, failed } = batch;
        let mut mcp_tool_count = 0usize;
        let mut new_sessions = Vec::with_capacity(connected.len());
        let mut connected_names = Vec::with_capacity(connected.len());
        {
            let mut registry = self.tools.lock().unwrap_or_else(|e| e.into_inner());
            for (name, mut session) in connected {
                // A connected server is always configured; the fallback keeps every tool.
                let enabled_tools = self
                    .mcp_servers
                    .get(&name)
                    .map(|config| config.enabled_tools.clone())
                    .unwrap_or_else(|| vec![MCP_ALL_TOOLS.to_string()]);
                let tool_names: Vec<String> =
                    session.tools.iter().map(|tool| tool.name()).collect();
                for entry in unmatched_enabled_tools(&enabled_tools, &name, &tool_names) {
                    log::warn!(
                        "MCP server '{name}': enabledTools entry '{entry}' matches none of its tools"
                    );
                }
                for tool in session.tools.drain(..) {
                    let tool_name = tool.name();
                    if self.disabled_tools.contains(&tool_name)
                        || !is_mcp_tool_enabled(&enabled_tools, &name, &tool_name)
                    {
                        continue;
                    }
                    registry.register(tool);
                    mcp_tool_count += 1;
                }
                connected_names.push(name);
                new_sessions.push(session);
            }
        }
        self.mcp_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(new_sessions);

        let (connected_total, all_connected) = {
            let mut state = self.mcp_state.lock().unwrap_or_else(|e| e.into_inner());
            for name in connected_names {
                state.failures.remove(&name);
                state.connected_names.insert(name);
            }
            for (name, error) in failed {
                log::error!(
                    "Failed to connect MCP server '{name}' (will retry in {}s): {error}",
                    MCP_RETRY_INTERVAL.as_secs()
                );
                state.failures.insert(name, error.to_string());
            }
            state.last_attempt = Some(Instant::now());
            (state.connected_names.len(), state.failures.is_empty())
        };
        self.mcp_connected.store(all_connected, Ordering::Relaxed);
        log::info!(
            "{connected_total} of {} MCP server(s) connected; {mcp_tool_count} MCP tools registered",
            self.mcp_servers.len()
        );
    }

    /// Sandbox used when an MCP argument references a local file.
    ///
    /// Mirrors the filesystem tools' scope, so `restrictToWorkspace` applies to
    /// `file://` arguments too and they cannot read outside the workspace.
    fn mcp_file_ref_resolver(&self) -> Option<FileRefResolver> {
        let (allowed_dir, extra_read) = filesystem_tool_scope(
            &self.workspace,
            self.restrict_to_workspace,
            &self.exec_config.sandbox,
        );
        Some(FileRefResolver::with_scope(
            Some(self.workspace.clone()),
            allowed_dir,
            extra_read,
        ))
    }

    /// Connect to configured MCP servers if not already connected or connecting.
    pub async fn ensure_mcp_connected(&self) {
        self.connect_mcp().await;
    }

    /// Whether all configured MCP servers are connected.
    pub fn is_mcp_connected(&self) -> bool {
        self.mcp_connected.load(Ordering::Relaxed)
    }

    /// Whether any MCP servers are configured.
    pub fn is_mcp_configured(&self) -> bool {
        !self.mcp_servers.is_empty()
    }

    /// Connected MCP servers as `(name, endpoint)` pairs, sorted by name. Servers that
    /// failed to connect are not included (see [`Self::failed_mcp_servers`]).
    pub fn connected_mcp_endpoints(&self) -> Vec<(String, String)> {
        let state = self.mcp_state.lock().unwrap_or_else(|e| e.into_inner());
        let mut servers: Vec<(String, String)> = self
            .mcp_servers
            .iter()
            .filter(|(name, _)| state.connected_names.contains(*name))
            .map(|(name, config)| (name.clone(), mcp_server_endpoint(config)))
            .collect();
        servers.sort_by(|a, b| a.0.cmp(&b.0));
        servers
    }

    /// MCP servers whose last connection attempt failed, as `(name, error)` pairs sorted
    /// by name.
    pub fn failed_mcp_servers(&self) -> Vec<(String, String)> {
        let state = self.mcp_state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .failures
            .iter()
            .map(|(name, error)| (name.clone(), error.clone()))
            .collect()
    }

    /// Shared message bus handle for publishing outbound messages.
    pub fn bus(&self) -> Arc<MessageBus> {
        Arc::clone(&self.bus)
    }

    /// Publish a channel-agnostic [`OutboundEvent::TurnEnd`] for `msg`'s chat.
    ///
    /// A second bus message, never mixed into the final assistant payload:
    /// that payload may carry `_streamed` and would be dropped for WebSocket.
    /// Inbound metadata is copied through so `_websocket_turn_owner` /
    /// `webui_turn_id` survive to the channel that started the registry turn.
    pub(crate) fn publish_turn_end(&self, msg: &InboundMessage, started_at: Option<Instant>) {
        let latency_ms = started_at.map(|t| t.elapsed().as_millis() as i64);
        let outbound = outbound_message_for_event(
            &msg.channel,
            &msg.chat_id,
            OutboundEvent::TurnEnd(TurnEndEvent {
                latency_ms,
                goal_state: None,
            }),
            None,
            Some(msg.metadata.clone()),
        );
        if let Err(e) = self.bus.publish_outbound(outbound) {
            log::error!(
                "Failed to publish TurnEnd for {}:{}: {e}",
                msg.channel,
                msg.chat_id
            );
        }
    }

    /// Cancel all active tasks and subagents for `msg`'s session, then
    /// publish a `TurnEnd` so channels/the UI don't stay stuck "running" —
    /// aborting drops `dispatch` before its own post-`process_message`
    /// `TurnEnd`. Returns the number of tasks (agent turns + subagents)
    /// cancelled.
    ///
    /// Extracted from `/stop`'s handler (`command::builtin::CmdStop`) so
    /// other callers with no real [`crate::command::CommandContext`] — e.g.
    /// the WebSocket `delete_chat` envelope, which builds a minimal,
    /// synthetic `msg` just to carry `channel`/`chat_id` — can reuse the
    /// same cancellation body instead of duplicating it.
    pub async fn abort_session(&self, msg: &InboundMessage) -> u32 {
        let session_key = msg.session_key();
        let tasks = self
            .active_tasks
            .lock()
            .await
            .remove(&session_key)
            .unwrap_or_default();
        let mut cancelled: u32 = 0;
        for handle in tasks.into_values() {
            handle.abort();
            cancelled += 1;
        }
        let sub_cancelled = self.subagents.cancel_by_session(&session_key).await;
        // A stopped request must not be re-driven after a restart.
        let ledger = InboxLedger::for_session(&self.journal_store(), &session_key);
        for entry in ledger.pending() {
            ledger.mark_entry_done(&entry, None);
        }
        let total = cancelled + sub_cancelled;
        self.publish_turn_end(msg, None);
        total
    }

    /// Update context for all tools that need routing info.
    pub fn set_tool_context(&self, channel: &str, chat_id: &str, message_id: Option<&str>) {
        let registry = self.tools.lock().unwrap_or_else(|e| e.into_inner());
        for name in CONTEXT_AWARE_TOOLS {
            let Some(tool) = registry.get(name) else {
                continue;
            };
            let message_id = if *name == "message" { message_id } else { None };
            tool.set_tool_context(channel, chat_id, message_id);
        }
    }

    /// Copy inbound `webui` / `webui_turn_id` onto the `message` tool so
    /// owner-bound sends persist to the WebUI transcript. An empty map
    /// (or a non-WebUI inbound) clears a stale snapshot from a previous turn.
    fn apply_message_tool_inbound_metadata(&self, metadata: &HashMap<String, Value>) {
        let registry = self.tools.lock().unwrap_or_else(|e| e.into_inner());
        let Some(tool) = registry.get("message") else {
            return;
        };
        let Some(message_tool) =
            (tool.as_ref() as &dyn std::any::Any).downcast_ref::<MessageTool>()
        else {
            return;
        };
        message_tool.set_inbound_webui_metadata(metadata);
    }

    /// Run the agent iteration loop.
    ///
    /// `on_stream` is called with each content delta during streaming.
    /// `on_stream_end(resuming)` is called when a streaming session finishes:
    /// `resuming = true` means tool calls follow (spinner should restart),
    /// `resuming = false` means this is the final response.
    #[allow(clippy::too_many_arguments)]
    async fn run_agent_loop(
        self: &Arc<Self>,
        initial_messages: Vec<Value>,
        on_progress: Option<ProgressCallback>,
        on_stream: Option<StreamCallback>,
        on_stream_end: Option<StreamEndCallback>,
        session: Option<Session>,
        channel: &str,
        chat_id: &str,
        message_id: Option<&str>,
        attempt: u32,
        request_id: Option<String>,
    ) -> AgentRunResult {
        // The same progress callback, for tools that report while they block.
        let tool_progress = on_progress.clone();
        let loop_hook = LoopHook::with_context(
            Arc::clone(self),
            on_progress,
            on_stream,
            on_stream_end,
            channel,
            chat_id,
            message_id.map(|s| s.to_string()),
        );
        let hook: Arc<dyn AgentHook> = if self.extra_hooks.is_empty() {
            Arc::new(loop_hook)
        } else {
            Arc::new(LoopHookChain::new(
                Arc::new(loop_hook),
                self.extra_hooks.clone(),
            ))
        };

        let session_key = session.as_ref().map(|s| s.key.clone());
        let checkpoint_callback: Option<Arc<dyn Fn(Value) + Send + Sync>> =
            session_key.clone().map(|key| {
                let this = Arc::clone(self);
                Arc::new(move |payload: Value| {
                    this.set_runtime_checkpoint(&key, payload);
                }) as Arc<dyn Fn(Value) + Send + Sync>
            });

        let mode = self.mode_for_session(session.as_ref());
        let mut run_tools = self.tools_for_session(session.as_ref());
        // `tool_search` exists only when this view has something to load. It shares the
        // view's loaded-tools handle, so its loads reach the runner's next request, and
        // it saves each load on the session right away, so later turns (and a restart)
        // keep it.
        if run_tools.has_deferred_tools() {
            let mut tool_search = ToolSearchTool::new(run_tools.clone());
            if let Some(key) = session_key.clone() {
                let this = Arc::clone(self);
                tool_search = tool_search.on_load(Arc::new(move |loaded_tools: &[String]| {
                    this.record_session_loaded_tools(&key, loaded_tools);
                }));
            }
            run_tools.register(Box::new(tool_search));
        }
        log::info!(
            "Running agent loop with {} tools (mode={})",
            run_tools.len(),
            mode
        );

        // Resolve the runtime for this specific session (its stored preset
        // override, if any, else the process-wide default) — no fixed
        // provider/model here; see `ModelRuntimeResolver`.
        let runtime = self.runtime_resolver.runtime_for_session(session.as_ref());
        log::info!(
            "Running turn with model={} (preset={}), max_tokens={}",
            runtime.model,
            runtime.preset_name,
            runtime.max_tokens
        );
        let scope = self
            .workspace_scopes
            .for_session(session.as_ref().map(|s| &s.metadata));
        let workspace_scope_token = bind_workspace_scope(scope);

        let runner = AgentRunner::new(runtime.provider.clone());
        let turn_journal = session_key.as_deref().map(|key| {
            self.begin_turn_journal(
                key,
                &initial_messages,
                channel,
                chat_id,
                attempt,
                request_id.clone(),
            )
        });
        // Aborting the turn (`/stop`) drops this future, which closes the turn as cancelled.
        let turn_guard = turn_journal.as_ref().map(TurnJournal::guard);
        let turn_journal_handle = turn_journal.clone();
        let result = with_tool_progress(
            tool_progress,
            runner.run(AgentRunSpec {
                initial_messages,
                tools: run_tools,
                model: runtime.model.clone(),
                max_iterations: self.max_iterations as usize,
                max_tool_result_chars: self.max_tool_result_chars as usize,
                hook: Some(hook),
                error_message: Some(
                    "Sorry, I encountered an error calling the AI model.".to_string(),
                ),
                concurrent_tools: true,
                workspace: Some(self.workspace.clone()),
                session_key: session_key.clone(),
                channel: Some(channel.to_string()),
                chat_id: Some(chat_id.to_string()),
                context_window_tokens: Some(runtime.context_window_tokens),
                context_block_limit: self.context_block_limit,
                provider_retry_mode: self.provider_retry_mode.clone(),
                progress_callback: None,
                checkpoint_callback,
                fail_on_tool_error: false,
                temperature: Some(runtime.temperature),
                max_iterations_message: None,
                max_tokens: Some(runtime.max_tokens as usize),
                reasoning_effort: runtime.reasoning_effort.clone(),
                journal: turn_journal,
            }),
        )
        .await;
        crash_point("after_final_before_turn_end");
        if let Some(turn_guard) = turn_guard {
            turn_guard.finish(Self::turn_outcome(&result));
            if let Some(journal) = &turn_journal_handle {
                if let Err(e) = recovery::prune_journal(journal.journal()) {
                    log::warn!("Failed to compact run journal: {e}");
                }
                let ledger_ttl = Duration::from_secs(self.durability.ledger_ttl_days * 86_400);
                if let Err(e) = InboxLedger::new(Arc::clone(journal.journal())).compact(
                    Utc::now(),
                    ledger_ttl,
                    self.durability.ledger_max_entries,
                ) {
                    log::warn!("Failed to compact inbox ledger: {e}");
                }
            }
        }
        reset_workspace_scope(workspace_scope_token);
        *self.last_usage.lock().unwrap_or_else(|e| e.into_inner()) = result.usage;
        if result.stop_reason == "max_iterations" {
            log::warn!("Max iterations ({}) reached", self.max_iterations);
        } else if result.stop_reason == "error" {
            let message = result
                .final_content
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>();
            log::error!("LLM returned error: {message}");
        } else if result.stop_reason == CIRCUIT_BREAKER_STOP_REASON {
            log::warn!("Message circuit breaker tripped");
        }
        if let Some(session_key) = session_key
            && result.usage != LLMUsage::new()
        {
            let mut manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let session = manager.get_or_create_session(&session_key);
            session.update_usage(result.usage);
            let snapshot = session.clone();
            if let Err(e) = manager.save(snapshot) {
                log::error!("Failed to save session token usage for {session_key}: {e}");
            }
        }
        result
    }

    /// Open the run journal of `session_key` and record the start of a new turn.
    ///
    /// The turn's user message goes into the journal because the session file
    /// only receives it once the turn has finished.
    fn begin_turn_journal(
        &self,
        session_key: &str,
        initial_messages: &[Value],
        channel: &str,
        chat_id: &str,
        attempt: u32,
        request_id: Option<String>,
    ) -> TurnJournal {
        let turn = TurnJournal::new(
            RunJournal::new(self.journal_store(), session_key),
            uuid::Uuid::new_v4().to_string(),
        );
        let user_message = initial_messages
            .iter()
            .rev()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
            .cloned();
        turn.append(
            JournalRecord::TurnStart {
                turn_id: turn.turn_id().to_string(),
                session_key: session_key.to_string(),
                request_id,
                user_message,
                attempt,
                channel: Some(channel.to_string()),
                chat_id: Some(chat_id.to_string()),
            },
            true,
        );
        crash_point("after_turn_start");
        turn
    }

    /// Terminal journal outcome of a finished run.
    fn turn_outcome(result: &AgentRunResult) -> TurnOutcome {
        if result.stop_reason == "error" {
            TurnOutcome::Failed
        } else {
            TurnOutcome::Completed
        }
    }

    // ── crash recovery: resuming interrupted turns ────────────────────────────

    /// Start of this process; recovery only touches records from before it.
    fn process_start(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.process_started_ms.load(Ordering::Relaxed))
            .unwrap_or_else(Utc::now)
    }

    /// Turns that a previous run left unfinished, for the one-line notice of
    /// the modes that do not auto-resume (interactive and one-shot `agent`).
    pub fn unfinished_turn_count(&self) -> usize {
        resume::count_unfinished_turns(self.journal_store().as_ref())
    }

    /// Where the shared session manager keeps the run journals.
    fn journal_store(&self) -> Arc<dyn JournalStore> {
        self.session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .journal_store()
    }

    /// Resume the turns that a crash left unfinished (gateway startup).
    ///
    /// Plans with [`resume::plan_resumes`] (cancelled, stale and out-of-attempts
    /// turns are not resumed), then drives each resumable turn: replay-safe tool
    /// calls are re-executed once, the rest are reported to the model as
    /// interrupted, and the model is asked to continue. Resumes of different
    /// sessions run in parallel up to `durability.maxConcurrentResumes`.
    /// Resumed turns are not registered in `active_tasks`, so `/stop` cannot
    /// abort them.
    pub async fn resume_interrupted_turns(self: &Arc<Self>) -> ResumeSummary {
        let journals = self.journal_store();
        let policy = ResumePolicy::from_config(&self.durability);
        let actions =
            resume::plan_resumes(journals.as_ref(), Utc::now(), self.process_start(), &policy);
        let mut summary = ResumeSummary::default();
        let gate = Arc::new(Semaphore::new(
            self.durability.max_concurrent_resumes.max(1),
        ));
        let mut tasks = Vec::new();
        for action in actions {
            match action {
                ResumeAction::Abandon { turn } => {
                    log::warn!(
                        "abandoning turn={} session={} after {} resume attempt(s)",
                        turn.turn_id,
                        turn.session_key,
                        turn.attempts
                    );
                    let journal = RunJournal::new(Arc::clone(&journals), &turn.session_key);
                    TurnJournal::new(journal, turn.turn_id).record_turn_end(TurnOutcome::Abandoned);
                    summary.abandoned += 1;
                }
                ResumeAction::Stale { turn } => {
                    log::warn!(
                        "not resuming stale turn={} session={} (started {})",
                        turn.turn_id,
                        turn.session_key,
                        turn.started_at
                    );
                    summary.stale += 1;
                }
                ResumeAction::Resume { turn, attempt } => {
                    summary.resumed += 1;
                    let this = Arc::clone(self);
                    let gate = Arc::clone(&gate);
                    tasks.push(tokio::spawn(async move {
                        let _permit = gate.acquire_owned().await.ok();
                        this.resume_turn(turn, attempt).await;
                    }));
                }
            }
        }
        for task in tasks {
            let _ = task.await;
        }
        summary
    }

    /// Where the reply of `turn` goes: the journaled route, else the session key.
    fn resume_route(turn: &UnfinishedTurn) -> Option<(String, String)> {
        match (&turn.channel, &turn.chat_id) {
            (Some(channel), Some(chat_id)) => Some((channel.clone(), chat_id.clone())),
            _ => turn
                .session_key
                .split_once(':')
                .map(|(channel, chat_id)| (channel.to_string(), chat_id.to_string())),
        }
    }

    /// Resume one interrupted turn as attempt number `attempt`.
    async fn resume_turn(self: Arc<Self>, turn: UnfinishedTurn, attempt: u32) {
        // Tools read the ambient workspace scope, which every turn entry point establishes.
        with_workspace_scope_stack(move || self.resume_turn_inner(turn, attempt)).await
    }

    async fn resume_turn_inner(self: Arc<Self>, turn: UnfinishedTurn, attempt: u32) {
        let key = turn.session_key.clone();
        let Some((channel, chat_id)) = Self::resume_route(&turn) else {
            log::error!(
                "turn {} of session {key} has no route; abandoning",
                turn.turn_id
            );
            let journal = RunJournal::new(self.journal_store(), &key);
            TurnJournal::new(journal, turn.turn_id).record_turn_end(TurnOutcome::Abandoned);
            return;
        };
        // Same per-session serialization as `dispatch`.
        let _guard = self.session_lock(&key).await.lock_owned().await;
        log::warn!(
            "resume turn={} attempt={attempt} session={key}",
            turn.turn_id
        );

        self.replay_interrupted_calls(&key, &turn.turn_id, &channel, &chat_id)
            .await;

        // Merge the interrupted turn into the transcript (journal-first).
        let mut session = {
            let mut manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let journals = manager.journal_store();
            let session = manager.get_or_create_session(&key);
            let restored = self.restore_runtime_checkpoint(session, &journals);
            let snapshot = session.clone();
            if restored && let Err(e) = manager.save(snapshot.clone()) {
                log::error!("Failed to save restored session: {e}");
            }
            snapshot
        };
        let history = session.get_history(Some(0));

        // A turn that died after its final answer only lacks the delivery.
        if let Some(answer) = Self::final_answer_of(&history) {
            log::warn!(
                "resume turn={}: answer already complete, delivering it",
                turn.turn_id
            );
            self.close_resumed_request(&turn, &answer);
            self.publish_resumed_reply(&channel, &chat_id, answer);
            return;
        }

        self.set_tool_context(&channel, &chat_id, None);
        // The system prompt only: the transcript already holds the turn so far.
        let mut initial_messages = self.context.build_messages(
            &[],
            "",
            None,
            None,
            Some(&channel),
            Some(&chat_id),
            Some(&session.metadata),
            None,
            DEFAULT_CURRENT_ROLE,
            None,
        );
        initial_messages.truncate(1);
        initial_messages.extend(history.iter().cloned());

        let result = self
            .run_agent_loop(
                initial_messages,
                None,
                None,
                None,
                Some(session.clone()),
                &channel,
                &chat_id,
                None,
                attempt,
                turn.request_id.clone(),
            )
            .await;
        self.persist_finished_turn(
            &mut session,
            &result.messages,
            1 + history.len() as u32,
            result.usage,
            "resuming turn",
        );
        let answer = result
            .final_content
            .filter(|content| !content.trim().is_empty())
            .unwrap_or_else(|| EMPTY_FINAL_RESPONSE_MESSAGE.to_string());
        self.close_resumed_request(&turn, &answer);
        self.publish_resumed_reply(&channel, &chat_id, answer);
    }

    /// Mark the inbound request behind a resumed turn as answered in the ledger.
    fn close_resumed_request(&self, turn: &UnfinishedTurn, answer: &str) {
        let Some(request_id) = &turn.request_id else {
            return;
        };
        let ledger = InboxLedger::for_session(&self.journal_store(), &turn.session_key);
        if let Some(entry) = ledger
            .entries()
            .into_iter()
            .find(|entry| &entry.request_id == request_id)
        {
            ledger.mark_entry_done(&entry, Some(answer.to_string()));
        }
    }

    /// Re-drive inbound messages that were accepted before a crash but never
    /// answered, and close ledger entries whose turn already ended.
    ///
    /// Runs after interrupted turns were resumed, so per session the resumed
    /// turn goes first and queued messages follow in ledger order. Returns how
    /// many messages were fed back to the agent.
    pub fn redrive_pending_inbound(&self) -> usize {
        let mut redriven = 0;
        let cutoff = self.process_start();
        for scan in self.journal_store().scan_all() {
            let entries = scan.entries;
            for action in inbox::plan_redrive(&entries) {
                // Entries touched by this process are live, not left over from a crash.
                if !resume::started_before_cutoff(action.entry().updated_at.as_str(), cutoff) {
                    continue;
                }
                match action {
                    RedriveAction::Keep(_) => {}
                    RedriveAction::Close(entry) => {
                        InboxLedger::for_session(&self.journal_store(), &entry.session_key)
                            .mark_entry_done(&entry, None);
                    }
                    RedriveAction::Redrive(entry) => {
                        let message = entry
                            .message
                            .clone()
                            .and_then(|value| serde_json::from_value::<InboundMessage>(value).ok());
                        let Some(message) = message else {
                            log::error!("unreadable stored message for {}", entry.request_id);
                            continue;
                        };
                        log::warn!("re-driving queued request {}", entry.request_id);
                        if self.bus.republish_inbound(message).is_ok() {
                            redriven += 1;
                        }
                    }
                }
            }
        }
        redriven
    }

    /// The text of the last message when it is a finished assistant answer.
    fn final_answer_of(history: &[Value]) -> Option<String> {
        let last = history.last()?;
        let is_assistant = last.get("role").and_then(Value::as_str) == Some("assistant");
        let wants_tools = last
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty());
        let text = last.get("content").and_then(Value::as_str)?;
        (is_assistant && !wants_tools && !text.trim().is_empty()).then(|| text.to_string())
    }

    /// Send the reply of a resumed turn to its chat.
    fn publish_resumed_reply(&self, channel: &str, chat_id: &str, content: String) {
        let outbound = OutboundMessage {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            content,
            reply_to: None,
            media: vec![],
            metadata: HashMap::new(),
            event: None,
        };
        if let Err(e) = self.bus.publish_outbound(outbound) {
            log::error!("Failed to publish resumed reply to {channel}:{chat_id}: {e}");
        }
    }

    /// Re-execute the interrupted calls of `turn_id` that are replay safe and not
    /// yet replayed, journaling each with the original arguments. The recorded
    /// result is what the restored transcript then shows the model. Calls that
    /// are not replay safe are never executed here.
    async fn replay_interrupted_calls(
        &self,
        session_key: &str,
        turn_id: &str,
        channel: &str,
        chat_id: &str,
    ) {
        let journal = RunJournal::new(self.journal_store(), session_key);
        let states = recovery::tool_call_states(&journal.scan().entries);
        let turn = TurnJournal::new(journal, turn_id);
        let scope = {
            let mut manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let metadata = manager.get_or_create_session(session_key).metadata.clone();
            self.workspace_scopes.for_session(Some(&metadata))
        };
        let scope_token = bind_workspace_scope(scope);
        for state in states.iter().filter(|state| state.turn_id == turn_id) {
            let ToolRecovery::Replay { args } =
                recovery::decide_tool_recovery(state, state.was_replayed())
            else {
                continue;
            };
            let tool = self
                .tools
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&state.tool_name);
            // The registry is authoritative: a tool that no longer declares itself safe stays unrun.
            let Some(tool) = tool.filter(|tool| tool.replay_safe()) else {
                continue;
            };
            log::warn!(
                "replaying interrupted call turn={turn_id} call={} tool={}",
                state.call_id,
                state.tool_name
            );
            self.set_tool_context(channel, chat_id, None);
            let in_flight = turn.begin_tool_call(&state.call_id, &state.tool_name, &args, true);
            let identity = ToolCallIdentity {
                turn_id: turn_id.to_string(),
                call_id: state.call_id.clone(),
            };
            let result = with_tool_call(identity, tool.execute(&args)).await;
            let status = if result.starts_with("Error") {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Done
            };
            in_flight.finish(status, result);
        }
        reset_workspace_scope(scope_token);
    }

    /// Add `loaded_tools` to the session's `loaded_tools` and save the session if that
    /// changed it (see [`Session::record_loaded_tools`]).
    fn record_session_loaded_tools(&self, session_key: &str, loaded_tools: &[String]) {
        let mut manager = self
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = manager.get_or_create_session(session_key);
        if !session.record_loaded_tools(loaded_tools) {
            return;
        }
        let snapshot = session.clone();
        if let Err(e) = manager.save(snapshot) {
            log::error!("Failed to save the tools loaded by tool_search: {e}");
        }
    }

    /// Persist the latest in-flight turn state into session metadata.
    ///
    /// Resolves the canonical session from the manager by key (rather than
    /// mutating a stale snapshot captured before the run started), so saving the
    /// checkpoint never clobbers the live message history.
    fn set_runtime_checkpoint(&self, session_key: &str, payload: Value) {
        let mut manager = self
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = manager.get_or_create_session(session_key);
        session
            .metadata
            .insert(RUNTIME_CHECKPOINT_KEY.to_string(), payload);
        let snapshot = session.clone();
        if let Err(e) = manager.save(snapshot) {
            log::error!("Failed to save runtime checkpoint: {e}");
        }
    }

    /// Scan idle sessions and schedule background archival, no more often
    /// than once per `idle_compact_check_interval` — `next_check_at` is the
    /// caller's throttle gate, advanced here on every actual scan. Mirrors
    /// nanobot's `_check_expired_sessions_if_due` (`loop.py:1099-1109`).
    async fn check_expired_sessions_if_due(self: &Arc<Self>, next_check_at: &mut Instant) {
        let now = Instant::now();
        if now < *next_check_at {
            return;
        }
        *next_check_at = now + self.idle_compact_check_interval;

        let active_session_keys: Vec<String> = {
            let active_tasks = self.active_tasks.lock().await;
            active_tasks.keys().cloned().collect()
        };
        let this = Arc::clone(self);
        self.auto_compact.check_expired(
            move |fut| {
                // `Autocompact::check_expired` hands us a boxed future
                // fire-and-forget (nanobot's `schedule_background` is sync
                // too); `AgentLoop::schedule_background` is async only to
                // acquire the task-registry lock, so spawn a tiny task to
                // drive that await without blocking this scan.
                let this = Arc::clone(&this);
                tokio::spawn(async move {
                    this.schedule_background(fut).await;
                });
            },
            |session| self.runtime_resolver.runtime_for_session(Some(session)),
            &active_session_keys,
        );
    }

    /// Run the agent loop, dispatching messages as tasks to stay responsive to /stop.
    pub async fn run(self: &Arc<Self>) {
        self.running.store(true, Ordering::Relaxed);
        self.connect_mcp().await;
        log::info!("Agent loop started");
        if self.durability.auto_resume {
            let this = Arc::clone(self);
            tokio::spawn(async move {
                let summary = this.resume_interrupted_turns().await;
                let redriven = this.redrive_pending_inbound();
                log::info!(
                    "journal: {} resumed, {} abandoned, {} stale unfinished turn(s); {redriven} queued message(s) re-driven",
                    summary.resumed,
                    summary.abandoned,
                    summary.stale
                );
            });
        }
        // Local to this call: `_check_expired_sessions_if_due`'s throttle gate
        // (`loop.py`'s `_next_idle_compact_check_at`) only needs to live for
        // one `run()` invocation, so a plain local avoids adding shared
        // mutable state to `AgentLoop` itself.
        let mut next_idle_compact_check_at = Instant::now();
        while self.running.load(Ordering::Relaxed) {
            // `consume_inbound` takes `&self` (the receiver is locked internally),
            // so producers can keep publishing while we wait here.
            let msg = match tokio::time::timeout(Duration::from_secs(1), self.bus.consume_inbound())
                .await
            {
                Ok(Some(msg)) => msg, // got a message
                Ok(None) => break,    // channel closed (all senders dropped)
                Err(_elapsed) => {
                    // Timed out → poll again, but first check whether it's
                    // due to scan for idle sessions to auto-compact.
                    self.check_expired_sessions_if_due(&mut next_idle_compact_check_at)
                        .await;
                    continue;
                }
            };
            let raw = msg.content.trim();
            if self.commands.is_priority(raw) {
                // Priority commands (/stop, /restart) run inline so they can't be
                // queued behind the very work they're meant to interrupt.
                let ctx = CommandContext::with_options(
                    msg.clone(),
                    None,
                    msg.session_key(),
                    raw,
                    "",
                    Some(Arc::clone(self)),
                );
                if let Some(result) = self.commands.dispatch_priority(&ctx).await {
                    self.persist_command_turn(&msg.session_key(), &msg.content, raw, &result);
                    if let Err(error) = self.bus.publish_outbound(result) {
                        log::error!("Failed to publish outbound message: {error}");
                    }
                }
            } else {
                // Everything else (user messages and system messages such as a
                // sub-agent's result) is dispatched as its own task so the loop
                // stays responsive and the task is cancellable via /stop. A long
                // follow-up turn must not hold up the next message, or `/stop`.
                let this = Arc::clone(self);
                let is_system = msg.channel.eq_ignore_ascii_case("system");
                let key = msg.session_key();
                let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
                let active_tasks = Arc::clone(&self.active_tasks);
                let cleanup_key = key.clone();

                // Hold the registry lock across spawn + insert so the task's tail
                // cleanup can't run (and no-op) before the handle is registered.
                let mut map = self.active_tasks.lock().await;
                let handle = tokio::spawn(async move {
                    if is_system {
                        this.dispatch_system(msg).await;
                    } else {
                        this.dispatch(msg).await;
                    }
                    // `add_done_callback` analog: drop our own entry when finished.
                    let mut map = active_tasks.lock().await;
                    if let Some(slot) = map.get_mut(&cleanup_key) {
                        slot.remove(&task_id);
                        if slot.is_empty() {
                            map.remove(&cleanup_key);
                        }
                    }
                });
                map.entry(key).or_default().insert(task_id, handle);
            }
        }
    }

    /// The per-session lock, created on first use. Held for the length of a turn
    /// so a session never runs two at once.
    async fn session_lock(&self, session_key: &str) -> Arc<AsyncMutex<()>> {
        // setdefault: get-or-create the per-session lock, then release the map.
        let mut locks = self.session_locks.lock().await;
        locks
            .entry(session_key.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Wait for this session's turn, then for a place under the global
    /// concurrency cap. Held (dropped) for the length of the turn.
    async fn acquire_turn_slot(&self, session_key: &str) -> TurnSlot {
        // `async with lock, gate:` — serialize this session, then cap global concurrency.
        let session_guard = self.session_lock(session_key).await.lock_owned().await;
        let permit = match &self.concurrency_gate {
            Some(gate) => Some(
                Arc::clone(gate)
                    .acquire_owned()
                    .await
                    .expect("concurrency semaphore closed"),
            ),
            None => None,
        };
        TurnSlot {
            _session_guard: session_guard,
            _permit: permit,
        }
    }

    /// Run the turn for a system message (a sub-agent's result) and publish the
    /// reply. Serialized with the session's other turns, like [`Self::dispatch`],
    /// but on its own task: aborting that task (`/stop`) ends the turn.
    async fn dispatch_system(self: Arc<Self>, msg: InboundMessage) {
        let session_key = msg.session_key();
        let _slot = self.acquire_turn_slot(&session_key).await;
        let processed = AssertUnwindSafe(Arc::clone(&self).process_system_message(msg))
            .catch_unwind()
            .await;
        match processed {
            Ok(Some(response)) => {
                if let Err(error) = self.bus.publish_outbound(response) {
                    log::error!("Failed to publish outbound message: {error}");
                }
            }
            Ok(None) => {}
            Err(panic) => log::error!(
                "Error processing system message for session {session_key}: {}",
                panic
                    .downcast_ref::<&str>()
                    .copied()
                    .unwrap_or("(non-string panic)")
            ),
        }
    }

    /// Process a message: per-session serial, cross-session concurrent.
    async fn dispatch(self: Arc<Self>, msg: InboundMessage) {
        let session_key = msg.session_key();

        let _slot = self.acquire_turn_slot(&session_key).await;

        let mut on_stream: Option<StreamCallback> = None;
        let mut on_stream_end: Option<StreamEndCallback> = None;
        if msg
            .metadata
            .get("_wants_stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // Split one answer into distinct stream segments. The segment counter
            // is shared (and mutated) by both callbacks, so it lives in an atomic
            // (Rust's stand-in for Python's `nonlocal stream_segment`).
            let now_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let stream_base_id = format!("{session_key}:{now_ns}");
            let stream_segment = Arc::new(AtomicUsize::new(0));

            on_stream = Some({
                let bus = Arc::clone(&self.bus);
                let channel = msg.channel.clone();
                let chat_id = msg.chat_id.clone();
                let base_meta = msg.metadata.clone();
                let base_id = stream_base_id.clone();
                let segment = Arc::clone(&stream_segment);
                Arc::new(move |delta: String| {
                    let bus = Arc::clone(&bus);
                    let channel = channel.clone();
                    let chat_id = chat_id.clone();
                    let mut meta = base_meta.clone();
                    let base_id = base_id.clone();
                    let segment = Arc::clone(&segment);
                    Box::pin(async move {
                        let stream_id = format!("{base_id}:{}", segment.load(Ordering::Relaxed));
                        meta.insert("_stream_delta".into(), Value::Bool(true));
                        meta.insert("_stream_id".into(), Value::String(stream_id.clone()));
                        let _ = bus.publish_outbound(OutboundMessage {
                            channel,
                            chat_id,
                            content: delta,
                            reply_to: None,
                            media: vec![],
                            metadata: meta,
                            event: Some(OutboundEvent::StreamDelta(StreamDeltaEvent {
                                stream_id: Some(stream_id),
                            })),
                        });
                    }) as Pin<Box<dyn Future<Output = ()> + Send>>
                }) as StreamCallback
            });

            on_stream_end = Some({
                let bus = Arc::clone(&self.bus);
                let channel = msg.channel.clone();
                let chat_id = msg.chat_id.clone();
                let base_meta = msg.metadata.clone();
                let base_id = stream_base_id.clone();
                let segment = Arc::clone(&stream_segment);
                Arc::new(move |resuming: bool| {
                    let bus = Arc::clone(&bus);
                    let channel = channel.clone();
                    let chat_id = chat_id.clone();
                    let mut meta = base_meta.clone();
                    let base_id = base_id.clone();
                    let segment = Arc::clone(&segment);
                    Box::pin(async move {
                        let stream_id = format!("{base_id}:{}", segment.load(Ordering::Relaxed));
                        meta.insert("_stream_end".into(), Value::Bool(true));
                        meta.insert("_resuming".into(), Value::Bool(resuming));
                        meta.insert("_stream_id".into(), Value::String(stream_id.clone()));
                        let _ = bus.publish_outbound(OutboundMessage {
                            channel,
                            chat_id,
                            content: String::new(),
                            reply_to: None,
                            media: vec![],
                            metadata: meta,
                            event: Some(OutboundEvent::StreamEnd(StreamEndEvent {
                                stream_id: Some(stream_id),
                                resuming,
                                // `merge_next` keeps the channel's per-`stream_id`
                                // buffer. Each stream_end increments
                                // `stream_segment`, so the next deltas use a new
                                // id. Setting this true would leak the old buffer
                                // without helping the next segment. Clients keep
                                // the chat turn open via `resuming` instead.
                                merge_next: false,
                            })),
                        });
                        // `nonlocal stream_segment += 1`
                        segment.fetch_add(1, Ordering::Relaxed);
                    }) as Pin<Box<dyn Future<Output = ()> + Send>>
                }) as StreamEndCallback
            });
        }

        // `try` block. There's no explicit `except asyncio.CancelledError` arm:
        // Tokio cancellation drops this future instead of raising, so it already
        // propagates cleanly. `catch_unwind` only intercepts panics, which is the
        // equivalent of Python's `except Exception`. `/stop` abort is that
        // cancellation path — `CmdStop` publishes `TurnEnd` itself because this
        // future never reaches the match below.
        let started_at = Instant::now();
        let ledger = InboxLedger::for_session(&self.journal_store(), &session_key);
        ledger.mark_processing(&msg);
        let processed = AssertUnwindSafe(Arc::clone(&self).process_message(
            msg.clone(),
            &session_key,
            None,
            on_stream,
            on_stream_end,
        ))
        .catch_unwind()
        .await;

        match processed {
            Ok(Some(response)) => {
                let reply = response.content.clone();
                if let Err(e) = self.bus.publish_outbound(response) {
                    log::error!("Failed to publish outbound message: {e}");
                }
                ledger.mark_done(&msg, &reply, None);
            }
            Ok(None) if msg.channel == "cli" => {
                ledger.mark_done(&msg, "", None);
                let _ = self.bus.publish_outbound(OutboundMessage {
                    channel: msg.channel.clone(),
                    chat_id: msg.chat_id.clone(),
                    content: String::new(),
                    reply_to: None,
                    media: vec![],
                    metadata: msg.metadata.clone(),
                    event: None,
                });
            }
            Ok(None) => ledger.mark_done(&msg, "", None),
            Err(panic) => {
                ledger.mark_done(&msg, "Sorry, I encountered an error.", None);
                log::error!(
                    "Error processing message for session {session_key}: {}",
                    panic
                        .downcast_ref::<&str>()
                        .copied()
                        .unwrap_or("(non-string panic)")
                );
                let _ = self.bus.publish_outbound(OutboundMessage {
                    channel: msg.channel.clone(),
                    chat_id: msg.chat_id.clone(),
                    content: "Sorry, I encountered an error.".to_string(),
                    reply_to: None,
                    media: vec![],
                    metadata: HashMap::new(),
                    event: None,
                });
            }
        }
        // After content (or the lack of it): a separate TurnEnd so streamed
        // replies with `_streamed` still notify channels the turn is done.
        // Title generation is scheduled inside `process_message` so CLI/API
        // `process_direct` (which never goes through `dispatch`) gets it too.
        self.publish_turn_end(&msg, Some(started_at));
    }

    /// Fire-and-forget title generation after a finished turn.
    ///
    /// Skips when the session already has a title or has no user text. The LLM
    /// call runs in [`Self::schedule_background`] so it does not block the
    /// turn or hold the session-manager lock.
    async fn maybe_schedule_title_generation(&self, session_key: &str) {
        let runtime = {
            let manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !manager.session_needs_title(session_key) {
                return;
            }
            let session = manager.get_session_internal(session_key);
            self.runtime_resolver.runtime_for_session(session.as_ref())
        };
        let sessions = Arc::clone(&self.session_manager);
        let key = session_key.to_string();
        self.schedule_background(async move {
            SessionManager::generate_title(sessions.as_ref(), &key, &runtime).await;
        })
        .await;
    }

    /// A progress callback that publishes each progress item (tool hints, narration,
    /// reasoning) to the bus as an outbound message for `channel`/`chat_id`, with
    /// `base_meta` as its metadata.
    fn bus_progress_callback(
        &self,
        channel: &str,
        chat_id: &str,
        base_meta: HashMap<String, Value>,
    ) -> ProgressCallback {
        let bus = Arc::clone(&self.bus);
        let channel = channel.to_string();
        let chat_id = chat_id.to_string();
        let reasoning_stream_id = {
            let now_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{chat_id}:reasoning:{now_ns}")
        };
        Arc::new(move |content: String, kind: ProgressKind| {
            let bus = Arc::clone(&bus);
            let channel = channel.clone();
            let chat_id = chat_id.clone();
            let mut meta = base_meta.clone();
            let reasoning_stream_id = reasoning_stream_id.clone();
            Box::pin(async move {
                let is_reasoning = is_reasoning_progress(kind);
                if is_reasoning {
                    meta.insert(
                        "_stream_id".into(),
                        Value::String(reasoning_stream_id.clone()),
                    );
                } else {
                    meta.insert("_progress".into(), Value::Bool(true));
                    meta.insert(
                        "_tool_hint".into(),
                        Value::Bool(kind == ProgressKind::ToolHint),
                    );
                }
                if let Err(e) = bus.publish_outbound(OutboundMessage {
                    channel,
                    chat_id,
                    content,
                    reply_to: None,
                    media: vec![],
                    metadata: meta,
                    event: Some(OutboundEvent::Progress(ProgressEvent {
                        kind,
                        stream_id: is_reasoning.then_some(reasoning_stream_id),
                        ..ProgressEvent::default()
                    })),
                }) {
                    log::error!("Failed to publish progress message: {e}");
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        })
    }

    /// The progress callback of a system-message turn: [`Self::bus_progress_callback`]
    /// without the reasoning stream, which a WebUI chat has no turn to show on.
    fn system_turn_progress(
        &self,
        channel: &str,
        chat_id: &str,
        base_meta: HashMap<String, Value>,
    ) -> ProgressCallback {
        let publish = self.bus_progress_callback(channel, chat_id, base_meta);
        Arc::new(move |content: String, kind: ProgressKind| {
            let publish = Arc::clone(&publish);
            Box::pin(async move {
                if !is_reasoning_progress(kind) {
                    publish(content, kind).await;
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        })
    }

    /// Handle system-channel messages (checkpoint restore + consolidation).
    ///
    /// Kept separate from [`Self::process_message`] so spawned `dispatch`
    /// tasks stay `Send` (consolidation calls the `?Send` LLM provider).
    /// Establishes a fresh ambient workspace-scope stack for this turn (see
    /// `agent::workspace_context`), then runs
    /// [`Self::process_system_message_inner`].
    pub async fn process_system_message(
        self: Arc<Self>,
        msg: InboundMessage,
    ) -> Option<OutboundMessage> {
        // Counted while the turn runs, so channels can say the agent is busy with
        // a result instead of looking idle. Dropped on every exit, `/stop` included.
        let (channel, chat_id) = split_system_chat_id(&msg.chat_id);
        let _working = self
            .subagents
            .activity()
            .begin(&format!("{channel}:{chat_id}"), ActivityKind::FollowUp);
        with_workspace_scope_stack(move || self.process_system_message_inner(msg)).await
    }

    async fn process_system_message_inner(
        self: Arc<Self>,
        msg: InboundMessage,
    ) -> Option<OutboundMessage> {
        let (channel, chat_id) = split_system_chat_id(&msg.chat_id);
        log::info!("Processing system message from {}", msg.sender_id);
        let key = format!("{channel}:{chat_id}");

        // Restore any runtime checkpoint (a partially completed turn from a crash
        // or interrupt). Persist only when something was actually restored.
        {
            let mut manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let journals = manager.journal_store();
            let session = manager.get_or_create_session(&key);
            if self.restore_runtime_checkpoint(session, &journals) {
                let restored = session.clone();
                if let Err(e) = manager.save(restored) {
                    log::error!("Failed to save restored session: {e}");
                }
            }
        }

        // Consolidate if the session history is getting large. The consolidator
        // mutates the stored session in place via the shared session manager.
        self.consolidator.maybe_consolidate_by_tokens(&key).await;

        // Route tool output (e.g. `message`, `spawn`) back to the originating chat.
        self.set_tool_context(
            channel,
            chat_id,
            msg.metadata.get("message_id").and_then(Value::as_str),
        );

        // Re-read the session AFTER consolidation so history reflects any archiving.
        let snapshot = {
            let mut manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            manager.get_or_create_session(&key).clone()
        };
        // A reply to a sub-agent's result has no user turn to inherit the WebUI
        // markers from; without them the websocket channel would deliver it live
        // but never record it in the chat's transcript, and a refresh or a switch
        // to another chat would lose it.
        let webui_turn_metadata = webui_system_turn_metadata(channel, &snapshot);
        let mut turn_metadata = msg.metadata.clone();
        turn_metadata.extend(webui_turn_metadata.clone());
        self.apply_message_tool_inbound_metadata(&turn_metadata);
        // Show the turn's tool calls to a WebUI chat while it runs.
        let on_progress = (!webui_turn_metadata.is_empty())
            .then(|| self.system_turn_progress(channel, chat_id, turn_metadata.clone()));
        // Pick up any idle-compaction summary produced since this session was
        // last used, refreshing `snapshot` again if a background archive is
        // still in flight (or the session still looks idle-expired).
        let (mut snapshot, pending_summary) = self.auto_compact.prepare_session(snapshot, &key);

        let history = snapshot.get_history(Some(0));
        let runtime_context_blocks =
            crate::runtime_context::runtime_context_blocks_from_metadata(&msg.metadata);
        let messages = self.context.build_messages(
            history.as_slice(),
            msg.content.as_str(),
            None,
            None,
            Some(channel),
            Some(chat_id),
            Some(&snapshot.metadata),
            (!runtime_context_blocks.is_empty()).then_some(runtime_context_blocks.as_slice()),
            SUBAGENT_ANNOUNCE_ROLE,
            pending_summary.as_deref(),
        );
        let agent_run_result = self
            .run_agent_loop(
                messages,
                on_progress,
                None,
                None,
                Some(snapshot.clone()),
                channel,
                chat_id,
                msg.metadata.get("message_id").and_then(Value::as_str),
                0,
                None,
            )
            .await;
        let final_content = agent_run_result.final_content;
        let mut all_msgs = agent_run_result.messages;
        // After the system prompt and the history comes the sub-agent result.
        let announce_index = 1 + history.len();
        mark_announcement_hidden(&mut all_msgs, announce_index);

        self.persist_finished_turn(
            &mut snapshot,
            all_msgs.as_slice(),
            announce_index as u32,
            agent_run_result.usage,
            "processing system message",
        );

        // Schedule background consolidation (Python's `_schedule_background`).
        let consolidator = Arc::clone(&self.consolidator);
        let key = snapshot.key.clone();
        self.schedule_background(async move {
            consolidator.maybe_consolidate_by_tokens(&key).await;
        })
        .await;

        Some(OutboundMessage {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            content: final_content.unwrap_or_else(|| "Background task completed.".to_string()),
            reply_to: None,
            media: vec![],
            metadata: webui_turn_metadata,
            event: None,
        })
    }

    /// Persist a slash-command turn so it is in session history but filtered
    /// out of LLM context by [`Session::get_history`]. `/new` is skipped because
    /// it clears the session.
    fn persist_command_turn(
        &self,
        session_key: &str,
        user_content: &str,
        raw_command: &str,
        reply: &OutboundMessage,
    ) {
        if raw_command
            .trim()
            .eq_ignore_ascii_case(&ChatCommand::New.to_string())
        {
            return;
        }
        let mut extras = serde_json::Map::new();
        extras.insert(COMMAND_KEY.to_string(), Value::Bool(true));
        let mut session_manager = self
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let snapshot = {
            let session = session_manager.get_or_create_session(session_key);
            session.add_message("user", user_content, extras.clone());
            session.add_message("assistant", reply.content.clone(), extras);
            session.clone()
        };
        if let Err(e) = session_manager.save(snapshot) {
            log::error!("Failed to save command turn for session {session_key}: {e}");
        }
    }

    /// Persist a finished turn: fold this run's usage into `session`, append
    /// messages, drop the runtime checkpoint, and write JSONL.
    ///
    /// `session` is the pre-run clone held by the caller. [`Self::run_agent_loop`]
    /// already writes usage onto the session-manager cache, but saving this
    /// clone afterward would replace that cache entry and wipe `token_usage`
    /// from disk (title generation then re-saves the wiped metadata, which is
    /// why a session file can have `title` and no usage). Apply usage here so
    /// the snapshot we persist keeps the totals.
    fn persist_finished_turn(
        &self,
        session: &mut Session,
        messages: &[Value],
        skip: u32,
        usage: LLMUsage,
        save_error_context: &str,
    ) {
        session.update_usage(usage);
        self.carry_loaded_tools_from_live_session(session);
        self.save_turn(session, messages, skip);
        self.clear_runtime_checkpoint(session);
        if let Err(e) = self
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .save(session.clone())
        {
            log::error!("Failed to save session after {save_error_context}: {e}");
        }
    }

    /// Copy the `loaded_tools` `tool_search` wrote to the live session during the turn into
    /// `session`, the snapshot taken when the turn started, so saving the snapshot does
    /// not drop the loads.
    fn carry_loaded_tools_from_live_session(&self, session: &mut Session) {
        let live_loaded_tools = self
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_session_internal(&session.key)
            .map(|live| live.loaded_tools())
            .unwrap_or_default();
        session.record_loaded_tools(&live_loaded_tools);
    }

    /// Save new-turn messages into session, truncating large tool results.
    fn save_turn(&self, session: &mut Session, messages: &[Value], skip: u32) {
        let max_tool_result_chars = self.max_tool_result_chars;
        for message in messages[skip as usize..].iter() {
            let Some(mut entry) = message.as_object().cloned() else {
                continue;
            };
            let role = entry.get("role").and_then(Value::as_str).unwrap_or("");
            // Mirror Python `not content` (None, "", [], null are empty; strings/arrays are not).
            let empty_content = match entry.get("content") {
                None | Some(Value::Null) => true,
                Some(Value::String(s)) => s.is_empty(),
                Some(Value::Array(a)) => a.is_empty(),
                _ => false,
            };
            let has_tool_calls = entry
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|tc| !tc.is_empty());
            if role == "assistant" && empty_content && !has_tool_calls {
                continue;
            }
            if role == "tool" {
                match entry.get("content") {
                    Some(Value::String(content))
                        if content.len() > max_tool_result_chars as usize =>
                    {
                        entry.insert(
                            "content".into(),
                            Value::String(truncate_text(content, max_tool_result_chars as usize)),
                        );
                    }
                    Some(Value::Array(blocks)) => {
                        let filtered =
                            self.sanitize_persisted_blocks(blocks.as_slice(), true, false);
                        if filtered.is_empty() {
                            continue;
                        }
                        entry.insert("content".into(), Value::Array(filtered));
                    }
                    _ => {}
                }
            } else if role == "user" {
                match entry.get("content") {
                    Some(Value::String(content)) if content.starts_with(RUNTIME_CONTEXT_TAG) => {
                        // Strip the runtime-context prefix, keep only the user text.
                        let parts = content.splitn(2, "\n\n").collect::<Vec<&str>>();
                        if parts.len() > 1 && !parts[1].trim().is_empty() {
                            entry.insert("content".into(), Value::String(parts[1].to_string()));
                        } else {
                            continue;
                        }
                    }
                    Some(Value::Array(blocks)) => {
                        let filtered =
                            self.sanitize_persisted_blocks(blocks.as_slice(), false, true);
                        if filtered.is_empty() {
                            continue;
                        }
                        entry.insert("content".into(), Value::Array(filtered));
                    }
                    _ => {}
                }
            }
            if entry.get("timestamp").is_none() {
                entry.insert("timestamp".into(), Value::String(Utc::now().to_rfc3339()));
            }
            session.messages.push(Value::Object(entry));
        }
        session.updated_at = Utc::now();
    }

    /// Strip volatile multimodal payloads before writing session history.
    fn sanitize_persisted_blocks(
        &self,
        content: &[Value],
        should_truncate_text: bool,
        drop_runtime: bool,
    ) -> Vec<Value> {
        let mut filtered = Vec::new();
        for block in content {
            let Some(block_obj) = block.as_object() else {
                filtered.push(block.clone());
                continue;
            };

            if drop_runtime
                && block_obj.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = block_obj.get("text").and_then(Value::as_str)
                && text.starts_with(RUNTIME_CONTEXT_TAG)
            {
                continue;
            }

            if block_obj.get("type").and_then(Value::as_str) == Some("image_url") {
                let url = block_obj
                    .get("image_url")
                    .and_then(Value::as_object)
                    .and_then(|iu| iu.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if url.starts_with("data:image/") {
                    let path = block_obj
                        .get("_meta")
                        .and_then(Value::as_object)
                        .and_then(|m| m.get("path"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty());
                    filtered.push(serde_json::json!({
                        "type": "text",
                        "text": image_placeholder_text(path, "[image]"),
                    }));
                    continue;
                }
            }

            if block_obj.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = block_obj.get("text").and_then(Value::as_str)
            {
                let text =
                    if should_truncate_text && text.len() > self.max_tool_result_chars as usize {
                        truncate_text(text, self.max_tool_result_chars as usize)
                    } else {
                        text.to_string()
                    };
                let mut updated = block_obj.clone();
                updated.insert("text".into(), Value::String(text));
                filtered.push(Value::Object(updated));
                continue;
            }

            filtered.push(block.clone());
        }
        filtered
    }

    /// Process a single inbound message and return the response.
    /// The type of this message should not be "system".
    /// Establishes a fresh ambient workspace-scope stack for this turn (see
    /// `agent::workspace_context`), then runs [`Self::process_message_inner`].
    /// Mirrors how `cli::commands::run_gateway` wraps each cron job body in
    /// `with_cron_context_stack`.
    ///
    /// Title generation is scheduled here (not in [`Self::dispatch`]) so every
    /// caller — gateway bus, CLI/API `process_direct`, cron, heartbeat — gets
    /// a title after the first user turn.
    async fn process_message(
        self: Arc<Self>,
        msg: InboundMessage,
        session_key: &str,
        on_progress: Option<ProgressCallback>,
        on_stream: Option<StreamCallback>,
        on_stream_end: Option<StreamEndCallback>,
    ) -> Option<OutboundMessage> {
        let title_key = if session_key.is_empty() {
            msg.session_key()
        } else {
            session_key.to_string()
        };
        let this = Arc::clone(&self);
        let result = with_workspace_scope_stack(move || {
            self.process_message_inner(msg, session_key, on_progress, on_stream, on_stream_end)
        })
        .await;
        this.maybe_schedule_title_generation(&title_key).await;
        result
    }

    async fn process_message_inner(
        self: Arc<Self>,
        msg: InboundMessage,
        session_key: &str,
        on_progress: Option<ProgressCallback>,
        on_stream: Option<StreamCallback>,
        on_stream_end: Option<StreamEndCallback>,
    ) -> Option<OutboundMessage> {
        let preview = if msg.content.len() > 80 {
            format!("{}...", msg.content.chars().take(80).collect::<String>())
        } else {
            msg.content.clone()
        };
        log::info!(
            "Processing message from {}:{}: {}. Media: {}",
            msg.channel,
            msg.sender_id,
            preview,
            msg.media.join(",")
        );

        let key = if session_key.is_empty() {
            msg.session_key()
        } else {
            session_key.to_string()
        };

        // Restore any runtime checkpoint and snapshot the session. Scope the guard
        // so the `!Send` `MutexGuard` is dropped before the `.await`s below.
        let session = {
            let mut session_manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let journals = session_manager.journal_store();
            let session = session_manager.get_or_create_session(&key);
            let restored = self.restore_runtime_checkpoint(session, &journals);
            // End the `&mut session` borrow before re-borrowing the manager to save.
            let snapshot = session.clone();
            if restored && let Err(e) = session_manager.save(snapshot.clone()) {
                log::error!("Failed to save restored session: {e}");
            }
            snapshot
        };

        // Slash commands
        let raw = msg.content.trim();
        let mut ctx = CommandContext {
            msg: msg.clone(),
            session: Some(session.clone()),
            key: key.clone(),
            raw: raw.to_string(),
            args: String::new(),
            agent_loop: Some(self.clone()),
        };
        // Priority commands (/stop, /restart, /status) are normally handled inline by
        // `run()`'s bus loop before it ever reaches `dispatch()`. Callers that skip the
        // bus (API, CLI `process_direct`) still need them to be recognized here.
        if self.commands.is_priority(raw)
            && let Some(result) = self.commands.dispatch_priority(&ctx).await
        {
            self.persist_command_turn(&key, &msg.content, raw, &result);
            return Some(result);
        }
        if let Some(result) = self.commands.dispatch(&mut ctx).await {
            self.persist_command_turn(&key, &msg.content, raw, &result);
            return Some(result);
        }
        self.consolidator.maybe_consolidate_by_tokens(&key).await;
        self.set_tool_context(
            msg.channel.as_str(),
            msg.chat_id.as_str(),
            msg.metadata.get("message_id").and_then(Value::as_str),
        );
        self.apply_message_tool_inbound_metadata(&msg.metadata);
        if let Some(message_tool) = self
            .tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get("message")
        {
            // `isinstance(message_tool, MessageTool)` → downcast the trait object.
            if let Some(message_tool) =
                (message_tool.as_ref() as &dyn std::any::Any).downcast_ref::<MessageTool>()
            {
                message_tool.start_turn();
            }
        }

        // Re-read the session AFTER consolidation so history reflects any archiving.
        let session = {
            let mut session_manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager.get_or_create_session(&key).clone()
        };
        // Pick up any idle-compaction summary produced since this session was
        // last used, refreshing `session` again if a background archive is
        // still in flight (or the session still looks idle-expired).
        let (mut session, pending_summary) = self.auto_compact.prepare_session(session, &key);
        let history = session.get_history(Some(0));
        let media = if !msg.media.is_empty() {
            Some(msg.media.as_slice())
        } else {
            None
        };
        log::info!("Building messages with media: {}", media.is_some());
        let runtime_context_blocks =
            crate::runtime_context::runtime_context_blocks_from_metadata(&msg.metadata);
        let initial_messages = self.context.build_messages(
            history.as_slice(),
            msg.content.as_str(),
            None,
            media,
            Some(msg.channel.as_str()),
            Some(msg.chat_id.as_str()),
            Some(&session.metadata),
            (!runtime_context_blocks.is_empty()).then_some(runtime_context_blocks.as_slice()),
            DEFAULT_CURRENT_ROLE,
            pending_summary.as_deref(),
        );

        let bus_progress =
            self.bus_progress_callback(&msg.channel, &msg.chat_id, msg.metadata.clone());

        let result = self
            .run_agent_loop(
                initial_messages,
                Some(on_progress.unwrap_or(bus_progress)),
                on_stream.clone(),
                on_stream_end,
                Some(session.clone()),
                msg.channel.as_str(),
                msg.chat_id.as_str(),
                msg.metadata.get("message_id").and_then(Value::as_str),
                0,
                msg.request_id.clone(),
            )
            .await;
        let mut final_content = result.final_content.unwrap_or_default();
        let all_msgs = result.messages;
        let stop_reason = result.stop_reason.as_str();

        if final_content.trim().is_empty() {
            final_content = EMPTY_FINAL_RESPONSE_MESSAGE.to_string();
        }
        self.persist_finished_turn(
            &mut session,
            &all_msgs,
            1 + history.len() as u32,
            result.usage,
            "processing message",
        );
        let consolidator = Arc::clone(&self.consolidator);
        let consolidate_key = key.clone();
        self.schedule_background(async move {
            consolidator
                .maybe_consolidate_by_tokens(&consolidate_key)
                .await;
        })
        .await;
        if stop_reason == CIRCUIT_BREAKER_STOP_REASON {
            log::warn!("Message circuit breaker tripped; delivering stop notice");
        }
        if stop_reason != CIRCUIT_BREAKER_STOP_REASON
            && let Some(message_tool) = self
                .tools
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get("message")
            && let Some(message_tool) =
                (message_tool.as_ref() as &dyn std::any::Any).downcast_ref::<MessageTool>()
            && *message_tool
                .sent_in_turn
                .lock()
                .unwrap_or_else(|e| e.into_inner())
        {
            return None;
        }
        let limit: usize = 120;
        let preview = if final_content.len() > limit {
            format!(
                "{}...",
                final_content.get(..limit).unwrap_or(&final_content)
            )
        } else {
            final_content.clone()
        };
        log::info!("Response to {}:{}: {}", msg.channel, msg.sender_id, preview);

        let mut meta = msg.metadata.clone();
        let event = if on_stream.is_some() {
            meta.insert("_streamed".into(), Value::Bool(true));
            Some(OutboundEvent::StreamedResponse(StreamedResponseEvent))
        } else {
            None
        };
        let mut outbound = OutboundMessage {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            content: final_content,
            reply_to: None,
            media: vec![],
            metadata: meta,
            event,
        };
        Self::copy_token_usage_to_outbound(&mut outbound, result.usage);
        Some(outbound)
    }

    /// Stable identity tuple for a message, used to detect overlap between the
    /// restored checkpoint messages and what's already in session history.
    fn checkpoint_message_key(message: &Value) -> [Option<&Value>; 7] {
        [
            message.get("role"),
            message.get("content"),
            message.get("tool_call_id"),
            message.get("name"),
            message.get("tool_calls"),
            message.get("reasoning_content"),
            message.get("thinking_blocks"),
        ]
    }

    /// Materialize an unfinished turn into session history before a new request.
    ///
    /// Reconstructs the assistant message, any completed tool results, and a
    /// synthetic "interrupted" result for every pending tool call, then appends
    /// only the portion not already present (suffix/prefix overlap detection
    /// keeps re-materialization idempotent). Returns whether anything was
    /// restored.
    /// Merge the unfinished part of an interrupted turn into `session`.
    ///
    /// The run journal in `journals` is the source of truth; the
    /// `RUNTIME_CHECKPOINT_KEY` metadata checkpoint is only read as a fallback for
    /// sessions written before the journal existed. Returns whether the session
    /// changed. Never executes a tool: calls that were cut off get their journaled
    /// outcome or an "interrupted" result.
    fn restore_runtime_checkpoint(
        &self,
        session: &mut Session,
        journals: &Arc<dyn JournalStore>,
    ) -> bool {
        let from_journal = self.restore_from_journal(session, journals);
        if from_journal {
            // The journal wins: a leftover metadata checkpoint must not restore it twice.
            self.clear_runtime_checkpoint(session);
            return true;
        }
        self.restore_from_metadata_checkpoint(session)
    }

    /// Journal half of [`Self::restore_runtime_checkpoint`].
    fn restore_from_journal(
        &self,
        session: &mut Session,
        journals: &Arc<dyn JournalStore>,
    ) -> bool {
        let journal = RunJournal::new(Arc::clone(journals), &session.key);
        let entries = journal.scan().entries;
        let mut restored_any = false;
        for turn_id in recovery::turns_to_restore(&entries) {
            let turn_messages = recovery::turn_messages_for_restore(&entries, &turn_id);
            log::warn!(
                "restoring interrupted turn {turn_id} of session {}: {} message(s)",
                session.key,
                turn_messages.len()
            );
            if !turn_messages.is_empty() {
                // `save_turn` skips the leading history; here there is none.
                let framed: Vec<Value> =
                    std::iter::once(Value::Null).chain(turn_messages).collect();
                self.save_turn(session, &framed, 1);
                restored_any = true;
            }
            if let Err(e) = journal.append(JournalRecord::TurnRestored { turn_id }) {
                log::error!("Failed to mark turn restored in journal: {e}");
            }
        }
        restored_any
    }

    /// Metadata-checkpoint half of [`Self::restore_runtime_checkpoint`] (pre-journal sessions).
    fn restore_from_metadata_checkpoint(&self, session: &mut Session) -> bool {
        let Some(checkpoint) = session
            .metadata
            .get(RUNTIME_CHECKPOINT_KEY)
            .and_then(Value::as_object)
        else {
            return false;
        };

        let mut restored_messages = checkpoint_payload::materialize(checkpoint, |_| {
            checkpoint_payload::INTERRUPTED_RESULT.to_string()
        });

        // Find the longest history suffix that already matches the restored
        // prefix; only the non-overlapping tail is appended.
        let mut overlap = 0;
        let max_overlap = session.messages.len().min(restored_messages.len());
        for size in (1..=max_overlap).rev() {
            let existing = &session.messages[session.messages.len() - size..];
            let restored = &restored_messages[..size];
            if existing.iter().zip(restored).all(|(left, right)| {
                Self::checkpoint_message_key(left) == Self::checkpoint_message_key(right)
            }) {
                overlap = size;
                break;
            }
        }
        session
            .messages
            .extend(restored_messages.split_off(overlap));

        self.clear_runtime_checkpoint(session);
        true
    }

    fn clear_runtime_checkpoint(&self, session: &mut Session) {
        session.metadata.remove(RUNTIME_CHECKPOINT_KEY);
    }

    /// Schedule a future as a tracked background task (drained by [`Self::close_mcp`]).
    ///
    /// Returns once the task handle is registered; the future itself runs in the
    /// background (Python's `_schedule_background` analog).
    async fn schedule_background<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task_id = self.next_background_task_id.fetch_add(1, Ordering::Relaxed);
        let bg = Arc::clone(&self.background_tasks);

        let handle = tokio::spawn(async move {
            future.await;
            let mut tasks = bg.lock().await;
            tasks.remove(&task_id);
        });

        self.background_tasks.lock().await.insert(task_id, handle);
    }

    /// Drain pending background tasks, then close MCP connections.
    ///
    /// Mirrors Python's `close_mcp`: `asyncio.gather(*_background_tasks)` followed
    /// by `_mcp_stack.aclose()`. MCP sessions use RAII — clearing the vec drops
    /// [`LoadedMcpTools`] and closes each connection.
    pub async fn close_mcp(&self) {
        let handles: Vec<JoinHandle<()>> = {
            let mut tasks = self.background_tasks.lock().await;
            tasks.drain().map(|(_, handle)| handle).collect()
        };
        for handle in handles {
            let _ = handle.await;
        }

        self.mcp_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self.mcp_state.lock().unwrap_or_else(|e| e.into_inner()) = McpConnectionState::default();
        self.mcp_connected.store(false, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        log::info!("Agent loop stopping ... shutting down");
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn process_direct(
        self: Arc<Self>,
        content: &str,
        session_key: Option<&str>,
        channel: Option<&str>,
        chat_id: Option<&str>,
        media: Option<Vec<String>>,
        on_progress: Option<ProgressCallback>,
        on_stream: Option<StreamCallback>,
        on_stream_end: Option<StreamEndCallback>,
    ) -> Option<OutboundMessage> {
        let session_key = session_key.unwrap_or("cli:direct");
        let channel = channel.unwrap_or("cli");
        let chat_id = chat_id.unwrap_or("direct");

        self.connect_mcp().await;
        let media = media.unwrap_or_default();
        if !media.is_empty() {
            for media_url in &media {
                log::info!("Processing direct message with media: {}", media_url);
            }
        }
        let msg = InboundMessage {
            content: content.to_string(),
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            sender_id: "user".to_string(),
            media,
            timestamp: Utc::now(),
            session_key_override: None,
            request_id: None,
            metadata: HashMap::new(),
        };
        self.process_message(msg, session_key, on_progress, on_stream, on_stream_end)
            .await
    }

    fn copy_token_usage_to_outbound(outbound: &mut OutboundMessage, usage: LLMUsage) {
        if usage == LLMUsage::new() {
            return;
        }
        let mut usage_obj = match serde_json::to_value(usage) {
            Ok(Value::Object(map)) => map,
            _ => return,
        };
        if let Some(n) = usage.prompt_tokens() {
            usage_obj.insert("prompt_tokens".into(), Value::from(n));
        }
        if let Some(n) = usage.output_tokens {
            usage_obj.insert("completion_tokens".into(), Value::from(n));
        }
        if let Some(n) = usage.total_tokens() {
            usage_obj.insert("total_tokens".into(), Value::from(n));
        }
        if let Some(n) = usage.cache_read_input_tokens {
            usage_obj.insert("cached_tokens".into(), Value::from(n));
        }
        outbound.metadata.insert(
            OutboundMessage::TOKEN_USAGE_KEY.into(),
            Value::Object(usage_obj),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::providers::base::{GenerationSettings, LLMResponse};
    use serde_json::json;

    fn mcp_servers(names: &[&str]) -> HashMap<String, McpServerConfig> {
        names
            .iter()
            .map(|name| (name.to_string(), McpServerConfig::default()))
            .collect()
    }

    /// A registry of probe built-ins named `names`.
    fn builtin_registry(names: &[&'static str]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for name in names {
            registry.register(Box::new(CountingProbe {
                name,
                safe: false,
                runs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }));
        }
        registry
    }

    fn to_strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn deferrable_builtins_keeps_registered_builtins_only() {
        let builtins = builtin_registry(&["docx", "read_file"]);

        let deferrable = deferrable_builtins(
            &to_strings(&["docx", "mcp_jira_search", "ghost"]),
            &[],
            &builtins,
        );

        assert_eq!(deferrable, to_strings(&["docx"]));
    }

    #[test]
    fn deferrable_builtins_lets_disabled_win() {
        // A disabled tool is no longer registered, so only the disabled check can name it.
        let builtins = builtin_registry(&["read_file"]);

        let deferrable =
            deferrable_builtins(&to_strings(&["docx"]), &to_strings(&["docx"]), &builtins);

        assert!(deferrable.is_empty());
    }

    #[test]
    fn pending_mcp_servers_skips_connected_ones_and_sorts_by_name() {
        let configured = mcp_servers(&["zeta", "alpha", "playwright"]);
        let connected = HashSet::from(["playwright".to_string()]);

        let pending = pending_mcp_servers(&configured, &connected);

        let names: Vec<&str> = pending.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn enabled_mcp_servers_drops_servers_switched_off() {
        let mut configured = mcp_servers(&["on", "off"]);
        configured.get_mut("off").unwrap().enabled = false;

        let enabled = enabled_mcp_servers(&configured);

        assert_eq!(enabled.keys().collect::<Vec<_>>(), vec!["on"]);
    }

    #[test]
    fn pending_mcp_servers_is_empty_once_everything_is_connected() {
        let configured = mcp_servers(&["a", "b"]);
        let connected = HashSet::from(["a".to_string(), "b".to_string()]);
        assert!(pending_mcp_servers(&configured, &connected).is_empty());
    }

    #[test]
    fn mcp_retry_is_due_before_the_first_attempt() {
        assert!(mcp_retry_due(None, Instant::now(), MCP_RETRY_INTERVAL));
    }

    #[test]
    fn mcp_retry_waits_for_the_interval_after_an_attempt() {
        let attempt = Instant::now();
        let interval = Duration::from_secs(60);
        assert!(!mcp_retry_due(
            Some(attempt),
            attempt + Duration::from_secs(59),
            interval
        ));
        assert!(mcp_retry_due(
            Some(attempt),
            attempt + Duration::from_secs(60),
            interval
        ));
    }

    /// Minimal provider placeholder until `AgentLoop` is fully wired.
    struct PlaceholderProvider {
        settings: GenerationSettings,
    }

    #[async_trait]
    impl LLMProviderDyn for PlaceholderProvider {
        fn api_key(&self) -> Option<String> {
            None
        }
        fn api_base(&self) -> Option<String> {
            None
        }
        fn extra_headers(&self) -> Option<std::collections::HashMap<String, String>> {
            None
        }
        fn generation_settings(&self) -> &GenerationSettings {
            &self.settings
        }
        fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
            &mut self.settings
        }
        fn spec(&self) -> Option<&crate::providers::registry::ProviderSpec> {
            None
        }
        fn get_default_model(&self) -> String {
            String::new()
        }
        async fn chat(
            &self,
            _: Vec<serde_json::Value>,
            _: Option<Vec<serde_json::Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<serde_json::Value>,
        ) -> LLMResponse {
            LLMResponse::new()
        }
        async fn safe_chat(
            &self,
            _: Vec<serde_json::Value>,
            _: Option<Vec<serde_json::Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<serde_json::Value>,
        ) -> LLMResponse {
            LLMResponse::new()
        }
        async fn chat_with_retry(
            &self,
            _: Vec<serde_json::Value>,
            _: Option<Vec<serde_json::Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<serde_json::Value>,
        ) -> LLMResponse {
            LLMResponse::new()
        }
        async fn chat_stream_with_retry_boxed(
            &self,
            _: Vec<serde_json::Value>,
            _: Option<Vec<serde_json::Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<serde_json::Value>,
            _: Option<crate::providers::base::BoxedStreamCallback>,
            _: Option<crate::providers::base::BoxedProgressCallback>,
        ) -> LLMResponse {
            LLMResponse::new()
        }
    }

    fn make_ctx() -> AgentHookContext {
        AgentHookContext::new(1, vec![])
    }

    // ── LoopHookChain ─────────────────────────────────────────────────────────

    #[derive(Default)]
    struct OrderRecordingHook {
        calls: Arc<Mutex<Vec<String>>>,
        label: &'static str,
    }

    #[async_trait]
    impl AgentHook for OrderRecordingHook {
        async fn on_stream_end(&self, _ctx: &mut AgentHookContext, _resuming: bool) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:on_stream_end", self.label));
        }

        async fn before_execute_tools(&self, _ctx: &mut AgentHookContext) -> ToolHookDecision {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:before_execute_tools", self.label));
            ToolHookDecision::Continue
        }

        async fn after_iteration(&self, _ctx: &mut AgentHookContext) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:after_iteration", self.label));
        }

        fn finalize_content(
            &self,
            _ctx: &AgentHookContext,
            content: Option<String>,
        ) -> Option<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:finalize_content", self.label));
            content.map(|s| format!("{}:{}", self.label, s))
        }
    }

    fn loop_hook_chain_fixture() -> (LoopHookChain, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let primary_calls = Arc::clone(&calls);
        let extra_calls = Arc::clone(&calls);
        let chain = LoopHookChain::new(
            Arc::new(OrderRecordingHook {
                calls: primary_calls,
                label: "primary",
            }),
            vec![Arc::new(OrderRecordingHook {
                calls: extra_calls,
                label: "extra",
            })],
        );
        (chain, calls)
    }

    #[tokio::test]
    async fn test_loop_hook_chain_runs_primary_before_extras() {
        let (chain, calls) = loop_hook_chain_fixture();
        let mut ctx = make_ctx();

        chain.on_stream_end(&mut ctx, true).await;
        chain.before_execute_tools(&mut ctx).await;
        chain.after_iteration(&mut ctx).await;

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "primary:on_stream_end".to_string(),
                "extra:on_stream_end".to_string(),
                "primary:before_execute_tools".to_string(),
                "extra:before_execute_tools".to_string(),
                "primary:after_iteration".to_string(),
                "extra:after_iteration".to_string(),
            ]
        );
    }

    struct DecisionRecordingHook {
        calls: Arc<Mutex<Vec<String>>>,
        label: &'static str,
        decision: ToolHookDecision,
    }

    #[async_trait]
    impl AgentHook for DecisionRecordingHook {
        async fn before_execute_tools(&self, _ctx: &mut AgentHookContext) -> ToolHookDecision {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:before_execute_tools", self.label));
            self.decision.clone()
        }
    }

    #[tokio::test]
    async fn test_loop_hook_chain_abort_skips_extras() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let chain = LoopHookChain::new(
            Arc::new(DecisionRecordingHook {
                calls: Arc::clone(&calls),
                label: "primary",
                decision: ToolHookDecision::Abort {
                    reason: "stop".into(),
                },
            }),
            vec![Arc::new(DecisionRecordingHook {
                calls: Arc::clone(&calls),
                label: "extra",
                decision: ToolHookDecision::Continue,
            })],
        );
        let mut ctx = make_ctx();
        let decision = chain.before_execute_tools(&mut ctx).await;
        assert_eq!(
            decision,
            ToolHookDecision::Abort {
                reason: "stop".into()
            }
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["primary:before_execute_tools".to_string()]
        );
    }

    #[tokio::test]
    async fn test_loop_hook_chain_merges_primary_deny_with_extras() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let chain = LoopHookChain::new(
            Arc::new(DecisionRecordingHook {
                calls: Arc::clone(&calls),
                label: "primary",
                decision: ToolHookDecision::DenyCalls {
                    ids: vec!["a".into()],
                    reason: "primary".into(),
                },
            }),
            vec![Arc::new(DecisionRecordingHook {
                calls: Arc::clone(&calls),
                label: "extra",
                decision: ToolHookDecision::DenyCalls {
                    ids: vec!["b".into()],
                    reason: "extra".into(),
                },
            })],
        );
        let mut ctx = make_ctx();
        let decision = chain.before_execute_tools(&mut ctx).await;
        assert_eq!(
            decision,
            ToolHookDecision::DenyCalls {
                ids: vec!["a".into(), "b".into()],
                reason: "primary; extra".into(),
            }
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "primary:before_execute_tools".to_string(),
                "extra:before_execute_tools".to_string(),
            ]
        );
    }

    #[test]
    fn test_loop_hook_chain_finalize_content_pipelines_primary_then_extras() {
        let (chain, calls) = loop_hook_chain_fixture();
        let ctx = make_ctx();

        let result = chain.finalize_content(&ctx, Some("hello".into()));

        assert_eq!(result, Some("extra:primary:hello".into()));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "primary:finalize_content".to_string(),
                "extra:finalize_content".to_string(),
            ]
        );
    }

    // ── save_turn ─────────────────────────────────────────────────────────────

    fn make_save_turn_loop(max_tool_result_chars: u32) -> Arc<AgentLoop> {
        let bus = Arc::new(MessageBus::new());
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(PlaceholderProvider {
            settings: GenerationSettings::new(),
        });
        let mut config = Config::default();
        config.agents.max_tool_result_chars = max_tool_result_chars;
        Arc::new(AgentLoop::new(
            bus,
            provider,
            std::env::temp_dir(),
            config,
            None,
            None,
            None,
        ))
    }

    // ── journal-backed restore ────────────────────────────────────────────────

    fn make_loop_in(workspace: &Path) -> (Arc<AgentLoop>, Arc<dyn JournalStore>) {
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(PlaceholderProvider {
            settings: GenerationSettings::new(),
        });
        let loop_ = Arc::new(AgentLoop::new(
            Arc::new(MessageBus::new()),
            provider,
            workspace.to_path_buf(),
            Config::default(),
            None,
            None,
            None,
        ));
        let journals = loop_.journal_store();
        (loop_, journals)
    }

    fn two_call_turn() -> Vec<Value> {
        let call = |id: &str| json!({"id": id, "type": "function", "function": {"name": "t", "arguments": "{}"}});
        vec![
            json!({"role": "user", "content": "do two things"}),
            json!({"role": "assistant", "content": null, "tool_calls": [call("a"), call("b")]}),
        ]
    }

    /// Journal of a turn that died with call `a` finished and call `b` in flight.
    fn write_crashed_turn(
        journals: &Arc<dyn JournalStore>,
        key: &str,
        outcome: Option<TurnOutcome>,
    ) {
        use crate::session::journal::{StepKind, ToolCallStatus};
        let journal = RunJournal::new(Arc::clone(journals), key);
        let turn = two_call_turn();
        journal
            .append(JournalRecord::TurnStart {
                turn_id: "t1".into(),
                session_key: key.into(),
                request_id: None,
                user_message: Some(turn[0].clone()),
                attempt: 0,
                channel: None,
                chat_id: None,
            })
            .unwrap();
        journal
            .append(JournalRecord::StepCheckpoint {
                turn_id: "t1".into(),
                step: StepKind::PreTool,
                payload: checkpoint_payload::build("awaiting_tools", &turn),
            })
            .unwrap();
        let tool = |call_id: &str, status, result: Option<&str>| JournalRecord::ToolIntent {
            turn_id: "t1".into(),
            call_id: call_id.into(),
            tool_name: "t".into(),
            args: json!({}),
            replay_safe: false,
            status,
            result: result.map(str::to_string),
        };
        journal
            .append(tool("a", ToolCallStatus::Intent, None))
            .unwrap();
        journal
            .append(tool("a", ToolCallStatus::Done, Some("A-result")))
            .unwrap();
        journal
            .append(tool("b", ToolCallStatus::Intent, None))
            .unwrap();
        if let Some(outcome) = outcome {
            journal
                .append(JournalRecord::TurnEnd {
                    turn_id: "t1".into(),
                    outcome,
                })
                .unwrap();
        }
    }

    fn roles_and_contents(session: &Session) -> Vec<(String, String)> {
        session
            .messages
            .iter()
            .map(|m| {
                (
                    m["role"].as_str().unwrap_or_default().to_string(),
                    m["content"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn restore_rebuilds_the_unfinished_turn_from_the_journal_including_the_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let (loop_, journals) = make_loop_in(dir.path());
        write_crashed_turn(&journals, "t:1", None);
        let mut session = Session::new("t:1".to_string());

        assert!(loop_.restore_runtime_checkpoint(&mut session, &journals));

        assert_eq!(
            roles_and_contents(&session),
            [
                ("user".to_string(), "do two things".to_string()),
                ("assistant".to_string(), String::new()),
                ("tool".to_string(), "A-result".to_string()),
                (
                    "tool".to_string(),
                    checkpoint_payload::INTERRUPTED_RESULT.to_string()
                ),
            ]
        );
    }

    #[test]
    fn restore_runs_once_per_turn_even_when_called_again() {
        let dir = tempfile::tempdir().unwrap();
        let (loop_, journals) = make_loop_in(dir.path());
        write_crashed_turn(&journals, "t:1", None);
        let mut session = Session::new("t:1".to_string());

        assert!(loop_.restore_runtime_checkpoint(&mut session, &journals));
        let after_first = session.messages.len();
        assert!(!loop_.restore_runtime_checkpoint(&mut session, &journals));
        assert_eq!(session.messages.len(), after_first);
    }

    #[test]
    fn restore_merges_a_stopped_turn_but_not_a_completed_one() {
        let dir = tempfile::tempdir().unwrap();
        let (loop_, journals) = make_loop_in(dir.path());
        write_crashed_turn(&journals, "stopped:1", Some(TurnOutcome::Cancelled));
        write_crashed_turn(&journals, "done:1", Some(TurnOutcome::Completed));

        let mut stopped = Session::new("stopped:1".to_string());
        let mut done = Session::new("done:1".to_string());
        assert!(loop_.restore_runtime_checkpoint(&mut stopped, &journals));
        assert!(!loop_.restore_runtime_checkpoint(&mut done, &journals));
        assert!(done.messages.is_empty());
    }

    #[test]
    fn journal_wins_over_a_leftover_metadata_checkpoint_and_nothing_is_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let (loop_, journals) = make_loop_in(dir.path());
        write_crashed_turn(&journals, "t:1", None);
        let mut session = Session::new("t:1".to_string());
        session.metadata.insert(
            RUNTIME_CHECKPOINT_KEY.to_string(),
            checkpoint_payload::build("awaiting_tools", &two_call_turn()),
        );

        assert!(loop_.restore_runtime_checkpoint(&mut session, &journals));

        assert_eq!(session.messages.len(), 4);
        assert!(!session.metadata.contains_key(RUNTIME_CHECKPOINT_KEY));
    }

    #[test]
    fn pre_journal_sessions_still_restore_from_the_metadata_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let (loop_, journals) = make_loop_in(dir.path());
        let call =
            json!({"id": "b", "type": "function", "function": {"name": "t", "arguments": "{}"}});
        let mut session = Session::new("old:1".to_string());
        session.metadata.insert(
            RUNTIME_CHECKPOINT_KEY.to_string(),
            json!({
                "assistant_message": {"role": "assistant", "content": null, "tool_calls": [call.clone()]},
                "completed_tool_results": [],
                "pending_tool_calls": [call],
            }),
        );

        assert!(loop_.restore_runtime_checkpoint(&mut session, &journals));

        assert_eq!(session.messages.len(), 2);
        assert_eq!(
            session.messages[1]["content"],
            checkpoint_payload::INTERRUPTED_RESULT
        );
        assert!(!session.metadata.contains_key(RUNTIME_CHECKPOINT_KEY));
    }

    #[test]
    fn restore_with_neither_journal_nor_checkpoint_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (loop_, journals) = make_loop_in(dir.path());
        let mut session = Session::new("fresh:1".to_string());
        assert!(!loop_.restore_runtime_checkpoint(&mut session, &journals));
        assert!(session.messages.is_empty());
    }

    // ── resuming interrupted turns ────────────────────────────────────────────

    /// Provider that answers with a fixed text and records every request it receives.
    struct RecordingProvider {
        settings: GenerationSettings,
        reply: String,
        /// Responses handed out first, in order; `reply` is the fallback.
        script: Mutex<Vec<LLMResponse>>,
        requests: Arc<Mutex<Vec<Vec<Value>>>>,
    }

    impl RecordingProvider {
        fn respond(&self, messages: Vec<Value>) -> LLMResponse {
            self.requests.lock().unwrap().push(messages);
            {
                let mut script = self.script.lock().unwrap();
                if !script.is_empty() {
                    return script.remove(0);
                }
            }
            LLMResponse {
                content: Some(self.reply.clone()),
                finish_reason: "stop".into(),
                ..LLMResponse::new()
            }
        }
    }

    #[async_trait]
    impl LLMProviderDyn for RecordingProvider {
        fn api_key(&self) -> Option<String> {
            None
        }
        fn api_base(&self) -> Option<String> {
            None
        }
        fn extra_headers(&self) -> Option<std::collections::HashMap<String, String>> {
            None
        }
        fn generation_settings(&self) -> &GenerationSettings {
            &self.settings
        }
        fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
            &mut self.settings
        }
        fn spec(&self) -> Option<&crate::providers::registry::ProviderSpec> {
            None
        }
        fn get_default_model(&self) -> String {
            String::new()
        }
        async fn chat(
            &self,
            messages: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            self.respond(messages)
        }
        async fn safe_chat(
            &self,
            messages: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            self.respond(messages)
        }
        async fn chat_with_retry(
            &self,
            messages: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            self.respond(messages)
        }
        async fn chat_stream_with_retry_boxed(
            &self,
            messages: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
            _: Option<crate::providers::base::BoxedStreamCallback>,
            _: Option<crate::providers::base::BoxedProgressCallback>,
        ) -> LLMResponse {
            self.respond(messages)
        }
    }

    struct ResumeFixture {
        _dir: tempfile::TempDir,
        loop_: Arc<AgentLoop>,
        journals: Arc<dyn JournalStore>,
        requests: Arc<Mutex<Vec<Vec<Value>>>>,
        note_path: PathBuf,
    }

    fn resume_fixture(config: Config) -> ResumeFixture {
        let dir = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(RecordingProvider {
            settings: GenerationSettings::new(),
            reply: "all done".into(),
            script: Mutex::new(Vec::new()),
            requests: requests.clone(),
        });
        let loop_ = Arc::new(AgentLoop::new(
            Arc::new(MessageBus::new()),
            provider,
            dir.path().to_path_buf(),
            config,
            None,
            None,
            None,
        ));
        let journals = loop_.journal_store();
        let note_path = dir.path().join("note.txt");
        std::fs::write(&note_path, "note body").unwrap();
        ResumeFixture {
            _dir: dir,
            loop_,
            journals,
            requests,
            note_path,
        }
    }

    impl ResumeFixture {
        /// Pretend the process restarted: everything journaled so far is from a previous run.
        fn simulate_restart(&self) {
            let later = Utc::now() + chrono::Duration::hours(1);
            self.loop_
                .process_started_ms
                .store(later.timestamp_millis(), Ordering::Relaxed);
        }
    }

    /// Journal of a turn that died with a safe `read_file` call and an unsafe
    /// `exec` call both in flight.
    fn write_turn_killed_mid_tools(fixture: &ResumeFixture, key: &str, attempt: u32) {
        use crate::session::journal::StepKind;
        let call = |id: &str, name: &str| json!({"id": id, "type": "function", "function": {"name": name, "arguments": "{}"}});
        let turn = vec![
            json!({"role": "user", "content": "read the note and run it"}),
            json!({"role": "assistant", "content": null,
                   "tool_calls": [call("c-read", "read_file"), call("c-exec", "exec")]}),
        ];
        fixture.simulate_restart();
        let journal = RunJournal::new(Arc::clone(&fixture.journals), key);
        let (channel, chat_id) = key.split_once(':').unwrap();
        journal
            .append(JournalRecord::TurnStart {
                turn_id: "killed".into(),
                session_key: key.into(),
                request_id: None,
                user_message: Some(turn[0].clone()),
                attempt,
                channel: Some(channel.into()),
                chat_id: Some(chat_id.into()),
            })
            .unwrap();
        journal
            .append(JournalRecord::StepCheckpoint {
                turn_id: "killed".into(),
                step: StepKind::PreTool,
                payload: checkpoint_payload::build("awaiting_tools", &turn),
            })
            .unwrap();
        let intent =
            |call_id: &str, tool: &str, args: Value, replay_safe: bool| JournalRecord::ToolIntent {
                turn_id: "killed".into(),
                call_id: call_id.into(),
                tool_name: tool.into(),
                args,
                replay_safe,
                status: ToolCallStatus::Intent,
                result: None,
            };
        journal
            .append(intent(
                "c-read",
                "read_file",
                json!({"path": fixture.note_path.to_string_lossy()}),
                true,
            ))
            .unwrap();
        journal
            .append(intent(
                "c-exec",
                "exec",
                json!({"command": "echo hi"}),
                false,
            ))
            .unwrap();
    }

    async fn next_reply(loop_: &AgentLoop) -> OutboundMessage {
        tokio::time::timeout(Duration::from_secs(5), loop_.bus.consume_outbound())
            .await
            .expect("a resumed reply within 5s")
            .expect("bus open")
    }

    #[tokio::test]
    async fn resume_replays_safe_calls_interrupts_unsafe_ones_and_answers_once() {
        let fixture = resume_fixture(Config::default());
        write_turn_killed_mid_tools(&fixture, "chan:chat1", 0);

        let summary = fixture.loop_.resume_interrupted_turns().await;

        assert_eq!(
            summary,
            ResumeSummary {
                resumed: 1,
                ..Default::default()
            }
        );
        let reply = next_reply(&fixture.loop_).await;
        assert_eq!(
            (reply.channel.as_str(), reply.chat_id.as_str()),
            ("chan", "chat1")
        );
        assert_eq!(reply.content, "all done");

        // One model request, carrying the user prompt, the safe call's re-run
        // result and the interrupted marker for the unsafe call.
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        let tool_content = |id: &str| {
            sent.iter()
                .find(|m| m["tool_call_id"] == id)
                .and_then(|m| m["content"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        assert!(tool_content("c-read").contains("note body"), "{sent:?}");
        assert_eq!(
            tool_content("c-exec"),
            checkpoint_payload::INTERRUPTED_RESULT
        );
        assert!(sent.iter().any(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("read the note"))
        }));
    }

    #[tokio::test]
    async fn resume_persists_the_whole_turn_and_does_not_run_twice() {
        let fixture = resume_fixture(Config::default());
        write_turn_killed_mid_tools(&fixture, "chan:chat1", 0);

        fixture.loop_.resume_interrupted_turns().await;
        let again = fixture.loop_.resume_interrupted_turns().await;

        assert_eq!(again, ResumeSummary::default());
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        let session = fixture
            .loop_
            .session_manager
            .lock()
            .unwrap()
            .get_or_create_session("chan:chat1")
            .clone();
        let roles: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "tool", "tool", "assistant"]);
        assert_eq!(session.messages[4]["content"], "all done");
    }

    #[tokio::test]
    async fn resume_skips_turns_that_were_stopped() {
        let fixture = resume_fixture(Config::default());
        write_turn_killed_mid_tools(&fixture, "chan:chat1", 0);
        RunJournal::new(Arc::clone(&fixture.journals), "chan:chat1")
            .append(JournalRecord::TurnEnd {
                turn_id: "killed".into(),
                outcome: TurnOutcome::Cancelled,
            })
            .unwrap();

        let summary = fixture.loop_.resume_interrupted_turns().await;

        assert_eq!(summary, ResumeSummary::default());
        assert!(fixture.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resume_honors_the_kill_switch() {
        let mut config = Config::default();
        config.durability.auto_resume = false;
        let fixture = resume_fixture(config);
        write_turn_killed_mid_tools(&fixture, "chan:chat1", 0);

        let summary = fixture.loop_.resume_interrupted_turns().await;

        assert_eq!(summary, ResumeSummary::default());
        assert!(fixture.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_poison_turn_is_abandoned_after_the_attempt_limit_and_never_calls_the_model() {
        let fixture = resume_fixture(Config::default());
        write_turn_killed_mid_tools(&fixture, "chan:chat1", 2);

        let summary = fixture.loop_.resume_interrupted_turns().await;

        assert_eq!(
            summary,
            ResumeSummary {
                abandoned: 1,
                ..Default::default()
            }
        );
        assert!(fixture.requests.lock().unwrap().is_empty());
        // The partial work is still merged when the session is next used.
        let mut session = Session::new("chan:chat1".to_string());
        assert!(
            fixture
                .loop_
                .restore_runtime_checkpoint(&mut session, &fixture.journals)
        );
    }

    #[tokio::test]
    async fn a_turn_that_died_after_its_answer_only_redelivers_it() {
        use crate::session::journal::StepKind;
        let fixture = resume_fixture(Config::default());
        fixture.simulate_restart();
        let journal = RunJournal::new(Arc::clone(&fixture.journals), "chan:chat1");
        let turn = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "the answer"}),
        ];
        journal
            .append(JournalRecord::TurnStart {
                turn_id: "t".into(),
                session_key: "chan:chat1".into(),
                request_id: None,
                user_message: Some(turn[0].clone()),
                attempt: 0,
                channel: Some("chan".into()),
                chat_id: Some("chat1".into()),
            })
            .unwrap();
        journal
            .append(JournalRecord::StepCheckpoint {
                turn_id: "t".into(),
                step: StepKind::Final,
                payload: checkpoint_payload::build("final_response", &turn),
            })
            .unwrap();

        fixture.loop_.resume_interrupted_turns().await;

        assert_eq!(next_reply(&fixture.loop_).await.content, "the answer");
        assert!(fixture.requests.lock().unwrap().is_empty());
    }

    // ── inbox ledger through the running loop ─────────────────────────────────

    fn inbound_with_request_id(content: &str, request_id: &str) -> InboundMessage {
        InboundMessage {
            channel: "chan".into(),
            sender_id: "user".into(),
            chat_id: "chat1".into(),
            content: content.into(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
            session_key_override: None,
            request_id: Some(request_id.into()),
        }
    }

    /// Next outbound message that satisfies `wanted`, skipping progress/turn events.
    async fn next_outbound_where(
        loop_: &AgentLoop,
        wanted: impl Fn(&OutboundMessage) -> bool,
    ) -> OutboundMessage {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let outbound = loop_.bus.consume_outbound().await.expect("bus open");
                if wanted(&outbound) {
                    return outbound;
                }
            }
        })
        .await
        .expect("the expected outbound message within 5s")
    }

    /// Roles of the persisted transcript of `key`.
    fn transcript_roles(fixture: &ResumeFixture, key: &str) -> Vec<String> {
        let mut manager = fixture
            .loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        manager
            .get_or_create_session(key)
            .messages
            .iter()
            .map(|m| m["role"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_redelivered_request_runs_one_turn_and_gets_the_original_reply() {
        let fixture = resume_fixture(Config::default());
        let running = tokio::spawn({
            let loop_ = fixture.loop_.clone();
            async move { loop_.run().await }
        });
        let msg = inbound_with_request_id("hello", "chan:chat1:m1");

        fixture.loop_.bus.publish_inbound(msg.clone()).unwrap();
        let first = next_outbound_where(&fixture.loop_, |o| o.content == "all done").await;
        assert!(!first.metadata.contains_key("_redelivered"));

        // The ledger reaches Done after the reply is published; wait for it.
        let ledger = InboxLedger::for_session(&fixture.journals, "chan:chat1");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ledger.pending().is_empty() || ledger.entries().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ledger entry closed");

        fixture.loop_.bus.publish_inbound(msg).unwrap();
        let replayed =
            next_outbound_where(&fixture.loop_, |o| o.metadata.contains_key("_redelivered")).await;
        assert_eq!(replayed.content, "all done");
        assert_eq!(
            transcript_roles(&fixture, "chan:chat1"),
            ["user", "assistant"],
            "exactly one model turn"
        );

        fixture.loop_.stop();
        running.abort();
    }

    #[tokio::test]
    async fn a_message_queued_before_a_crash_is_redriven_exactly_once() {
        let fixture = resume_fixture(Config::default());
        // The previous process accepted the message, then died before consuming it.
        let msg = inbound_with_request_id("queued before the crash", "chan:chat1:m9");
        let ledger = InboxLedger::for_session(&fixture.journals, "chan:chat1");
        assert_eq!(ledger.admit(&msg), crate::session::inbox::Admission::New);
        fixture.simulate_restart();

        // `run()` resumes interrupted turns and then re-drives queued messages itself.
        let running = tokio::spawn({
            let loop_ = fixture.loop_.clone();
            async move { loop_.run().await }
        });
        let reply = next_outbound_where(&fixture.loop_, |o| o.content == "all done").await;
        assert_eq!(reply.chat_id, "chat1");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ledger.pending().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ledger entry closed");

        assert_eq!(fixture.loop_.redrive_pending_inbound(), 0);
        assert_eq!(
            transcript_roles(&fixture, "chan:chat1"),
            ["user", "assistant"],
            "exactly one model turn"
        );
        fixture.loop_.stop();
        running.abort();
    }

    #[tokio::test]
    async fn a_stopped_request_is_not_redriven_after_a_restart() {
        let fixture = resume_fixture(Config::default());
        let msg = inbound_with_request_id("long job", "chan:chat1:m3");
        let ledger = InboxLedger::for_session(&fixture.journals, "chan:chat1");
        ledger.admit(&msg);
        ledger.mark_processing(&msg);
        fixture.simulate_restart();

        fixture.loop_.abort_session(&msg).await;

        assert!(ledger.pending().is_empty());
        assert_eq!(fixture.loop_.redrive_pending_inbound(), 0);
    }

    // ── crash sweep: kill at every boundary, restart, assert the invariants ───

    /// Tool that counts how often it really ran; `safe` decides `replay_safe()`.
    struct CountingProbe {
        name: &'static str,
        safe: bool,
        runs: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl crate::agent::tools::base::Tool for CountingProbe {
        fn name(&self) -> String {
            self.name.to_string()
        }
        fn description(&self) -> String {
            "counts executions".into()
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn replay_safe(&self) -> bool {
            self.safe
        }
        async fn execute(&self, _: &Value) -> String {
            self.runs.fetch_add(1, Ordering::SeqCst);
            format!("{} ran", self.name)
        }
    }

    fn tool_call_response(calls: &[(&str, &str)]) -> LLMResponse {
        LLMResponse {
            tool_calls: calls
                .iter()
                .map(|(id, name)| crate::providers::base::ToolCallRequest {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: HashMap::new(),
                    extra_content: None,
                    provider_specific_fields: None,
                    function_provider_specific_fields: None,
                })
                .collect(),
            finish_reason: "tool_calls".into(),
            ..LLMResponse::new()
        }
    }

    fn text_response(text: &str) -> LLMResponse {
        LLMResponse {
            content: Some(text.to_string()),
            finish_reason: "stop".into(),
            ..LLMResponse::new()
        }
    }

    /// One "process": an agent loop over `workspace` with a scripted model and
    /// the two counting tools (a replay-safe one and an unsafe one).
    struct CrashProcess {
        loop_: Arc<AgentLoop>,
        safe_runs: Arc<std::sync::atomic::AtomicUsize>,
        unsafe_runs: Arc<std::sync::atomic::AtomicUsize>,
        model_requests: Arc<Mutex<Vec<Vec<Value>>>>,
    }

    fn start_process(
        workspace: &Path,
        script: Vec<LLMResponse>,
        safe_runs: Arc<std::sync::atomic::AtomicUsize>,
        unsafe_runs: Arc<std::sync::atomic::AtomicUsize>,
    ) -> CrashProcess {
        let model_requests = Arc::new(Mutex::new(Vec::new()));
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(RecordingProvider {
            settings: GenerationSettings::new(),
            reply: "final".into(),
            script: Mutex::new(script),
            requests: model_requests.clone(),
        });
        let loop_ = Arc::new(AgentLoop::new(
            Arc::new(MessageBus::new()),
            provider,
            workspace.to_path_buf(),
            Config::default(),
            None,
            None,
            None,
        ));
        {
            let mut registry = loop_.tools.lock().unwrap();
            registry.register(Box::new(CountingProbe {
                name: "safe_probe",
                safe: true,
                runs: safe_runs.clone(),
            }));
            registry.register(Box::new(CountingProbe {
                name: "unsafe_probe",
                safe: false,
                runs: unsafe_runs.clone(),
            }));
        }
        CrashProcess {
            loop_,
            safe_runs,
            unsafe_runs,
            model_requests,
        }
    }

    fn assistant_tool_call_ids(messages: &[Value]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|m| m["tool_calls"].as_array())
            .flatten()
            .filter_map(|call| call["id"].as_str().map(str::to_string))
            .collect()
    }

    /// Kill the turn at `point` (the `nth` time it is reached), restart, and
    /// check what recovery guarantees.
    async fn crash_then_recover(point: &'static str, nth: usize) {
        use futures::FutureExt;
        use std::sync::atomic::AtomicUsize;
        let workspace = tempfile::tempdir().unwrap();
        let safe_runs = Arc::new(AtomicUsize::new(0));
        let unsafe_runs = Arc::new(AtomicUsize::new(0));

        // ── process 1: runs until the crash point ─────────────────────────────
        let first = start_process(
            workspace.path(),
            vec![
                tool_call_response(&[("c-safe", "safe_probe"), ("c-unsafe", "unsafe_probe")]),
                text_response("final"),
            ],
            safe_runs.clone(),
            unsafe_runs.clone(),
        );
        crate::agent::crash_points::arm(point, nth);
        let crashed = AssertUnwindSafe(Arc::clone(&first.loop_).process_direct(
            "run both probes",
            Some("chan:chat1"),
            Some("chan"),
            Some("chat1"),
            None,
            None,
            None,
            None,
        ))
        .catch_unwind()
        .await
        .is_err();
        crate::agent::crash_points::disarm();
        assert!(crashed, "{point}#{nth}: the crash point was never reached");
        let (safe_at_crash, unsafe_at_crash) = (
            safe_runs.load(Ordering::SeqCst),
            unsafe_runs.load(Ordering::SeqCst),
        );
        drop(first);

        // ── process 2: restarts over the same workspace ───────────────────────
        let second = start_process(
            workspace.path(),
            vec![text_response("final")],
            safe_runs.clone(),
            unsafe_runs.clone(),
        );
        // Anything journaled so far belongs to the dead process.
        second.loop_.process_started_ms.store(
            (Utc::now() + chrono::Duration::hours(1)).timestamp_millis(),
            Ordering::Relaxed,
        );
        second.loop_.resume_interrupted_turns().await;

        let transcript = second
            .loop_
            .session_manager
            .lock()
            .unwrap()
            .get_or_create_session("chan:chat1")
            .messages
            .clone();
        let label = format!("{point}#{nth}");

        // One user prompt, one final answer, nothing lost or duplicated.
        let count = |role: &str| transcript.iter().filter(|m| m["role"] == role).count();
        assert_eq!(count("user"), 1, "{label}: user prompt exactly once");
        assert_eq!(
            transcript
                .iter()
                .filter(|m| m["role"] == "assistant" && m["content"] == "final")
                .count(),
            1,
            "{label}: exactly one final answer: {transcript:?}"
        );
        // Every requested tool call has a result.
        let results: Vec<&str> = transcript
            .iter()
            .filter_map(|m| m["tool_call_id"].as_str())
            .collect();
        for id in assistant_tool_call_ids(&transcript) {
            assert!(
                results.contains(&id.as_str()),
                "{label}: {id} has no result"
            );
        }
        // Replay budget: safe tools at most once more, unsafe tools never again.
        assert!(
            second.safe_runs.load(Ordering::SeqCst) <= safe_at_crash + 1,
            "{label}: safe tool re-run more than once"
        );
        assert_eq!(
            second.unsafe_runs.load(Ordering::SeqCst),
            unsafe_at_crash,
            "{label}: an unsafe tool was executed again by recovery"
        );
        // The answer was delivered exactly once.
        let mut delivered = 0;
        while let Ok(Some(out)) = tokio::time::timeout(
            Duration::from_millis(100),
            second.loop_.bus.consume_outbound(),
        )
        .await
        {
            if out.content == "final" {
                delivered += 1;
            }
        }
        assert_eq!(delivered, 1, "{label}: reply delivered once");

        // Recovering again changes nothing.
        let again = second.loop_.resume_interrupted_turns().await;
        assert_eq!(again, ResumeSummary::default(), "{label}: second pass");
        assert_eq!(
            second
                .loop_
                .session_manager
                .lock()
                .unwrap()
                .get_or_create_session("chan:chat1")
                .messages
                .len(),
            transcript.len(),
            "{label}: second pass changed the transcript"
        );
        let _ = second.model_requests;
    }

    #[tokio::test]
    async fn crash_after_turn_start() {
        crash_then_recover("after_turn_start", 0).await;
    }

    #[tokio::test]
    async fn crash_before_the_model_request() {
        crash_then_recover("before_model", 0).await;
    }

    #[tokio::test]
    async fn crash_after_the_safe_tools_intent() {
        crash_then_recover("after_intent", 0).await;
    }

    #[tokio::test]
    async fn crash_after_the_unsafe_tools_intent() {
        crash_then_recover("after_intent", 1).await;
    }

    #[tokio::test]
    async fn crash_after_the_safe_tool_ran_before_its_terminal_record() {
        crash_then_recover("before_terminal", 0).await;
    }

    #[tokio::test]
    async fn crash_after_the_unsafe_tool_ran_before_its_terminal_record() {
        crash_then_recover("before_terminal", 1).await;
    }

    #[tokio::test]
    async fn crash_after_all_tools_finished_before_the_checkpoint() {
        crash_then_recover("after_tools_before_checkpoint", 0).await;
    }

    #[tokio::test]
    async fn crash_after_the_final_answer_before_the_turn_end_record() {
        crash_then_recover("after_final_before_turn_end", 0).await;
    }

    #[tokio::test]
    async fn a_normal_turn_leaves_only_the_transcript_and_an_empty_journal() {
        let fixture = resume_fixture(Config::default());

        let reply = Arc::clone(&fixture.loop_)
            .process_direct(
                "hello",
                Some("chan:chat1"),
                Some("chan"),
                Some("chat1"),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("a reply");

        assert_eq!(reply.content, "all done");
        let mut manager = fixture.loop_.session_manager.lock().unwrap();
        let session = manager.get_or_create_session("chan:chat1").clone();
        let roles: Vec<&str> = session
            .messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant"]);
        // No per-step metadata checkpoint any more, and nothing left to recover.
        assert!(!session.metadata.contains_key(RUNTIME_CHECKPOINT_KEY));
        let journal = RunJournal::new(Arc::clone(&fixture.journals), "chan:chat1");
        assert!(journal.scan().entries.is_empty());
    }

    /// Names of every registered tool that declares `replay_safe() == true`.
    fn replay_safe_tool_names(loop_: &AgentLoop) -> Vec<String> {
        let registry = loop_.tools.lock().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<String> = registry
            .tool_names()
            .into_iter()
            .filter(|name| registry.get(name).is_some_and(|tool| tool.replay_safe()))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn only_the_reviewed_read_only_tools_are_replay_safe() {
        // Any new opt-in must be a deliberate edit of this list.
        let loop_ = make_save_turn_loop(1000);
        assert_eq!(
            replay_safe_tool_names(&loop_),
            ["glob", "grep", "list_dir", "read_file", "spawn"]
        );
    }

    #[test]
    fn side_effecting_tools_are_not_replay_safe() {
        let loop_ = make_save_turn_loop(1000);
        let registry = loop_.tools.lock().unwrap_or_else(|e| e.into_inner());
        for name in ["message", "exec", "write_file", "edit_file"] {
            if let Some(tool) = registry.get(name) {
                assert!(!tool.replay_safe(), "{name} must not be replay safe");
            }
        }
        assert!(registry.get("message").is_some());
    }

    fn saved_content(msg: &Value) -> Option<&str> {
        msg.get("content").and_then(Value::as_str)
    }

    #[test]
    fn persist_finished_turn_keeps_usage_that_the_pre_run_snapshot_did_not_have() {
        let loop_ = make_save_turn_loop(1000);
        let key = format!(
            "test:stale-usage-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let mut pre_run = Session::new(key.clone());
        let run_usage = LLMUsage {
            input_tokens: Some(15),
            output_tokens: Some(7),
            ..LLMUsage::new()
        };

        {
            let mut manager = loop_
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let live = manager.get_or_create_session(&key);
            live.update_usage(run_usage);
            let snap = live.clone();
            manager.save(snap).unwrap();
        }

        loop_.persist_finished_turn(
            &mut pre_run,
            &[
                json!({"role": "user", "content": "hello"}),
                json!({"role": "assistant", "content": "Hello!"}),
            ],
            1,
            run_usage,
            "test",
        );

        let manager = loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let live = manager
            .get_session_internal(&key)
            .expect("session should be cached after save");
        let usage = live
            .usage()
            .expect("token_usage must survive the post-run save");
        assert_eq!(usage.input_tokens, Some(15));
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(live.messages.len(), 1);
        assert_eq!(saved_content(&live.messages[0]), Some("Hello!"));
    }

    #[test]
    fn test_save_turn_filters_and_persists_messages() {
        const MAX_CHARS: u32 = 10;
        let loop_ = make_save_turn_loop(MAX_CHARS);
        let before = Utc::now();
        let mut session = Session::new("test:save_turn".into());

        let long_tool_output = "0123456789abcdef";
        let runtime_user = format!("{RUNTIME_CONTEXT_TAG}\n\nhello user");
        let messages = vec![
            json!({"role": "system", "content": "prompt"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "assistant", "content": "visible reply"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "read"}}]
            }),
            json!({"role": "tool", "content": long_tool_output, "tool_call_id": "tc1", "name": "read"}),
            json!({"role": "user", "content": runtime_user}),
            json!({"role": "user", "content": format!("{RUNTIME_CONTEXT_TAG}\n\n")}),
            json!({"role": "user", "content": "plain user", "timestamp": "2020-01-01T00:00:00Z"}),
        ];

        loop_.save_turn(&mut session, &messages, 1);

        // skip=1 drops the system prompt; empty assistant is dropped too.
        assert_eq!(session.messages.len(), 5);

        assert_eq!(saved_content(&session.messages[0]), Some("visible reply"));

        assert_eq!(
            session.messages[1]
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(1),
            "assistant with only tool_calls should be kept"
        );

        let tool_content = saved_content(&session.messages[2]).unwrap();
        assert_ne!(tool_content, long_tool_output);
        assert!(tool_content.contains("(truncated)"));

        assert_eq!(saved_content(&session.messages[3]), Some("hello user"));

        assert_eq!(saved_content(&session.messages[4]), Some("plain user"));
        assert_eq!(
            session.messages[4].get("timestamp").and_then(Value::as_str),
            Some("2020-01-01T00:00:00Z"),
            "existing timestamp must not be overwritten"
        );

        for (idx, msg) in session.messages.iter().enumerate() {
            if idx != 4 {
                assert!(
                    msg.get("timestamp").and_then(Value::as_str).is_some(),
                    "message {idx} should have a timestamp"
                );
            }
        }

        assert!(session.updated_at >= before);
    }

    fn command_reply(content: &str) -> OutboundMessage {
        OutboundMessage {
            channel: "cli".into(),
            chat_id: "direct".into(),
            content: content.to_string(),
            reply_to: None,
            media: vec![],
            metadata: HashMap::new(),
            event: None,
        }
    }

    #[test]
    fn persist_command_turn_writes_marked_messages_omitted_from_get_history() {
        let loop_ = make_save_turn_loop(1000);
        let key = "test:persist_command_turn";
        let _ = {
            let mut manager = loop_
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            manager.delete_session(key)
        };

        loop_.persist_command_turn(
            key,
            "/help",
            "/help",
            &command_reply("Available commands..."),
        );

        let session = {
            let mut manager = loop_
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            manager.get_or_create_session(key).clone()
        };
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0]["role"], json!("user"));
        assert_eq!(session.messages[0]["content"], json!("/help"));
        assert_eq!(session.messages[0][COMMAND_KEY], json!(true));
        assert_eq!(session.messages[1]["role"], json!("assistant"));
        assert_eq!(
            session.messages[1]["content"],
            json!("Available commands...")
        );
        assert_eq!(session.messages[1][COMMAND_KEY], json!(true));
        assert!(session.get_history(None).is_empty());
    }

    #[test]
    fn persist_command_turn_skips_new() {
        let loop_ = make_save_turn_loop(1000);
        let key = "test:persist_command_turn_new";
        loop_.persist_command_turn(key, "/NEW", "/NEW", &command_reply("New session started."));

        let mut manager = loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = manager.get_or_create_session(key);
        assert!(session.messages.is_empty());
    }

    // ── workspace scope ──────────────────────────────────────────────────────

    #[test]
    fn set_session_workspace_scope_persists_metadata_and_returns_scope() {
        let loop_ = make_save_turn_loop(1000);
        let dir = tempfile::tempdir().unwrap();

        let scope = {
            let mut manager = loop_
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            loop_
                .set_session_workspace_scope(
                    &mut manager,
                    "test:workspace_scope",
                    dir.path(),
                    WorkspaceAccessMode::Restricted,
                )
                .unwrap()
        };
        assert_eq!(scope.project_path, dir.path());
        assert_eq!(scope.access_mode, WorkspaceAccessMode::Restricted);

        let mut manager = loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = manager.get_or_create_session("test:workspace_scope");
        let stored = session.metadata.get(WORKSPACE_SCOPE_METADATA_KEY).unwrap();
        assert_eq!(stored, &scope.metadata());
    }

    #[test]
    fn set_session_workspace_scope_rejects_relative_path() {
        let loop_ = make_save_turn_loop(1000);
        let err = {
            let mut manager = loop_
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            loop_
                .set_session_workspace_scope(
                    &mut manager,
                    "test:workspace_scope_relative",
                    std::path::Path::new("relative/dir"),
                    WorkspaceAccessMode::Restricted,
                )
                .unwrap_err()
        };
        assert_eq!(err.status, 400);

        let mut manager = loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = manager.get_or_create_session("test:workspace_scope_relative");
        assert!(!session.metadata.contains_key(WORKSPACE_SCOPE_METADATA_KEY));
    }

    #[test]
    fn clear_session_workspace_scope_removes_metadata_key() {
        let loop_ = make_save_turn_loop(1000);
        let dir = tempfile::tempdir().unwrap();

        {
            let mut manager = loop_
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            loop_
                .set_session_workspace_scope(
                    &mut manager,
                    "test:workspace_scope_clear",
                    dir.path(),
                    WorkspaceAccessMode::Full,
                )
                .unwrap();
        }
        {
            let mut manager = loop_
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            AgentLoop::clear_session_workspace_scope(&mut manager, "test:workspace_scope_clear");
        }

        let mut manager = loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = manager.get_or_create_session("test:workspace_scope_clear");
        assert!(!session.metadata.contains_key(WORKSPACE_SCOPE_METADATA_KEY));
    }

    #[test]
    fn workspace_scopes_resolver_constructed_from_agent_loop_defaults() {
        let loop_ = make_save_turn_loop(1000);
        assert_eq!(loop_.workspace_scopes.default_workspace, loop_.workspace);
        assert_eq!(
            loop_.workspace_scopes.default_restrict_to_workspace,
            loop_.restrict_to_workspace
        );
    }

    #[test]
    fn set_session_mode_persists_and_default_clears() {
        let loop_ = make_save_turn_loop(1000);
        let key = "test:agent_mode";
        {
            let mut manager = loop_
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mode = loop_
                .set_session_mode(&mut manager, key, "minimal")
                .unwrap();
            assert_eq!(mode, AgentMode::Minimal);
            let session = manager.get_or_create_session(key);
            assert_eq!(
                session
                    .metadata
                    .get(SESSION_AGENT_MODE_METADATA_KEY)
                    .and_then(Value::as_str),
                Some("minimal")
            );
            assert_eq!(loop_.mode_for_session(Some(session)), AgentMode::Minimal);
        }
        {
            let mut manager = loop_
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mode = loop_
                .set_session_mode(&mut manager, key, RESERVED_AGENT_MODE_NAME)
                .unwrap();
            assert_eq!(mode, AgentMode::Standard);
            let session = manager.get_or_create_session(key);
            assert!(
                !session
                    .metadata
                    .contains_key(SESSION_AGENT_MODE_METADATA_KEY)
            );
        }
    }

    #[test]
    fn set_session_mode_rejects_unknown_without_mutating() {
        let loop_ = make_save_turn_loop(1000);
        let mut manager = loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let err = loop_
            .set_session_mode(&mut manager, "test:agent_mode_bad", "ptc")
            .unwrap_err();
        assert!(err.contains("Unknown agent mode"));
        let session = manager.get_or_create_session("test:agent_mode_bad");
        assert!(
            !session
                .metadata
                .contains_key(SESSION_AGENT_MODE_METADATA_KEY)
        );
    }

    // ── WebUI markers on system-message turns ─────────────────────────────────

    fn session_with_webui_flag(key: &str, webui: Option<bool>) -> Session {
        let mut session = Session::new(key.to_string());
        if let Some(webui) = webui {
            session
                .metadata
                .insert(SESSION_WEBUI_METADATA_KEY.to_string(), Value::Bool(webui));
        }
        session
    }

    #[test]
    fn webui_websocket_chats_get_the_webui_markers_and_a_turn_id() {
        let session = session_with_webui_flag("websocket:chat-1", Some(true));

        let metadata = webui_system_turn_metadata("websocket", &session);

        assert_eq!(metadata.get("webui"), Some(&Value::Bool(true)));
        let turn_id = metadata
            .get("webui_turn_id")
            .and_then(Value::as_str)
            .expect("a turn id");
        assert!(turn_id.starts_with(WEBUI_SYSTEM_TURN_PREFIX), "{turn_id}");
        assert!(turn_id.len() > WEBUI_SYSTEM_TURN_PREFIX.len());
    }

    #[test]
    fn every_system_turn_gets_its_own_turn_id() {
        let session = session_with_webui_flag("websocket:chat-1", Some(true));
        let first = webui_system_turn_metadata("websocket", &session);
        let second = webui_system_turn_metadata("websocket", &session);
        assert_ne!(first.get("webui_turn_id"), second.get("webui_turn_id"));
    }

    #[test]
    fn chats_that_are_not_webui_get_no_markers() {
        for session in [
            session_with_webui_flag("websocket:chat-1", None),
            session_with_webui_flag("websocket:chat-1", Some(false)),
        ] {
            assert!(webui_system_turn_metadata("websocket", &session).is_empty());
        }
    }

    #[test]
    fn other_channels_get_no_markers_even_with_the_flag() {
        let session = session_with_webui_flag("cli:direct", Some(true));
        assert!(webui_system_turn_metadata("cli", &session).is_empty());
        assert!(webui_system_turn_metadata("telegram", &session).is_empty());
    }

    /// A sub-agent's result announced to `chat_id` on `channel`, as `announce_result` builds it.
    fn announcement_for(channel: &str, chat_id: &str) -> InboundMessage {
        InboundMessage {
            channel: "system".to_string(),
            sender_id: "subagent".to_string(),
            chat_id: format!("{channel}:{chat_id}"),
            content: "[Subagent 'job' completed successfully]".to_string(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
            session_key_override: Some(format!("{channel}:{chat_id}")),
            request_id: None,
        }
    }

    fn mark_session_webui(loop_: &AgentLoop, key: &str) {
        let mut manager = loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = manager.get_or_create_session(key);
        session
            .metadata
            .insert(SESSION_WEBUI_METADATA_KEY.to_string(), Value::Bool(true));
        let snapshot = session.clone();
        manager.save(snapshot).unwrap();
    }

    #[tokio::test]
    async fn the_reply_to_a_subagent_result_is_marked_for_the_webui_transcript() {
        let fixture = resume_fixture(Config::default());
        mark_session_webui(&fixture.loop_, "websocket:chat-1");

        let reply = Arc::clone(&fixture.loop_)
            .process_system_message(announcement_for("websocket", "chat-1"))
            .await
            .expect("a reply");

        assert_eq!(reply.channel, "websocket");
        assert_eq!(reply.chat_id, "chat-1");
        assert_eq!(reply.content, "all done");
        assert_eq!(reply.metadata.get("webui"), Some(&Value::Bool(true)));
        assert!(
            reply
                .metadata
                .get("webui_turn_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with(WEBUI_SYSTEM_TURN_PREFIX))
        );
    }

    #[tokio::test]
    async fn the_reply_to_a_subagent_result_in_a_cli_session_stays_unmarked() {
        let fixture = resume_fixture(Config::default());

        let reply = Arc::clone(&fixture.loop_)
            .process_system_message(announcement_for("cli", "direct"))
            .await
            .expect("a reply");

        assert!(reply.metadata.is_empty());
    }

    /// Give `key` a finished user turn: a question and its text-only answer.
    fn seed_finished_turn(loop_: &AgentLoop, key: &str, question: &str, answer: &str) {
        let mut manager = loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = manager.get_or_create_session(key);
        session
            .messages
            .push(json!({"role": "user", "content": question}));
        session
            .messages
            .push(json!({"role": "assistant", "content": answer}));
        let snapshot = session.clone();
        manager.save(snapshot).unwrap();
    }

    fn session_messages(loop_: &AgentLoop, key: &str) -> Vec<Value> {
        loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_create_session(key)
            .messages
            .clone()
    }

    #[tokio::test]
    async fn a_subagent_result_reaches_the_model_as_a_user_turn_of_its_own() {
        let fixture = resume_fixture(Config::default());
        seed_finished_turn(
            &fixture.loop_,
            "cli:direct",
            "spawn a job",
            "the job is running",
        );

        Arc::clone(&fixture.loop_)
            .process_system_message(announcement_for("cli", "direct"))
            .await
            .expect("a reply");

        let requests = fixture.requests.lock().unwrap();
        let request = requests.last().expect("one model request");
        let tail = &request[request.len() - 2..];
        assert_eq!(tail[0]["role"], "assistant");
        assert_eq!(
            tail[0]["content"], "the job is running",
            "the earlier reply stays untouched"
        );
        assert_eq!(tail[1]["role"], "user");
        assert!(
            tail[1]["content"]
                .as_str()
                .is_some_and(|content| content.contains("[Subagent 'job' completed successfully]")),
            "{}",
            tail[1]
        );
    }

    #[tokio::test]
    async fn a_subagent_result_is_saved_hidden_before_the_reply_to_it() {
        let fixture = resume_fixture(Config::default());
        seed_finished_turn(
            &fixture.loop_,
            "cli:direct",
            "spawn a job",
            "the job is running",
        );

        Arc::clone(&fixture.loop_)
            .process_system_message(announcement_for("cli", "direct"))
            .await
            .expect("a reply");

        let messages = session_messages(&fixture.loop_, "cli:direct");
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert_eq!(messages[1]["content"], "the job is running");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"],
            "[Subagent 'job' completed successfully]"
        );
        assert_eq!(messages[2][HIDDEN_HISTORY_KEY], json!(true));
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["content"], "all done");
    }

    #[tokio::test]
    async fn a_reply_that_copies_the_runtime_context_is_cut_before_it() {
        let leaky_reply = format!(
            "Still waiting on one batch.

{RUNTIME_CONTEXT_TAG}
Channel: cli

[Subagent 'other' completed successfully]"
        );
        let fixture = fixture_with_provider(&leaky_reply, |_| Vec::new());

        let reply = Arc::clone(&fixture.loop_)
            .process_system_message(announcement_for("cli", "direct"))
            .await
            .expect("a reply");

        assert_eq!(reply.content, "Still waiting on one batch.");
        let messages = session_messages(&fixture.loop_, "cli:direct");
        assert_eq!(
            messages.last().map(|message| message["content"].clone()),
            Some(json!("Still waiting on one batch."))
        );
    }

    #[test]
    fn mark_announcement_hidden_marks_only_a_user_message_at_the_index() {
        let mut messages = vec![
            json!({"role": "system", "content": "prompt"}),
            json!({"role": "user", "content": "result"}),
            json!({"role": "assistant", "content": "reply"}),
        ];
        mark_announcement_hidden(&mut messages, 1);
        mark_announcement_hidden(&mut messages, 2);
        mark_announcement_hidden(&mut messages, 9);

        assert_eq!(messages[1][HIDDEN_HISTORY_KEY], json!(true));
        assert!(messages[2].get(HIDDEN_HISTORY_KEY).is_none());
    }

    #[test]
    fn clean_final_content_strips_think_blocks_and_leaked_runtime_context() {
        let reply = format!(
            "<think>plan</think>Done.
{RUNTIME_CONTEXT_TAG}
Channel: cli"
        );
        assert_eq!(clean_final_content(Some(&reply)), Some("Done.".to_string()));
        assert_eq!(
            clean_final_content(Some("Done.")),
            Some("Done.".to_string())
        );
    }

    #[test]
    fn clean_final_content_of_nothing_but_leaked_context_is_none() {
        let reply = format!(
            "{RUNTIME_CONTEXT_TAG}
Channel: cli"
        );
        assert_eq!(clean_final_content(Some(&reply)), None);
        assert_eq!(clean_final_content(None), None);
    }

    // ── system-message turns run as tasks and show their work ─────────────────

    /// A provider whose requests never return, standing in for a long model call;
    /// `requested` turns true once the first request has been made.
    struct HangingProvider {
        settings: GenerationSettings,
        requested: Arc<AtomicBool>,
    }

    impl HangingProvider {
        async fn hang(&self) -> LLMResponse {
            self.requested.store(true, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    #[async_trait]
    impl LLMProviderDyn for HangingProvider {
        fn api_key(&self) -> Option<String> {
            None
        }
        fn api_base(&self) -> Option<String> {
            None
        }
        fn extra_headers(&self) -> Option<std::collections::HashMap<String, String>> {
            None
        }
        fn generation_settings(&self) -> &GenerationSettings {
            &self.settings
        }
        fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
            &mut self.settings
        }
        fn spec(&self) -> Option<&crate::providers::registry::ProviderSpec> {
            None
        }
        fn get_default_model(&self) -> String {
            String::new()
        }
        async fn chat(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            self.hang().await
        }
        async fn safe_chat(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: usize,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            self.hang().await
        }
        async fn chat_with_retry(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
        ) -> LLMResponse {
            self.hang().await
        }
        async fn chat_stream_with_retry_boxed(
            &self,
            _: Vec<Value>,
            _: Option<Vec<Value>>,
            _: Option<String>,
            _: Option<usize>,
            _: Option<f32>,
            _: Option<String>,
            _: Option<Value>,
            _: Option<crate::providers::base::BoxedStreamCallback>,
            _: Option<crate::providers::base::BoxedProgressCallback>,
        ) -> LLMResponse {
            self.hang().await
        }
    }

    /// Poll `condition` until it holds, failing the test after a timeout.
    async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        for _ in 0..400 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Every outbound message published within a short quiet period.
    async fn drain_outbound(bus: &MessageBus) -> Vec<OutboundMessage> {
        let mut collected = Vec::new();
        while let Ok(Some(message)) =
            tokio::time::timeout(Duration::from_millis(300), bus.consume_outbound()).await
        {
            collected.push(message);
        }
        collected
    }

    fn user_message(channel: &str, chat_id: &str, content: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.to_string(),
            sender_id: "user".to_string(),
            chat_id: chat_id.to_string(),
            content: content.to_string(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
            session_key_override: None,
            request_id: None,
        }
    }

    #[tokio::test]
    async fn a_long_system_turn_does_not_block_stop() {
        let dir = tempfile::tempdir().unwrap();
        let requested = Arc::new(AtomicBool::new(false));
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(HangingProvider {
            settings: GenerationSettings::new(),
            requested: Arc::clone(&requested),
        });
        let bus = Arc::new(MessageBus::new());
        let loop_ = Arc::new(AgentLoop::new(
            Arc::clone(&bus),
            provider,
            dir.path().to_path_buf(),
            Config::default(),
            None,
            None,
            None,
        ));
        let running = tokio::spawn({
            let loop_ = Arc::clone(&loop_);
            async move { loop_.run().await }
        });

        // A sub-agent's result arrives and its follow-up turn hangs on the model.
        bus.publish_inbound(announcement_for("cli", "direct"))
            .unwrap();
        wait_until("the follow-up turn to ask the model", || {
            requested.load(Ordering::SeqCst)
        })
        .await;
        let activity = loop_.subagents.activity();
        assert_eq!(activity.snapshot("cli:direct").follow_up_runs, 1);

        // The loop is free: `/stop` is read, answered, and ends the turn.
        bus.publish_inbound(user_message("cli", "direct", "/stop"))
            .unwrap();
        // (`/stop` first publishes a turn end, then its reply.)
        let reply = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let message = bus.consume_outbound().await.expect("the bus stays open");
                if message.content.contains("Stopped") {
                    return message;
                }
            }
        })
        .await
        .expect("/stop must be answered while a system turn runs");
        assert!(
            reply.content.contains("Stopped 1 task"),
            "{}",
            reply.content
        );
        wait_until("the follow-up to stop counting", || {
            activity.snapshot("cli:direct").follow_up_runs == 0
        })
        .await;

        loop_.stop();
        running.abort();
    }

    #[tokio::test]
    async fn a_system_turn_counts_as_follow_up_work_while_it_runs() {
        let fixture = resume_fixture(Config::default());
        let activity = fixture.loop_.subagents.activity();
        let mut changes = activity.subscribe();

        Arc::clone(&fixture.loop_)
            .process_system_message(announcement_for("cli", "direct"))
            .await
            .expect("a reply");

        let started = changes.try_recv().expect("start announced");
        assert_eq!(started.session_key, "cli:direct");
        assert_eq!(started.snapshot.follow_up_runs, 1);
        let ended = changes.try_recv().expect("end announced");
        assert!(ended.snapshot.is_idle());
    }

    fn read_file_call(path: &Path) -> LLMResponse {
        LLMResponse {
            tool_calls: vec![crate::providers::base::ToolCallRequest {
                id: "call_read".to_string(),
                name: "read_file".to_string(),
                arguments: HashMap::from([(
                    "path".to_string(),
                    json!(path.to_string_lossy().to_string()),
                )]),
                extra_content: None,
                provider_specific_fields: None,
                function_provider_specific_fields: None,
            }],
            finish_reason: "tool_calls".to_string(),
            ..LLMResponse::new()
        }
    }

    /// A fixture whose provider first hands out `script` (built from the
    /// fixture's note path), then answers every request with `reply`.
    fn fixture_with_provider(
        reply: &str,
        script: impl FnOnce(&Path) -> Vec<LLMResponse>,
    ) -> ResumeFixture {
        let fixture = resume_fixture(Config::default());
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(RecordingProvider {
            settings: GenerationSettings::new(),
            reply: reply.into(),
            script: Mutex::new(script(&fixture.note_path)),
            requests: fixture.requests.clone(),
        });
        let loop_ = Arc::new(AgentLoop::new(
            Arc::new(MessageBus::new()),
            provider,
            fixture._dir.path().to_path_buf(),
            Config::default(),
            None,
            None,
            None,
        ));
        ResumeFixture { loop_, ..fixture }
    }

    /// A fixture whose provider first asks to read the note, then answers.
    fn fixture_with_a_tool_call() -> ResumeFixture {
        fixture_with_provider("all done", |note_path| vec![read_file_call(note_path)])
    }

    #[tokio::test]
    async fn a_webui_system_turn_publishes_its_tool_calls_as_progress() {
        let fixture = fixture_with_a_tool_call();
        mark_session_webui(&fixture.loop_, "websocket:chat-1");

        Arc::clone(&fixture.loop_)
            .process_system_message(announcement_for("websocket", "chat-1"))
            .await
            .expect("a reply");

        let published = drain_outbound(&fixture.loop_.bus()).await;
        let hints: Vec<&OutboundMessage> = published
            .iter()
            .filter(|message| {
                matches!(
                    &message.event,
                    Some(OutboundEvent::Progress(event)) if event.kind == ProgressKind::ToolHint
                )
            })
            .collect();
        assert_eq!(hints.len(), 1, "one tool call, one hint: {published:?}");
        assert!(
            hints[0].content.contains("note.txt"),
            "{}",
            hints[0].content
        );
        assert_eq!(hints[0].channel, "websocket");
        assert_eq!(hints[0].chat_id, "chat-1");
        // Marked like the final reply, so the channel records it in the transcript
        // under the same turn.
        assert_eq!(hints[0].metadata.get("webui"), Some(&Value::Bool(true)));
        assert!(hints[0].metadata.contains_key("webui_turn_id"));
    }

    #[tokio::test]
    async fn a_system_turn_publishes_no_reasoning_stream() {
        let fixture = fixture_with_a_tool_call();
        mark_session_webui(&fixture.loop_, "websocket:chat-1");

        Arc::clone(&fixture.loop_)
            .process_system_message(announcement_for("websocket", "chat-1"))
            .await
            .expect("a reply");

        let published = drain_outbound(&fixture.loop_.bus()).await;
        assert!(published.iter().all(|message| !matches!(
            &message.event,
            Some(OutboundEvent::Progress(event)) if is_reasoning_progress(event.kind)
        )));
    }

    #[tokio::test]
    async fn a_system_turn_outside_a_webui_chat_publishes_no_progress() {
        let fixture = fixture_with_a_tool_call();

        Arc::clone(&fixture.loop_)
            .process_system_message(announcement_for("cli", "direct"))
            .await
            .expect("a reply");

        assert!(drain_outbound(&fixture.loop_.bus()).await.is_empty());
    }

    #[test]
    fn a_system_chat_id_splits_into_channel_and_chat() {
        assert_eq!(split_system_chat_id("websocket:abc"), ("websocket", "abc"));
        assert_eq!(split_system_chat_id("telegram:a:b"), ("telegram", "a:b"));
        assert_eq!(split_system_chat_id("direct"), ("cli", "direct"));
    }

    // ── deferred tools across turns ───────────────────────────────────────────

    /// A deferred tool of the MCP server `srv` that counts its runs.
    struct DeferredProbe {
        runs: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl crate::agent::tools::base::Tool for DeferredProbe {
        fn name(&self) -> String {
            "mcp_srv_probe".into()
        }
        fn description(&self) -> String {
            "Count deferred probe runs.".into()
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn exposure(&self) -> crate::agent::tools::base::ToolExposure {
            crate::agent::tools::base::ToolExposure::Deferred
        }
        fn mcp_server_name(&self) -> Option<String> {
            Some("srv".into())
        }
        async fn execute(&self, _: &Value) -> String {
            self.runs.fetch_add(1, Ordering::SeqCst);
            "probe ran".into()
        }
    }

    fn register_deferred_probe(loop_: &AgentLoop) -> Arc<std::sync::atomic::AtomicUsize> {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        loop_
            .tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .register(Box::new(DeferredProbe { runs: runs.clone() }));
        runs
    }

    fn tool_search_response(query: &str) -> LLMResponse {
        LLMResponse {
            tool_calls: vec![crate::providers::base::ToolCallRequest {
                id: "c-search".into(),
                name: "tool_search".into(),
                arguments: HashMap::from([("query".to_string(), json!(query))]),
                extra_content: None,
                provider_specific_fields: None,
                function_provider_specific_fields: None,
            }],
            finish_reason: "tool_calls".into(),
            ..LLMResponse::new()
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn tools_for_session_seeds_the_loads_the_view_can_take() {
        let loop_ = make_save_turn_loop(1000);
        register_deferred_probe(&loop_);
        let mut session = Session::new("test:seed-loaded".into());
        session.record_loaded_tools(&names(&["mcp_srv_probe", "ghost", "read_file"]));

        let view = loop_.tools_for_session(Some(&session));

        assert_eq!(view.loaded_tools(), names(&["mcp_srv_probe"]));
    }

    #[test]
    fn tools_for_session_skips_loads_the_mode_hides_but_keeps_them_in_the_session() {
        let loop_ = make_save_turn_loop(1000);
        register_deferred_probe(&loop_);
        let mut session = Session::new("test:seed-no-mcp".into());
        session.record_loaded_tools(&names(&["mcp_srv_probe"]));
        session.metadata.insert(
            SESSION_AGENT_MODE_METADATA_KEY.to_string(),
            json!(AgentMode::NoMcp.as_str()),
        );

        let view = loop_.tools_for_session(Some(&session));

        assert!(view.loaded_tools().is_empty());
        assert_eq!(session.loaded_tools(), names(&["mcp_srv_probe"]));
    }

    #[test]
    fn recording_loads_does_not_drop_names_the_turn_could_not_see() {
        let loop_ = make_save_turn_loop(1000);
        let key = format!("test:record-loaded-{}", uuid::Uuid::new_v4());
        {
            let mut manager = loop_.session_manager.lock().unwrap();
            let session = manager.get_or_create_session(&key);
            session.record_loaded_tools(&names(&["hidden_by_mode"]));
        }

        loop_.record_session_loaded_tools(&key, &names(&["mcp_srv_probe"]));

        let mut manager = loop_.session_manager.lock().unwrap();
        assert_eq!(
            manager.get_or_create_session(&key).loaded_tools(),
            names(&["hidden_by_mode", "mcp_srv_probe"])
        );
    }

    #[test]
    fn persist_finished_turn_keeps_loads_the_pre_run_snapshot_did_not_have() {
        let loop_ = make_save_turn_loop(1000);
        let key = format!("test:persist-loaded-{}", uuid::Uuid::new_v4());
        let mut pre_run = Session::new(key.clone());
        loop_.record_session_loaded_tools(&key, &names(&["mcp_srv_probe"]));

        loop_.persist_finished_turn(
            &mut pre_run,
            &[
                json!({"role": "user", "content": "hello"}),
                json!({"role": "assistant", "content": "Hello!"}),
            ],
            1,
            LLMUsage::new(),
            "test",
        );

        let manager = loop_.session_manager.lock().unwrap();
        let live = manager.get_session_internal(&key).unwrap();
        assert_eq!(live.loaded_tools(), names(&["mcp_srv_probe"]));
    }

    #[tokio::test]
    async fn a_tool_loaded_in_one_turn_can_be_called_in_the_next() {
        let workspace = tempfile::tempdir().unwrap();
        let model_requests = Arc::new(Mutex::new(Vec::new()));
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(RecordingProvider {
            settings: GenerationSettings::new(),
            reply: "done".into(),
            script: Mutex::new(vec![
                tool_search_response("probe"),
                text_response("loaded"),
                tool_call_response(&[("c-probe", "mcp_srv_probe")]),
                text_response("probed"),
            ]),
            requests: model_requests.clone(),
        });
        let loop_ = Arc::new(AgentLoop::new(
            Arc::new(MessageBus::new()),
            provider,
            workspace.path().to_path_buf(),
            Config::default(),
            None,
            None,
            None,
        ));
        let runs = register_deferred_probe(&loop_);
        let send = |text: &'static str| {
            Arc::clone(&loop_).process_direct(
                text,
                Some("chan:chat1"),
                Some("chan"),
                Some("chat1"),
                None,
                None,
                None,
                None,
            )
        };

        send("find the probe").await;
        let loaded_after_first_turn = loop_
            .session_manager
            .lock()
            .unwrap()
            .get_or_create_session("chan:chat1")
            .loaded_tools();
        send("run the probe").await;

        assert_eq!(loaded_after_first_turn, names(&["mcp_srv_probe"]));
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the second turn could not call the probe"
        );
    }

    // ── enabledTools ──────────────────────────────────────────────────────────

    /// An agent loop over `workspace` whose only MCP server, `fake`, lists `tool_names`
    /// and has `enabled_tools`; `None` when no Python is on `PATH`.
    fn loop_with_fake_mcp_server(
        workspace: &Path,
        tool_names: &[&str],
        enabled_tools: &[&str],
    ) -> Option<Arc<AgentLoop>> {
        let mut server =
            crate::agent::tools::mcp::test_support::fake_python_mcp_server(workspace, tool_names)?;
        server.enabled_tools = enabled_tools.iter().map(|name| name.to_string()).collect();
        let mut config = Config::default();
        config.tools.mcp_servers.insert("fake".to_string(), server);
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(PlaceholderProvider {
            settings: GenerationSettings::new(),
        });
        Some(Arc::new(AgentLoop::new(
            Arc::new(MessageBus::new()),
            provider,
            workspace.to_path_buf(),
            config,
            None,
            None,
            None,
        )))
    }

    fn registered_mcp_tools(loop_: &AgentLoop) -> Vec<String> {
        let mut names: Vec<String> = loop_
            .tools
            .lock()
            .unwrap()
            .tool_names()
            .into_iter()
            .filter(|name| name.starts_with("mcp_"))
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn enabled_tools_registers_only_the_named_mcp_tools() {
        let workspace = tempfile::tempdir().unwrap();
        let Some(loop_) = loop_with_fake_mcp_server(
            workspace.path(),
            &["ping", "pong", "skip"],
            &["ping", "mcp_fake_pong", "typo"],
        ) else {
            eprintln!("skipped: no working python on PATH");
            return;
        };

        loop_.connect_mcp().await;

        assert_eq!(
            registered_mcp_tools(&loop_),
            vec!["mcp_fake_ping", "mcp_fake_pong"]
        );
    }

    #[tokio::test]
    async fn enabled_tools_wildcard_registers_every_mcp_tool() {
        let workspace = tempfile::tempdir().unwrap();
        let Some(loop_) = loop_with_fake_mcp_server(workspace.path(), &["ping", "pong"], &["*"])
        else {
            eprintln!("skipped: no working python on PATH");
            return;
        };

        loop_.connect_mcp().await;

        assert_eq!(
            registered_mcp_tools(&loop_),
            vec!["mcp_fake_ping", "mcp_fake_pong"]
        );
    }

    #[tokio::test]
    async fn empty_enabled_tools_registers_no_mcp_tool() {
        let workspace = tempfile::tempdir().unwrap();
        let Some(loop_) = loop_with_fake_mcp_server(workspace.path(), &["ping", "pong"], &[])
        else {
            eprintln!("skipped: no working python on PATH");
            return;
        };

        loop_.connect_mcp().await;

        assert!(registered_mcp_tools(&loop_).is_empty());
    }
}
