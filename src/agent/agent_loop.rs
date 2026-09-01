use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use tera::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::agent::circuit_breaker::CIRCUIT_BREAKER_STOP_REASON;
use crate::agent::context::{ContextBuilder, DEFAULT_CURRENT_ROLE};
use crate::agent::hook::{AgentHook, AgentHookContext, CompositeHook};
use crate::agent::memory::MessageBuilder;
use crate::agent::memory::{Consolidator, Dream};
use crate::agent::model_runtime::{
    ModelRuntime, ModelRuntimeResolver, SESSION_MODEL_PRESET_METADATA_KEY,
};
use crate::agent::modes::{AgentMode, RESERVED_AGENT_MODE_NAME, SESSION_AGENT_MODE_METADATA_KEY};
use crate::agent::runner::{AgentRunResult, AgentRunSpec, AgentRunner};
use crate::agent::subagent::SubagentManager;
use crate::agent::tools::cron::CronTool;
use crate::agent::tools::filesystem::FsToolConfig;
use crate::agent::tools::goal::UpdateGoalTool;
use crate::agent::tools::mcp::mcp_file_ref::FileRefResolver;
use crate::agent::tools::mcp::{LoadMcpToolsError, LoadedMcpTools, load_mcp_tools_with_file_refs};
use crate::agent::tools::message::MessageTool;
use crate::agent::tools::registry::ToolRegistry;
use crate::agent::tools::shell::ShellTool;
use crate::agent::tools::spawn::SpawnTool;
use crate::agent::workspace_context::{
    bind_workspace_scope, reset_workspace_scope, with_workspace_scope_stack,
};
use crate::bus::events::{InboundMessage, OutboundMessage};
use crate::bus::outbound_events::{
    OutboundEvent, ProgressEvent, ProgressKind, StreamDeltaEvent, StreamEndEvent,
    StreamedResponseEvent, TurnEndEvent, outbound_message_for_event,
};
use crate::bus::queue::MessageBus;
use crate::command::CommandContext;
use crate::command::types::ChatCommand;
use crate::command::{CommandRouter, builtin::register_builtin_commands};
use crate::config::schema::{
    ChannelsConfig, Config, DocxToolConfig, ExecToolConfig, GmailToolConfig,
    ImageGenerationToolConfig, McpServerConfig, OcrToolConfig, RESERVED_MODEL_PRESET_NAME,
    WebToolsConfig,
};
use crate::cron::CronService;
use crate::providers::base::{LLMProviderDyn, LLMUsage};
use crate::runtime_context::RUNTIME_CONTEXT_TAG;
use crate::security::workspace_access::{
    WORKSPACE_SCOPE_METADATA_KEY, WorkspaceAccessMode, WorkspaceScope, WorkspaceScopeError,
    WorkspaceScopeResolver, validate_workspace_scope_payload,
};
use crate::security::workspace_requests::WorkspaceRequestHandler;
use crate::session::goal_state;
use crate::session::keys::{COMMAND_KEY, RUNTIME_CHECKPOINT_KEY};
use crate::session::manager::{Session, SessionManager};
use crate::utils::helpers::{image_placeholder_text, strip_think, truncate_text};
use crate::utils::registry_helper::{
    filesystem_tool_scope, register_conversion_tools, register_filesystem_tools,
    register_gmail_tools, register_image_generation_tools, register_ocr_tools, register_web_tools,
};
use crate::utils::runtime::EMPTY_FINAL_RESPONSE_MESSAGE;
use crate::utils::tool_hints::format_tool_hints;

const CONTEXT_AWARE_TOOLS: &[&str] = &["message", "spawn", "cron", "update_goal"];

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
    if text.is_none() {
        return None;
    }
    let text = strip_think(text.unwrap());
    if text.is_empty() { None } else { Some(text) }
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

        if !incremental.is_empty() {
            if let Some(on_stream) = &self.on_stream {
                on_stream(incremental).await;
            }
        }
    }

    async fn on_stream_end(&self, _ctx: &mut AgentHookContext, resuming: bool) {
        if let Some(on_stream_end) = &self.on_stream_end {
            on_stream_end(resuming).await;
        }
        let mut buf = self.stream_buf.lock().unwrap_or_else(|e| e.into_inner());
        buf.clear();
    }

    async fn before_execute_tools(&self, context: &mut AgentHookContext) {
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
    }

    async fn after_iteration(&self, context: &mut AgentHookContext) {
        let prompt = context.usage.prompt_tokens().unwrap_or(0);
        let completion = context.usage.output_tokens.unwrap_or(0);
        let cached = context.usage.cache_read_input_tokens.unwrap_or(0);
        log::debug!("LLM usage: prompt={prompt} completion={completion} cached={cached}");
    }

    fn finalize_content(&self, _ctx: &AgentHookContext, content: Option<String>) -> Option<String> {
        safe_strip_think(content.as_deref())
    }
}

/// Run the core hook before extra hooks.
struct LoopHookChain {
    primary: Arc<dyn AgentHook>,
    extras: CompositeHook,
}

impl LoopHookChain {
    pub fn new(primary: Arc<dyn AgentHook>, extras: Vec<Arc<dyn AgentHook>>) -> Self {
        Self {
            primary,
            extras: CompositeHook::new(extras),
        }
    }
}

#[async_trait]
impl AgentHook for LoopHookChain {
    fn wants_streaming(&self) -> bool {
        self.primary.wants_streaming() || self.extras.wants_streaming()
    }

    async fn before_iteration(&self, context: &mut AgentHookContext) {
        self.primary.before_iteration(context).await;
        self.extras.before_iteration(context).await;
    }

    async fn on_stream(&self, context: &mut AgentHookContext, delta: &str) {
        self.primary.on_stream(context, delta).await;
        self.extras.on_stream(context, delta).await;
    }

    async fn on_stream_end(&self, context: &mut AgentHookContext, resuming: bool) {
        self.primary.on_stream_end(context, resuming).await;
        self.extras.on_stream_end(context, resuming).await;
    }

    async fn before_execute_tools(&self, context: &mut AgentHookContext) {
        self.primary.before_execute_tools(context).await;
        self.extras.before_execute_tools(context).await;
    }

    async fn after_iteration(&self, context: &mut AgentHookContext) {
        self.primary.after_iteration(context).await;
        self.extras.after_iteration(context).await;
    }

    fn finalize_content(
        &self,
        context: &AgentHookContext,
        content: Option<String>,
    ) -> Option<String> {
        let content = self.primary.finalize_content(context, content);
        self.extras.finalize_content(context, content)
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
    _channels_config: Option<ChannelsConfig>,
    _timezone: Option<String>,
    pub start_time: SystemTime,
    pub last_usage: Mutex<LLMUsage>,
    extra_hooks: Vec<Arc<dyn AgentHook>>,
    context: Arc<ContextBuilder>,
    pub(crate) tools: Arc<Mutex<ToolRegistry>>,
    pub subagents: Arc<SubagentManager>,
    /// In-flight per-session tasks, keyed by session then by a unique task id so
    /// each task can remove itself on completion (the `add_done_callback` analog).
    pub active_tasks: Arc<AsyncMutex<HashMap<String, HashMap<u64, JoinHandle<()>>>>>,
    /// Monotonic source of task ids for `active_tasks`.
    next_task_id: AtomicU64,
    background_tasks: Arc<AsyncMutex<HashMap<u64, JoinHandle<()>>>>,
    /// Monotonic source of task ids for `background_tasks`.
    next_background_task_id: AtomicU64,
    session_locks: Arc<AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    _max: usize,
    running: AtomicBool,
    concurrency_gate: Option<Arc<Semaphore>>,
    pub consolidator: Arc<Consolidator>,
    pub dream: Arc<Dream>,
    commands: CommandRouter,
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
        let restrict_to_workspace = tools_cfg.restrict_to_workspace;
        let workspace_scopes =
            WorkspaceScopeResolver::new(workspace.clone(), restrict_to_workspace);
        let mcp_servers = tools_cfg.mcp_servers.clone();

        let context_window_tokens = agents_cfg.context_window_tokens;
        let max_tool_result_chars = agents_cfg.max_tool_result_chars;
        let max_iterations = agents_cfg.max_tool_iterations;
        let context_block_limit = agents_cfg.context_block_limit;
        let provider_retry_mode = agents_cfg.provider_retry_mode.clone();

        let max = std::env::var("RUST_BOT_MAX_CONCURRENT_REQUESTS")
            .unwrap_or_else(|_| "3".to_string())
            .parse()
            .unwrap_or(3);

        let concurrency_gate = if max > 0 {
            Some(Arc::new(Semaphore::new(max)))
        } else {
            None
        };
        let session_manager = session_manager
            .unwrap_or_else(|| Arc::new(Mutex::new(SessionManager::new(workspace.clone()))));
        let runtime_resolver =
            Arc::new(ModelRuntimeResolver::new(config.clone(), provider.clone()));
        let subagents = Arc::new(SubagentManager::new(
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
        ));
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
            bus.clone(),
            &cron_service,
            &timezone,
            &workspace,
            session_manager.clone(),
        );
        tools.register(Box::new(SpawnTool::new(subagents.clone())));
        let tools = Arc::new(Mutex::new(tools));
        let context = Arc::new(ContextBuilder::with_default_mode(
            workspace.clone(),
            timezone.clone(),
            tools.clone(),
            agents_cfg.mode,
        ));
        let consolidator = Arc::new(Consolidator::new(
            Arc::clone(&context.memory),
            runtime_resolver.clone(),
            session_manager.clone(),
            context_window_tokens,
            Box::new(Arc::clone(&context)),
            max_tool_result_chars as usize,
        ));

        let dream_cfg = agents_cfg.dream.clone();

        let agent_loop = Self {
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
            workspace_scopes,
            _timezone: timezone,
            start_time: SystemTime::now(),
            last_usage: Mutex::new(LLMUsage::new()),
            extra_hooks: hooks.unwrap_or(Vec::new()),
            context: context.clone(),
            session_manager: session_manager.clone(),
            tools,
            subagents,
            running: AtomicBool::new(false),
            mcp_servers,
            mcp_connected: AtomicBool::new(false),
            mcp_connecting: AtomicBool::new(false),
            mcp_sessions: Mutex::new(Vec::new()),
            active_tasks: Arc::new(AsyncMutex::new(HashMap::new())),
            next_task_id: AtomicU64::new(0),
            background_tasks: Arc::new(AsyncMutex::new(HashMap::new())),
            next_background_task_id: AtomicU64::new(0),
            session_locks: Arc::new(AsyncMutex::new(HashMap::new())),
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
            commands: {
                let mut router = CommandRouter::new();
                register_builtin_commands(&mut router);
                router
            },
            config,
        };
        agent_loop
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
            format!("Unknown agent mode '{name}'. Available modes: standard, minimal")
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

    /// Tools visible to this session after applying its agent mode.
    pub fn tools_for_session(&self, session: Option<&Session>) -> ToolRegistry {
        let mode = self.mode_for_session(session);
        self.tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .restrict(mode.allowed_tool_names())
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
        let scope = validate_workspace_scope_payload(
            &serde_json::json!({
                "project_path": project_path.display().to_string(),
                "access_mode": access_mode.as_str(),
            }),
            &self.workspace,
            self.restrict_to_workspace,
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
    pub fn clear_session_workspace_scope(
        &self,
        session_manager: &mut SessionManager,
        session_key: &str,
    ) {
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
    fn register_default_tools(
        tools: &mut ToolRegistry,
        restrict_to_workspace: bool,
        exec_config: &ExecToolConfig,
        web_config: &WebToolsConfig,
        gmail_config: &GmailToolConfig,
        ocr_config: &OcrToolConfig,
        docx_config: &DocxToolConfig,
        image_generation_config: &ImageGenerationToolConfig,
        bus: Arc<MessageBus>,
        cron_service: &Option<Arc<CronService>>,
        timezone: &Option<String>,
        workspace: &PathBuf,
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
                Some(workspace.clone()),
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
            &FsToolConfig::new(Some(workspace.clone()), allowed_dir, Some(extra_read)),
            tools,
        );
        register_image_generation_tools(image_generation_config, workspace, tools);
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

    /// Connect to configured MCP servers (one-time, lazy).
    ///
    /// Takes `&self` (state is held in atomics / a mutex) so it can be called
    /// from a shared `Arc<Self>` in the run loop.
    pub async fn connect_mcp(&self) {
        if self.mcp_connected.load(Ordering::Relaxed)
            || self.mcp_connecting.load(Ordering::Relaxed)
            || self.mcp_servers.is_empty()
        {
            return;
        }
        self.mcp_connecting.store(true, Ordering::Relaxed);

        // Rust uses RAII instead of an AsyncExitStack: each established session
        // closes its connection when dropped. On success we keep the sessions
        // alive by storing them on `self`; on failure they are dropped here,
        // which is the equivalent of `await stack.aclose()`.
        match Self::connect_mcp_servers(&self.mcp_servers, self.mcp_file_ref_resolver()).await {
            Ok(mut sessions) => {
                let mut mcp_tool_count = 0usize;
                {
                    let mut registry = self.tools.lock().unwrap_or_else(|e| e.into_inner());
                    for session in &mut sessions {
                        mcp_tool_count += session.tools.len();
                        for tool in session.tools.drain(..) {
                            registry.register(tool);
                        }
                    }
                }
                *self.mcp_sessions.lock().unwrap_or_else(|e| e.into_inner()) = sessions;
                self.mcp_connected.store(true, Ordering::Relaxed);
                log::info!(
                    "{} MCP server(s) connected successfully",
                    self.mcp_servers.len()
                );
                log::info!("{mcp_tool_count} MCP tools registered");
            }
            Err(e) => {
                log::error!("Failed to connect MCP servers (will retry next message): {e}");
                // Drop any partially-established sessions (closes them).
                self.mcp_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
            }
        }

        // `finally`: always release the connecting flag.
        self.mcp_connecting.store(false, Ordering::Relaxed);
    }

    /// Connect to every configured MCP server and load its tools.
    ///
    /// Returns the live sessions (each owns its connection plus the loaded tool
    /// wrappers). Fails fast on the first server that cannot be reached; the
    /// sessions established so far are dropped as the error unwinds.
    async fn connect_mcp_servers(
        servers: &HashMap<String, McpServerConfig>,
        file_refs: Option<FileRefResolver>,
    ) -> Result<Vec<LoadedMcpTools>, LoadMcpToolsError> {
        let mut sessions = Vec::with_capacity(servers.len());
        for (name, config) in servers {
            sessions.push(load_mcp_tools_with_file_refs(config, name, file_refs.clone()).await?);
        }
        Ok(sessions)
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

    /// Connected MCP servers as `(name, endpoint)` pairs, sorted by name.
    ///
    /// Returns an empty vec when not connected.
    pub fn connected_mcp_endpoints(&self) -> Vec<(String, String)> {
        if !self.is_mcp_connected() {
            return Vec::new();
        }
        let mut servers: Vec<(String, String)> = self
            .mcp_servers
            .iter()
            .map(|(name, config)| (name.clone(), mcp_server_endpoint(config)))
            .collect();
        servers.sort_by(|a, b| a.0.cmp(&b.0));
        servers
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

    /// Run the agent iteration loop.
    ///
    /// `on_stream` is called with each content delta during streaming.
    /// `on_stream_end(resuming)` is called when a streaming session finishes:
    /// `resuming = true` means tool calls follow (spinner should restart),
    /// `resuming = false` means this is the final response.
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
    ) -> AgentRunResult {
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
        let run_tools = self.tools_for_session(session.as_ref());
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
        let result = runner
            .run(AgentRunSpec {
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
            })
            .await;
        reset_workspace_scope(workspace_scope_token);
        *self.last_usage.lock().unwrap_or_else(|e| e.into_inner()) = result.usage.clone();
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
        if let Some(session_key) = session_key {
            if result.usage != LLMUsage::new() {
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
        }
        result
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

    /// Run the agent loop, dispatching messages as tasks to stay responsive to /stop.
    pub async fn run(self: &Arc<Self>) {
        self.running.store(true, Ordering::Relaxed);
        self.connect_mcp().await;
        log::info!("Agent loop started");
        while self.running.load(Ordering::Relaxed) {
            // `consume_inbound` takes `&self` (the receiver is locked internally),
            // so producers can keep publishing while we wait here.
            let msg = match tokio::time::timeout(Duration::from_secs(1), self.bus.consume_inbound())
                .await
            {
                Ok(Some(msg)) => msg,      // got a message
                Ok(None) => break,         // channel closed (all senders dropped)
                Err(_elapsed) => continue, // timed out → poll again
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
            } else if msg.channel.eq_ignore_ascii_case("system") {
                // System messages may run consolidation, which calls the `?Send`
                // LLM provider — handled on the run-loop task, not via `spawn`.
                if let Some(response) = Arc::clone(self).process_system_message(msg).await {
                    if let Err(error) = self.bus.publish_outbound(response) {
                        log::error!("Failed to publish outbound message: {error}");
                    }
                }
            } else {
                // Everything else is dispatched as its own task so the loop stays
                // responsive (and the task is cancellable via /stop).
                let this = Arc::clone(self);
                let key = msg.session_key();
                let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
                let active_tasks = Arc::clone(&self.active_tasks);
                let cleanup_key = key.clone();

                // Hold the registry lock across spawn + insert so the task's tail
                // cleanup can't run (and no-op) before the handle is registered.
                let mut map = self.active_tasks.lock().await;
                let handle = tokio::spawn(async move {
                    this.dispatch(msg).await;
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

    /// Process a message: per-session serial, cross-session concurrent.
    async fn dispatch(self: Arc<Self>, msg: InboundMessage) {
        let session_key = msg.session_key();

        // setdefault: get-or-create the per-session lock, then release the map.
        let lock = {
            let mut locks = self.session_locks.lock().await;
            locks
                .entry(session_key.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        // `async with lock, gate:` — serialize this session, then cap global concurrency.
        let _guard = lock.lock().await;
        let _permit = match &self.concurrency_gate {
            Some(gate) => Some(
                Arc::clone(gate)
                    .acquire_owned()
                    .await
                    .expect("concurrency semaphore closed"),
            ),
            None => None,
        };

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
                if let Err(e) = self.bus.publish_outbound(response) {
                    log::error!("Failed to publish outbound message: {e}");
                }
            }
            Ok(None) if msg.channel == "cli" => {
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
            Ok(None) => {}
            Err(panic) => {
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
        with_workspace_scope_stack(move || self.process_system_message_inner(msg)).await
    }

    async fn process_system_message_inner(
        self: Arc<Self>,
        msg: InboundMessage,
    ) -> Option<OutboundMessage> {
        let (channel, chat_id) = if let Some((channel, chat_id)) = msg.chat_id.split_once(':') {
            (channel, chat_id)
        } else {
            ("cli", msg.chat_id.as_str())
        };
        log::info!("Processing system message from {}", msg.sender_id);
        let key = format!("{channel}:{chat_id}");

        // Restore any runtime checkpoint (a partially completed turn from a crash
        // or interrupt). Persist only when something was actually restored.
        {
            let mut manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let session = manager.get_or_create_session(&key);
            if self.restore_runtime_checkpoint(session) {
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
        let mut snapshot = {
            let mut manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            manager.get_or_create_session(&key).clone()
        };

        let history = snapshot.get_history(Some(0));
        let turn_runtime = self.runtime_resolver.runtime_for_session(Some(&snapshot));
        let current_role = Self::subagent_announce_role(&turn_runtime.model);
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
            current_role,
        );
        let agent_run_result = self
            .run_agent_loop(
                messages,
                None,
                None,
                None,
                Some(snapshot.clone()),
                channel,
                chat_id,
                msg.metadata.get("message_id").and_then(Value::as_str),
            )
            .await;
        let final_content = agent_run_result.final_content;
        let all_msgs = agent_run_result.messages;

        self.persist_finished_turn(
            &mut snapshot,
            all_msgs.as_slice(),
            1 + history.len() as u32,
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
            metadata: HashMap::new(),
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
            format!("{}...", &msg.content.chars().take(80).collect::<String>())
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
            let session = session_manager.get_or_create_session(&key);
            let restored = self.restore_runtime_checkpoint(session);
            // End the `&mut session` borrow before re-borrowing the manager to save.
            let snapshot = session.clone();
            if restored {
                if let Err(e) = session_manager.save(snapshot.clone()) {
                    log::error!("Failed to save restored session: {e}");
                }
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
        if self.commands.is_priority(raw) {
            if let Some(result) = self.commands.dispatch_priority(&ctx).await {
                self.persist_command_turn(&key, &msg.content, raw, &result);
                return Some(result);
            }
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
        let mut session = {
            let mut session_manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            session_manager.get_or_create_session(&key).clone()
        };
        let history = session.get_history(Some(0));
        let media = if !msg.media.is_empty() {
            Some(&msg.media.as_slice()[..])
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
        );

        let bus_progress: ProgressCallback = {
            let bus = Arc::clone(&self.bus);
            let channel = msg.channel.clone();
            let chat_id = msg.chat_id.clone();
            let base_meta = msg.metadata.clone();
            Arc::new(move |content: String, kind: ProgressKind| {
                let bus = Arc::clone(&bus);
                let channel = channel.clone();
                let chat_id = chat_id.clone();
                let mut meta = base_meta.clone();
                Box::pin(async move {
                    // `Reasoning*` isn't published through this bus path yet —
                    // nothing upstream produces it here, so treat it as a
                    // tripwire rather than silently wiring up the wrong
                    // dispatch (reasoning has its own send path in
                    // `ChannelManager::send_once`).
                    if matches!(
                        kind,
                        ProgressKind::Reasoning
                            | ProgressKind::ReasoningDelta
                            | ProgressKind::ReasoningEnd
                    ) {
                        log::warn!(
                            "bus_progress: ignoring unsupported {kind:?} progress event \
                             (reasoning is not yet wired through this callback)"
                        );
                        return;
                    }
                    meta.insert("_progress".into(), Value::Bool(true));
                    meta.insert(
                        "_tool_hint".into(),
                        Value::Bool(kind == ProgressKind::ToolHint),
                    );
                    if let Err(e) = bus.publish_outbound(OutboundMessage {
                        channel,
                        chat_id,
                        content,
                        reply_to: None,
                        media: vec![],
                        metadata: meta,
                        event: Some(OutboundEvent::Progress(ProgressEvent {
                            kind,
                            ..ProgressEvent::default()
                        })),
                    }) {
                        log::error!("Failed to publish progress message: {e}");
                    }
                }) as Pin<Box<dyn Future<Output = ()> + Send>>
            })
        };

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
            result.usage.clone(),
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
        if stop_reason != CIRCUIT_BREAKER_STOP_REASON {
            if let Some(message_tool) = self
                .tools
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get("message")
            {
                if let Some(message_tool) =
                    (message_tool.as_ref() as &dyn std::any::Any).downcast_ref::<MessageTool>()
                {
                    if *message_tool
                        .sent_in_turn
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                    {
                        return None;
                    }
                }
            }
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
    fn restore_runtime_checkpoint(&self, session: &mut Session) -> bool {
        let Some(checkpoint) = session
            .metadata
            .get(RUNTIME_CHECKPOINT_KEY)
            .and_then(Value::as_object)
        else {
            return false;
        };

        let mut restored_messages: Vec<Value> = Vec::new();

        if let Some(assistant) = checkpoint
            .get("assistant_message")
            .and_then(Value::as_object)
        {
            let mut restored = assistant.clone();
            restored
                .entry("timestamp")
                .or_insert_with(|| Value::String(Utc::now().to_rfc3339()));
            restored_messages.push(Value::Object(restored));
        }

        if let Some(results) = checkpoint
            .get("completed_tool_results")
            .and_then(Value::as_array)
        {
            for message in results {
                if let Some(obj) = message.as_object() {
                    let mut restored = obj.clone();
                    restored
                        .entry("timestamp")
                        .or_insert_with(|| Value::String(Utc::now().to_rfc3339()));
                    restored_messages.push(Value::Object(restored));
                }
            }
        }

        if let Some(pending) = checkpoint
            .get("pending_tool_calls")
            .and_then(Value::as_array)
        {
            for tool_call in pending {
                let Some(tool_call) = tool_call.as_object() else {
                    continue;
                };
                let tool_id = tool_call.get("id").cloned().unwrap_or(Value::Null);
                let name = tool_call
                    .get("function")
                    .and_then(Value::as_object)
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let mut obj = serde_json::Map::new();
                obj.insert("role".into(), Value::String("tool".into()));
                obj.insert("tool_call_id".into(), tool_id);
                obj.insert("name".into(), Value::String(name));
                obj.insert(
                    "content".into(),
                    Value::String("Error: Task interrupted before this tool finished.".into()),
                );
                obj.insert("timestamp".into(), Value::String(Utc::now().to_rfc3339()));
                restored_messages.push(Value::Object(obj));
            }
        }

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
        self.mcp_connected.store(false, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        log::info!("Agent loop stopping ... shutting down");
    }

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
            metadata: HashMap::new(),
        };
        self.process_message(msg, session_key, on_progress, on_stream, on_stream_end)
            .await
    }

    fn subagent_announce_role(model: &str) -> &'static str {
        const REQUIRES_USER_LAST: &[&str] = &["claude", "anthropic"];
        let model = model.to_ascii_lowercase();
        if REQUIRES_USER_LAST.iter().any(|m| model.contains(m)) {
            "user"
        } else {
            "assistant"
        }
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

        async fn before_execute_tools(&self, _ctx: &mut AgentHookContext) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:before_execute_tools", self.label));
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
        assert!(session.metadata.get(WORKSPACE_SCOPE_METADATA_KEY).is_none());
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
            loop_.clear_session_workspace_scope(&mut manager, "test:workspace_scope_clear");
        }

        let mut manager = loop_
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = manager.get_or_create_session("test:workspace_scope_clear");
        assert!(session.metadata.get(WORKSPACE_SCOPE_METADATA_KEY).is_none());
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
                session
                    .metadata
                    .get(SESSION_AGENT_MODE_METADATA_KEY)
                    .is_none()
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
            session
                .metadata
                .get(SESSION_AGENT_MODE_METADATA_KEY)
                .is_none()
        );
    }
}
