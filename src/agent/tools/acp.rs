//! The `acp_*` tools: create, update, run and list child agents.
//!
//! "Create" assembles a folder and a config overlay for a new rust-bot; it never
//! writes code. "Run" launches that rust-bot over ACP, sends it one prompt and
//! returns its reply, so the parent can answer the operator and, on the next
//! question, run the same agent again with its own memory.
//!
//! The tools exist only when `tools.acp.enabled` is on and this process is not
//! already at `tools.acp.maxDepth`. The LLM names an agent and a preset *key*;
//! it never supplies a command line.

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::agent::acp::agents_section::agents_section;
use crate::agent::acp::client::{ActivityEntry, ActivityStatus, ProgressSink, TurnEnd};
use crate::agent::acp::escalation::{EscalationWiring, escalator_for};
use crate::agent::acp::launch::RUSTBOT_PRESET;
use crate::agent::acp::manager::{
    AcpManager, AcpManagerSettings, ManagerTimings, RunError, RunRequest, RunResult,
};
use crate::agent::acp::permission::{PermissionResponder, PolicyResponder};
use crate::agent::acp::project_folder::{ProjectRequest, choose_project_folder};
use crate::agent::acp::store::{
    AgentChanges, AgentSummary, FileOverride, NewAgent, OverrideMode, Overrides, StoreError,
};
use crate::agent::tool_approval::ToolApprovalBroker;
use crate::agent::tool_progress::current_tool_progress;
use crate::agent::tools::base::Tool;
use crate::agent::tools::registry::ToolRegistry;
use crate::agent::workspace_context::current_tool_workspace;
use crate::bus::outbound_events::ProgressKind;
use crate::bus::queue::MessageBus;
use crate::config::child_env::current_depth;
use crate::config::loader::get_config_path;
use crate::config::overlay::active_overlay_chain;
use crate::config::schema::AcpConfig;

pub const CREATE_TOOL_NAME: &str = "acp_create_agent";
pub const UPDATE_TOOL_NAME: &str = "acp_update_agent";
pub const RUN_TOOL_NAME: &str = "acp_run_agent";
pub const LIST_TOOL_NAME: &str = "acp_list_agents";

/// Longest a single run may be asked to take.
const MAX_TIMEOUT_SECS: u64 = 3600;
/// How many of the child's tool calls a result lists.
const ACTIVITY_TAIL: usize = 12;

/// Where the current turn came from (set by the loop before each turn).
#[derive(Debug, Clone, Default)]
struct Route {
    channel: String,
    chat_id: String,
}

/// What the four tools share.
pub struct AcpToolsContext {
    manager: Arc<AcpManager>,
    acp: AcpConfig,
    parent_workspace: PathBuf,
    restrict_to_workspace: bool,
    route: StdMutex<Route>,
    escalation: StdMutex<Option<EscalationWiring>>,
}

impl AcpToolsContext {
    pub fn new(
        manager: Arc<AcpManager>,
        acp: AcpConfig,
        parent_workspace: PathBuf,
        restrict_to_workspace: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            manager,
            acp,
            parent_workspace,
            restrict_to_workspace,
            route: StdMutex::new(Route::default()),
            escalation: StdMutex::new(None),
        })
    }

    /// The context for this process, or `None` when the tools must not exist:
    /// ACP is disabled, or this process is as deep as `maxDepth` allows.
    pub fn for_process(
        acp: &AcpConfig,
        workspace: PathBuf,
        restrict_to_workspace: bool,
    ) -> Option<Arc<Self>> {
        let depth = current_depth();
        if !acp.enabled || depth >= acp.max_depth {
            return None;
        }
        let config_path =
            std::path::absolute(get_config_path()).unwrap_or_else(|_| get_config_path());
        let manager = Arc::new(AcpManager::new(AcpManagerSettings {
            parent_workspace: workspace.clone(),
            config_path,
            inherited_overlays: active_overlay_chain(),
            depth,
            executable_override: None,
            parent_env: std::env::vars().collect(),
            timings: ManagerTimings::default(),
        }));
        Some(Self::new(
            manager,
            acp.clone(),
            workspace,
            restrict_to_workspace,
        ))
    }

    /// Let `permissionPolicy: escalate` ask the person on the web-socket chat.
    pub fn set_escalation(&self, broker: Arc<ToolApprovalBroker>, bus: Arc<MessageBus>) {
        *self.escalation.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(EscalationWiring { broker, bus });
    }

    pub fn manager(&self) -> &Arc<AcpManager> {
        &self.manager
    }

    pub fn allows_dynamic_agents(&self) -> bool {
        self.acp.allow_dynamic_agents
    }

    /// The "Available ACP agents" prompt section for the agents that exist now.
    pub fn prompt_section(&self) -> Option<String> {
        agents_section(&self.manager.store().list())
    }

    fn set_route(&self, channel: &str, chat_id: &str) {
        *self.route.lock().unwrap_or_else(|e| e.into_inner()) = Route {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
        };
    }

    fn route(&self) -> Route {
        self.route.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The parent session this turn belongs to.
    fn session_key(&self) -> String {
        let route = self.route();
        if route.channel.is_empty() && route.chat_id.is_empty() {
            "cli:direct".to_string()
        } else {
            format!("{}:{}", route.channel, route.chat_id)
        }
    }

    /// The folder this turn works in, and whether it is confined to it.
    fn parent_project(&self) -> (PathBuf, bool) {
        let tool_workspace = current_tool_workspace(
            Some(self.parent_workspace.clone()),
            self.restrict_to_workspace,
            false,
        );
        (
            tool_workspace
                .project_path
                .unwrap_or_else(|| self.parent_workspace.clone()),
            tool_workspace.restrict_to_workspace,
        )
    }

    fn refuse_if_dynamic_agents_are_off(&self) -> Result<(), String> {
        if self.acp.allow_dynamic_agents {
            Ok(())
        } else {
            Err(
                "Error: this setup does not allow creating or changing agents \
                 (tools.acp.allowDynamicAgents is off); you can still run the existing ones"
                    .to_string(),
            )
        }
    }

    /// Check a default project folder given to create / update, the same way a
    /// run would check it. Returns the warnings to pass on.
    fn check_default_folder(
        &self,
        cwd: &str,
        child_shell_enabled: bool,
    ) -> Result<Vec<String>, String> {
        let (project, restricted) = self.parent_project();
        choose_project_folder(&ProjectRequest {
            requested: Some(cwd),
            agent_default: None,
            parent_project: &project,
            parent_restricted: restricted,
            parent_workspace: &self.parent_workspace,
            child_shell_enabled,
        })
        .map(|choice| choice.warnings)
        .map_err(|error| format!("Error: {error}"))
    }
}

// ── parameters ──────────────────────────────────────────────────────────────

fn string_param<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn required_string<'a>(params: &'a Value, name: &str) -> Result<&'a str, String> {
    string_param(params, name).ok_or_else(|| format!("Error: '{name}' is required"))
}

/// A bootstrap-file override: a plain string (appended to the parent's file) or
/// `{"text": ..., "mode": "append" | "replace"}`.
fn override_param(params: &Value, name: &str) -> Result<Option<FileOverride>, String> {
    let Some(value) = params.get(name).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match value {
        Value::String(text) if text.trim().is_empty() => Ok(None),
        Value::String(text) => Ok(Some(FileOverride {
            mode: OverrideMode::Append,
            text: text.clone(),
        })),
        Value::Object(_) => serde_json::from_value::<FileOverride>(value.clone())
            .map(Some)
            .map_err(|e| {
                format!("Error: '{name}' must be a string or {{\"text\": ..., \"mode\": \"append\"|\"replace\"}}: {e}")
            }),
        _ => Err(format!(
            "Error: '{name}' must be a string or {{\"text\": ..., \"mode\": \"append\"|\"replace\"}}"
        )),
    }
}

fn overrides_from(params: &Value) -> Result<Overrides, String> {
    Ok(Overrides {
        soul: override_param(params, "soul")?,
        agents: override_param(params, "agents")?,
        user: override_param(params, "user")?,
    })
}

fn overlay_param(params: &Value) -> Result<Option<Value>, String> {
    match params.get("overlay") {
        None | Some(Value::Null) => Ok(None),
        Some(overlay @ Value::Object(_)) => Ok(Some(overlay.clone())),
        Some(_) => Err("Error: 'overlay' must be a JSON object".to_string()),
    }
}

/// Parameter schema fragment shared by create and update.
fn override_schema(file: &str) -> Value {
    json!({
        "description": format!(
            "Change to {file} for this agent. A string is added after the parent's text; \
             {{\"text\", \"mode\": \"replace\"}} replaces it entirely."
        ),
        "oneOf": [
            {"type": "string"},
            {"type": "object", "properties": {
                "text": {"type": "string"},
                "mode": {"type": "string", "enum": ["append", "replace"]}
            }, "required": ["text"]}
        ]
    })
}

fn store_error(error: &StoreError) -> String {
    format!("Error: {error}")
}

// ── create ──────────────────────────────────────────────────────────────────

pub struct CreateAgentTool(Arc<AcpToolsContext>);

impl CreateAgentTool {
    pub fn new(context: Arc<AcpToolsContext>) -> Self {
        Self(context)
    }
}

#[async_trait]
impl Tool for CreateAgentTool {
    fn name(&self) -> String {
        CREATE_TOOL_NAME.to_string()
    }

    fn description(&self) -> String {
        "Create a new agent: another rust-bot with its own long-term memory, started on demand \
         and driven with acp_run_agent. It starts as a copy of you (your SOUL.md, AGENTS.md, \
         USER.md and TOOLS.md, your model and your settings) and you shape it with `agents` \
         (its instructions, e.g. \"You are a code review specialist...\"), `soul`, `user` and \
         `overlay`. The name must not exist yet; use acp_update_agent to change an agent."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Short unique name: lowercase letters, digits, '-' and '_' (e.g. code-review)."},
                "purpose": {"type": "string", "description": "One line on what the agent is for. Shown to you later so you route questions to it."},
                "preset": {"type": "string", "description": "How to launch it. Only \"rustbot\" (the default) is available."},
                "agents": override_schema("AGENTS.md (its operating instructions)"),
                "soul": override_schema("SOUL.md (its personality and values)"),
                "user": override_schema("USER.md (what it knows about the user)"),
                "overlay": {"type": "object", "description": "Settings that differ from yours, as a JSON merge patch. Allowed: agents.model, agents.modelPreset, agents.provider, and ways to give the agent LESS power: tools.exec.enable=false (no shell), tools.restrictToWorkspace=true, tools.disabledTools (e.g. [\"write_file\",\"edit_file\"] for a read-only reviewer), tools.mcpServers.<name>=null (drop an MCP server), tools.acp.* limits. Nothing else, and nothing that gives it more than you have."},
                "cwd": {"type": "string", "description": "Default project folder for runs that do not pass one: an absolute path to an existing folder."}
            },
            "required": ["name", "purpose"]
        })
    }

    fn set_tool_context(&self, channel: &str, chat_id: &str, _message_id: Option<&str>) {
        self.0.set_route(channel, chat_id);
    }

    async fn execute(&self, params: &Value) -> String {
        match self.create(params) {
            Ok(message) | Err(message) => message,
        }
    }
}

impl CreateAgentTool {
    fn create(&self, params: &Value) -> Result<String, String> {
        let context = &self.0;
        context.refuse_if_dynamic_agents_are_off()?;
        let name = required_string(params, "name")?;
        let purpose = required_string(params, "purpose")?;
        let preset = string_param(params, "preset").unwrap_or(RUSTBOT_PRESET);
        if preset != RUSTBOT_PRESET {
            return Err(format!(
                "Error: preset '{preset}' is not available; the only preset is '{RUSTBOT_PRESET}'"
            ));
        }
        let overlay = overlay_param(params)?.unwrap_or_else(|| json!({}));
        let overrides = overrides_from(params)?;
        let parent_config = context
            .manager
            .load_parent_config()
            .map_err(|e| format!("Error: cannot read the config: {e}"))?;

        let cwd = string_param(params, "cwd").map(str::to_string);
        let mut warnings = Vec::new();
        if let Some(cwd) = &cwd {
            // Would the agent have a shell? Judge it from the config it will run on.
            let child_config = crate::config::overlay::effective_config(&parent_config, &overlay)
                .map_err(|e| format!("Error: {e}"))?;
            let shell = child_config.tools.exec.enable
                && !child_config
                    .tools
                    .disabled_tools
                    .iter()
                    .any(|tool| tool == "shell");
            warnings = context.check_default_folder(cwd, shell)?;
        }

        let depth = current_depth() + 1;
        let meta = context
            .manager
            .store()
            .create(
                NewAgent {
                    name: name.to_string(),
                    purpose: purpose.to_string(),
                    preset: preset.to_string(),
                    overlay,
                    overrides,
                    cwd,
                    created_by: context.session_key(),
                    depth,
                },
                &parent_config,
            )
            .map_err(|e| store_error(&e))?;

        let mut message = format!(
            "Created agent '{}' ({}). It has its own memory and starts as a copy of your \
             instructions. Run it with acp_run_agent {{name: \"{}\", prompt, cwd}}.",
            meta.name, meta.purpose, meta.name
        );
        for warning in warnings {
            message.push_str(&format!("\nNote: {warning}"));
        }
        Ok(message)
    }
}

// ── update ──────────────────────────────────────────────────────────────────

pub struct UpdateAgentTool(Arc<AcpToolsContext>);

impl UpdateAgentTool {
    pub fn new(context: Arc<AcpToolsContext>) -> Self {
        Self(context)
    }
}

#[async_trait]
impl Tool for UpdateAgentTool {
    fn name(&self) -> String {
        UPDATE_TOOL_NAME.to_string()
    }

    fn description(&self) -> String {
        "Change an existing agent: its purpose, default folder, settings overlay, or its \
         SOUL.md / AGENTS.md / USER.md. Setting `agents`, `soul` or `user` re-copies that \
         file from you and applies the new text, so anything the agent itself learned and \
         wrote into that file is replaced. `resyncFromParent` re-copies files from you \
         (for example USER.md after you learned more about the user). The agent's \
         memory and conversations are never touched."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The agent to change."},
                "purpose": {"type": "string"},
                "agents": override_schema("AGENTS.md (its operating instructions)"),
                "soul": override_schema("SOUL.md (its personality and values)"),
                "user": override_schema("USER.md (what it knows about the user)"),
                "overlay": {"type": "object", "description": "REPLACES the agent's whole settings overlay (same rules as acp_create_agent)."},
                "cwd": {"type": "string", "description": "New default project folder; an empty string clears it."},
                "resyncFromParent": {"type": "array", "items": {"type": "string", "enum": ["SOUL.md", "AGENTS.md", "USER.md", "TOOLS.md"]}, "description": "Files to re-copy from you (the agent's stored overrides are applied again)."}
            },
            "required": ["name"]
        })
    }

    fn set_tool_context(&self, channel: &str, chat_id: &str, _message_id: Option<&str>) {
        self.0.set_route(channel, chat_id);
    }

    async fn execute(&self, params: &Value) -> String {
        match self.update(params) {
            Ok(message) | Err(message) => message,
        }
    }
}

impl UpdateAgentTool {
    fn update(&self, params: &Value) -> Result<String, String> {
        let context = &self.0;
        context.refuse_if_dynamic_agents_are_off()?;
        let name = required_string(params, "name")?;
        let parent_config = context
            .manager
            .load_parent_config()
            .map_err(|e| format!("Error: cannot read the config: {e}"))?;

        let resync_from_parent = match params.get("resyncFromParent") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        "Error: 'resyncFromParent' must be a list of file names".to_string()
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err("Error: 'resyncFromParent' must be a list of file names".to_string());
            }
        };
        let overlay = overlay_param(params)?;

        let mut warnings = Vec::new();
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(cwd) = cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
            // The shell question depends on the overlay the agent will have after this update.
            let store = context.manager.store();
            let effective_overlay = match &overlay {
                Some(overlay) => overlay.clone(),
                None => store.read_overlay(name).map_err(|e| store_error(&e))?,
            };
            let child_config =
                crate::config::overlay::effective_config(&parent_config, &effective_overlay)
                    .map_err(|e| format!("Error: {e}"))?;
            let shell = child_config.tools.exec.enable
                && !child_config
                    .tools
                    .disabled_tools
                    .iter()
                    .any(|tool| tool == "shell");
            warnings = context.check_default_folder(cwd, shell)?;
        }

        let report = context
            .manager
            .store()
            .update(
                name,
                AgentChanges {
                    purpose: string_param(params, "purpose").map(str::to_string),
                    overlay,
                    overrides: overrides_from(params)?,
                    cwd,
                    resync_from_parent,
                },
                &parent_config,
            )
            .map_err(|e| store_error(&e))?;

        let mut message = format!("Updated agent '{name}'.");
        if !report.resynced_files.is_empty() {
            message.push_str(&format!(
                " Re-copied from you: {}. Anything the agent itself had written into those \
                 files is replaced.",
                report.resynced_files.join(", ")
            ));
        }
        for warning in warnings {
            message.push_str(&format!("\nNote: {warning}"));
        }
        Ok(message)
    }
}

// ── list ────────────────────────────────────────────────────────────────────

pub struct ListAgentsTool(Arc<AcpToolsContext>);

impl ListAgentsTool {
    pub fn new(context: Arc<AcpToolsContext>) -> Self {
        Self(context)
    }
}

/// One agent as the model reads it.
fn describe_agent(summary: &AgentSummary) -> String {
    let meta = &summary.meta;
    let mut line = format!("- {}: {}", meta.name, meta.purpose);
    line.push_str(&format!(" [preset {}, depth {}", meta.preset, meta.depth));
    if let Some(cwd) = &meta.cwd {
        line.push_str(&format!(", default folder {cwd}"));
    }
    line.push(']');
    if !summary.drifted_files.is_empty() {
        line.push_str(&format!(
            "\n  Your {} changed after this agent's copy was made; refresh with \
             acp_update_agent {{name: \"{}\", resyncFromParent: [...]}} if it matters.",
            summary.drifted_files.join(", "),
            meta.name
        ));
    }
    line
}

#[async_trait]
impl Tool for ListAgentsTool {
    fn name(&self) -> String {
        LIST_TOOL_NAME.to_string()
    }

    fn description(&self) -> String {
        "List the agents that exist, with what each is for and whether its copy of your \
         instruction files is out of date."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _params: &Value) -> String {
        let agents = self.0.manager.store().list();
        if agents.is_empty() {
            return "No agents exist yet. Create one with acp_create_agent.".to_string();
        }
        let lines: Vec<String> = agents.iter().map(describe_agent).collect();
        format!("Agents ({}):\n{}", agents.len(), lines.join("\n"))
    }
}

// ── run ─────────────────────────────────────────────────────────────────────

pub struct RunAgentTool(Arc<AcpToolsContext>);

impl RunAgentTool {
    pub fn new(context: Arc<AcpToolsContext>) -> Self {
        Self(context)
    }
}

/// Forwards the child's activity to the turn's progress callback, in order and
/// without blocking the connection that produces it.
struct ForwardingProgress {
    agent: String,
    lines: mpsc::UnboundedSender<String>,
}

impl ForwardingProgress {
    /// A sink for `agent`, when this turn has somewhere to show progress.
    fn for_current_turn(agent: &str) -> Option<Arc<dyn ProgressSink>> {
        let callback = current_tool_progress()?;
        let (lines, mut received) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = received.recv().await {
                callback(line, ProgressKind::ToolHint).await;
            }
        });
        Some(Arc::new(Self {
            agent: agent.to_string(),
            lines,
        }))
    }
}

impl ProgressSink for ForwardingProgress {
    fn tool_started(&self, title: &str) {
        let _ = self.lines.send(format!("{} › {title}", self.agent));
    }

    fn still_working(&self, elapsed: Duration) {
        let _ = self.lines.send(format!(
            "{} › still working ({})",
            self.agent,
            format_elapsed(elapsed)
        ));
    }
}

/// `2m 10s`, `45s`.
fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn activity_line(entry: &ActivityEntry) -> String {
    let status = match entry.status {
        ActivityStatus::Running => "did not finish",
        ActivityStatus::Completed => "ok",
        ActivityStatus::Failed => "failed",
    };
    format!("- {} ({status})", entry.title)
}

/// A run's outcome as the model reads it.
fn render_run(result: &RunResult, timeout: Duration) -> String {
    let reply = result.reply.trim();
    let mut text = match result.end {
        TurnEnd::Finished => {
            if reply.is_empty() {
                format!("Agent '{}' finished without a text reply.", result.agent)
            } else {
                format!("Agent '{}' replied:\n{reply}", result.agent)
            }
        }
        other => {
            let why = match other {
                TurnEnd::Cancelled => "was cancelled".to_string(),
                TurnEnd::MaxTokens => "ran out of tokens".to_string(),
                TurnEnd::MaxTurnRequests => "reached its limit of model calls".to_string(),
                TurnEnd::Refusal => "refused to continue".to_string(),
                TurnEnd::TimedOut { .. } => {
                    format!(
                        "did not finish within {} and was asked to stop",
                        format_elapsed(timeout)
                    )
                }
                TurnEnd::Finished => unreachable!("handled above"),
            };
            let mut text = format!("Agent '{}' {why}.", result.agent);
            if !reply.is_empty() {
                text.push_str(&format!(" What it had said so far:\n{reply}"));
            }
            text
        }
    };
    if !result.activity.is_empty() {
        let start = result.activity.len().saturating_sub(ACTIVITY_TAIL);
        let lines: Vec<String> = result.activity[start..].iter().map(activity_line).collect();
        text.push_str(&format!("\n\nWhat the agent did:\n{}", lines.join("\n")));
    }
    for warning in &result.warnings {
        text.push_str(&format!("\n\nNote: {warning}"));
    }
    text
}

#[async_trait]
impl Tool for RunAgentTool {
    fn name(&self) -> String {
        RUN_TOOL_NAME.to_string()
    }

    fn description(&self) -> String {
        "Send a prompt to an agent and get its reply. The agent remembers earlier \
         conversations with you and keeps its own long-term memory, so use the same agent \
         for follow-ups on the same subject. It works in `cwd` (an absolute project folder; \
         for example the repository to review) and may ask you for permission, which your \
         settings answer. This call waits until the agent finishes."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The agent to run (see acp_list_agents)."},
                "prompt": {"type": "string", "description": "What to ask or tell the agent. Include what it needs: it does not see your conversation."},
                "cwd": {"type": "string", "description": "Absolute path of the project folder the agent works in. Optional when the agent has a default folder."},
                "timeoutSecs": {"type": "integer", "description": "How long to wait for the reply; the agent is stopped after that."}
            },
            "required": ["name", "prompt"]
        })
    }

    fn set_tool_context(&self, channel: &str, chat_id: &str, _message_id: Option<&str>) {
        self.0.set_route(channel, chat_id);
    }

    async fn execute(&self, params: &Value) -> String {
        match self.run(params).await {
            Ok(message) | Err(message) => message,
        }
    }
}

impl RunAgentTool {
    async fn run(&self, params: &Value) -> Result<String, String> {
        let context = &self.0;
        let agent = required_string(params, "name")?;
        let prompt = required_string(params, "prompt")?;
        let timeout_secs = match params.get("timeoutSecs") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|secs| (1..=MAX_TIMEOUT_SECS).contains(secs))
                    .ok_or_else(|| {
                        format!("Error: 'timeoutSecs' must be a whole number from 1 to {MAX_TIMEOUT_SECS}")
                    })?,
            ),
        };
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(context.acp.default_timeout_secs));

        let route = context.route();
        let escalation = context
            .escalation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let responder: Arc<dyn PermissionResponder> = Arc::new(PolicyResponder::new(
            context.acp.permission_policy,
            escalator_for(escalation.as_ref(), &route.channel, &route.chat_id),
        ));
        let (parent_project, parent_restricted) = context.parent_project();

        let request = RunRequest {
            agent: agent.to_string(),
            prompt: prompt.to_string(),
            cwd: string_param(params, "cwd").map(str::to_string),
            parent_session_key: context.session_key(),
            parent_project,
            parent_restricted,
            acp: context.acp.clone(),
            timeout: Some(timeout),
            responder,
            progress: ForwardingProgress::for_current_turn(agent),
        };
        // Dropping this future (the operator stopped the turn) stops the child.
        let result = context
            .manager
            .run(request)
            .await
            .map_err(|error: RunError| format!("Error: {error}"))?;
        Ok(render_run(&result, timeout))
    }
}

// ── registration ────────────────────────────────────────────────────────────

/// Register the `acp_*` tools for `context`: create and update only when the
/// setup allows dynamic agents.
pub fn register_acp_tools(tools: &mut ToolRegistry, context: &Arc<AcpToolsContext>) {
    if context.allows_dynamic_agents() {
        tools.register(Box::new(CreateAgentTool::new(Arc::clone(context))));
        tools.register(Box::new(UpdateAgentTool::new(Arc::clone(context))));
    }
    tools.register(Box::new(RunAgentTool::new(Arc::clone(context))));
    tools.register(Box::new(ListAgentsTool::new(Arc::clone(context))));
}

/// Names of the four tools, for gating and documentation.
pub fn acp_tool_names() -> [&'static str; 4] {
    [
        CREATE_TOOL_NAME,
        UPDATE_TOOL_NAME,
        RUN_TOOL_NAME,
        LIST_TOOL_NAME,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn context(allow_dynamic_agents: bool) -> (tempfile::TempDir, Arc<AcpToolsContext>) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        for file in ["SOUL.md", "AGENTS.md", "USER.md", "TOOLS.md"] {
            std::fs::write(workspace.join(file), format!("parent {file}")).unwrap();
        }
        let config_path = dir.path().join("config.json");
        std::fs::write(&config_path, "{}").unwrap();
        let manager = Arc::new(AcpManager::new(AcpManagerSettings {
            parent_workspace: workspace.clone(),
            config_path,
            inherited_overlays: Vec::new(),
            depth: 0,
            executable_override: None,
            parent_env: HashMap::new(),
            timings: ManagerTimings::default(),
        }));
        let acp = AcpConfig {
            enabled: true,
            allow_dynamic_agents,
            ..AcpConfig::default()
        };
        (dir, AcpToolsContext::new(manager, acp, workspace, false))
    }

    fn create_params(name: &str) -> Value {
        json!({"name": name, "purpose": "reviews code"})
    }

    // ── create ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_makes_the_agent_and_says_how_to_run_it() {
        let (_dir, context) = context(true);
        let tool = CreateAgentTool::new(Arc::clone(&context));

        let result = tool
            .execute(&json!({
                "name": "code-review",
                "purpose": "reviews code",
                "agents": "You are a code review specialist.",
                "soul": {"text": "A terse reviewer.", "mode": "replace"},
                "overlay": {"tools": {"exec": {"enable": false}, "disabledTools": ["write_file", "edit_file"]}}
            }))
            .await;

        assert!(result.contains("Created agent 'code-review'"), "{result}");
        assert!(result.contains("acp_run_agent"), "{result}");
        let store = context.manager.store();
        let home = store.agent_dir("code-review");
        assert_eq!(
            std::fs::read_to_string(home.join("AGENTS.md")).unwrap(),
            "parent AGENTS.md\n\nYou are a code review specialist."
        );
        assert_eq!(
            std::fs::read_to_string(home.join("SOUL.md")).unwrap(),
            "A terse reviewer."
        );
        assert_eq!(store.get("code-review").unwrap().created_by, "cli:direct");
        assert_eq!(
            store.read_overlay("code-review").unwrap()["tools"]["disabledTools"],
            json!(["write_file", "edit_file"])
        );
    }

    #[tokio::test]
    async fn create_reports_what_is_wrong_in_words_the_model_can_act_on() {
        let (_dir, context) = context(true);
        let tool = CreateAgentTool::new(Arc::clone(&context));
        tool.execute(&create_params("taken")).await;

        for (params, expected) in [
            (json!({"purpose": "x"}), "'name' is required"),
            (json!({"name": "x"}), "'purpose' is required"),
            (
                json!({"name": "Bad Name", "purpose": "x"}),
                "invalid agent name",
            ),
            (json!({"name": "taken", "purpose": "x"}), "already exists"),
            (
                json!({"name": "other", "purpose": "x", "preset": "gemini"}),
                "preset 'gemini' is not available",
            ),
            (
                json!({"name": "other", "purpose": "x", "overlay": {"providers": {"openai": {"apiBase": "https://evil.example"}}}}),
                "providers.openai.apiBase",
            ),
            (
                json!({"name": "other", "purpose": "x", "overlay": "no"}),
                "'overlay' must be a JSON object",
            ),
            (
                json!({"name": "other", "purpose": "x", "agents": 5}),
                "'agents' must be a string",
            ),
            (
                json!({"name": "other", "purpose": "x", "cwd": "relative/path"}),
                "absolute",
            ),
        ] {
            let result = tool.execute(&params).await;
            assert!(result.starts_with("Error:"), "{params}: {result}");
            assert!(result.contains(expected), "{params}: {result}");
        }
        assert!(
            !context.manager.store().agent_dir("other").exists(),
            "a refused create leaves nothing behind"
        );
    }

    #[tokio::test]
    async fn create_accepts_a_default_folder_and_warns_about_a_contained_home() {
        let (dir, context) = context(true);
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("memory")).unwrap();
        std::fs::create_dir_all(project.join(".rust-bot").join("ws").join("memory")).unwrap();
        std::fs::create_dir_all(project.join(".rust-bot").join("ws").join("sessions")).unwrap();
        std::fs::write(project.join(".rust-bot").join("ws").join("SOUL.md"), "x").unwrap();
        let tool = CreateAgentTool::new(Arc::clone(&context));

        let result = tool
            .execute(&json!({"name": "reviewer", "purpose": "x", "cwd": project.to_string_lossy()}))
            .await;

        assert!(result.contains("Created agent"), "{result}");
        assert!(
            result.contains("Note:") && result.contains("shell"),
            "{result}"
        );
        assert_eq!(
            context
                .manager
                .store()
                .get("reviewer")
                .unwrap()
                .cwd
                .as_deref(),
            Some(project.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn the_parents_own_home_is_refused_as_a_default_folder() {
        let (_dir, context) = context(true);
        let tool = CreateAgentTool::new(Arc::clone(&context));
        let home = context.parent_workspace.to_string_lossy().into_owned();

        let result = tool
            .execute(&json!({"name": "reviewer", "purpose": "x", "cwd": home}))
            .await;

        assert!(
            result.starts_with("Error:") && result.contains("rust-bot home"),
            "{result}"
        );
    }

    // ── update ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn update_changes_the_agent_and_says_what_was_replaced() {
        let (_dir, context) = context(true);
        CreateAgentTool::new(Arc::clone(&context))
            .execute(&create_params("reviewer"))
            .await;
        std::fs::write(
            context.parent_workspace.join("USER.md"),
            "parent learned more",
        )
        .unwrap();
        let tool = UpdateAgentTool::new(Arc::clone(&context));

        let result = tool
            .execute(&json!({
                "name": "reviewer",
                "purpose": "reviews Rust",
                "resyncFromParent": ["USER.md"],
                "agents": {"text": "Focus on security.", "mode": "append"}
            }))
            .await;

        assert!(result.contains("Updated agent 'reviewer'"), "{result}");
        assert!(
            result.contains("USER.md") && result.contains("replaced"),
            "{result}"
        );
        let store = context.manager.store();
        assert_eq!(store.get("reviewer").unwrap().purpose, "reviews Rust");
        assert_eq!(
            std::fs::read_to_string(store.agent_dir("reviewer").join("USER.md")).unwrap(),
            "parent learned more"
        );
    }

    #[tokio::test]
    async fn update_reports_errors_in_words() {
        let (_dir, context) = context(true);
        let tool = UpdateAgentTool::new(Arc::clone(&context));
        for (params, expected) in [
            (json!({}), "'name' is required"),
            (json!({"name": "ghost"}), "does not exist"),
            (
                json!({"name": "ghost", "resyncFromParent": "USER.md"}),
                "list of file names",
            ),
        ] {
            let result = tool.execute(&params).await;
            assert!(
                result.starts_with("Error:") && result.contains(expected),
                "{params}: {result}"
            );
        }
        CreateAgentTool::new(Arc::clone(&context))
            .execute(&create_params("reviewer"))
            .await;
        let result = tool
            .execute(&json!({"name": "reviewer", "resyncFromParent": ["memory/MEMORY.md"]}))
            .await;
        assert!(
            result.starts_with("Error:") && result.contains("cannot be resynced"),
            "{result}"
        );
        let result = tool
            .execute(&json!({"name": "reviewer", "overlay": {"channels": {}}}))
            .await;
        assert!(
            result.starts_with("Error:") && result.contains("channels"),
            "{result}"
        );
    }

    // ── dynamic agents switched off ─────────────────────────────────────────

    #[tokio::test]
    async fn create_and_update_refuse_when_dynamic_agents_are_off() {
        let (_dir, context) = context(false);
        let created = CreateAgentTool::new(Arc::clone(&context))
            .execute(&create_params("reviewer"))
            .await;
        let updated = UpdateAgentTool::new(Arc::clone(&context))
            .execute(&json!({"name": "reviewer", "purpose": "x"}))
            .await;
        for result in [created, updated] {
            assert!(
                result.starts_with("Error:") && result.contains("allowDynamicAgents"),
                "{result}"
            );
        }
        assert!(context.manager.store().list().is_empty());
    }

    #[test]
    fn registration_follows_allow_dynamic_agents() {
        let names_for = |allow: bool| {
            let (_dir, context) = context(allow);
            let mut tools = ToolRegistry::new();
            register_acp_tools(&mut tools, &context);
            let mut names = tools.tool_names();
            names.sort();
            names
        };
        assert_eq!(
            names_for(true),
            vec![
                "acp_create_agent",
                "acp_list_agents",
                "acp_run_agent",
                "acp_update_agent"
            ]
        );
        assert_eq!(names_for(false), vec!["acp_list_agents", "acp_run_agent"]);
    }

    // ── list ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_says_so_when_there_are_no_agents() {
        let (_dir, context) = context(true);
        let result = ListAgentsTool::new(context).execute(&json!({})).await;
        assert!(
            result.contains("No agents exist yet") && result.contains("acp_create_agent"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn list_shows_purpose_and_drift() {
        let (_dir, context) = context(true);
        let create = CreateAgentTool::new(Arc::clone(&context));
        create.execute(&create_params("alpha")).await;
        create.execute(&create_params("beta")).await;
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(context.parent_workspace.join("USER.md"), "changed").unwrap();

        let result = ListAgentsTool::new(Arc::clone(&context))
            .execute(&json!({}))
            .await;

        assert!(result.starts_with("Agents (2):"), "{result}");
        assert!(
            result.contains("- alpha: reviews code [preset rustbot, depth 1]"),
            "{result}"
        );
        assert!(result.contains("USER.md changed after"), "{result}");
        assert!(result.contains("resyncFromParent"), "{result}");
    }

    // ── run (the failures that need no child process) ───────────────────────

    #[tokio::test]
    async fn run_validates_its_parameters() {
        let (_dir, context) = context(true);
        let tool = RunAgentTool::new(context);
        for (params, expected) in [
            (json!({"prompt": "x"}), "'name' is required"),
            (json!({"name": "x"}), "'prompt' is required"),
            (
                json!({"name": "x", "prompt": "y", "timeoutSecs": 0}),
                "'timeoutSecs'",
            ),
            (
                json!({"name": "x", "prompt": "y", "timeoutSecs": 99999}),
                "'timeoutSecs'",
            ),
            (
                json!({"name": "x", "prompt": "y", "timeoutSecs": "soon"}),
                "'timeoutSecs'",
            ),
        ] {
            let result = tool.execute(&params).await;
            assert!(
                result.starts_with("Error:") && result.contains(expected),
                "{params}: {result}"
            );
        }
    }

    #[tokio::test]
    async fn run_on_an_unknown_agent_lists_the_known_ones() {
        let (_dir, context) = context(true);
        CreateAgentTool::new(Arc::clone(&context))
            .execute(&create_params("code-review"))
            .await;

        let result = RunAgentTool::new(context)
            .execute(&json!({"name": "code-reviw", "prompt": "hi"}))
            .await;

        assert!(result.starts_with("Error:"), "{result}");
        assert!(result.contains("code-review"), "{result}");
    }

    // ── rendering ───────────────────────────────────────────────────────────

    fn finished_run(end: TurnEnd, reply: &str) -> RunResult {
        RunResult {
            agent: "code-review".to_string(),
            reply: reply.to_string(),
            activity: vec![
                ActivityEntry {
                    title: "read src/main.rs".to_string(),
                    status: ActivityStatus::Completed,
                },
                ActivityEntry {
                    title: "shell cargo test".to_string(),
                    status: ActivityStatus::Failed,
                },
            ],
            end,
            session_id: "s1".to_string(),
            resumed: false,
            cwd: PathBuf::from("/repo"),
            warnings: vec!["a home is inside the folder".to_string()],
            exit: tokio::runtime::Handle::current().spawn(async {
                crate::agent::acp::transport::ExitReport {
                    status: None,
                    killed: false,
                    stderr_tail: String::new(),
                }
            }),
        }
    }

    #[tokio::test]
    async fn a_finished_run_shows_the_reply_the_activity_and_the_warnings() {
        let text = render_run(
            &finished_run(TurnEnd::Finished, "Looks fine."),
            Duration::from_secs(60),
        );
        assert!(
            text.starts_with("Agent 'code-review' replied:\nLooks fine."),
            "{text}"
        );
        assert!(text.contains("- read src/main.rs (ok)"), "{text}");
        assert!(text.contains("- shell cargo test (failed)"), "{text}");
        assert!(text.contains("Note: a home is inside the folder"), "{text}");
    }

    #[tokio::test]
    async fn an_empty_reply_is_said_so() {
        let text = render_run(
            &finished_run(TurnEnd::Finished, "  "),
            Duration::from_secs(60),
        );
        assert!(text.contains("finished without a text reply"), "{text}");
    }

    #[tokio::test]
    async fn runs_that_did_not_finish_say_why_and_keep_the_partial_reply() {
        for (end, expected) in [
            (
                TurnEnd::TimedOut {
                    cancel_confirmed: true,
                },
                "did not finish within 2m 05s",
            ),
            (TurnEnd::Cancelled, "was cancelled"),
            (TurnEnd::MaxTokens, "ran out of tokens"),
            (TurnEnd::MaxTurnRequests, "limit of model calls"),
            (TurnEnd::Refusal, "refused to continue"),
        ] {
            let text = render_run(&finished_run(end, "so far"), Duration::from_secs(125));
            assert!(text.contains(expected), "{end:?}: {text}");
            assert!(
                text.contains("What it had said so far:\nso far"),
                "{end:?}: {text}"
            );
        }
    }

    #[tokio::test]
    async fn only_the_most_recent_activity_is_listed() {
        let mut run = finished_run(TurnEnd::Finished, "ok");
        run.activity = (0..30)
            .map(|n| ActivityEntry {
                title: format!("step {n}"),
                status: ActivityStatus::Completed,
            })
            .collect();
        let text = render_run(&run, Duration::from_secs(60));
        assert!(
            text.contains("step 29") && text.contains("step 18"),
            "{text}"
        );
        assert!(!text.contains("step 17"), "{text}");
    }

    #[test]
    fn elapsed_time_reads_naturally() {
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(format_elapsed(Duration::from_secs(130)), "2m 10s");
        assert_eq!(format_elapsed(Duration::from_secs(600)), "10m 00s");
    }

    // ── parameters ──────────────────────────────────────────────────────────

    #[test]
    fn overrides_accept_a_string_or_an_object() {
        let params =
            json!({"agents": "extra", "soul": {"text": "only", "mode": "replace"}, "user": ""});
        let overrides = overrides_from(&params).unwrap();
        assert_eq!(
            overrides.agents,
            Some(FileOverride {
                mode: OverrideMode::Append,
                text: "extra".into()
            })
        );
        assert_eq!(
            overrides.soul,
            Some(FileOverride {
                mode: OverrideMode::Replace,
                text: "only".into()
            })
        );
        assert_eq!(overrides.user, None, "an empty string sets nothing");
        // An object without a mode appends.
        let appended = override_param(&json!({"soul": {"text": "x"}}), "soul")
            .unwrap()
            .unwrap();
        assert_eq!(appended.mode, OverrideMode::Append);
        // A bad mode is an error naming the field.
        let error = override_param(&json!({"soul": {"text": "x", "mode": "sideways"}}), "soul")
            .unwrap_err();
        assert!(error.contains("'soul'"), "{error}");
    }

    #[test]
    fn the_parent_session_key_comes_from_the_turns_route() {
        let (_dir, context) = context(true);
        assert_eq!(context.session_key(), "cli:direct");
        context.set_route("websocket", "chat-9");
        assert_eq!(context.session_key(), "websocket:chat-9");
    }

    #[test]
    fn the_prompt_section_lists_existing_agents_only() {
        let (_dir, context) = context(true);
        assert!(context.prompt_section().is_none());
        context
            .manager
            .store()
            .create(
                NewAgent {
                    name: "docs".to_string(),
                    purpose: "writes documentation".to_string(),
                    preset: "rustbot".to_string(),
                    overlay: json!({}),
                    overrides: Overrides::default(),
                    cwd: None,
                    created_by: "cli:direct".to_string(),
                    depth: 1,
                },
                &crate::config::schema::Config::default(),
            )
            .unwrap();
        let section = context.prompt_section().unwrap();
        assert!(
            section.contains("- docs: writes documentation"),
            "{section}"
        );
    }

    #[test]
    fn the_context_exists_only_when_enabled_and_not_too_deep() {
        let mut acp = AcpConfig::default();
        let workspace = PathBuf::from("unused");
        // Disabled: no tools.
        assert!(AcpToolsContext::for_process(&acp, workspace.clone(), false).is_none());
        // Enabled in a root process (depth 0 here): tools exist.
        acp.enabled = true;
        assert!(AcpToolsContext::for_process(&acp, workspace.clone(), false).is_some());
        // Depth limit 0 means no children at all.
        acp.max_depth = 0;
        assert!(AcpToolsContext::for_process(&acp, workspace, false).is_none());
    }
}
