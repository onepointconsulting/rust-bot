use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use chrono::Utc;
use tera::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::agent::context::ContextBuilder;
use crate::agent::hook::{AgentHook, AgentHookContext, CompositeHook};
use crate::agent::memory::{Consolidator, Dream};
use crate::agent::runner::{AgentRunResult, AgentRunSpec, AgentRunner};
use crate::agent::skills::BUILTIN_SKILLS_DIR;
use crate::agent::subagent::SubagentManager;
use crate::agent::tools::base::Tool;
use crate::agent::tools::cron::CronTool;
use crate::agent::tools::filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool};
use crate::agent::tools::mcp::{LoadMcpToolsError, LoadedMcpTools, load_mcp_tools_from_config};
use crate::agent::tools::message::MessageTool;
use crate::agent::tools::registry::ToolRegistry;
use crate::agent::tools::search::{GlobTool, GrepTool};
use crate::agent::tools::shell::ShellTool;
use crate::agent::tools::spawn::SpawnTool;
use crate::agent::tools::web::{WebFetchTool, WebSearchTool};
use crate::bus::events::InboundMessage;
use crate::bus::queue::MessageBus;
use crate::command::CommandContext;
use crate::command::{CommandRouter, builtin::register_builtin_commands};
use crate::config::schema::{
    AgentDefaults, ChannelsConfig, ExecToolConfig, McpServerConfig, ProviderRetryMode,
    WebToolsConfig,
};
use crate::cron::CronService;
use crate::providers::base::LLMProviderDyn;
use crate::session::manager::{Session, SessionManager};
use crate::utils::helpers::strip_think;
use crate::utils::tool_hints::format_tool_hints;

const CONTEXT_AWARE_TOOLS: &[&str] = &["message", "spawn", "cron"];

// Match Python's optional async callbacks (`tool_hint=True` keyword in Python).
pub type ProgressCallback =
    Arc<dyn Fn(String, bool) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

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
                    on_progress(thought, false).await;
                }
            }
            let tool_hint =
                safe_strip_think(Some(format_tool_hints(context.tool_calls.clone()).as_str()));
            if let Some(tool_hint) = tool_hint {
                on_progress(tool_hint, true).await;
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
        let prompt = context.usage.get("prompt_tokens").copied().unwrap_or(0);
        let completion = context.usage.get("completion_tokens").copied().unwrap_or(0);
        let cached = context.usage.get("cached_tokens").copied().unwrap_or(0);
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
    defaults: AgentDefaults,
    bus: Arc<MessageBus>,
    provider: Arc<dyn LLMProviderDyn>,
    workspace: PathBuf,
    model: String,
    max_iterations: u32,
    context_window_tokens: u32,
    context_block_limit: Option<u32>,
    max_tool_result_chars: u32,
    provider_retry_mode: String,
    web_config: WebToolsConfig,
    exec_config: ExecToolConfig,
    cron_service: Option<Arc<CronService>>,
    restrict_to_workspace: bool,
    pub session_manager: Arc<Mutex<SessionManager>>,
    mcp_servers: HashMap<String, Arc<McpServerConfig>>,
    mcp_connected: AtomicBool,
    mcp_connecting: AtomicBool,
    /// Live MCP sessions. Holding these keeps the connections open; dropping
    /// them closes the connections (RAII equivalent of Python's AsyncExitStack).
    mcp_sessions: Mutex<Vec<LoadedMcpTools>>,
    channels_config: Option<ChannelsConfig>,
    timezone: Option<String>,
    start_time: SystemTime,
    last_usage: Mutex<HashMap<String, u64>>,
    extra_hooks: Vec<Arc<dyn AgentHook>>,
    context: Arc<ContextBuilder>,
    tools: Arc<ToolRegistry>,
    runner: Arc<AgentRunner>,
    pub subagents: Arc<SubagentManager>,
    /// In-flight per-session tasks, keyed by session then by a unique task id so
    /// each task can remove itself on completion (the `add_done_callback` analog).
    pub active_tasks: Arc<AsyncMutex<HashMap<String, HashMap<u64, JoinHandle<()>>>>>,
    /// Monotonic source of task ids for `active_tasks`.
    next_task_id: AtomicU64,
    background_tasks: Arc<AsyncMutex<Vec<JoinHandle<()>>>>,
    session_locks: Arc<AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    max: usize,
    running: AtomicBool,
    concurrency_gate: Option<Arc<Semaphore>>,
    consolidator: Arc<Consolidator>,
    dream: Arc<Dream>,
    commands: CommandRouter,
}

impl AgentLoop {
    const RUNTIME_CHECKPOINT_KEY: &str = "runtime_checkpoint";

    pub fn new(
        bus: Arc<MessageBus>,
        provider: Arc<dyn LLMProviderDyn>,
        workspace: PathBuf,
        model: Option<String>,
        max_iterations: Option<u32>,
        context_window_tokens: Option<u32>,
        context_block_limit: Option<u32>,
        max_tool_result_chars: Option<u32>,
        provider_retry_mode: Option<ProviderRetryMode>,
        web_config: Option<WebToolsConfig>,
        exec_config: Option<ExecToolConfig>,
        cron_service: Option<Arc<CronService>>,
        restrict_to_workspace: Option<bool>,
        session_manager: Option<Arc<Mutex<SessionManager>>>,
        mcp_servers: Option<HashMap<String, Arc<McpServerConfig>>>,
        channels_config: Option<ChannelsConfig>,
        timezone: Option<String>,
        hooks: Option<Vec<Arc<dyn AgentHook>>>,
    ) -> Self {
        let defaults = AgentDefaults::default();
        let model = model.unwrap_or(provider.clone().get_default_model());
        let web_config = web_config.unwrap_or(WebToolsConfig::default());
        let exec_config = exec_config.clone().unwrap_or(ExecToolConfig::default());
        let restrict_to_workspace = restrict_to_workspace.unwrap_or(false);
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
        let context_window_tokens =
            context_window_tokens.unwrap_or(defaults.clone().context_window_tokens);
        let max_tool_result_chars =
            max_tool_result_chars.unwrap_or(defaults.clone().max_tool_result_chars);
        let subagents = Arc::new(SubagentManager::new(
            provider.clone(),
            workspace.clone(),
            bus.clone(),
            max_tool_result_chars as usize,
            Some(model.clone()),
            Some(web_config.clone()),
            Some(exec_config.clone()),
            Some(restrict_to_workspace),
        ));
        let mut tools = ToolRegistry::new();
        AgentLoop::register_default_tools(
            &mut tools,
            restrict_to_workspace,
            &exec_config,
            &web_config,
            bus.clone(),
            &cron_service,
            &timezone,
            &workspace,
        );
        tools.register(Box::new(SpawnTool::new(subagents.clone())));
        let tools = Arc::new(tools);
        let context = Arc::new(ContextBuilder::new(
            workspace.clone(),
            timezone.clone(),
            tools.clone(),
        ));
        let agent_loop = Self {
            defaults: defaults.clone(),
            bus: bus.clone(),
            channels_config,
            provider: provider.clone(),
            workspace: workspace.clone(),
            model: model.clone(),
            max_iterations: max_iterations.unwrap_or(defaults.clone().max_tool_iterations),
            context_window_tokens,
            context_block_limit: context_block_limit,
            max_tool_result_chars,
            provider_retry_mode: provider_retry_mode
                .unwrap_or(defaults.clone().provider_retry_mode)
                .to_string(),
            web_config: web_config.clone(),
            exec_config: exec_config.clone(),
            cron_service: cron_service,
            restrict_to_workspace: restrict_to_workspace,
            timezone: timezone,
            start_time: SystemTime::now(),
            last_usage: Mutex::new(HashMap::new()),
            extra_hooks: hooks.unwrap_or(Vec::new()),
            context: context.clone(),
            session_manager: session_manager.clone(),
            tools: tools,
            runner: Arc::new(AgentRunner::new(provider.clone())),
            subagents,
            running: AtomicBool::new(false),
            mcp_servers: mcp_servers.unwrap_or(HashMap::new()),
            mcp_connected: AtomicBool::new(false),
            mcp_connecting: AtomicBool::new(false),
            mcp_sessions: Mutex::new(Vec::new()),
            active_tasks: Arc::new(AsyncMutex::new(HashMap::new())),
            next_task_id: AtomicU64::new(0),
            background_tasks: Arc::new(AsyncMutex::new(Vec::new())),
            session_locks: Arc::new(AsyncMutex::new(HashMap::new())),
            max: max,
            concurrency_gate: concurrency_gate,
            consolidator: Arc::new(Consolidator::new(
                Arc::clone(&context.memory),
                provider.clone(),
                model.clone(),
                session_manager.clone(),
                context_window_tokens,
                Box::new(Arc::clone(&context)),
                max_tool_result_chars as usize,
            )),
            dream: Arc::new(Dream::new(
                Arc::clone(&context.memory),
                provider,
                &model,
                SessionManager::new(workspace),
                defaults.dream.max_batch_size as usize,
                defaults.dream.max_iterations as usize,
                max_tool_result_chars as usize,
            )),
            commands: {
                let mut router = CommandRouter::new();
                register_builtin_commands(&mut router);
                router
            },
        };
        agent_loop
    }

    /// Register the default set of tools.
    fn register_default_tools(
        tools: &mut ToolRegistry,
        restrict_to_workspace: bool,
        exec_config: &ExecToolConfig,
        web_config: &WebToolsConfig,
        bus: Arc<MessageBus>,
        cron_service: &Option<Arc<CronService>>,
        timezone: &Option<String>,
        workspace: &PathBuf,
    ) {
        let allowed_dir = if restrict_to_workspace || !exec_config.sandbox.is_empty() {
            Some(workspace.clone())
        } else {
            None
        };
        let extra_read = if allowed_dir.is_some() {
            vec![BUILTIN_SKILLS_DIR.clone()]
        } else {
            vec![]
        };
        let workspace = Some(workspace.clone());
        tools.register(Box::new(ReadFileTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            Some(extra_read),
        )));
        for tool in [
            Box::new(WriteFileTool::new(
                workspace.clone(),
                allowed_dir.clone(),
                None,
            )) as Box<dyn Tool>,
            Box::new(EditFileTool::new(
                workspace.clone(),
                allowed_dir.clone(),
                None,
            )),
            Box::new(ListDirTool::new(
                workspace.clone(),
                allowed_dir.clone(),
                None,
            )),
            Box::new(GlobTool::new(workspace.clone(), allowed_dir.clone(), None)),
            Box::new(GrepTool::new(workspace.clone(), allowed_dir.clone(), None)),
        ] {
            tools.register(tool);
        }
        if exec_config.enable {
            tools.register(Box::new(ShellTool::new(
                exec_config.timeout as u64,
                workspace.clone(),
                None,
                None,
                restrict_to_workspace,
                None,
                Some(exec_config.path_append.clone()),
            )));
        }
        if web_config.enable {
            tools.register(Box::new(WebSearchTool::new(
                Some(web_config.search.clone()),
                web_config.proxy.clone(),
            )));
            tools.register(Box::new(WebFetchTool::new(None, web_config.proxy.clone())));
        }
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
    }

    /// Connect to configured MCP servers (one-time, lazy).
    ///
    /// Takes `&self` (state is held in atomics / a mutex) so it can be called
    /// from a shared `Arc<Self>` in the run loop.
    async fn connect_mcp(&self) {
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
        match Self::connect_mcp_servers(&self.mcp_servers).await {
            Ok(sessions) => {
                *self.mcp_sessions.lock().unwrap_or_else(|e| e.into_inner()) = sessions;
                self.mcp_connected.store(true, Ordering::Relaxed);
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
        servers: &HashMap<String, Arc<McpServerConfig>>,
    ) -> Result<Vec<LoadedMcpTools>, LoadMcpToolsError> {
        let mut sessions = Vec::with_capacity(servers.len());
        for (name, config) in servers {
            sessions.push(load_mcp_tools_from_config(config, name).await?);
        }
        Ok(sessions)
    }

    /// Shared message bus handle for publishing outbound messages.
    pub fn bus(&self) -> Arc<MessageBus> {
        Arc::clone(&self.bus)
    }

    /// Update context for all tools that need routing info.
    pub fn set_tool_context(&self, channel: &str, chat_id: &str, message_id: Option<&str>) {
        for name in CONTEXT_AWARE_TOOLS {
            let Some(tool) = self.tools.get(name) else {
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
        message_id: Option<String>,
    ) -> AgentRunResult {
        let loop_hook = LoopHook::with_context(
            Arc::clone(self),
            on_progress,
            on_stream,
            on_stream_end,
            channel,
            chat_id,
            message_id,
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

        let result = self
            .runner
            .run(AgentRunSpec {
                initial_messages,
                tools: (*self.tools).clone(),
                model: self.model.clone(),
                max_iterations: self.max_iterations as usize,
                max_tool_result_chars: self.max_tool_result_chars as usize,
                hook: Some(hook),
                error_message: Some(
                    "Sorry, I encountered an error calling the AI model.".to_string(),
                ),
                concurrent_tools: true,
                workspace: Some(self.workspace.clone()),
                session_key,
                context_window_tokens: Some(self.context_window_tokens),
                context_block_limit: self.context_block_limit,
                provider_retry_mode: self.provider_retry_mode.clone(),
                progress_callback: None,
                checkpoint_callback,
                fail_on_tool_error: false,
                temperature: None,
                max_iterations_message: None,
                max_tokens: None,
                reasoning_effort: None,
            })
            .await;
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
            .insert(AgentLoop::RUNTIME_CHECKPOINT_KEY.to_string(), payload);
        let snapshot = session.clone();
        if let Err(e) = manager.save(snapshot) {
            log::error!("Failed to save runtime checkpoint: {e}");
        }
    }

    /// Run the agent loop, dispatching messages as tasks to stay responsive to /stop.
    async fn run(self: &Arc<Self>) {
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
                let ctx = CommandContext::new(msg.clone(), None, msg.session_key(), raw);
                if let Some(result) = self.commands.dispatch_priority(&ctx).await {
                    if let Err(error) = self.bus.publish_outbound(result) {
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
        // setdefault: get-or-create the per-session lock, then release the map.
        let lock = {
            let mut locks = self.session_locks.lock().await;
            locks
                .entry(msg.session_key())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        // `async with lock:` — now serialize only this session.
        let _guard = lock.lock().await;
    }

    /// Process a single inbound message and return the response.
    async fn process_message(
        self: Arc<Self>,
        msg: InboundMessage,
        session_key: &str,
        on_progress: Option<ProgressCallback>,
        on_stream: Option<StreamCallback>,
        on_stream_end: Option<StreamEndCallback>,
    ) {
        if msg.channel.to_lowercase() == "system" {
            let (channel, chat_id) = if msg.chat_id.contains(':') {
                let mut splits = msg.chat_id.split(':');
                let channel = splits.next().unwrap();
                let chat_id = splits.next().unwrap();
                (channel, chat_id)
            } else {
                ("cli", msg.chat_id.as_str())
            };
            log::info!("Processing system message from {}", msg.sender_id);
            let key = format!("{channel}:{chat_id}");
            let mut manager = self
                .session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let session = manager.get_or_create_session(&key);
            if self.restore_runtime_checkpoint(session) {
                let snapshot = session.clone();
                if let Err(e) = manager.save(snapshot) {
                    log::error!("Failed to save restored session: {e}");
                }
            }
        }
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
            .get(AgentLoop::RUNTIME_CHECKPOINT_KEY)
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
        session.metadata.remove(AgentLoop::RUNTIME_CHECKPOINT_KEY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::providers::base::{GenerationSettings, LLMResponse, ToolCallRequest};

    /// Minimal provider placeholder until `AgentLoop` is fully wired.
    struct PlaceholderProvider {
        settings: GenerationSettings,
    }

    #[async_trait(?Send)]
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
            _: f32,
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
            _: f32,
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

    fn recording_stream_callback() -> (StreamCallback, Arc<Mutex<Vec<String>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_cb = Arc::clone(&received);
        let callback: StreamCallback = Arc::new(move |chunk| {
            let received = Arc::clone(&received_cb);
            Box::pin(async move {
                received.lock().unwrap().push(chunk);
            })
        });
        (callback, received)
    }

    fn recording_stream_end_callback() -> (StreamEndCallback, Arc<Mutex<Vec<bool>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_cb = Arc::clone(&received);
        let callback: StreamEndCallback = Arc::new(move |resuming| {
            let received = Arc::clone(&received_cb);
            Box::pin(async move {
                received.lock().unwrap().push(resuming);
            })
        });
        (callback, received)
    }

    fn recording_progress_callback() -> (ProgressCallback, Arc<Mutex<Vec<(String, bool)>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_cb = Arc::clone(&received);
        let callback: ProgressCallback = Arc::new(move |message, tool_hint| {
            let received = Arc::clone(&received_cb);
            Box::pin(async move {
                received.lock().unwrap().push((message, tool_hint));
            })
        });
        (callback, received)
    }

    fn stream_buf_snapshot(hook: &LoopHook) -> String {
        hook.stream_buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
}
