use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;

use crate::{
    agent::{
        context::ContextBuilder,
        hook::{AgentHook, AgentHookContext},
        runner::{AgentRunResult, AgentRunSpec, AgentRunner},
        skills::{BUILTIN_SKILLS_DIR, SkillsLoader},
        tools::{
            filesystem::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool},
            registry::ToolRegistry,
            search::{GlobTool, GrepTool},
            shell::ShellTool,
            web::{WebFetchTool, WebSearchTool},
        },
    },
    bus::{events::InboundMessage, queue::MessageBus},
    config::schema::{ExecToolConfig, WebToolsConfig},
    providers::base::LLMProviderDyn,
    utils::prompt_templates::render_template,
};

use tera::Context;

struct SubagentHook {
    _task_id: String,
}

impl SubagentHook {
    pub fn new(task_id: String) -> Self {
        Self { _task_id: task_id }
    }
}

/// Logging-only hook for subagent execution.
#[async_trait]
impl AgentHook for SubagentHook {
    async fn before_execute_tools(&self, context: &mut AgentHookContext) {
        for tool_call in context.tool_calls.iter() {
            let args_str = serde_json::to_string(&tool_call.arguments).unwrap();
            log::info!(
                "Subagent [{}] executing: {} with arguments: {}  ",
                self._task_id,
                tool_call.name,
                args_str
            );
        }
    }
}

pub struct SubagentManager {
    pub workspace: PathBuf,
    pub bus: Arc<MessageBus>,
    pub max_tool_result_chars: usize,
    pub model: String,
    pub web_config: WebToolsConfig,
    pub exec_config: ExecToolConfig,
    pub restrict_to_workspace: bool,
    pub runner: AgentRunner,
    running_tasks: Arc<Mutex<HashMap<String, std::thread::JoinHandle<()>>>>,
    session_tasks: Arc<Mutex<HashMap<String, HashSet<String>>>>,
}

impl SubagentManager {
    pub fn new(
        provider: Arc<dyn LLMProviderDyn>,
        workspace: PathBuf,
        bus: Arc<MessageBus>,
        max_tool_result_chars: usize,
        model: Option<String>,
        web_config: Option<WebToolsConfig>,
        exec_config: Option<ExecToolConfig>,
        restrict_to_workspace: Option<bool>,
    ) -> Self {
        let model = model.unwrap_or_else(|| provider.get_default_model());
        Self {
            workspace,
            bus,
            max_tool_result_chars,
            model,
            web_config: web_config.unwrap_or(WebToolsConfig::default()),
            exec_config: exec_config.unwrap_or(ExecToolConfig::default()),
            restrict_to_workspace: restrict_to_workspace.unwrap_or(false),
            runner: AgentRunner::new(provider),
            running_tasks: Arc::new(Mutex::new(HashMap::new())),
            session_tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn new_simple(
        provider: Arc<dyn LLMProviderDyn>,
        workspace: PathBuf,
        bus: Arc<MessageBus>,
        max_tool_result_chars: usize,
    ) -> Self {
        SubagentManager::new(
            provider,
            workspace,
            bus,
            max_tool_result_chars,
            None,
            None,
            None,
            None,
        )
    }

    /// Spawn a subagent to execute a task in the background.
    pub fn spawn(
        self: Arc<Self>,
        task: &str,
        label: Option<&str>,
        original_channel_option: Option<&str>,
        origin_chat_id_option: Option<&str>,
        session_key: Option<&str>,
    ) -> String {
        let original_channel = original_channel_option.unwrap_or("cli");
        let origin_chat_id = origin_chat_id_option.unwrap_or("direct");
        let task_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let display_label = label.map(str::to_string).unwrap_or_else(|| {
            if task.len() > 30 {
                format!("{}...", &task[..30])
            } else {
                task.to_string()
            }
        });
        let display_label_owned = display_label.clone();
        let origin = HashMap::from([
            ("channel".to_string(), original_channel.to_string()),
            ("chat_id".to_string(), origin_chat_id.to_string()),
        ]);

        let task_owned = task.to_string();
        let manager = Arc::clone(&self);
        let running_tasks = Arc::clone(&self.running_tasks);
        let session_tasks = Arc::clone(&self.session_tasks);
        let task_id_bg = task_id.clone();
        let session_key_owned = session_key.map(str::to_string);

        // LLMProviderDyn uses `?Send` futures; run on a dedicated thread with a
        // single-threaded runtime instead of `tokio::spawn`.
        let handle = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("subagent runtime")
                .block_on(async move {
                    manager
                        .run_subagent(&task_id_bg, &task_owned, &display_label, &origin)
                        .await;
                    running_tasks.lock().unwrap().remove(&task_id_bg);
                    if let Some(session_key) = session_key_owned {
                        if let Some(tasks) = session_tasks.lock().unwrap().get_mut(&session_key)
                        {
                            tasks.remove(&task_id_bg);
                        }
                    }
                });
        });

        self.running_tasks
            .lock()
            .unwrap()
            .insert(task_id.clone(), handle);
        if let Some(session_key) = session_key {
            self.session_tasks
                .lock()
                .unwrap()
                .entry(session_key.to_string())
                .or_default()
                .insert(task_id.clone());
        }

        log::info!("Spawned subagent [{}]: {}", task_id, display_label_owned);
        return format!("Subagent [{display_label_owned}] started (id: {task_id}). I'll notify you when it completes.");
    }

    /// Execute the subagent task and announce the result.
    async fn run_subagent(
        &self,
        task_id: &str,
        task: &str,
        label: &str,
        origin: &HashMap<String, String>,
    ) {
        if let Err(e) = self
            .run_subagent_inner(task_id, task, label, origin)
            .await
        {
            log::error!("Subagent [{task_id}] failed: {e}");
            let error_msg = format!("Error: {e}");
            self.announce_result(task_id, label, task, &error_msg, origin, "error")
                .await;
        }
    }

    async fn run_subagent_inner(
        &self,
        task_id: &str,
        task: &str,
        label: &str,
        origin: &HashMap<String, String>,
    ) -> Result<(), String> {
        log::info!("Subagent [{}] starting task: {}", task_id, label);
        // Build subagent tools (no message tool, no spawn tool)
        let mut tools = ToolRegistry::new();
        let allowed_dir = if self.restrict_to_workspace || !self.exec_config.sandbox.is_empty() {
            Some(self.workspace.clone())
        } else {
            None
        };
        let extra_read = if allowed_dir.is_some() {
            vec![BUILTIN_SKILLS_DIR.clone()]
        } else {
            vec![]
        };
        let workspace = Some(self.workspace.clone());
        tools.register(Box::new(ReadFileTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            Some(extra_read),
        )));
        tools.register(Box::new(WriteFileTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            None,
        )));
        tools.register(Box::new(EditFileTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            None,
        )));
        tools.register(Box::new(ListDirTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            None,
        )));
        tools.register(Box::new(GlobTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            None,
        )));
        tools.register(Box::new(GrepTool::new(
            workspace.clone(),
            allowed_dir.clone(),
            None,
        )));
        if self.exec_config.enable {
            tools.register(Box::new(ShellTool::new(
                self.exec_config.timeout as u64,
                workspace.clone(),
                None,
                None,
                self.restrict_to_workspace,
                if self.exec_config.sandbox.is_empty() {
                    None
                } else {
                    Some(self.exec_config.sandbox.clone())
                },
                if self.exec_config.path_append.is_empty() {
                    None
                } else {
                    Some(self.exec_config.path_append.clone())
                },
            )));
        }
        if self.web_config.enable {
            tools.register(Box::new(WebSearchTool::new(
                Some(self.web_config.search.clone()),
                self.web_config.proxy.clone(),
            )));
            tools.register(Box::new(WebFetchTool::new(
                None,
                self.web_config.proxy.clone(),
            )));
        }
        let system_prompt = self.build_subagent_prompt();
        if system_prompt.is_empty() {
            return Err("Failed to build subagent prompt".to_string());
        }
        // Building the messages for the agent run
        let messages: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "system", "content": system_prompt}),
            serde_json::json!({"role": "user", "content": task}),
        ];
        let result = self
            .runner
            .run(AgentRunSpec {
                initial_messages: messages,
                tools: tools,
                model: self.model.clone(),
                max_iterations: 15,
                max_tool_result_chars: self.max_tool_result_chars,
                hook: Some(Arc::new(SubagentHook::new(task_id.to_string()))),
                max_iterations_message: Some(
                    "Task completed but no final response was generated.".to_string(),
                ),
                error_message: None,
                fail_on_tool_error: true,
                ..Default::default()
            })
            .await;
        if result.stop_reason == "tool_error" {
            let progress = SubagentManager::format_partial_progress(result);
            self.announce_result(task_id, label, task, &progress, origin, "error")
                .await;
            return Ok(());
        }
        if result.stop_reason == "error" {
            let error = result
                .error
                .or(result.final_content)
                .unwrap_or_else(|| "Error: subagent execution failed.".to_string());
            self.announce_result(task_id, label, task, &error, origin, "error")
                .await;
            return Ok(());
        }
        let final_result = result.final_content.unwrap_or(
            "Task completed but no final response was generated.".to_string(),
        );
        log::info!("Subagent [{task_id}] completed successfully");
        self.announce_result(task_id, label, task, &final_result, origin, "ok")
            .await;
        Ok(())
    }

    fn format_partial_progress(result: AgentRunResult) -> String {
        let completed = result
            .tool_events
            .iter()
            .filter(|e| e.get("status").unwrap_or(&"".to_string()) == "ok")
            .collect::<Vec<_>>();
        let failure = result
            .tool_events
            .iter()
            .rev()
            .find(|e| e.get("status").unwrap_or(&"".to_string()) == "error");
        let mut lines = Vec::new();
        if !completed.is_empty() {
            lines.push("Completed steps:".to_string());
            let start = completed.len().saturating_sub(3);
            for event in completed[start..].iter() {
                lines.push(format!(
                    "- {}: {}",
                    event.get("name").unwrap_or(&"".to_string()),
                    event.get("detail").unwrap_or(&"".to_string())
                ));
            }
        }
        if let Some(failure) = failure {
            if !lines.is_empty() {
                lines.push("".to_string());
            }
            lines.push("Failure:".to_string());
            lines.push(format!(
                "- {}: {}",
                failure.get("name").unwrap_or(&"".to_string()),
                failure.get("detail").unwrap_or(&"".to_string())
            ));
        }
        let error = result.error.clone();
        if error.is_some() && failure.is_none() {
            if !lines.is_empty() {
                lines.push("".to_string());
            }
            lines.push("Failure:".to_string());
            lines.push(format!("- {}", result.error.unwrap()));
        }
        if !lines.is_empty() {
            lines.join("\n")
        } else {
            error.unwrap_or("Error: subagent execution failed.".to_string())
        }
    }

    /// Build a focused system prompt for the subagent.
    fn build_subagent_prompt(&self) -> String {
        let time_ctx = ContextBuilder::build_runtime_context(None, None, None);
        let skills_summary = SkillsLoader::new(&self.workspace, None).build_skills_summary();
        let mut ctx = Context::new();
        ctx.insert("time_ctx", &time_ctx);
        ctx.insert("workspace", &self.workspace.to_string_lossy().to_string());
        ctx.insert("skills_summary", &skills_summary);
        let result = render_template("agent/subagent_system.md", &ctx, true);
        match result {
            Ok(result) => result,
            Err(e) => {
                log::error!("Failed to build subagent prompt: {e}");
                "".to_string()
            }
        }
    }

    /// Announce the subagent result to the main agent via the message bus.
    async fn announce_result(
        &self,
        task_id: &str,
        label: &str,
        task: &str,
        result: &str,
        origin: &HashMap<String, String>,
        status: &str,
    ) {
        let status_text = if status == "ok" {
            "completed successfully"
        } else {
            "failed"
        };
        let mut ctx = Context::new();
        ctx.insert("label", label);
        ctx.insert("status_text", status_text);
        ctx.insert("task", task);
        ctx.insert("result", result);
        let announce_content_result = render_template("agent/subagent_announce.md", &ctx, true);

        if let Ok(announce_content) = announce_content_result {
            let origin_channel = origin.get("channel").map(|s| s.as_str()).unwrap_or("cli");
            let origin_chat_id = origin
                .get("chat_id")
                .map(|s| s.as_str())
                .unwrap_or("direct");

            let msg = InboundMessage {
                channel: "system".to_string(),
                sender_id: "subagent".to_string(),
                chat_id: format!("{}:{}", origin_channel, origin_chat_id),
                content: announce_content,
                timestamp: Utc::now(),
                media: vec![],
                metadata: HashMap::new(),
                session_key_override: Some(format!("{origin_channel}:{origin_chat_id}")),
            };
            let result = self.bus.publish_inbound(msg);
            if let Err(e) = result {
                log::error!("Failed to publish subagent announce message for task {task_id}: {e}");
            }
        } else {
            log::error!("Failed to render subagent announce content for task {task_id}");
        }
    }

    /// Cancel all subagents for the given session. Returns count cancelled.
    pub async fn cancel_by_session(&self, session_key: &str) -> u32 {
        let task_ids: Vec<String> = self
            .session_tasks
            .lock()
            .unwrap()
            .get(session_key)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default();

        let mut handles = Vec::new();
        {
            let mut running = self.running_tasks.lock().unwrap();
            for tid in task_ids {
                let Some(handle) = running.get(&tid) else {
                    continue;
                };
                if handle.is_finished() {
                    continue;
                }
                if let Some(handle) = running.remove(&tid) {
                    handles.push(handle);
                }
            }
        }

        let count = handles.len() as u32;
        if handles.is_empty() {
            return count;
        }

        // std::thread::JoinHandle has no cancel(); wait for each thread to finish.
        tokio::task::spawn_blocking(move || {
            for handle in handles {
                let _ = handle.join();
            }
        })
        .await
        .ok();

        count
    }
}

#[cfg(test)]
mod tests {
    use super::SubagentHook;
    use crate::agent::hook::{AgentHook, AgentHookContext};
    use crate::providers::base::ToolCallRequest;
    use std::collections::HashMap;

    fn make_tool_call(name: &str, args: HashMap<String, serde_json::Value>) -> ToolCallRequest {
        ToolCallRequest {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: args,
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }
    }

    #[tokio::test]
    async fn before_execute_tools_no_panic_with_empty_tool_calls() {
        let hook = SubagentHook::new("task-1".to_string());
        let mut ctx = AgentHookContext::new(0, vec![]);
        // Must complete without panic even when there are no tool calls.
        hook.before_execute_tools(&mut ctx).await;
    }

    #[tokio::test]
    async fn before_execute_tools_no_panic_with_single_tool_call() {
        let hook = SubagentHook::new("task-2".to_string());
        let mut ctx = AgentHookContext::new(0, vec![]);
        ctx.tool_calls.push(make_tool_call(
            "read_file",
            HashMap::from([("path".to_string(), serde_json::json!("/tmp/foo.txt"))]),
        ));
        hook.before_execute_tools(&mut ctx).await;
    }

    #[tokio::test]
    async fn before_execute_tools_no_panic_with_multiple_tool_calls() {
        let hook = SubagentHook::new("task-3".to_string());
        let mut ctx = AgentHookContext::new(0, vec![]);
        ctx.tool_calls
            .push(make_tool_call("tool_a", HashMap::new()));
        ctx.tool_calls
            .push(make_tool_call("tool_b", HashMap::new()));
        ctx.tool_calls
            .push(make_tool_call("tool_c", HashMap::new()));
        hook.before_execute_tools(&mut ctx).await;
    }

    #[tokio::test]
    async fn before_execute_tools_no_panic_with_complex_arguments() {
        let hook = SubagentHook::new("task-4".to_string());
        let mut ctx = AgentHookContext::new(0, vec![]);
        ctx.tool_calls.push(make_tool_call(
            "write_file",
            HashMap::from([
                ("path".to_string(), serde_json::json!("/tmp/out.json")),
                ("content".to_string(), serde_json::json!({"key": [1, 2, 3]})),
            ]),
        ));
        hook.before_execute_tools(&mut ctx).await;
    }

    #[tokio::test]
    async fn before_execute_tools_does_not_mutate_context() {
        let hook = SubagentHook::new("task-5".to_string());
        let mut ctx = AgentHookContext::new(0, vec![]);
        ctx.tool_calls
            .push(make_tool_call("my_tool", HashMap::new()));
        let tool_count_before = ctx.tool_calls.len();
        hook.before_execute_tools(&mut ctx).await;
        assert_eq!(
            ctx.tool_calls.len(),
            tool_count_before,
            "hook must not modify tool_calls"
        );
    }

    // ── format_partial_progress ──────────────────────────────────────────────

    use super::SubagentManager;
    use crate::agent::runner::AgentRunResult;

    fn tool_event(status: &str, name: &str, detail: &str) -> HashMap<String, String> {
        HashMap::from([
            ("status".to_string(), status.to_string()),
            ("name".to_string(), name.to_string()),
            ("detail".to_string(), detail.to_string()),
        ])
    }

    fn make_run_result(
        tool_events: Vec<HashMap<String, String>>,
        error: Option<&str>,
    ) -> AgentRunResult {
        AgentRunResult {
            tool_events,
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn format_partial_progress_empty_returns_default_message() {
        let out = SubagentManager::format_partial_progress(make_run_result(vec![], None));
        assert_eq!(out, "Error: subagent execution failed.");
    }

    #[test]
    fn format_partial_progress_shows_last_three_completed_steps_in_order() {
        let events = vec![
            tool_event("ok", "step1", "done 1"),
            tool_event("ok", "step2", "done 2"),
            tool_event("ok", "step3", "done 3"),
            tool_event("ok", "step4", "done 4"),
        ];
        let out = SubagentManager::format_partial_progress(make_run_result(events, None));
        assert!(out.starts_with("Completed steps:\n"));
        assert!(out.contains("- step2: done 2"));
        assert!(out.contains("- step3: done 3"));
        assert!(out.contains("- step4: done 4"));
        assert!(!out.contains("step1"));
        let step2 = out.find("- step2: done 2").unwrap();
        let step3 = out.find("- step3: done 3").unwrap();
        let step4 = out.find("- step4: done 4").unwrap();
        assert!(
            step2 < step3 && step3 < step4,
            "steps should stay chronological"
        );
    }

    #[test]
    fn format_partial_progress_shows_tool_failure_event() {
        let events = vec![
            tool_event("ok", "grep", "found 3 matches"),
            tool_event("error", "write_file", "permission denied"),
        ];
        let out = SubagentManager::format_partial_progress(make_run_result(events, None));
        assert!(out.contains("Completed steps:"));
        assert!(out.contains("- grep: found 3 matches"));
        assert!(out.contains("Failure:"));
        assert!(out.contains("- write_file: permission denied"));
    }

    #[test]
    fn format_partial_progress_uses_most_recent_tool_error() {
        let events = vec![
            tool_event("error", "first", "err1"),
            tool_event("ok", "middle", "ok"),
            tool_event("error", "last", "err2"),
        ];
        let out = SubagentManager::format_partial_progress(make_run_result(events, None));
        assert!(out.contains("- last: err2"));
        assert!(!out.contains("first: err1"));
    }

    #[test]
    fn format_partial_progress_uses_result_error_when_no_tool_failure() {
        let out = SubagentManager::format_partial_progress(make_run_result(
            vec![tool_event("ok", "read_file", "contents")],
            Some("LLM rate limited"),
        ));
        assert!(out.contains("Completed steps:"));
        assert!(out.contains("- read_file: contents"));
        assert!(out.contains("Failure:"));
        assert!(out.contains("- LLM rate limited"));
    }

    #[test]
    fn format_partial_progress_tool_failure_takes_precedence_over_result_error() {
        let out = SubagentManager::format_partial_progress(make_run_result(
            vec![tool_event("error", "exec", "command failed")],
            Some("should not appear"),
        ));
        assert_eq!(out, "Failure:\n- exec: command failed");
        assert!(!out.contains("should not appear"));
    }

    #[test]
    fn format_partial_progress_result_error_only() {
        let out = SubagentManager::format_partial_progress(make_run_result(
            vec![],
            Some("provider unavailable"),
        ));
        assert_eq!(out, "Failure:\n- provider unavailable");
    }

    #[test]
    fn format_partial_progress_missing_event_keys_do_not_panic() {
        let events = vec![HashMap::from([("status".to_string(), "ok".to_string())])];
        let out = SubagentManager::format_partial_progress(make_run_result(events, None));
        assert_eq!(out, "Completed steps:\n- : ");
    }

    // ── announce_result ──────────────────────────────────────────────────────

    use crate::{
        bus::{events::InboundMessage, queue::MessageBus},
        providers::base::{GenerationSettings, LLMProviderDyn, LLMResponse},
    };
    use async_trait::async_trait;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct TestProvider {
        settings: GenerationSettings,
    }

    impl TestProvider {
        fn arc() -> Arc<dyn LLMProviderDyn> {
            Arc::new(Self {
                settings: GenerationSettings::new(),
            })
        }
    }

    #[async_trait(?Send)]
    impl LLMProviderDyn for TestProvider {
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

    fn origin(channel: &str, chat_id: &str) -> HashMap<String, String> {
        HashMap::from([
            ("channel".to_string(), channel.to_string()),
            ("chat_id".to_string(), chat_id.to_string()),
        ])
    }

    async fn announce_and_consume(origin: HashMap<String, String>, status: &str) -> InboundMessage {
        let tmp = TempDir::new().unwrap();
        let bus = Arc::new(MessageBus::new());
        let manager = SubagentManager::new_simple(
            TestProvider::arc(),
            tmp.path().to_path_buf(),
            bus.clone(),
            4096,
        );
        manager
            .announce_result(
                "task-1",
                "worker-1",
                "summarise logs",
                "Done.",
                &origin,
                status,
            )
            .await;
        drop(manager);
        let bus = match Arc::try_unwrap(bus) {
            Ok(bus) => bus,
            Err(_) => panic!("manager should release bus Arc"),
        };
        let msg = bus.consume_inbound().await;
        msg.expect("announce should publish")
    }

    #[tokio::test]
    async fn announce_result_publishes_system_message_with_session_key() {
        let msg = announce_and_consume(origin("telegram", "chat-42"), "ok").await;
        assert_eq!(msg.channel, "system");
        assert_eq!(msg.sender_id, "subagent");
        assert_eq!(msg.chat_id, "telegram:chat-42");
        assert_eq!(msg.session_key(), "telegram:chat-42");
    }

    #[tokio::test]
    async fn announce_result_ok_content_includes_task_details() {
        let msg = announce_and_consume(origin("cli", "direct"), "ok").await;
        assert!(msg.content.contains("worker-1"));
        assert!(msg.content.contains("completed successfully"));
        assert!(msg.content.contains("summarise logs"));
        assert!(msg.content.contains("Done."));
    }

    #[tokio::test]
    async fn announce_result_failed_status_uses_failed_text() {
        let msg = announce_and_consume(origin("cli", "direct"), "error").await;
        assert!(msg.content.contains("failed"));
        assert!(!msg.content.contains("completed successfully"));
    }

    #[tokio::test]
    async fn announce_result_empty_origin_uses_defaults() {
        let msg = announce_and_consume(HashMap::new(), "ok").await;
        assert_eq!(msg.chat_id, "cli:direct");
        assert_eq!(msg.session_key(), "cli:direct");
    }

    #[tokio::test]
    async fn announce_result_increases_inbound_queue_size() {
        let tmp = TempDir::new().unwrap();
        let bus = Arc::new(MessageBus::new());
        assert_eq!(bus.inbound_size(), 0);
        let manager = SubagentManager::new_simple(
            TestProvider::arc(),
            tmp.path().to_path_buf(),
            bus.clone(),
            4096,
        );
        manager
            .announce_result(
                "task-2",
                "worker",
                "task",
                "result",
                &origin("cli", "direct"),
                "ok",
            )
            .await;
        assert_eq!(bus.inbound_size(), 1);
    }

    // ── run_subagent_inner ───────────────────────────────────────────────────

    use std::sync::Mutex;

    struct ScriptedProvider {
        settings: GenerationSettings,
        responses: Mutex<Vec<LLMResponse>>,
    }

    impl ScriptedProvider {
        fn arc(responses: Vec<LLMResponse>) -> Arc<dyn LLMProviderDyn> {
            Arc::new(Self {
                settings: GenerationSettings::new(),
                responses: Mutex::new(responses),
            })
        }

        fn take_response(&self) -> LLMResponse {
            let mut guard = self.responses.lock().unwrap();
            assert!(
                !guard.is_empty(),
                "ScriptedProvider: unexpected chat_with_retry call"
            );
            guard.remove(0)
        }
    }

    #[async_trait(?Send)]
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
            "scripted-model".to_string()
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
            self.take_response()
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
            self.take_response()
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
            self.take_response()
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
            self.take_response()
        }
    }

    fn llm_text(content: &str) -> LLMResponse {
        LLMResponse {
            content: Some(content.to_string()),
            finish_reason: "stop".to_string(),
            ..LLMResponse::new()
        }
    }

    fn llm_error(content: &str) -> LLMResponse {
        LLMResponse {
            content: Some(content.to_string()),
            finish_reason: "error".to_string(),
            ..LLMResponse::new()
        }
    }

    fn llm_read_missing_file() -> LLMResponse {
        LLMResponse {
            tool_calls: vec![ToolCallRequest {
                id: "call_read".to_string(),
                name: "read_file".to_string(),
                arguments: HashMap::from([(
                    "path".to_string(),
                    serde_json::json!("missing.txt"),
                )]),
                extra_content: None,
                provider_specific_fields: None,
                function_provider_specific_fields: None,
            }],
            finish_reason: "tool_calls".to_string(),
            ..LLMResponse::new()
        }
    }

    async fn run_inner_and_consume(
        provider: Arc<dyn LLMProviderDyn>,
        task: &str,
        label: &str,
    ) -> (Result<(), String>, InboundMessage) {
        let tmp = TempDir::new().unwrap();
        let bus = Arc::new(MessageBus::new());
        let manager = SubagentManager::new_simple(
            provider,
            tmp.path().to_path_buf(),
            bus.clone(),
            4096,
        );
        let origin = origin("cli", "direct");
        let result = manager
            .run_subagent_inner("task-1", task, label, &origin)
            .await;
        drop(manager);
        let bus = match Arc::try_unwrap(bus) {
            Ok(bus) => bus,
            Err(_) => panic!("manager should release bus Arc"),
        };
        let msg = bus.consume_inbound().await.expect("announce should publish");
        (result, msg)
    }

    #[tokio::test]
    async fn run_subagent_inner_success_announces_ok() {
        let (result, msg) = run_inner_and_consume(
            ScriptedProvider::arc(vec![llm_text("All done.")]),
            "summarise logs",
            "worker-1",
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(msg.channel, "system");
        assert_eq!(msg.sender_id, "subagent");
        assert!(msg.content.contains("worker-1"));
        assert!(msg.content.contains("completed successfully"));
        assert!(msg.content.contains("summarise logs"));
        assert!(msg.content.contains("All done."));
    }

    #[tokio::test]
    async fn run_subagent_inner_tool_error_announces_partial_progress() {
        let (result, msg) = run_inner_and_consume(
            ScriptedProvider::arc(vec![llm_read_missing_file()]),
            "read config",
            "reader",
        )
        .await;

        assert!(result.is_ok());
        assert!(msg.content.contains("failed"));
        assert!(msg.content.contains("read config"));
        assert!(msg.content.contains("Failure:"));
        assert!(msg.content.contains("read_file"));
        assert!(msg.content.contains("File not found"));
    }

    #[tokio::test]
    async fn run_subagent_inner_provider_error_announces_failure() {
        let (result, msg) = run_inner_and_consume(
            ScriptedProvider::arc(vec![llm_error("Provider unavailable")]),
            "run analysis",
            "analyst",
        )
        .await;

        assert!(result.is_ok());
        assert!(msg.content.contains("failed"));
        assert!(msg.content.contains("analyst"));
        assert!(msg.content.contains("Sorry, I encountered an error"));
    }

    #[tokio::test]
    async fn run_subagent_inner_empty_final_response_announces_fallback() {
        let empty = LLMResponse::new();
        let (result, msg) = run_inner_and_consume(
            ScriptedProvider::arc(vec![
                empty.clone(),
                empty.clone(),
                empty.clone(),
                empty,
            ]),
            "empty reply task",
            "worker",
        )
        .await;

        assert!(result.is_ok());
        assert!(msg.content.contains("completed successfully"));
        assert!(msg.content.contains("couldn't produce a final answer"));
    }
}
