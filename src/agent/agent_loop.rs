use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::agent::hook::{AgentHook, AgentHookContext, CompositeHook};
use crate::agent::tools::registry::ToolRegistry;
use crate::bus::queue::MessageBus;
use crate::config::schema::{ExecToolConfig, WebToolsConfig};
use crate::providers::base::LLMProviderDyn;
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
    bus: MessageBus,
    provider: Arc<dyn LLMProviderDyn>,
    workspace: PathBuf,
    model: Option<String>,
    max_iterations: Option<usize>,
    context_window_tokens: Option<usize>,
    context_block_limit: Option<usize>,
    max_tool_result_chars: Option<usize>,
    provider_retry_mode: String,
    web_config: Option<WebToolsConfig>,
    exec_config: Option<ExecToolConfig>,
    // cron_service: Option<CronService>,
    tools: Arc<ToolRegistry>,
}

impl AgentLoop {
    const RUNTIME_CHECKPOINT_KEY: &str = "runtime_checkpoint";

    pub fn new(tools: Arc<ToolRegistry>, provider: Arc<dyn LLMProviderDyn>) -> Self {
        Self {
            bus: MessageBus::new(),
            provider: provider,
            workspace: PathBuf::from("."),
            model: None,
            max_iterations: None,
            context_window_tokens: None,
            context_block_limit: None,
            max_tool_result_chars: None,
            provider_retry_mode: "standard".to_string(),
            web_config: None,
            exec_config: None,
            tools,
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

    fn test_agent_loop() -> Arc<AgentLoop> {
        Arc::new(AgentLoop::new(Arc::new(ToolRegistry::new()),
        Arc::new(PlaceholderProvider {
            settings: GenerationSettings::new(),
        })))
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

    fn hook_with_stream(on_stream: StreamCallback) -> LoopHook {
        LoopHook::new(test_agent_loop(), None, Some(on_stream), None)
    }

    fn hook_without_stream() -> LoopHook {
        LoopHook::new(test_agent_loop(), None, None, None)
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

    fn hook_with_stream_end(on_stream_end: StreamEndCallback) -> LoopHook {
        LoopHook::new(test_agent_loop(), None, None, Some(on_stream_end))
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

    fn hook_with_progress(on_progress: ProgressCallback) -> LoopHook {
        LoopHook::new(test_agent_loop(), Some(on_progress), None, None)
    }

    fn stream_buf_snapshot(hook: &LoopHook) -> String {
        hook.stream_buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    #[test]
    fn test_wants_streaming_reflects_callback_presence() {
        let (callback, _) = recording_stream_callback();
        assert!(!hook_without_stream().wants_streaming());
        assert!(hook_with_stream(callback).wants_streaming());
    }

    #[tokio::test]
    async fn test_on_stream_accumulates_raw_delta_without_callback() {
        let hook = hook_without_stream();
        let mut ctx = make_ctx();

        hook.on_stream(&mut ctx, "hel").await;
        hook.on_stream(&mut ctx, "lo").await;

        assert_eq!(stream_buf_snapshot(&hook), "hello");
    }

    #[tokio::test]
    async fn test_on_stream_emits_incremental_plain_text() {
        let (callback, received) = recording_stream_callback();
        let hook = hook_with_stream(callback);
        let mut ctx = make_ctx();

        hook.on_stream(&mut ctx, "Hello").await;

        assert_eq!(*received.lock().unwrap(), vec!["Hello".to_string()]);
        assert_eq!(stream_buf_snapshot(&hook), "Hello");
    }

    #[tokio::test]
    async fn test_on_stream_emits_only_clean_suffix_on_later_deltas() {
        let (callback, received) = recording_stream_callback();
        let hook = hook_with_stream(callback);
        let mut ctx = make_ctx();

        hook.on_stream(&mut ctx, "Hel").await;
        hook.on_stream(&mut ctx, "lo").await;

        assert_eq!(
            *received.lock().unwrap(),
            vec!["Hel".to_string(), "lo".to_string()]
        );
    }

    #[tokio::test]
    async fn test_on_stream_suppresses_incomplete_think_blocks() {
        let (callback, received) = recording_stream_callback();
        let hook = hook_with_stream(callback);
        let mut ctx = make_ctx();

        hook.on_stream(&mut ctx, "<think>secret").await;

        assert!(received.lock().unwrap().is_empty());
        assert_eq!(stream_buf_snapshot(&hook), "<think>secret");
    }

    #[tokio::test]
    async fn test_on_stream_emits_visible_text_after_think_block_closes() {
        let (callback, received) = recording_stream_callback();
        let hook = hook_with_stream(callback);
        let mut ctx = make_ctx();

        hook.on_stream(&mut ctx, "<think>secret").await;
        hook.on_stream(&mut ctx, "</think>Hi").await;

        assert_eq!(*received.lock().unwrap(), vec!["Hi".to_string()]);
    }

    #[tokio::test]
    async fn test_on_stream_short_new_clean_does_not_panic() {
        let (callback, received) = recording_stream_callback();
        let hook = hook_with_stream(callback);
        let mut ctx = make_ctx();

        hook.on_stream(&mut ctx, "Hello").await;
        hook.on_stream(&mut ctx, " <think>x").await;

        assert_eq!(*received.lock().unwrap(), vec!["Hello".to_string()]);
    }

    #[tokio::test]
    async fn test_on_stream_end_clears_buffer_without_callback() {
        let hook = hook_without_stream();
        let mut ctx = make_ctx();

        hook.on_stream(&mut ctx, "hello").await;
        assert_eq!(stream_buf_snapshot(&hook), "hello");

        hook.on_stream_end(&mut ctx, false).await;
        assert_eq!(stream_buf_snapshot(&hook), "");
    }

    #[tokio::test]
    async fn test_on_stream_end_invokes_callback_with_resuming_flag() {
        let (callback, received) = recording_stream_end_callback();
        let hook = hook_with_stream_end(callback);
        let mut ctx = make_ctx();

        hook.on_stream_end(&mut ctx, true).await;
        hook.on_stream_end(&mut ctx, false).await;

        assert_eq!(*received.lock().unwrap(), vec![true, false]);
    }

    #[tokio::test]
    async fn test_on_stream_end_clears_buffer_after_callback() {
        let (callback, received) = recording_stream_end_callback();
        let hook = hook_with_stream_end(callback);
        let mut ctx = make_ctx();

        hook.on_stream(&mut ctx, "partial").await;
        hook.on_stream_end(&mut ctx, false).await;

        assert_eq!(*received.lock().unwrap(), vec![false]);
        assert_eq!(stream_buf_snapshot(&hook), "");
    }

    #[tokio::test]
    async fn test_before_execute_tools_emits_thought_without_tool_hint_flag() {
        let (callback, received) = recording_progress_callback();
        let hook = hook_with_progress(callback);
        let mut ctx = make_ctx();
        ctx.response = Some(LLMResponse {
            content: Some("Let me check that file.".into()),
            tool_calls: vec![],
            finish_reason: "tool_calls".into(),
            usage: HashMap::new(),
            reasoning_content: None,
            thinking_blocks: None,
        });
        ctx.tool_calls.push(ToolCallRequest {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: HashMap::from([("path".into(), serde_json::json!("src/main.rs"))]),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        });

        hook.before_execute_tools(&mut ctx).await;

        let events = received.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "Let me check that file.");
        assert!(!events[0].1);
        assert_eq!(events[1].0, "read src/main.rs");
        assert!(events[1].1);
    }

    #[tokio::test]
    async fn test_before_execute_tools_skips_thought_when_streaming() {
        let (progress_cb, progress_received) = recording_progress_callback();
        let (stream_cb, _) = recording_stream_callback();
        let hook = LoopHook::new(test_agent_loop(), Some(progress_cb), Some(stream_cb), None);
        let mut ctx = make_ctx();
        ctx.response = Some(LLMResponse {
            content: Some("hidden thought".into()),
            tool_calls: vec![],
            finish_reason: "tool_calls".into(),
            usage: HashMap::new(),
            reasoning_content: None,
            thinking_blocks: None,
        });
        ctx.tool_calls.push(ToolCallRequest {
            id: "call_1".into(),
            name: "glob".into(),
            arguments: HashMap::from([("pattern".into(), serde_json::json!("*.rs"))]),
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        });

        hook.before_execute_tools(&mut ctx).await;

        let events = progress_received.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "glob \"*.rs\"");
        assert!(events[0].1);
    }

    #[tokio::test]
    async fn test_after_iteration_does_not_mutate_context() {
        let hook = hook_without_stream();
        let mut ctx = make_ctx();
        ctx.usage.insert("prompt_tokens".into(), 100);
        ctx.usage.insert("completion_tokens".into(), 25);
        ctx.usage.insert("cached_tokens".into(), 10);

        hook.after_iteration(&mut ctx).await;

        assert_eq!(ctx.usage.get("prompt_tokens"), Some(&100));
    }

    #[test]
    fn test_finalize_content_strips_think_blocks() {
        let hook = hook_without_stream();
        let ctx = make_ctx();

        assert_eq!(
            hook.finalize_content(&ctx, Some("<think>secret</think>Hello".into())),
            Some("Hello".into())
        );
        assert_eq!(hook.finalize_content(&ctx, None), None);
    }

    #[test]
    fn test_set_tool_context_noops_when_tools_missing() {
        let agent_loop = AgentLoop::new(Arc::new(ToolRegistry::new()), Arc::new(PlaceholderProvider {
            settings: GenerationSettings::new(),
        }));
        agent_loop.set_tool_context("telegram", "chat-42", Some("msg-1"));
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
