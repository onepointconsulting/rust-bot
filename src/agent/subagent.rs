use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::{
    agent::{
        context::ContextBuilder,
        hook::{AgentHook, AgentHookContext, ToolHookDecision},
        model_runtime::ModelRuntimeResolver,
        runner::{AgentRunResult, AgentRunSpec, AgentRunner},
        skills::SkillsLoader,
        tools::{filesystem::FsToolConfig, registry::ToolRegistry, shell::ShellTool},
    },
    bus::{events::InboundMessage, queue::MessageBus},
    config::schema::{
        Config, DocxToolConfig, ExecToolConfig, GmailToolConfig, ImageGenerationToolConfig,
        OcrToolConfig, SubagentConfig, WebToolsConfig,
    },
    providers::base::LLMProviderDyn,
    session::manager::SessionManager,
    utils::{
        prompt_templates::render_template,
        registry_helper::{
            filesystem_tool_scope, register_conversion_tools, register_filesystem_tools,
            register_gmail_tools, register_image_generation_tools, register_ocr_tools,
            register_web_tools,
        },
        relative_time::format_relative_time,
    },
};

use tera::Context;

/// Longest label derived from a task when the caller gave none.
const DERIVED_LABEL_MAX_CHARS: usize = 30;
/// Longest task text kept in a [`SubagentRecord`].
const TASK_SUMMARY_MAX_CHARS: usize = 80;
/// How many finished records are kept; running ones are never pruned.
const MAX_FINISHED_RECORDS: usize = 50;

/// Where a spawned sub-agent is in its life. Terminal states never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    /// Stopped by a tool error; the announcement carries the partial progress.
    Partial,
    Failed,
    Cancelled,
}

impl SubagentStatus {
    /// Lower-case token shown in listings.
    pub fn as_str(&self) -> &'static str {
        match self {
            SubagentStatus::Running => "running",
            SubagentStatus::Completed => "completed",
            SubagentStatus::Partial => "partial",
            SubagentStatus::Failed => "failed",
            SubagentStatus::Cancelled => "cancelled",
        }
    }
}

/// What is remembered about one spawned sub-agent (in memory, this process only).
#[derive(Debug, Clone)]
pub struct SubagentRecord {
    pub task_id: String,
    pub label: String,
    /// The first [`TASK_SUMMARY_MAX_CHARS`] characters of the task.
    pub task_summary: String,
    pub session_key: Option<String>,
    pub channel: String,
    pub chat_id: String,
    pub spawned_at: DateTime<Utc>,
    /// Set once, when the status leaves [`SubagentStatus::Running`].
    pub finished_at: Option<DateTime<Utc>>,
    pub status: SubagentStatus,
}

/// `text` cut to `max_chars` characters (never inside a multi-byte character),
/// with `...` appended when something was cut.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        format!("{}...", text.chars().take(max_chars).collect::<String>())
    } else {
        text.to_string()
    }
}

/// Render `records` as the "Spawn tasks" section of `/subagents`. Rows of
/// `current_session` are marked with `*`. Records are rendered in the order
/// given (see [`SubagentManager::list`]).
pub fn format_subagents_list(records: &[SubagentRecord], current_session: Option<&str>) -> String {
    format_subagents_list_at(records, current_session, Utc::now())
}

/// [`format_subagents_list`] with an explicit "now", for deterministic tests.
fn format_subagents_list_at(
    records: &[SubagentRecord],
    current_session: Option<&str>,
    now: DateTime<Utc>,
) -> String {
    if records.is_empty() {
        return "No spawn tasks.".to_string();
    }
    let mut lines = vec!["Spawn tasks (runs, this process):".to_string()];
    for record in records {
        let marker =
            if current_session.is_some() && record.session_key.as_deref() == current_session {
                "*"
            } else {
                " "
            };
        let mut timing = format!("spawned {}", format_relative_time(now, record.spawned_at));
        if let Some(finished_at) = record.finished_at {
            timing.push_str(&format!(
                ", finished {}",
                format_relative_time(now, finished_at)
            ));
        }
        lines.push(format!(
            "{marker} {id} [{status}] {label} — {summary} (session: {session}; {timing})",
            id = record.task_id,
            status = record.status.as_str().to_uppercase(),
            label = record.label,
            summary = record.task_summary,
            session = record.session_key.as_deref().unwrap_or("—"),
        ));
    }
    if current_session.is_some() {
        lines.push("(* = this session)".to_string());
    }
    lines.join(
        "
",
    )
}

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
    async fn before_execute_tools(&self, context: &mut AgentHookContext) -> ToolHookDecision {
        for tool_call in context.tool_calls.iter() {
            let args_str = serde_json::to_string(&tool_call.arguments).unwrap();
            log::info!(
                "Subagent [{}] executing: {} with arguments: {}  ",
                self._task_id,
                tool_call.name,
                args_str
            );
        }
        ToolHookDecision::Continue
    }
}

pub struct SubagentManager {
    pub workspace: PathBuf,
    pub bus: Arc<MessageBus>,
    pub max_tool_result_chars: usize,
    /// Resolves each subagent run's provider/model/generation settings — by
    /// the spawning session's stored preset override when known, else the
    /// process-wide default. No fixed provider/model is held here.
    pub runtime_resolver: Arc<ModelRuntimeResolver>,
    sessions: Arc<Mutex<SessionManager>>,
    pub web_config: WebToolsConfig,
    pub exec_config: ExecToolConfig,
    pub gmail_config: GmailToolConfig,
    pub ocr_config: OcrToolConfig,
    pub docx_config: DocxToolConfig,
    pub image_generation_config: ImageGenerationToolConfig,
    pub subagent_config: SubagentConfig,
    pub restrict_to_workspace: bool,
    /// Tool names subagents never get (`tools.disabledTools`).
    disabled_tools: Vec<String>,
    running_tasks: Arc<Mutex<HashMap<String, std::thread::JoinHandle<()>>>>,
    session_tasks: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    /// One record per spawned sub-agent, kept after it finishes (bounded).
    tasks: Arc<Mutex<HashMap<String, SubagentRecord>>>,
}

impl SubagentManager {
    pub fn new(
        runtime_resolver: Arc<ModelRuntimeResolver>,
        sessions: Arc<Mutex<SessionManager>>,
        workspace: PathBuf,
        bus: Arc<MessageBus>,
        max_tool_result_chars: usize,
        web_config: Option<WebToolsConfig>,
        exec_config: Option<ExecToolConfig>,
        gmail_config: Option<GmailToolConfig>,
        ocr_config: Option<OcrToolConfig>,
        docx_config: Option<DocxToolConfig>,
        image_generation_config: Option<ImageGenerationToolConfig>,
        subagent_config: Option<SubagentConfig>,
        restrict_to_workspace: Option<bool>,
    ) -> Self {
        Self {
            workspace,
            bus,
            max_tool_result_chars,
            runtime_resolver,
            sessions,
            web_config: web_config.unwrap_or_default(),
            exec_config: exec_config.unwrap_or_default(),
            gmail_config: gmail_config.unwrap_or_default(),
            ocr_config: ocr_config.unwrap_or_default(),
            docx_config: docx_config.unwrap_or_default(),
            image_generation_config: image_generation_config.unwrap_or_default(),
            subagent_config: subagent_config.unwrap_or_default(),
            restrict_to_workspace: restrict_to_workspace.unwrap_or(false),
            disabled_tools: Vec::new(),
            running_tasks: Arc::new(Mutex::new(HashMap::new())),
            session_tasks: Arc::new(Mutex::new(HashMap::new())),
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Tools (by name) that subagents must not get, mirroring `tools.disabledTools`.
    pub fn with_disabled_tools(mut self, names: Vec<String>) -> Self {
        self.disabled_tools = names;
        self
    }

    /// Convenience constructor for tests/tools that don't care about presets
    /// or a shared session store: builds a private resolver (from
    /// `Config::default()`) and a private session manager around `provider`.
    pub fn new_simple(
        provider: Arc<dyn LLMProviderDyn>,
        workspace: PathBuf,
        bus: Arc<MessageBus>,
        max_tool_result_chars: usize,
    ) -> Self {
        let runtime_resolver = Arc::new(ModelRuntimeResolver::new(Config::default(), provider));
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            workspace.clone(),
        )));
        SubagentManager::new(
            runtime_resolver,
            sessions,
            workspace,
            bus,
            max_tool_result_chars,
            None,
            None,
            None,
            None,
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
        let display_label = label
            .map(str::to_string)
            .unwrap_or_else(|| truncate_chars(task, DERIVED_LABEL_MAX_CHARS));
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
        self.insert_running_record(SubagentRecord {
            task_id: task_id.clone(),
            label: display_label.clone(),
            task_summary: truncate_chars(task, TASK_SUMMARY_MAX_CHARS),
            session_key: session_key_owned.clone(),
            channel: original_channel.to_string(),
            chat_id: origin_chat_id.to_string(),
            spawned_at: Utc::now(),
            finished_at: None,
            status: SubagentStatus::Running,
        });

        // LLMProviderDyn uses `?Send` futures; run on a dedicated thread with a
        // single-threaded runtime instead of `tokio::spawn`.
        let handle = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("subagent runtime")
                .block_on(async move {
                    log::info!("Running subagent task: {}", task_id_bg);
                    manager
                        .run_subagent(
                            &task_id_bg,
                            &task_owned,
                            &display_label,
                            &origin,
                            session_key_owned.as_deref(),
                        )
                        .await;
                    log::info!("Completed: {}", task_id_bg);
                    // Safety net for a run that ended without reporting a status;
                    // a no-op whenever one was already recorded (first writer wins).
                    manager.set_status(&task_id_bg, SubagentStatus::Failed);
                    running_tasks.lock().unwrap().remove(&task_id_bg);
                    log::info!("Removed from running tasks: {}", task_id_bg);
                    if let Some(session_key) = session_key_owned
                        && let Some(tasks) = session_tasks.lock().unwrap().get_mut(&session_key)
                    {
                        tasks.remove(&task_id_bg);
                        log::info!("Removed from tasks: {}", task_id_bg);
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
        format!(
            "Subagent [{display_label_owned}] started (id: {task_id}). I'll notify you when it completes."
        )
    }

    /// Store a freshly spawned record, then drop the oldest finished records
    /// beyond [`MAX_FINISHED_RECORDS`]. Running records are never dropped.
    fn insert_running_record(&self, record: SubagentRecord) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.insert(record.task_id.clone(), record);
        let mut finished: Vec<(DateTime<Utc>, String)> = tasks
            .values()
            .filter(|record| record.status != SubagentStatus::Running)
            .map(|record| (record.spawned_at, record.task_id.clone()))
            .collect();
        if finished.len() <= MAX_FINISHED_RECORDS {
            return;
        }
        finished.sort();
        let surplus = finished.len() - MAX_FINISHED_RECORDS;
        for (_, task_id) in finished.into_iter().take(surplus) {
            tasks.remove(&task_id);
        }
    }

    /// Move a record out of [`SubagentStatus::Running`]. One-shot: the first
    /// terminal status wins, later calls (and unknown ids) are ignored. This is
    /// what keeps a cancelled task `Cancelled` when it still finishes later.
    fn set_status(&self, task_id: &str, status: SubagentStatus) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = tasks.get_mut(task_id)
            && record.status == SubagentStatus::Running
        {
            record.status = status;
            record.finished_at = Some(Utc::now());
        }
    }

    /// A snapshot of every sub-agent this process has spawned, in all sessions:
    /// running ones first, then the rest; each group newest first.
    pub fn list(&self) -> Vec<SubagentRecord> {
        let mut records: Vec<SubagentRecord> = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        records.sort_by(|a, b| {
            let a_running = a.status == SubagentStatus::Running;
            let b_running = b.status == SubagentStatus::Running;
            b_running
                .cmp(&a_running)
                .then(b.spawned_at.cmp(&a.spawned_at))
        });
        records
    }

    /// Execute the subagent task and announce the result.
    async fn run_subagent(
        &self,
        task_id: &str,
        task: &str,
        label: &str,
        origin: &HashMap<String, String>,
        session_key: Option<&str>,
    ) {
        if let Err(e) = self
            .run_subagent_inner(task_id, task, label, origin, session_key)
            .await
        {
            log::error!("Subagent [{task_id}] failed: {e}");
            self.set_status(task_id, SubagentStatus::Failed);
            let error_msg = format!("Error: {e}");
            self.announce_result(task_id, label, task, &error_msg, origin, "error")
                .await;
        }
    }

    /// The tools a subagent gets: files, shell (when enabled), web, gmail, OCR,
    /// document conversion and image generation. Never the message or spawn
    /// tool, and never a tool named in `tools.disabledTools`.
    fn build_tool_registry(&self) -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        let (allowed_dir, extra_read) = filesystem_tool_scope(
            &self.workspace,
            self.restrict_to_workspace,
            &self.exec_config.sandbox,
        );
        register_filesystem_tools(
            &mut tools,
            &self.workspace,
            allowed_dir.clone(),
            extra_read.clone(),
        );
        if self.exec_config.enable {
            tools.register(Box::new(ShellTool::new(
                self.exec_config.timeout as u64,
                Some(self.workspace.clone()),
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
        register_web_tools(&self.web_config, &mut tools);
        register_gmail_tools(&self.gmail_config, &self.workspace, &mut tools);
        register_ocr_tools(
            &self.ocr_config,
            &self.workspace,
            allowed_dir.clone(),
            extra_read.clone(),
            &mut tools,
        );
        register_conversion_tools(
            &self.docx_config,
            &FsToolConfig::new(Some(self.workspace.clone()), allowed_dir, Some(extra_read)),
            &mut tools,
        );
        register_image_generation_tools(&self.image_generation_config, &self.workspace, &mut tools);
        for name in &self.disabled_tools {
            tools.unregister(name);
        }
        tools
    }

    async fn run_subagent_inner(
        &self,
        task_id: &str,
        task: &str,
        label: &str,
        origin: &HashMap<String, String>,
        session_key: Option<&str>,
    ) -> Result<(), String> {
        log::info!("Subagent [{}] starting task: {}", task_id, label);
        let tools = self.build_tool_registry();

        let system_prompt = self.build_subagent_prompt();
        if system_prompt.is_empty() {
            return Err("Failed to build subagent prompt".to_string());
        }
        // Building the messages for the agent run
        let messages: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "system", "content": system_prompt}),
            serde_json::json!({"role": "user", "content": task}),
        ];
        let runtime = match session_key {
            Some(key) => self
                .runtime_resolver
                .resolve_for_session_key(&self.sessions, key),
            None => self.runtime_resolver.current_default(),
        };
        let runner = AgentRunner::new(runtime.provider.clone());
        let result = runner
            .run(AgentRunSpec {
                initial_messages: messages,
                tools,
                model: runtime.model.clone(),
                max_iterations: 15,
                max_tool_result_chars: self.max_tool_result_chars,
                hook: Some(Arc::new(SubagentHook::new(task_id.to_string()))),
                max_iterations_message: Some(
                    "Task completed but no final response was generated.".to_string(),
                ),
                error_message: None,
                fail_on_tool_error: self.subagent_config.fail_on_tool_error,
                ..Default::default()
            })
            .await;
        if result.stop_reason == "tool_error" {
            self.set_status(task_id, SubagentStatus::Partial);
            let progress = SubagentManager::format_partial_progress(result);
            self.announce_result(task_id, label, task, &progress, origin, "error")
                .await;
            return Ok(());
        }
        if result.stop_reason == "error" {
            self.set_status(task_id, SubagentStatus::Failed);
            let error = result
                .error
                .or(result.final_content)
                .unwrap_or_else(|| "Error: subagent execution failed.".to_string());
            self.announce_result(task_id, label, task, &error, origin, "error")
                .await;
            return Ok(());
        }
        let final_result = result
            .final_content
            .unwrap_or("Task completed but no final response was generated.".to_string());
        log::info!("Subagent [{task_id}] completed successfully");
        self.set_status(task_id, SubagentStatus::Completed);
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

        log::info!(
            "Announcing subagent result: {}",
            announce_content_result.is_ok()
        );

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
        let mut cancelled_ids = Vec::new();
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
                    cancelled_ids.push(tid);
                }
            }
        }
        // Before waiting: the task keeps running until it ends, but a result it
        // reports afterwards must not overwrite `Cancelled`.
        for task_id in &cancelled_ids {
            self.set_status(task_id, SubagentStatus::Cancelled);
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

    #[async_trait]
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

    #[async_trait]
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
            _: Option<f32>,
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
            _: Option<f32>,
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
            _: Option<crate::providers::base::BoxedProgressCallback>,
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
                arguments: HashMap::from([("path".to_string(), serde_json::json!("missing.txt"))]),
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
        let manager =
            SubagentManager::new_simple(provider, tmp.path().to_path_buf(), bus.clone(), 4096);
        let origin = origin("cli", "direct");
        let result = manager
            .run_subagent_inner("task-1", task, label, &origin, None)
            .await;
        drop(manager);
        let bus = match Arc::try_unwrap(bus) {
            Ok(bus) => bus,
            Err(_) => panic!("manager should release bus Arc"),
        };
        let msg = bus
            .consume_inbound()
            .await
            .expect("announce should publish");
        (result, msg)
    }

    fn subagent_tool_names(disabled: &[&str]) -> Vec<String> {
        let tmp = TempDir::new().unwrap();
        let manager = SubagentManager::new_simple(
            ScriptedProvider::arc(vec![]),
            tmp.path().to_path_buf(),
            Arc::new(MessageBus::new()),
            4096,
        )
        .with_disabled_tools(disabled.iter().map(|name| name.to_string()).collect());
        manager.build_tool_registry().tool_names()
    }

    #[test]
    fn subagents_get_the_usual_tools_but_never_spawn_or_message() {
        let names = subagent_tool_names(&[]);
        for expected in ["read_file", "write_file", "edit_file", "shell"] {
            assert!(
                names.iter().any(|name| name == expected),
                "{expected} missing: {names:?}"
            );
        }
        for never in ["spawn", "message"] {
            assert!(
                !names.iter().any(|name| name == never),
                "{never} present: {names:?}"
            );
        }
    }

    #[test]
    fn disabled_tools_are_not_given_to_subagents() {
        let names = subagent_tool_names(&["write_file", "edit_file", "shell"]);
        for gone in ["write_file", "edit_file", "shell"] {
            assert!(
                !names.iter().any(|name| name == gone),
                "{gone} present: {names:?}"
            );
        }
        assert!(names.iter().any(|name| name == "read_file"));
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
            ScriptedProvider::arc(vec![empty.clone(), empty.clone(), empty.clone(), empty]),
            "empty reply task",
            "worker",
        )
        .await;

        assert!(result.is_ok());
        assert!(msg.content.contains("completed successfully"));
        assert!(msg.content.contains("couldn't produce a final answer"));
    }

    // ── spawn-task records, list(), format_subagents_list ────────────────────

    use super::{
        MAX_FINISHED_RECORDS, SubagentRecord, SubagentStatus, format_subagents_list,
        format_subagents_list_at, truncate_chars,
    };
    use chrono::{DateTime, Duration, Utc};

    fn test_manager(provider: Arc<dyn LLMProviderDyn>) -> (Arc<SubagentManager>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(SubagentManager::new_simple(
            provider,
            tmp.path().to_path_buf(),
            Arc::new(MessageBus::new()),
            4096,
        ));
        (manager, tmp)
    }

    fn record_at(
        task_id: &str,
        session_key: Option<&str>,
        status: SubagentStatus,
        spawned_at: DateTime<Utc>,
    ) -> SubagentRecord {
        SubagentRecord {
            task_id: task_id.to_string(),
            label: format!("label-{task_id}"),
            task_summary: format!("summary-{task_id}"),
            session_key: session_key.map(str::to_string),
            channel: "cli".to_string(),
            chat_id: "direct".to_string(),
            spawned_at,
            finished_at: None,
            status,
        }
    }

    /// Insert a record as `spawn()` does, then move it to `status` the way a
    /// finished run does.
    fn seed(manager: &SubagentManager, record: SubagentRecord) {
        let status = record.status;
        let task_id = record.task_id.clone();
        manager.insert_running_record(SubagentRecord {
            status: SubagentStatus::Running,
            ..record
        });
        if status != SubagentStatus::Running {
            manager.set_status(&task_id, status);
        }
    }

    #[test]
    fn empty_manager_lists_nothing() {
        let (manager, _tmp) = test_manager(TestProvider::arc());
        assert!(manager.list().is_empty());
        assert_eq!(format_subagents_list(&[], None), "No spawn tasks.");
    }

    #[tokio::test]
    async fn spawn_records_metadata_and_run_completes_the_record() {
        let (manager, _tmp) = test_manager(ScriptedProvider::arc(vec![llm_text("All done.")]));
        let task = "summarise the logs of the nightly batch run";
        let ack = Arc::clone(&manager).spawn(
            task,
            None,
            Some("telegram"),
            Some("chat-42"),
            Some("telegram:chat-42"),
        );

        let records = manager.list();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert!(ack.contains(&record.task_id));
        assert_eq!(record.label, "summarise the logs of the nigh...");
        assert_eq!(record.task_summary, task);
        assert_eq!(record.session_key.as_deref(), Some("telegram:chat-42"));
        assert_eq!(record.channel, "telegram");
        assert_eq!(record.chat_id, "chat-42");
        assert!(Utc::now() - record.spawned_at < Duration::seconds(30));

        // The run finishes on its own thread; the record is kept and completed.
        let mut finished = None;
        for _ in 0..200 {
            let current = manager.list().remove(0);
            if current.status != SubagentStatus::Running {
                finished = Some(current);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let finished = finished.expect("sub-agent run should finish");
        assert_eq!(finished.status, SubagentStatus::Completed);
        assert!(finished.finished_at.is_some());
    }

    #[test]
    fn explicit_label_is_kept() {
        let (manager, _tmp) = test_manager(ScriptedProvider::arc(vec![llm_text("ok")]));
        Arc::clone(&manager).spawn("task", Some("my label"), None, None, None);
        let record = manager.list().remove(0);
        assert_eq!(record.label, "my label");
        assert_eq!(record.session_key, None);
        assert_eq!(record.channel, "cli");
        assert_eq!(record.chat_id, "direct");
    }

    #[tokio::test]
    async fn run_outcomes_map_to_statuses() {
        let cases: Vec<(Vec<LLMResponse>, SubagentStatus)> = vec![
            (vec![llm_text("fine")], SubagentStatus::Completed),
            (vec![llm_read_missing_file()], SubagentStatus::Partial),
            (vec![llm_error("boom")], SubagentStatus::Failed),
        ];
        for (responses, expected) in cases {
            let (manager, _tmp) = test_manager(ScriptedProvider::arc(responses));
            seed(
                &manager,
                record_at("task-1", None, SubagentStatus::Running, Utc::now()),
            );
            let result = manager
                .run_subagent_inner("task-1", "task", "label", &origin("cli", "direct"), None)
                .await;
            assert!(result.is_ok());
            let record = manager.list().remove(0);
            assert_eq!(record.status, expected);
            assert!(record.finished_at.is_some());
        }
    }

    #[test]
    fn status_is_one_shot_and_unknown_ids_are_ignored() {
        let (manager, _tmp) = test_manager(TestProvider::arc());
        seed(
            &manager,
            record_at("task-1", None, SubagentStatus::Running, Utc::now()),
        );
        manager.set_status("task-1", SubagentStatus::Cancelled);
        let first_finish = manager.list()[0].finished_at;
        manager.set_status("task-1", SubagentStatus::Completed);
        manager.set_status("missing", SubagentStatus::Failed);

        let records = manager.list();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, SubagentStatus::Cancelled);
        assert_eq!(records[0].finished_at, first_finish);
    }

    #[tokio::test]
    async fn cancel_by_session_marks_running_tasks_cancelled_for_good() {
        let (manager, _tmp) = test_manager(TestProvider::arc());
        seed(
            &manager,
            record_at("task-1", Some("cli:a"), SubagentStatus::Running, Utc::now()),
        );
        let handle = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(100));
        });
        manager
            .running_tasks
            .lock()
            .unwrap()
            .insert("task-1".to_string(), handle);
        manager
            .session_tasks
            .lock()
            .unwrap()
            .entry("cli:a".to_string())
            .or_default()
            .insert("task-1".to_string());

        assert_eq!(manager.cancel_by_session("cli:a").await, 1);
        // The run still ends later and reports success: it must not overwrite.
        manager.set_status("task-1", SubagentStatus::Completed);
        assert_eq!(manager.list()[0].status, SubagentStatus::Cancelled);
    }

    #[test]
    fn list_puts_running_first_then_newest_first() {
        let (manager, _tmp) = test_manager(TestProvider::arc());
        let now = Utc::now();
        let minutes_ago = |minutes: i64| now - Duration::minutes(minutes);
        seed(
            &manager,
            record_at("old-done", None, SubagentStatus::Completed, minutes_ago(30)),
        );
        seed(
            &manager,
            record_at("old-run", None, SubagentStatus::Running, minutes_ago(20)),
        );
        seed(
            &manager,
            record_at("new-done", None, SubagentStatus::Failed, minutes_ago(5)),
        );
        seed(
            &manager,
            record_at("new-run", None, SubagentStatus::Running, minutes_ago(1)),
        );

        let ids: Vec<String> = manager.list().into_iter().map(|r| r.task_id).collect();
        assert_eq!(ids, ["new-run", "old-run", "new-done", "old-done"]);
    }

    #[test]
    fn retention_prunes_oldest_finished_but_never_running() {
        let (manager, _tmp) = test_manager(TestProvider::arc());
        let now = Utc::now();
        seed(
            &manager,
            record_at(
                "still-running",
                None,
                SubagentStatus::Running,
                now - Duration::days(9),
            ),
        );
        for n in 0..=MAX_FINISHED_RECORDS {
            seed(
                &manager,
                record_at(
                    &format!("done-{n:02}"),
                    None,
                    SubagentStatus::Completed,
                    now - Duration::minutes((MAX_FINISHED_RECORDS - n) as i64),
                ),
            );
        }
        // The prune runs on insert: one more spawn trims the surplus.
        seed(
            &manager,
            record_at("latest", None, SubagentStatus::Running, now),
        );

        let records = manager.list();
        let finished = records
            .iter()
            .filter(|r| r.status != SubagentStatus::Running)
            .count();
        assert_eq!(finished, MAX_FINISHED_RECORDS);
        assert!(records.iter().any(|r| r.task_id == "still-running"));
        assert!(records.iter().any(|r| r.task_id == "latest"));
        assert!(
            !records.iter().any(|r| r.task_id == "done-00"),
            "oldest finished is pruned"
        );
    }

    #[test]
    fn listing_covers_all_sessions_and_marks_the_current_one() {
        let (manager, _tmp) = test_manager(TestProvider::arc());
        let now = Utc::now();
        seed(
            &manager,
            record_at("mine", Some("cli:me"), SubagentStatus::Running, now),
        );
        seed(
            &manager,
            record_at(
                "theirs",
                Some("web:other"),
                SubagentStatus::Running,
                now - Duration::minutes(1),
            ),
        );
        seed(
            &manager,
            record_at(
                "nokey",
                None,
                SubagentStatus::Completed,
                now - Duration::minutes(2),
            ),
        );

        let text = format_subagents_list_at(&manager.list(), Some("cli:me"), now);
        let line_of = |id: &str| text.lines().find(|l| l.contains(id)).unwrap().to_string();
        assert!(line_of("mine").starts_with("* "));
        assert!(line_of("theirs").starts_with("  "));
        assert!(line_of("theirs").contains("web:other"));
        assert!(line_of("nokey").contains("session: —"));
        assert!(text.contains("(* = this session)"));
    }

    #[test]
    fn format_snapshot_shows_status_label_summary_and_times() {
        let now = Utc::now();
        let mut record = record_at(
            "abc12345",
            Some("cli:direct"),
            SubagentStatus::Completed,
            now - Duration::minutes(3),
        );
        record.finished_at = Some(now - Duration::minutes(1));
        let text = format_subagents_list_at(&[record], None, now);
        assert_eq!(
            text,
            "Spawn tasks (runs, this process):\n  abc12345 [COMPLETED] label-abc12345 — summary-abc12345 (session: cli:direct; spawned 3m ago, finished 1m ago)"
        );
    }

    #[test]
    fn multibyte_tasks_are_truncated_on_character_boundaries() {
        let task = "日本語のタスク".repeat(10);
        assert!(task.len() > 30);
        let label = truncate_chars(&task, 30);
        assert_eq!(label.chars().count(), 33);
        assert!(label.ends_with("..."));
        assert_eq!(truncate_chars("short", 30), "short");

        let (manager, _tmp) = test_manager(ScriptedProvider::arc(vec![llm_text("ok")]));
        Arc::clone(&manager).spawn(&task, None, None, None, None);
        let record = manager.list().remove(0);
        assert_eq!(record.label.chars().count(), 33);
        assert_eq!(record.task_summary, task);
    }

    #[test]
    fn resolves_runtime_per_session_key_instead_of_a_fixed_provider() {
        use crate::agent::model_runtime::ModelRuntimeResolver;
        use crate::config::schema::{Config, ModelPresetConfig};
        use crate::session::manager::SessionManager;

        let mut config = Config::default();
        config.providers.anthropic.api_key = "test-key".to_string();
        config.model_presets.insert(
            "fast".to_string(),
            ModelPresetConfig {
                model: "claude-haiku".to_string(),
                provider: "anthropic".to_string(),
                ..Default::default()
            },
        );
        let initial_provider = TestProvider::arc();
        let runtime_resolver = Arc::new(ModelRuntimeResolver::new(config, initial_provider));

        let tmp = TempDir::new().unwrap();
        let sessions = Arc::new(Mutex::new(SessionManager::with_default_eviction_threshold(
            tmp.path().to_path_buf(),
        )));
        {
            let mut manager = sessions.lock().unwrap();
            let session = manager.get_or_create_session("preset-session");
            session.metadata.insert(
                "model_preset".to_string(),
                serde_json::Value::String("fast".to_string()),
            );
            let snapshot = session.clone();
            manager.save(snapshot).unwrap();
        }

        let subagent_manager = SubagentManager::new(
            runtime_resolver,
            sessions,
            tmp.path().to_path_buf(),
            Arc::new(MessageBus::new()),
            4096,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let default_runtime = subagent_manager.runtime_resolver.current_default();
        let session_runtime = subagent_manager
            .runtime_resolver
            .resolve_for_session_key(&subagent_manager.sessions, "preset-session");
        let other_session_runtime = subagent_manager
            .runtime_resolver
            .resolve_for_session_key(&subagent_manager.sessions, "no-override-session");

        assert_eq!(session_runtime.model, "claude-haiku");
        assert_ne!(session_runtime.model, default_runtime.model);
        assert_eq!(other_session_runtime.model, default_runtime.model);
    }
}
