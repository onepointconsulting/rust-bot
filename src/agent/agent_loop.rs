use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::agent::context::ContextBuilder;
use crate::agent::hook::{AgentHook, AgentHookContext, CompositeHook};
use crate::agent::memory::{Consolidator, Dream};
use crate::agent::runner::AgentRunner;
use crate::agent::subagent::SubagentManager;
use crate::agent::tools::registry::ToolRegistry;
use crate::bus::queue::MessageBus;
use crate::config::schema::{
    AgentDefaults, ChannelsConfig, ExecToolConfig, McpServerConfig, ProviderRetryMode,
    WebToolsConfig,
};
use crate::cron::CronService;
use crate::providers::base::LLMProviderDyn;
use crate::session::manager::SessionManager;
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
    pub fn new(primary: Arc<dyn AgentHook>, extras: Vec<Box<dyn AgentHook>>) -> Self {
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
    session_manager: Arc<Mutex<SessionManager>>,
    mcp_servers: HashMap<String, Arc<McpServerConfig>>,
    mcp_connected: bool,
    mcp_connecting: bool,
    channels_config: Option<ChannelsConfig>,
    timezone: Option<String>,
    start_time: SystemTime,
    last_usage: HashMap<String, usize>,
    extra_hooks: Vec<Arc<dyn AgentHook>>,
    context: Arc<ContextBuilder>,
    tools: Arc<ToolRegistry>,
    runner: Arc<AgentRunner>,
    subagents: Arc<SubagentManager>,
    active_tasks: Arc<AsyncMutex<HashMap<String, Vec<JoinHandle<()>>>>>,
    background_tasks: Arc<AsyncMutex<Vec<JoinHandle<()>>>>,
    session_locks: Arc<AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    max: usize,
    running: bool,
    concurrency_gate: Option<Arc<Semaphore>>,
    consolidator: Arc<Consolidator>,
    dream: Arc<Dream>,
}

impl AgentLoop {
    const RUNTIME_CHECKPOINT_KEY: &str = "runtime_checkpoint";

    pub fn new(
        bus: Arc<MessageBus>,
        channels_config: Option<ChannelsConfig>,
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
        provider: Arc<dyn LLMProviderDyn>,
        mcp_servers: Option<HashMap<String, Arc<McpServerConfig>>>,
    ) -> Self {
        let defaults = AgentDefaults::default();
        let model = model.unwrap_or(provider.clone().get_default_model());
        let web_config = web_config.unwrap_or(WebToolsConfig::default());
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
        let tools = Arc::new(ToolRegistry::new());
        let context = Arc::new(ContextBuilder::new(workspace.clone(), None, tools.clone()));
        let session_manager = session_manager
            .unwrap_or_else(|| Arc::new(Mutex::new(SessionManager::new(workspace.clone()))));
        let context_window_tokens =
            context_window_tokens.unwrap_or(defaults.clone().context_window_tokens);
        let max_tool_result_chars =
            max_tool_result_chars.unwrap_or(defaults.clone().max_tool_result_chars);
        Self {
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
            exec_config: exec_config.clone().unwrap_or(ExecToolConfig::default()),
            cron_service: cron_service,
            restrict_to_workspace: restrict_to_workspace,
            timezone: None,
            start_time: SystemTime::now(),
            last_usage: HashMap::new(),
            extra_hooks: Vec::new(),
            context: context.clone(),
            session_manager: session_manager.clone(),
            tools: tools.clone(),
            runner: Arc::new(AgentRunner::new(provider.clone())),
            subagents: Arc::new(SubagentManager::new(
                provider.clone(),
                workspace.clone(),
                bus,
                0,
                Some(model.clone()),
                Some(web_config),
                Some(exec_config.unwrap_or(ExecToolConfig::default())),
                Some(restrict_to_workspace),
            )),
            running: false,
            mcp_servers: mcp_servers.unwrap_or(HashMap::new()),
            mcp_connected: false,
            mcp_connecting: false,
            active_tasks: Arc::new(AsyncMutex::new(HashMap::new())),
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
        }
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
            vec![Box::new(OrderRecordingHook {
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
