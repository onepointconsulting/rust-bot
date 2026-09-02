use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::{
    providers::base::LLMProviderDyn,
    utils::{evaluator::evaluate_response, helpers::current_time_str},
};

/// Async handler invoked when a heartbeat is executed.
///
/// Takes task text by reference (same as gateway `on_heartbeat_execute`); the
/// returned future must be `'static`, so callers should copy `tasks` if needed
/// inside the closure before awaiting.
pub type HeartbeatExecuteCallback =
    Arc<dyn Fn(&str) -> Pin<Box<dyn Future<Output = String> + Send>> + Send + Sync>;

/// Async handler invoked when a heartbeat notifies the user.
pub type HeartbeatNotifyCallback =
    Arc<dyn Fn(&str) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

const HEARTBEAT_TOOL: LazyLock<Vec<serde_json::Value>> = LazyLock::new(|| {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "heartbeat",
            "description": "Report heartbeat decision after reviewing tasks.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["skip", "run"],
                        "description": "skip = nothing to do, run = has active tasks",
                    },
                    "tasks": {
                        "type": "string",
                        "description": "Natural-language summary of active tasks (required for run)",
                    },
                },
                "required": ["action"],
            },
        },
    })]
});

/**
 * Summary of the flow

every interval_s:
  read HEARTBEAT.md ──(empty)──▶ skip
        │
   LLM decide skip/run ──(skip)──▶ skip
        │ run
   agent.process_direct(tasks)  [full agent loop, "heartbeat" session, trimmed to 8 msgs]
        │
   evaluate_response: should_notify? ──(no)──▶ silence
        │ yes
   on_notify ──▶ OutboundMessage to picked channel  (skipped if channel == cli)
 */
pub struct HeartbeatService {
    workspace: PathBuf,
    provider: Arc<dyn LLMProviderDyn>,
    model: String,
    on_execute: Option<HeartbeatExecuteCallback>,
    on_notify: Option<HeartbeatNotifyCallback>,
    interval_s: u64,
    enabled: bool,
    timezone: Option<String>,
    running: AtomicBool,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl HeartbeatService {
    pub fn new(
        workspace: PathBuf,
        provider: Arc<dyn LLMProviderDyn>,
        model: String,
        on_execute: Option<HeartbeatExecuteCallback>,
        on_notify: Option<HeartbeatNotifyCallback>,
        interval_s: u64,
        enabled: bool,
        timezone: Option<String>,
    ) -> Self {
        Self {
            workspace,
            provider,
            model,
            on_execute,
            on_notify,
            interval_s,
            enabled,
            timezone,
            running: AtomicBool::new(false),
            task: Mutex::new(None),
        }
    }

    pub fn heartbeat_file(&self) -> PathBuf {
        self.workspace.join("HEARTBEAT.md")
    }

    fn read_heartbeat_file(&self) -> Option<String> {
        let path = self.heartbeat_file();
        match std::fs::read_to_string(&path) {
            Ok(content) => Some(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                log::warn!("Failed to read heartbeat file {}: {e}", path.display());
                None
            }
        }
    }

    /// "Phase 1: ask LLM to decide skip/run via virtual tool call.
    ///
    /// Returns (action, tasks) where action is 'skip' or 'run'.
    async fn decide(&self, content: &str) -> (String, String) {
        let response = self.provider.chat_with_retry(
            vec![
                serde_json::json!({
                    "role": "system",
                    "content": "You are a heartbeat agent. Call the heartbeat tool to report your decision.",
                }),
                serde_json::json!({
                    "role": "user",
                    "content": format!(
                        "Current Time: {}\n\n Review the following HEARTBEAT.md and decide whether there are active tasks.\n\n{}",
                        current_time_str(self.timezone.as_deref()),
                    content)
                })
            ],
            Some(HEARTBEAT_TOOL.clone()),
            Some(self.model.clone()),
            None,
            None,
            None,
            None,
        ).await;

        if response.finish_reason == "error" {
            log::warn!("Error in heartbeat response: {}", response.finish_reason);
            return ("skip".to_string(), "".to_string());
        }

        if response.tool_calls.is_empty() {
            log::warn!("No tool calls in heartbeat response");
            return ("skip".to_string(), "".to_string());
        }

        let args = response.tool_calls[0].arguments.clone();
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("skip");
        let tasks = args.get("tasks").and_then(|v| v.as_str()).unwrap_or("");
        (action.to_string(), tasks.to_string())
    }

    /// Start the heartbeat service.
    ///
    /// Python: `self._task = asyncio.create_task(self._run_loop())`
    pub async fn start(self: &Arc<Self>) {
        if !self.enabled {
            log::info!("Heartbeat service is disabled");
            return;
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            log::warn!("Heartbeat already running");
            return;
        }
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move {
            this.run_loop().await;
        });
        *self.task.lock().await = Some(handle);
        log::info!("Heartbeat started (every {}s)", self.interval_s);
    }

    /// Stop the heartbeat service.
    pub async fn stop(self: &Arc<Self>) {
        if !self.running.swap(false, Ordering::SeqCst) {
            log::warn!("Heartbeat not running");
        }
        if let Some(task) = self.task.lock().await.take() {
            // Tokio equivalent of asyncio.Task.cancel()
            task.abort();
        }
    }

    /// Main heartbeat loop.
    ///
    /// dead simple — sleep(interval_s) then _tick(), forever, swallowing exceptions so one bad tick doesn't kill the loop.
    /// Note the sleep happens first, so the first check is one interval after startup, not immediately.
    ///
    /// `CancelledError` has no direct analogue here: `stop()` calls `JoinHandle::abort()`,
    /// which drops this future at the next `.await` (usually the sleep).
    async fn run_loop(self: Arc<Self>) {
        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(self.interval_s)).await;
            if !self.running.load(Ordering::SeqCst) {
                break;
            }
            self.tick().await;
        }
    }

    /// Execute a single heartbeat tick.
    ///
    /// Python `_tick`: failures inside decide/execute/evaluate/notify are logged
    /// and swallowed so the loop keeps running.
    async fn tick(&self) {
        let content = self.read_heartbeat_file().unwrap_or_default();
        if content.is_empty() {
            log::debug!("Heartbeat: HEARTBEAT.md missing or empty");
            return;
        }

        log::info!("Heartbeat: checking for tasks...");

        if let Err(e) = self.tick_inner(&content).await {
            // Python: `except Exception: logger.exception("Heartbeat execution failed")`
            log::error!("Heartbeat execution failed: {e}");
        }
    }

    async fn tick_inner(&self, content: &str) -> Result<(), String> {
        let (action, tasks) = self.decide(content).await;

        if action != "run" {
            log::info!("Heartbeat: OK (nothing to report)");
            return Ok(());
        }

        log::info!("Heartbeat: tasks found, executing...");

        let Some(on_execute) = self.on_execute.as_ref() else {
            return Ok(());
        };

        let response = on_execute(tasks.as_str()).await;
        // Python: `if response:` — empty response is a quiet no-op, not an error.
        if response.is_empty() {
            return Ok(());
        }

        let should_notify =
            evaluate_response(&response, &tasks, Arc::clone(&self.provider), &self.model).await;

        if should_notify && let Some(on_notify) = self.on_notify.as_ref() {
            log::info!("Heartbeat: completed, delivering response");
            on_notify(&response).await?;
        } else {
            log::info!("Heartbeat: silenced by post-run evaluation");
        }

        Ok(())
    }

    /// Manually trigger a heartbeat (decide + execute).
    ///
    /// Unlike [`tick`], this does **not** run post-run evaluation or `on_notify`;
    /// it returns the agent response for the caller to handle (matches Python).
    pub async fn trigger_now(&self) -> Option<String> {
        let content = self.read_heartbeat_file().unwrap_or_default();
        if content.is_empty() {
            return None;
        }

        let (action, tasks) = self.decide(&content).await;
        if action != "run" {
            return None;
        }

        let on_execute = self.on_execute.as_ref()?;
        Some(on_execute(tasks.as_str()).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex as StdMutex;

    use crate::providers::base::{
        BoxedProgressCallback, BoxedStreamCallback, GenerationSettings, LLMResponse, LLMUsage,
        ToolCallRequest,
    };
    use tempfile::TempDir;

    struct ScriptedProvider {
        settings: GenerationSettings,
        responses: StdMutex<VecDeque<LLMResponse>>,
    }

    impl ScriptedProvider {
        fn arc(responses: Vec<LLMResponse>) -> Arc<dyn LLMProviderDyn> {
            Arc::new(Self {
                settings: GenerationSettings::new(),
                responses: StdMutex::new(responses.into()),
            })
        }
    }

    #[async_trait::async_trait]
    impl LLMProviderDyn for ScriptedProvider {
        fn api_key(&self) -> Option<String> {
            None
        }
        fn api_base(&self) -> Option<String> {
            None
        }
        fn extra_headers(&self) -> Option<HashMap<String, String>> {
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
            "test-model".to_string()
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
            unimplemented!("tests use chat_with_retry")
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
            unimplemented!("tests use chat_with_retry")
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
            self.responses
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or_else(|| LLMResponse {
                    content: Some("scripted provider exhausted".into()),
                    finish_reason: "error".into(),
                    tool_calls: vec![],
                    usage: LLMUsage::new(),
                    reasoning_content: None,
                    thinking_blocks: None,
                })
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
            _: Option<BoxedStreamCallback>,
            _: Option<BoxedProgressCallback>,
        ) -> LLMResponse {
            unimplemented!("tests use chat_with_retry")
        }
    }

    fn tool_call(name: &str, args: HashMap<String, serde_json::Value>) -> LLMResponse {
        LLMResponse {
            content: None,
            finish_reason: "tool_calls".into(),
            tool_calls: vec![ToolCallRequest {
                id: "call-1".into(),
                name: name.into(),
                arguments: args,
                extra_content: None,
                provider_specific_fields: None,
                function_provider_specific_fields: None,
            }],
            usage: LLMUsage::new(),
            reasoning_content: None,
            thinking_blocks: None,
        }
    }

    fn decide_run(tasks: &str) -> LLMResponse {
        tool_call(
            "heartbeat",
            HashMap::from([
                ("action".into(), serde_json::json!("run")),
                ("tasks".into(), serde_json::json!(tasks)),
            ]),
        )
    }

    fn decide_skip() -> LLMResponse {
        tool_call(
            "heartbeat",
            HashMap::from([("action".into(), serde_json::json!("skip"))]),
        )
    }

    fn evaluate(should_notify: bool) -> LLMResponse {
        tool_call(
            "evaluate_notification",
            HashMap::from([
                ("should_notify".into(), serde_json::json!(should_notify)),
                ("reason".into(), serde_json::json!("test")),
            ]),
        )
    }

    struct CallbackSpies {
        executed: Arc<StdMutex<Vec<String>>>,
        notified: Arc<StdMutex<Vec<String>>>,
    }

    impl CallbackSpies {
        fn new() -> Self {
            Self {
                executed: Arc::new(StdMutex::new(Vec::new())),
                notified: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn on_execute(
            &self,
            response: impl Into<String> + Send + Sync + 'static,
        ) -> HeartbeatExecuteCallback {
            let executed = Arc::clone(&self.executed);
            let response = response.into();
            Arc::new(move |tasks: &str| {
                let tasks = tasks.to_string();
                let executed = Arc::clone(&executed);
                let response = response.clone();
                Box::pin(async move {
                    executed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(tasks);
                    response
                })
            })
        }

        fn on_notify(&self) -> HeartbeatNotifyCallback {
            let notified = Arc::clone(&self.notified);
            Arc::new(move |content: &str| {
                let content = content.to_string();
                let notified = Arc::clone(&notified);
                Box::pin(async move {
                    notified
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(content);
                    Ok(())
                })
            })
        }

        fn executed(&self) -> Vec<String> {
            self.executed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        fn notified(&self) -> Vec<String> {
            self.notified
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    fn write_heartbeat(dir: &TempDir, content: &str) {
        std::fs::write(dir.path().join("HEARTBEAT.md"), content).unwrap();
    }

    fn make_service(
        workspace: PathBuf,
        provider: Arc<dyn LLMProviderDyn>,
        on_execute: Option<HeartbeatExecuteCallback>,
        on_notify: Option<HeartbeatNotifyCallback>,
    ) -> HeartbeatService {
        HeartbeatService::new(
            workspace,
            provider,
            "test-model".into(),
            on_execute,
            on_notify,
            60,
            true,
            None,
        )
    }

    #[tokio::test]
    async fn tick_missing_heartbeat_file_is_noop() {
        let tmp = TempDir::new().unwrap();
        let spies = CallbackSpies::new();
        let service = make_service(
            tmp.path().to_path_buf(),
            ScriptedProvider::arc(vec![decide_run("should not run")]),
            Some(spies.on_execute("unused")),
            Some(spies.on_notify()),
        );

        service.tick().await;

        assert!(spies.executed().is_empty());
        assert!(spies.notified().is_empty());
    }

    #[tokio::test]
    async fn tick_empty_heartbeat_file_is_noop() {
        let tmp = TempDir::new().unwrap();
        write_heartbeat(&tmp, "");
        let spies = CallbackSpies::new();
        let service = make_service(
            tmp.path().to_path_buf(),
            ScriptedProvider::arc(vec![decide_run("should not run")]),
            Some(spies.on_execute("unused")),
            Some(spies.on_notify()),
        );

        service.tick().await;

        assert!(spies.executed().is_empty());
        assert!(spies.notified().is_empty());
    }

    #[tokio::test]
    async fn tick_skip_does_not_execute() {
        let tmp = TempDir::new().unwrap();
        write_heartbeat(&tmp, "- check email");
        let spies = CallbackSpies::new();
        let service = make_service(
            tmp.path().to_path_buf(),
            ScriptedProvider::arc(vec![decide_skip()]),
            Some(spies.on_execute("unused")),
            Some(spies.on_notify()),
        );

        service.tick().await;

        assert!(spies.executed().is_empty());
        assert!(spies.notified().is_empty());
    }

    #[tokio::test]
    async fn tick_decide_error_does_not_execute() {
        let tmp = TempDir::new().unwrap();
        write_heartbeat(&tmp, "- check email");
        let spies = CallbackSpies::new();
        let service = make_service(
            tmp.path().to_path_buf(),
            ScriptedProvider::arc(vec![LLMResponse {
                content: Some("boom".into()),
                finish_reason: "error".into(),
                tool_calls: vec![],
                usage: LLMUsage::new(),
                reasoning_content: None,
                thinking_blocks: None,
            }]),
            Some(spies.on_execute("unused")),
            Some(spies.on_notify()),
        );

        service.tick().await;

        assert!(spies.executed().is_empty());
        assert!(spies.notified().is_empty());
    }

    #[tokio::test]
    async fn tick_run_with_empty_execute_response_skips_notify() {
        let tmp = TempDir::new().unwrap();
        write_heartbeat(&tmp, "- check email");
        let spies = CallbackSpies::new();
        let service = make_service(
            tmp.path().to_path_buf(),
            ScriptedProvider::arc(vec![decide_run("check email")]),
            Some(spies.on_execute("")),
            Some(spies.on_notify()),
        );

        service.tick().await;

        assert_eq!(spies.executed(), vec!["check email".to_string()]);
        assert!(spies.notified().is_empty());
    }

    #[tokio::test]
    async fn tick_run_notifies_when_evaluator_approves() {
        let tmp = TempDir::new().unwrap();
        write_heartbeat(&tmp, "- check email");
        let spies = CallbackSpies::new();
        let service = make_service(
            tmp.path().to_path_buf(),
            ScriptedProvider::arc(vec![decide_run("check email"), evaluate(true)]),
            Some(spies.on_execute("You have 2 unread emails.")),
            Some(spies.on_notify()),
        );

        service.tick().await;

        assert_eq!(spies.executed(), vec!["check email".to_string()]);
        assert_eq!(
            spies.notified(),
            vec!["You have 2 unread emails.".to_string()]
        );
    }

    #[tokio::test]
    async fn tick_run_silences_when_evaluator_rejects() {
        let tmp = TempDir::new().unwrap();
        write_heartbeat(&tmp, "- check email");
        let spies = CallbackSpies::new();
        let service = make_service(
            tmp.path().to_path_buf(),
            ScriptedProvider::arc(vec![decide_run("check email"), evaluate(false)]),
            Some(spies.on_execute("Nothing important.")),
            Some(spies.on_notify()),
        );

        service.tick().await;

        assert_eq!(spies.executed(), vec!["check email".to_string()]);
        assert!(spies.notified().is_empty());
    }

    #[tokio::test]
    async fn tick_run_without_on_execute_is_noop() {
        let tmp = TempDir::new().unwrap();
        write_heartbeat(&tmp, "- check email");
        let spies = CallbackSpies::new();
        let service = make_service(
            tmp.path().to_path_buf(),
            ScriptedProvider::arc(vec![decide_run("check email")]),
            None,
            Some(spies.on_notify()),
        );

        service.tick().await;

        assert!(spies.notified().is_empty());
    }

    #[tokio::test]
    async fn tick_run_without_on_notify_does_not_panic() {
        let tmp = TempDir::new().unwrap();
        write_heartbeat(&tmp, "- check email");
        let spies = CallbackSpies::new();
        let service = make_service(
            tmp.path().to_path_buf(),
            ScriptedProvider::arc(vec![decide_run("check email"), evaluate(true)]),
            Some(spies.on_execute("Important update")),
            None,
        );

        service.tick().await;

        assert_eq!(spies.executed(), vec!["check email".to_string()]);
        assert!(spies.notified().is_empty());
    }
}
