/// Shared lifecycle hook primitives for agent runs.
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;

use crate::providers::base::{LLMResponse, ToolCallRequest};

// ── AgentHookContext ──────────────────────────────────────────────────────────

/// Mutable per-iteration state exposed to runner hooks.
pub struct AgentHookContext {
    pub iteration: usize,
    pub messages: Vec<serde_json::Value>,
    pub response: Option<LLMResponse>,
    pub usage: HashMap<String, u64>,
    pub tool_calls: Vec<ToolCallRequest>,
    pub tool_results: Vec<serde_json::Value>,
    pub tool_events: Vec<HashMap<String, String>>,
    pub final_content: Option<String>,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
}

impl AgentHookContext {
    pub fn new(iteration: usize, messages: Vec<serde_json::Value>) -> Self {
        Self {
            iteration,
            messages,
            response: None,
            usage: HashMap::new(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            tool_events: Vec::new(),
            final_content: None,
            stop_reason: None,
            error: None,
        }
    }
}

// ── AgentHook ─────────────────────────────────────────────────────────────────

/// Minimal lifecycle surface for shared runner customization.
///
/// All async methods have no-op defaults so implementors only override
/// what they need. `finalize_content` is a synchronous pipeline step.
#[async_trait]
pub trait AgentHook: Send + Sync {
    /// Return `true` if this hook wants the runner to stream LLM output.
    fn wants_streaming(&self) -> bool {
        false
    }

    async fn before_iteration(&self, _ctx: &mut AgentHookContext) {}

    async fn on_stream(&self, _ctx: &mut AgentHookContext, _delta: &str) {}

    async fn on_stream_end(&self, _ctx: &mut AgentHookContext, _resuming: bool) {}

    async fn before_execute_tools(&self, _ctx: &mut AgentHookContext) {}

    async fn after_iteration(&self, _ctx: &mut AgentHookContext) {}

    /// Post-process the final content string. Called synchronously in a pipeline:
    /// each hook receives the output of the previous one.
    fn finalize_content(
        &self,
        _ctx: &AgentHookContext,
        content: Option<String>,
    ) -> Option<String> {
        content
    }
}

// ── CompositeHook ─────────────────────────────────────────────────────────────

/// Fan-out hook that delegates to an ordered list of child hooks.
///
/// **Error isolation**: async methods wrap each child call in
/// `AssertUnwindSafe` + `catch_unwind` so a panicking hook cannot crash the
/// agent loop (mirrors the `try/except` in the Python implementation).
/// `finalize_content` is a plain pipeline — panics surface normally.
pub struct CompositeHook {
    hooks: Vec<Arc<dyn AgentHook>>,
}

impl CompositeHook {
    pub fn new(hooks: Vec<Arc<dyn AgentHook>>) -> Self {
        Self { hooks }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }
}

#[async_trait]
impl AgentHook for CompositeHook {
    fn wants_streaming(&self) -> bool {
        self.hooks.iter().any(|h| h.wants_streaming())
    }

    async fn before_iteration(&self, ctx: &mut AgentHookContext) {
        for hook in &self.hooks {
            match AssertUnwindSafe(hook.before_iteration(ctx)).catch_unwind().await {
                Ok(()) => {}
                Err(e) => log::error!(
                    "AgentHook.before_iteration panicked: {:?}",
                    e.downcast_ref::<&str>().copied().unwrap_or("(non-string panic)")
                ),
            }
        }
    }

    async fn on_stream(&self, ctx: &mut AgentHookContext, delta: &str) {
        for hook in &self.hooks {
            match AssertUnwindSafe(hook.on_stream(ctx, delta)).catch_unwind().await {
                Ok(()) => {}
                Err(e) => log::error!(
                    "AgentHook.on_stream panicked: {:?}",
                    e.downcast_ref::<&str>().copied().unwrap_or("(non-string panic)")
                ),
            }
        }
    }

    async fn on_stream_end(&self, ctx: &mut AgentHookContext, resuming: bool) {
        for hook in &self.hooks {
            match AssertUnwindSafe(hook.on_stream_end(ctx, resuming)).catch_unwind().await {
                Ok(()) => {}
                Err(e) => log::error!(
                    "AgentHook.on_stream_end panicked: {:?}",
                    e.downcast_ref::<&str>().copied().unwrap_or("(non-string panic)")
                ),
            }
        }
    }

    async fn before_execute_tools(&self, ctx: &mut AgentHookContext) {
        for hook in &self.hooks {
            match AssertUnwindSafe(hook.before_execute_tools(ctx)).catch_unwind().await {
                Ok(()) => {}
                Err(e) => log::error!(
                    "AgentHook.before_execute_tools panicked: {:?}",
                    e.downcast_ref::<&str>().copied().unwrap_or("(non-string panic)")
                ),
            }
        }
    }

    async fn after_iteration(&self, ctx: &mut AgentHookContext) {
        for hook in &self.hooks {
            match AssertUnwindSafe(hook.after_iteration(ctx)).catch_unwind().await {
                Ok(()) => {}
                Err(e) => log::error!(
                    "AgentHook.after_iteration panicked: {:?}",
                    e.downcast_ref::<&str>().copied().unwrap_or("(non-string panic)")
                ),
            }
        }
    }

    fn finalize_content(
        &self,
        ctx: &AgentHookContext,
        mut content: Option<String>,
    ) -> Option<String> {
        for hook in &self.hooks {
            content = hook.finalize_content(ctx, content);
        }
        content
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // ── test fixtures ─────────────────────────────────────────────────────────

    fn make_ctx() -> AgentHookContext {
        AgentHookContext::new(1, vec![])
    }

    /// Records which lifecycle methods were called.
    #[derive(Default)]
    struct RecordingHook {
        calls: Arc<Mutex<Vec<String>>>,
        stream_content: Arc<Mutex<String>>,
        wants_stream: bool,
    }

    impl RecordingHook {
        fn new(wants_stream: bool) -> Self {
            Self {
                wants_stream,
                ..Default::default()
            }
        }
        fn recorded(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn streamed(&self) -> String {
            self.stream_content.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AgentHook for RecordingHook {
        fn wants_streaming(&self) -> bool {
            self.wants_stream
        }

        async fn before_iteration(&self, _ctx: &mut AgentHookContext) {
            self.calls.lock().unwrap().push("before_iteration".into());
        }

        async fn on_stream(&self, _ctx: &mut AgentHookContext, delta: &str) {
            self.calls.lock().unwrap().push("on_stream".into());
            self.stream_content.lock().unwrap().push_str(delta);
        }

        async fn on_stream_end(&self, _ctx: &mut AgentHookContext, resuming: bool) {
            self.calls.lock().unwrap().push(format!("on_stream_end(resuming={})", resuming));
        }

        async fn before_execute_tools(&self, _ctx: &mut AgentHookContext) {
            self.calls.lock().unwrap().push("before_execute_tools".into());
        }

        async fn after_iteration(&self, _ctx: &mut AgentHookContext) {
            self.calls.lock().unwrap().push("after_iteration".into());
        }

        fn finalize_content(&self, _ctx: &AgentHookContext, content: Option<String>) -> Option<String> {
            self.calls.lock().unwrap().push("finalize_content".into());
            content.map(|c| format!("[wrapped]{}", c))
        }
    }

    /// A hook that panics on every async call.
    struct PanickingHook;

    #[async_trait]
    impl AgentHook for PanickingHook {
        async fn before_iteration(&self, _ctx: &mut AgentHookContext) {
            panic!("deliberate panic");
        }
        async fn on_stream(&self, _ctx: &mut AgentHookContext, _delta: &str) {
            panic!("deliberate panic");
        }
        async fn on_stream_end(&self, _ctx: &mut AgentHookContext, _resuming: bool) {
            panic!("deliberate panic");
        }
        async fn before_execute_tools(&self, _ctx: &mut AgentHookContext) {
            panic!("deliberate panic");
        }
        async fn after_iteration(&self, _ctx: &mut AgentHookContext) {
            panic!("deliberate panic");
        }
    }

    // ── AgentHookContext ──────────────────────────────────────────────────────

    #[test]
    fn test_context_defaults() {
        let ctx = make_ctx();
        assert_eq!(ctx.iteration, 1);
        assert!(ctx.messages.is_empty());
        assert!(ctx.response.is_none());
        assert!(ctx.usage.is_empty());
        assert!(ctx.tool_calls.is_empty());
        assert!(ctx.tool_results.is_empty());
        assert!(ctx.tool_events.is_empty());
        assert!(ctx.final_content.is_none());
        assert!(ctx.stop_reason.is_none());
        assert!(ctx.error.is_none());
    }

    // ── default AgentHook (no-op) ─────────────────────────────────────────────

    struct NoopHook;

    #[async_trait]
    impl AgentHook for NoopHook {}

    #[tokio::test]
    async fn test_default_hook_is_noop() {
        let hook = NoopHook;
        let mut ctx = make_ctx();
        hook.before_iteration(&mut ctx).await;
        hook.on_stream(&mut ctx, "x").await;
        hook.on_stream_end(&mut ctx, false).await;
        hook.before_execute_tools(&mut ctx).await;
        hook.after_iteration(&mut ctx).await;
        assert!(!hook.wants_streaming());
        assert_eq!(hook.finalize_content(&ctx, Some("hi".into())), Some("hi".into()));
        assert_eq!(hook.finalize_content(&ctx, None), None);
    }

    // ── RecordingHook ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_recording_hook_lifecycle() {
        let hook = RecordingHook::new(false);
        let mut ctx = make_ctx();

        hook.before_iteration(&mut ctx).await;
        hook.on_stream(&mut ctx, "hello").await;
        hook.on_stream_end(&mut ctx, true).await;
        hook.before_execute_tools(&mut ctx).await;
        hook.after_iteration(&mut ctx).await;

        let calls = hook.recorded();
        assert_eq!(calls, vec![
            "before_iteration",
            "on_stream",
            "on_stream_end(resuming=true)",
            "before_execute_tools",
            "after_iteration",
        ]);
        assert_eq!(hook.streamed(), "hello");
    }

    #[test]
    fn test_recording_hook_finalize_wraps_content() {
        let hook = RecordingHook::new(false);
        let ctx = make_ctx();
        assert_eq!(
            hook.finalize_content(&ctx, Some("text".into())),
            Some("[wrapped]text".into())
        );
        assert_eq!(hook.finalize_content(&ctx, None), None);
    }

    // ── CompositeHook ─────────────────────────────────────────────────────────

    #[test]
    fn test_composite_wants_streaming_any() {
        let composite = CompositeHook::new(vec![
            Arc::new(RecordingHook::new(false)),
            Arc::new(RecordingHook::new(true)),
        ]);
        assert!(composite.wants_streaming());
    }

    #[test]
    fn test_composite_wants_streaming_none() {
        let composite = CompositeHook::new(vec![
            Arc::new(RecordingHook::new(false)),
            Arc::new(RecordingHook::new(false)),
        ]);
        assert!(!composite.wants_streaming());
    }

    #[tokio::test]
    async fn test_composite_fans_out_to_all_hooks() {
        let a = Arc::new(Mutex::new(Vec::<String>::new()));
        let b = Arc::new(Mutex::new(Vec::<String>::new()));

        let hook_a = RecordingHook { calls: Arc::clone(&a), ..Default::default() };
        let hook_b = RecordingHook { calls: Arc::clone(&b), ..Default::default() };

        let composite = CompositeHook::new(vec![Arc::new(hook_a), Arc::new(hook_b)]);
        let mut ctx = make_ctx();

        composite.before_iteration(&mut ctx).await;
        composite.after_iteration(&mut ctx).await;

        assert!(a.lock().unwrap().contains(&"before_iteration".to_string()));
        assert!(b.lock().unwrap().contains(&"before_iteration".to_string()));
        assert!(a.lock().unwrap().contains(&"after_iteration".to_string()));
        assert!(b.lock().unwrap().contains(&"after_iteration".to_string()));
    }

    #[tokio::test]
    async fn test_composite_on_stream_accumulates_delta() {
        let hook = RecordingHook::new(false);
        let stream_ref = Arc::clone(&hook.stream_content);
        let composite = CompositeHook::new(vec![Arc::new(hook)]);
        let mut ctx = make_ctx();

        composite.on_stream(&mut ctx, "foo").await;
        composite.on_stream(&mut ctx, "bar").await;

        assert_eq!(*stream_ref.lock().unwrap(), "foobar");
    }

    #[tokio::test]
    async fn test_composite_on_stream_end_resuming_flag() {
        let hook = RecordingHook::new(false);
        let calls_ref = Arc::clone(&hook.calls);
        let composite = CompositeHook::new(vec![Arc::new(hook)]);
        let mut ctx = make_ctx();

        composite.on_stream_end(&mut ctx, false).await;
        assert!(calls_ref.lock().unwrap().contains(&"on_stream_end(resuming=false)".to_string()));
    }

    #[test]
    fn test_composite_finalize_content_is_pipeline() {
        // Two wrapping hooks → content gets double-wrapped
        let composite = CompositeHook::new(vec![
            Arc::new(RecordingHook::new(false)),
            Arc::new(RecordingHook::new(false)),
        ]);
        let ctx = make_ctx();
        let result = composite.finalize_content(&ctx, Some("text".into()));
        assert_eq!(result, Some("[wrapped][wrapped]text".into()));
    }

    #[test]
    fn test_composite_finalize_content_none_propagates() {
        let composite = CompositeHook::new(vec![Arc::new(NoopHook)]);
        let ctx = make_ctx();
        assert_eq!(composite.finalize_content(&ctx, None), None);
    }

    #[test]
    fn test_composite_empty() {
        let composite = CompositeHook::new(vec![]);
        assert!(composite.is_empty());
        assert_eq!(composite.len(), 0);
        assert!(!composite.wants_streaming());
    }

    // ── error isolation ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_composite_panicking_hook_isolated_before_iteration() {
        let good = RecordingHook::new(false);
        let calls_ref = Arc::clone(&good.calls);

        // PanickingHook first, then the good hook — good hook must still run
        let composite = CompositeHook::new(vec![
            Arc::new(PanickingHook),
            Arc::new(good),
        ]);
        let mut ctx = make_ctx();
        composite.before_iteration(&mut ctx).await;

        assert!(calls_ref.lock().unwrap().contains(&"before_iteration".to_string()),
            "good hook should still run after panicking hook");
    }

    #[tokio::test]
    async fn test_composite_panicking_hook_isolated_on_stream() {
        let good = RecordingHook::new(false);
        let calls_ref = Arc::clone(&good.calls);
        let composite = CompositeHook::new(vec![Arc::new(PanickingHook), Arc::new(good)]);
        let mut ctx = make_ctx();
        composite.on_stream(&mut ctx, "x").await;
        assert!(calls_ref.lock().unwrap().contains(&"on_stream".to_string()));
    }

    #[tokio::test]
    async fn test_composite_panicking_hook_isolated_after_iteration() {
        let good = RecordingHook::new(false);
        let calls_ref = Arc::clone(&good.calls);
        let composite = CompositeHook::new(vec![Arc::new(PanickingHook), Arc::new(good)]);
        let mut ctx = make_ctx();
        composite.after_iteration(&mut ctx).await;
        assert!(calls_ref.lock().unwrap().contains(&"after_iteration".to_string()));
    }
}
