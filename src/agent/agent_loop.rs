use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use tera::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::agent::context::{ContextBuilder, DEFAULT_CURRENT_ROLE, RUNTIME_CONTEXT_TAG};
use crate::agent::hook::{AgentHook, AgentHookContext, CompositeHook};
use crate::agent::memory::MessageBuilder;
use crate::agent::memory::{Consolidator, Dream};
use crate::agent::runner::{AgentRunResult, AgentRunSpec, AgentRunner};
use crate::agent::subagent::SubagentManager;
use crate::agent::tools::cron::CronTool;
use crate::agent::tools::filesystem::FsToolConfig;
use crate::agent::tools::mcp::{LoadMcpToolsError, LoadedMcpTools, load_mcp_tools_with_file_refs};
use crate::agent::tools::mcp_file_ref::FileRefResolver;
use crate::agent::tools::message::MessageTool;
use crate::agent::tools::registry::ToolRegistry;
use crate::agent::tools::shell::ShellTool;
use crate::agent::tools::spawn::SpawnTool;
use crate::bus::events::{InboundMessage, OutboundMessage};
use crate::bus::queue::MessageBus;
use crate::command::CommandContext;
use crate::command::{CommandRouter, builtin::register_builtin_commands};
use crate::config::schema::{
    AgentDefaults, ChannelsConfig, DocxToolConfig, ExecToolConfig, GmailToolConfig, ImageGenerationToolConfig, McpServerConfig, OcrToolConfig, ProviderRetryMode, SubagentConfig, WebToolsConfig,
};
use crate::cron::CronService;
use crate::providers::base::LLMProviderDyn;
use crate::session::manager::{Session, SessionManager};
use crate::utils::helpers::{image_placeholder_text, strip_think, truncate_text};
use crate::utils::registry_helper::{
    filesystem_tool_scope, register_conversion_tools, register_filesystem_tools, register_gmail_tools, register_image_generation_tools, register_ocr_tools, register_web_tools,
};
use crate::utils::runtime::EMPTY_FINAL_RESPONSE_MESSAGE;
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
    pub defaults: AgentDefaults,
    bus: Arc<MessageBus>,
    pub provider: Arc<dyn LLMProviderDyn>,
    workspace: PathBuf,
    pub model: String,
    max_iterations: u32,
    max_tokens: u32,
    temperature: f32,
    reasoning_effort: Option<String>,
    pub context_window_tokens: u64,
    context_block_limit: Option<u32>,
    max_tool_result_chars: u32,
    provider_retry_mode: String,
    pub web_config: WebToolsConfig,
    exec_config: ExecToolConfig,
    pub cron_service: Option<Arc<CronService>>,
    restrict_to_workspace: bool,
    pub session_manager: Arc<Mutex<SessionManager>>,
    mcp_servers: HashMap<String, McpServerConfig>,
    mcp_connected: AtomicBool,
    mcp_connecting: AtomicBool,
    /// Live MCP sessions. Holding these keeps the connections open; dropping
    /// them closes the connections (RAII equivalent of Python's AsyncExitStack).
    mcp_sessions: Mutex<Vec<LoadedMcpTools>>,
    channels_config: Option<ChannelsConfig>,
    timezone: Option<String>,
    pub start_time: SystemTime,
    pub last_usage: Mutex<HashMap<String, u64>>,
    extra_hooks: Vec<Arc<dyn AgentHook>>,
    context: Arc<ContextBuilder>,
    pub(crate) tools: Arc<Mutex<ToolRegistry>>,
    runner: Arc<AgentRunner>,
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
    max: usize,
    running: AtomicBool,
    concurrency_gate: Option<Arc<Semaphore>>,
    pub consolidator: Arc<Consolidator>,
    pub dream: Arc<Dream>,
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
        max_tokens: Option<u32>,
        temperature: Option<f32>,
        reasoning_effort: Option<String>,
        context_window_tokens: Option<u64>,
        context_block_limit: Option<u32>,
        max_tool_result_chars: Option<u32>,
        provider_retry_mode: Option<ProviderRetryMode>,
        web_config: Option<WebToolsConfig>,
        exec_config: Option<ExecToolConfig>,
        gmail_config: Option<GmailToolConfig>,
        ocr_config: Option<OcrToolConfig>,
        docx_config: Option<DocxToolConfig>,
        image_generation_config: Option<ImageGenerationToolConfig>,
        subagent_config: Option<SubagentConfig>,
        cron_service: Option<Arc<CronService>>,
        restrict_to_workspace: Option<bool>,
        session_manager: Option<Arc<Mutex<SessionManager>>>,
        mcp_servers: Option<HashMap<String, McpServerConfig>>,
        channels_config: Option<ChannelsConfig>,
        timezone: Option<String>,
        hooks: Option<Vec<Arc<dyn AgentHook>>>,
    ) -> Self {
        let defaults = AgentDefaults::default();
        let model = model.unwrap_or(provider.clone().get_default_model());
        let web_config = web_config.unwrap_or(WebToolsConfig::default());
        let exec_config = exec_config.unwrap_or(ExecToolConfig::default());
        let gmail_config = gmail_config.unwrap_or(GmailToolConfig::default());
        let ocr_config = ocr_config.unwrap_or(OcrToolConfig::default());
        let docx_config = docx_config.unwrap_or(DocxToolConfig::default());
        let image_generation_config =
            image_generation_config.unwrap_or(ImageGenerationToolConfig::default());
        let subagent_config = subagent_config.unwrap_or(SubagentConfig::default());
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
        let max_tokens = max_tokens.unwrap_or(defaults.clone().max_tokens);
        let temperature = temperature.unwrap_or(defaults.clone().temperature);
        let reasoning_effort = reasoning_effort.or_else(|| defaults.clone().reasoning_effort);
        let subagents = Arc::new(SubagentManager::new(
            provider.clone(),
            workspace.clone(),
            bus.clone(),
            max_tool_result_chars as usize,
            Some(model.clone()),
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
        );
        tools.register(Box::new(SpawnTool::new(subagents.clone())));
        let tools = Arc::new(Mutex::new(tools));
        let context = Arc::new(ContextBuilder::new(
            workspace.clone(),
            timezone.clone(),
            tools.clone(),
        ));
        let consolidator = Arc::new(Consolidator::new(
            Arc::clone(&context.memory),
            provider.clone(),
            model.clone(),
            session_manager.clone(),
            context_window_tokens,
            Box::new(Arc::clone(&context)),
            max_tool_result_chars as usize,
        ));

        let agent_loop = Self {
            defaults: defaults.clone(),
            bus: bus.clone(),
            channels_config,
            provider: provider.clone(),
            workspace: workspace.clone(),
            model: model.clone(),
            max_iterations: max_iterations.unwrap_or(defaults.clone().max_tool_iterations),
            max_tokens,
            temperature,
            reasoning_effort,
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
            background_tasks: Arc::new(AsyncMutex::new(HashMap::new())),
            next_background_task_id: AtomicU64::new(0),
            session_locks: Arc::new(AsyncMutex::new(HashMap::new())),
            max: max,
            concurrency_gate: concurrency_gate,
            consolidator,
            dream: Arc::new(Dream::new(
                Arc::clone(&context.memory),
                provider,
                defaults
                    .dream
                    .model_override
                    .as_deref()
                    .unwrap_or(&model),
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
        gmail_config: &GmailToolConfig,
        ocr_config: &OcrToolConfig,
        docx_config: &DocxToolConfig,
        image_generation_config: &ImageGenerationToolConfig,
        bus: Arc<MessageBus>,
        cron_service: &Option<Arc<CronService>>,
        timezone: &Option<String>,
        workspace: &PathBuf,
    ) {
        log::info!("Registering default tools");
        let (allowed_dir, extra_read) = filesystem_tool_scope(
            workspace,
            restrict_to_workspace,
            &exec_config.sandbox,
        );
        register_filesystem_tools(
            tools,
            workspace,
            allowed_dir.clone(),
            extra_read.clone(),
        );
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
        register_ocr_tools(ocr_config, workspace, allowed_dir.clone(), extra_read.clone(), tools);
        register_conversion_tools(
            docx_config, 
            &FsToolConfig::new(
                Some(workspace.clone()),
                 allowed_dir, Some(extra_read)), tools
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
                    let mut registry = self
                        .tools
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    for session in &mut sessions {
                        mcp_tool_count += session.tools.len();
                        for tool in session.tools.drain(..) {
                            registry.register(tool);
                        }
                    }
                }
                *self.mcp_sessions.lock().unwrap_or_else(|e| e.into_inner()) = sessions;
                self.mcp_connected.store(true, Ordering::Relaxed);
                log::info!("{} MCP server(s) connected successfully", self.mcp_servers.len());
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
            sessions
                .push(load_mcp_tools_with_file_refs(config, name, file_refs.clone()).await?);
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
        
        let run_tools = self.tools.lock().unwrap_or_else(|e| e.into_inner()).clone();
        log::info!("Running agent loop with {} tools", run_tools.len());
        log::info!("Max Tokens: {}", self.max_tokens);
        let result = self
            .runner
            .run(AgentRunSpec {
                initial_messages,
                tools: run_tools,
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
                temperature: Some(self.temperature),
                max_iterations_message: None,
                max_tokens: Some(self.max_tokens as usize),
                reasoning_effort: self.reasoning_effort.clone(),
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
                let ctx = CommandContext::with_options(msg.clone(), None, msg.session_key(), raw, "", Some(Arc::clone(self)));
                if let Some(result) = self.commands.dispatch_priority(&ctx).await {
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
                        meta.insert("_stream_id".into(), Value::String(stream_id));
                        let _ = bus.publish_outbound(OutboundMessage {
                            channel,
                            chat_id,
                            content: delta,
                            reply_to: None,
                            media: vec![],
                            metadata: meta,
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
                        meta.insert("_stream_id".into(), Value::String(stream_id));
                        let _ = bus.publish_outbound(OutboundMessage {
                            channel,
                            chat_id,
                            content: String::new(),
                            reply_to: None,
                            media: vec![],
                            metadata: meta,
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
        // equivalent of Python's `except Exception`.
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
                });
            }
        }
    }

    

    /// Handle system-channel messages (checkpoint restore + consolidation).
    ///
    /// Kept separate from [`Self::process_message`] so spawned `dispatch`
    /// tasks stay `Send` (consolidation calls the `?Send` LLM provider).
    pub async fn process_system_message(
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
        let current_role = Self::subagent_announce_role(&self.model);
        let messages = self.context.build_messages(
            history.as_slice(),
            msg.content.as_str(),
            None,
            None,
            Some(channel),
            Some(chat_id),
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

        // Save the turn, clear the checkpoint, and persist the session.
        self.save_turn(&mut snapshot, all_msgs.as_slice(), 1 + history.len() as u32);
        self.clear_runtime_checkpoint(&mut snapshot);
        if let Err(e) = self
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .save(snapshot.clone())
        {
            log::error!("Failed to save session after processing system message: {e}");
        }

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
        })
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
    async fn process_message(
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
                return Some(result);
            }
        }
        if let Some(result) = self.commands.dispatch(&mut ctx).await {
            return Some(result);
        }
        self.consolidator.maybe_consolidate_by_tokens(&key).await;
        self.set_tool_context(
            msg.channel.as_str(),
            msg.chat_id.as_str(),
            msg.metadata.get("message_id").and_then(Value::as_str),
        );
        if let Some(message_tool) = self.tools.lock().unwrap_or_else(|e| e.into_inner()).get("message") {
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
        let initial_messages = self.context.build_messages(
            history.as_slice(),
            msg.content.as_str(),
            None,
            media,
            Some(msg.channel.as_str()),
            Some(msg.chat_id.as_str()),
            DEFAULT_CURRENT_ROLE,
        );

        let bus_progress: ProgressCallback = {
            let bus = Arc::clone(&self.bus);
            let channel = msg.channel.clone();
            let chat_id = msg.chat_id.clone();
            let base_meta = msg.metadata.clone();
            Arc::new(move |content: String, tool_hint: bool| {
                let bus = Arc::clone(&bus);
                let channel = channel.clone();
                let chat_id = chat_id.clone();
                let mut meta = base_meta.clone();
                Box::pin(async move {
                    meta.insert("_progress".into(), Value::Bool(true));
                    meta.insert("_tool_hint".into(), Value::Bool(tool_hint));
                    if let Err(e) = bus.publish_outbound(OutboundMessage {
                        channel,
                        chat_id,
                        content,
                        reply_to: None,
                        media: vec![],
                        metadata: meta,
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

        if final_content.trim().is_empty() {
            final_content = EMPTY_FINAL_RESPONSE_MESSAGE.to_string();
        }
        self.save_turn(&mut session, &all_msgs, 1 + history.len() as u32);
        self.clear_runtime_checkpoint(&mut session);
        if let Err(res) = self
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .save(session.clone())
        {
            log::error!("Failed to save session after processing message: {res}");
        }
        let consolidator = Arc::clone(&self.consolidator);
        let consolidate_key = key.clone();
        self.schedule_background(async move {
            consolidator
                .maybe_consolidate_by_tokens(&consolidate_key)
                .await;
        })
        .await;
        if let Some(message_tool) = self.tools.lock().unwrap_or_else(|e| e.into_inner()).get("message") {
            if let Some(message_tool) =
                (message_tool.as_ref() as &dyn std::any::Any).downcast_ref::<MessageTool>()
            {
                if *message_tool.sent_in_turn.lock().unwrap_or_else(|e| e.into_inner()) {
                    return None;
                }
            }
        }
        let limit: usize = 120;
        let preview = if final_content.len() > limit { 
            format!("{}...", final_content.get(..limit).unwrap_or(&final_content))
        } else {
            final_content.clone()
        };
        log::info!(
            "Response to {}:{}: {}",
            msg.channel,
            msg.sender_id,
            preview
        );

        let mut meta = msg.metadata.clone();
        if on_stream.is_some() {
            meta.insert("_streamed".into(), Value::Bool(true));
        }
        Some(OutboundMessage {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            content: final_content,
            reply_to: None,
            media: vec![],
            metadata: meta,
        })
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
        on_stream_end: Option<StreamEndCallback>
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
        self.process_message(msg, session_key, on_progress, on_stream, on_stream_end).await
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

    // ── save_turn ─────────────────────────────────────────────────────────────

    fn make_save_turn_loop(max_tool_result_chars: u32) -> Arc<AgentLoop> {
        let bus = Arc::new(MessageBus::new());
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(PlaceholderProvider {
            settings: GenerationSettings::new(),
        });
        Arc::new(AgentLoop::new(
            bus,
            provider,
            std::env::temp_dir(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(max_tool_result_chars),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ))
    }

    fn saved_content(msg: &Value) -> Option<&str> {
        msg.get("content").and_then(Value::as_str)
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
}
