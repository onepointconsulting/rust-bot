use crate::{
    PKG_VERSION,
    agent::context::BOOTSTRAP_FILES,
    bus::events::OutboundMessage,
    command::{CommandContext, CommandHandler, CommandRouter},
    utils::{
        cli::convert_text_to_markdown,
        gitstore::{CommitInfo, GitStore},
        helpers::build_status_content,
        restart::restart_with_notice,
        searchusage::fetch_search_usage,
    },
};
use async_trait::async_trait;
use futures::FutureExt;
use std::{fs, io, panic::AssertUnwindSafe, path::{Path, PathBuf}, sync::Arc, time::Instant};

/// Build an outbound reply addressed back to the inbound message's channel/chat.
fn reply(ctx: &CommandContext, content: impl Into<String>) -> OutboundMessage {
    OutboundMessage {
        channel: ctx.msg.channel.clone(),
        chat_id: ctx.msg.chat_id.clone(),
        content: content.into(),
        reply_to: None,
        media: vec![],
        metadata: ctx.msg.metadata.clone(),
    }
}

fn reply_as_text(ctx: &CommandContext, content: impl Into<String>) -> OutboundMessage {
    let mut metadata = ctx.msg.metadata.clone();
    metadata.insert("render_as".to_string(), "text".into());
    OutboundMessage {
        channel: ctx.msg.channel.clone(),
        chat_id: ctx.msg.chat_id.clone(),
        content: content.into(),
        reply_to: None,
        media: vec![],
        metadata,
    }
}

fn reply_no_loop(ctx: &CommandContext, command: &str) -> OutboundMessage {
    reply(
        ctx,
        format!("No agent available to execute command: {command}."),
    )
}

fn dream_git_uninitialized_message(last_dream_cursor: u64) -> &'static str {
    if last_dream_cursor == 0 {
        "Dream has not run yet. Run `/dream`, or wait for the next scheduled Dream cycle."
    } else {
        "Dream history is not available because memory versioning is not initialized."
    }
}

struct CmdStop;

/// Cancel all active tasks and subagents for the session.
#[async_trait]
impl CommandHandler for CmdStop {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/stop");
        };
        let agent_loop = Arc::clone(agent_loop);
        let session_key = ctx.msg.session_key();
        let tasks = agent_loop
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
        let sub_cancelled = agent_loop.subagents.cancel_by_session(&session_key).await;
        let total = cancelled + sub_cancelled;
        let content = if total > 0 {
            format!("Stopped {total} task(s).")
        } else {
            "No active task to stop.".to_string()
        };
        reply(ctx, content)
    }
}

struct CmdRestart;

/// Restart the process in-place via exec/spawn after a short delay.
#[async_trait]
impl CommandHandler for CmdRestart {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/restart");
        };
        let bus = agent_loop.bus();
        let msg = ctx.msg.clone();
        let channel = msg.channel.clone();
        let chat_id = msg.chat_id.clone();
        // The actual restart is deferred so the "Restarting..." reply can be
        // delivered first. On a successful Unix exec the process is replaced and
        // never returns; only the failure path falls through, so report it via
        // the bus since this task outlives the handler's return value.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if let Err(e) = restart_with_notice(&channel, &chat_id) {
                log::error!("Failed to restart: {e}");
                let _ = bus.publish_outbound(OutboundMessage {
                    channel,
                    chat_id,
                    content: format!("Failed to restart: {e}"),
                    reply_to: None,
                    media: vec![],
                    metadata: msg.metadata.clone(),
                });
            }
        });
        reply(ctx, "Restarting...")
    }
}

struct CmdNew;

/// Start a fresh session.
#[async_trait]
impl CommandHandler for CmdNew {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/new");
        };
        let (mut session_manager, mut session) = ctx.lock_session_manager_and_session(agent_loop);
        let session_key = session.key.clone();
        let snapshot = session
            .messages
            .get(session.last_consolidated..)
            .map(<[_]>::to_vec);
        session.clear();
        if let Err(e) = session_manager.save(session) {
            log::error!("Failed to save session: {e}");
        }
        if let Some(_snapshot) = snapshot {
            // Schedule background
        }
        session_manager.invalidate(&session_key);
        reply(ctx, "New session started.")
    }
}

struct CmdStatus;

/// Build an outbound status message for a session.
#[async_trait]
impl CommandHandler for CmdStatus {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/status");
        };
        // Scope the `MutexGuard` so it is dropped before `.await` (guard is `!Send`).
        let (session_msg_count, ctx_est) = {
            let (_session_manager, session) = ctx.lock_session_manager_and_session(agent_loop);
            let (mut ctx_est, _) = agent_loop
                .consolidator
                .estimate_session_prompt_tokens(&session);
            if ctx_est == 0 {
                ctx_est = agent_loop
                    .last_usage
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get("prompt_tokens")
                    .copied()
                    .unwrap_or(0);
            }
            (session.get_history(Some(0)).len(), ctx_est)
        };

        let web_config = agent_loop.web_config.clone();
        let search_config = web_config.search.clone();
        let provider = search_config.provider.clone();
        let api_key = search_config.api_key.clone();
        let usage = fetch_search_usage(
            &provider,
            if api_key.is_empty() {
                None
            } else {
                Some(&api_key)
            },
        )
        .await;
        let search_usage_text = usage.format();
        let mut metadata = ctx.msg.metadata.clone();
        metadata.insert("render_as".to_string(), "text".into());
        let last_usage = agent_loop
            .last_usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let start_time_secs = agent_loop
            .start_time
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        OutboundMessage {
            channel: ctx.msg.channel.clone(),
            chat_id: ctx.msg.chat_id.clone(),
            content: build_status_content(
                PKG_VERSION,
                agent_loop.model.as_str(),
                start_time_secs,
                &last_usage,
                agent_loop.context_window_tokens,
                session_msg_count,
                ctx_est,
                Some(search_usage_text.as_str()),
            ),
            reply_to: None,
            media: vec![],
            metadata,
        }
    }

}

struct CmdDream;

/// Manually trigger a Dream consolidation run.
#[async_trait]
impl CommandHandler for CmdDream {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/dream");
        };
        let dream = Arc::clone(&agent_loop.dream);
        let bus = agent_loop.bus();
        let channel = ctx.msg.channel.clone();
        let chat_id = ctx.msg.chat_id.clone();

        tokio::spawn(async move {
            let t0 = Instant::now();
            let content = match AssertUnwindSafe(dream.run()).catch_unwind().await {
                Ok(did_work) => {
                    let elapsed = t0.elapsed().as_secs_f64();
                    if did_work {
                        format!("Dream completed in {:.1}s.", elapsed)
                    } else {
                        "Dream: nothing to process.".to_string()
                    }
                }
                Err(panic) => {
                    let elapsed = t0.elapsed().as_secs_f64();
                    let detail = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| {
                            panic
                                .downcast_ref::<String>()
                                .map(std::string::ToString::to_string)
                        })
                        .unwrap_or_else(|| "internal error".to_string());
                    format!("Dream failed after {:.1}s: {detail}", elapsed)
                }
            };
            log::info!("Dream: content: {}", content);
            let _ = bus.publish_outbound(OutboundMessage {
                channel,
                chat_id,
                content,
                reply_to: None,
                media: vec![],
                metadata: Default::default(),
            });
        });

        reply(ctx, "Dreaming...")
    }
}

struct CmdHelp;

/// Show available commands.
#[async_trait]
impl CommandHandler for CmdHelp {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        reply(ctx, build_help_text())
    }
}

struct CmdModel;

/// Show the LLM model currently configured for this session.
#[async_trait]
impl CommandHandler for CmdModel {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/model");
        };
        reply_as_text(ctx, format!("Model: {}", agent_loop.model))
    }
}

struct CmdMcpList;

fn format_mcp_list(servers: &[(String, String)]) -> String {
    let mut lines = vec![format!("MCP servers ({} connected):", servers.len())];
    for (name, endpoint) in servers {
        lines.push(format!("- {name} — {endpoint}"));
    }
    lines.join("\n")
}

/// List connected MCP servers and their endpoints.
#[async_trait]
impl CommandHandler for CmdMcpList {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/mcp-list");
        };
        if !agent_loop.is_mcp_configured() {
            return reply_as_text(ctx, "No MCP servers configured.");
        }
        agent_loop.ensure_mcp_connected().await;
        let servers = agent_loop.connected_mcp_endpoints();
        let content = if servers.is_empty() {
            "No MCP servers connected.".to_string()
        } else {
            format_mcp_list(&servers)
        };
        reply_as_text(ctx, content)
    }
}

struct CmdDreamLog;

impl CmdDreamLog {
    fn extract_changed_files(diff: &str) -> Vec<String> {
        let mut files = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for line in diff.lines() {
            if !line.starts_with("diff --git ") {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 4 {
                continue;
            }
            let mut path = parts[3].to_string();
            if let Some(stripped) = path.strip_prefix("b/") {
                path = stripped.to_string();
            }
            if seen.insert(path.clone()) {
                files.push(path);
            }
        }
        files
    }

    fn format_changed_files(diff: &str) -> String {
        let files = Self::extract_changed_files(diff);
        if files.is_empty() {
            "No tracked memory files changed.".to_string()
        } else {
            files
                .iter()
                .map(|path| format!("`{path}`"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    fn format_dream_log_content(commit: &CommitInfo, diff: &str, requested_sha: Option<&str>) -> String {
        let files_line = Self::format_changed_files(diff);
        let intro = if requested_sha.is_none() {
            "Here is the latest Dream memory change."
        } else {
            "Here is the selected Dream memory change."
        };

        let mut lines = vec![
            "## Dream Update".to_string(),
            String::new(),
            intro.to_string(),
            String::new(),
            format!("- Commit: `{}`", commit.sha),
            format!("- Time: {}", commit.timestamp),
            format!("- Changed files: {files_line}"),
        ];

        if !diff.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "Use `/dream-restore {}` to undo this change.",
                commit.sha
            ));
            lines.push(String::new());
            lines.push("```diff".to_string());
            lines.push(diff.trim_end().to_string());
            lines.push("```".to_string());
        } else {
            lines.push(String::new());
            lines.push(
                "Dream recorded this version, but there is no file diff to display.".to_string(),
            );
        }

        lines.join("\n")
    }
}

/// Show what the last Dream changed.
/// Default: diff of the latest commit (HEAD~1 vs HEAD).
///With /dream-log <sha>: diff of that specific commit.
#[async_trait]
impl CommandHandler for CmdDreamLog {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/dream-log");
        };
        let store = &agent_loop.consolidator.store;
        let git = &store.git;

        if !git.is_initialized() {
            return reply_as_text(
                ctx,
                dream_git_uninitialized_message(store.get_last_dream_cursor()),
            );
        }

        let args = ctx.args.trim();
        let content = if !args.is_empty() {
            let sha = args.split_whitespace().next().unwrap_or("");
            match git.show_commit_diff(sha) {
                Some((commit, diff)) => {
                    Self::format_dream_log_content(&commit, &diff, Some(sha))
                }
                None => format!(
                    "Couldn't find Dream change `{sha}`.\n\n\
                     Use `/dream-restore` to list recent versions, \
                     or `/dream-log` to inspect the latest one."
                ),
            }
        } else {
            let commits = git.log(1);
            match commits.first().and_then(|c| git.show_commit_diff(&c.sha)) {
                Some((commit, diff)) => Self::format_dream_log_content(&commit, &diff, None),
                None => "Dream memory has no saved versions yet.".to_string(),
            }
        };

        reply_as_text(ctx, content)
    }
}

struct CmdDreamRestore;

impl CmdDreamRestore {
    fn format_restore_list(commits: &[CommitInfo]) -> String {
        let mut lines = vec![
            "## Dream Restore".to_string(),
            String::new(),
            "Choose a Dream memory version to restore. Latest first:".to_string(),
            String::new(),
        ];
        for commit in commits {
            let first_line = commit.message.lines().next().unwrap_or("");
            lines.push(format!(
                "- `{}` {} - {first_line}",
                commit.sha, commit.timestamp
            ));
        }
        lines.extend([
            String::new(),
            "Preview a version with `/dream-log <sha>` before restoring it.".to_string(),
            "Restore a version with `/dream-restore <sha>`.".to_string(),
        ]);
        lines.join("\n")
    }

    fn restore_content(git: &GitStore, sha: &str) -> String {
        let result = git.show_commit_diff(sha);
        let changed_files = if let Some((_, diff)) = &result {
            CmdDreamLog::format_changed_files(diff)
        } else {
            "the tracked memory files".to_string()
        };

        match git.revert(sha) {
            Some(new_sha) => format!(
                "Restored Dream memory to the state before `{sha}`.\n\n\
                 - New safety commit: `{new_sha}`\n\
                 - Restored files: {changed_files}\n\n\
                 Use `/dream-log {new_sha}` to inspect the restore diff."
            ),
            None => format!(
                "Couldn't restore Dream change `{sha}`.\n\n\
                 It may not exist, or it may be the first saved version with no earlier state to restore."
            ),
        }
    }
}

/// Revert memory to a previous state.
#[async_trait]
impl CommandHandler for CmdDreamRestore {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/dream-restore");
        };
        let store = &agent_loop.consolidator.store;
        let git = &store.git;
        if !git.is_initialized() {
            return reply_as_text(
                ctx,
                dream_git_uninitialized_message(store.get_last_dream_cursor()),
            );
        }

        let args = ctx.args.trim();
        let content = if args.is_empty() {
            let commits = git.log(10);
            if commits.is_empty() {
                "Dream memory has no saved versions to restore yet.".to_string()
            } else {
                Self::format_restore_list(&commits)
            }
        } else {
            let sha = args.split_whitespace().next().unwrap_or("");
            Self::restore_content(git, sha)
        };

        reply_as_text(ctx, content)
    }
}

struct CmdTools;

#[async_trait]
impl CommandHandler for CmdTools {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/tools");
        };
        let registry = agent_loop
            .tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut names = registry.tool_names();
        names.sort();
        let content = if names.is_empty() {
            "No tools registered.".to_string()
        } else {
            let lines: Vec<String> = names
                .iter()
                .filter_map(|name| {
                    registry
                        .get(name)
                        .map(|tool| format!("- **{name}** — {}", tool.description()))
                })
                .collect();
            format!("Tools ({} available):\n{}", lines.len(), lines.join("\n"))
        };
        reply_as_text(ctx, convert_text_to_markdown(&content))
    }
}

struct CmdWorkspace;

#[async_trait]
impl CommandHandler for CmdWorkspace {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/tools");
        };
        let store = &agent_loop.consolidator.store;
        let workspace = store.workspace.clone();
        reply_as_text(ctx, format!("Workspace: {}", workspace.display()))
    }
}

struct CmdCleanup;

/// Workspace subtrees that must never be touched by `/cleanup`.
const CLEANUP_EXCLUDED_DIRS: &[&str] = &[
    "skills",
    "memory",
    "credentials",
    "sessions",
    "cron",
    ".git",
];

/// Individual files to preserve anywhere in the workspace tree.
const CLEANUP_EXCLUDED_FILES: &[&str] = &["HEARTBEAT.md", ".gitignore"];

impl CmdCleanup {
    fn path_has_excluded_dir(path: &Path, workspace: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(workspace) else {
            return true;
        };
        rel.components().any(|component| {
            if let std::path::Component::Normal(name) = component {
                CLEANUP_EXCLUDED_DIRS
                    .iter()
                    .any(|excluded| name.eq_ignore_ascii_case(excluded))
            } else {
                false
            }
        })
    }

    fn is_protected_file(path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            return true;
        };
        BOOTSTRAP_FILES
            .iter()
            .any(|file| name.eq_ignore_ascii_case(file))
            || CLEANUP_EXCLUDED_FILES
                .iter()
                .any(|file| name.eq_ignore_ascii_case(file))
    }

    fn list_cleanable_files(dir: &Path, workspace: &Path) -> io::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if Self::path_has_excluded_dir(&path, workspace) {
                continue;
            }
            if path.is_dir() {
                files.extend(Self::list_cleanable_files(&path, workspace)?);
            } else if !Self::is_protected_file(&path) {
                files.push(path);
            }
        }
        Ok(files)
    }
}

#[async_trait]
impl CommandHandler for CmdCleanup {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/cleanup");
        };
        let workspace = agent_loop.consolidator.store.workspace.clone();
        let files = match CmdCleanup::list_cleanable_files(&workspace, &workspace) {
            Ok(files) => files,
            Err(err) => {
                return reply_as_text(
                    ctx,
                    format!("Cleanup failed while scanning workspace: {err}"),
                );
            }
        };
        let mut removed_count = 0;
        for file in files {
            log::info!("Cleanup removing {}", file.display());
            if fs::remove_file(&file).is_ok() {
                removed_count += 1;
            }
        }
        reply_as_text(ctx, format!("Cleaned up {} files.", removed_count))
    }
}

/// Build canonical help text shared across channels.
fn build_help_text() -> String {
    let lines = vec![
        "🦀 rust-bot commands:",
        "/new — Start a new conversation",
        "/stop — Stop the current task",
        "/restart — Restart the bot",
        "/status — Show bot status",
        "/model — Show the current LLM model",
        "/dream — Manually trigger Dream consolidation",
        "/dream-log — Show what the last Dream changed",
        "/dream-restore — Revert memory to a previous state",
        "/help — Show available commands",
        "/mcp-list — List available MCP servers",
        "/tools — List available tools",
        "/workspace — Display the current workspace directory",
        "/cleanup — Remove stray files from the workspace (keeps memory, sessions, skills, etc.)",
    ];
    lines.join("\n")
}

pub fn register_builtin_commands(router: &mut CommandRouter) {
    router.priority("/stop", Arc::new(CmdStop));
    router.priority("/restart", Arc::new(CmdRestart));
    router.priority("/status", Arc::new(CmdStatus));
    router.exact("/new", Arc::new(CmdNew));
    router.exact("/dream", Arc::new(CmdDream));
    router.exact("/dream-log", Arc::new(CmdDreamLog));
    router.exact("/dream-restore", Arc::new(CmdDreamRestore));
    router.exact("/help", Arc::new(CmdHelp));
    router.exact("/model", Arc::new(CmdModel));
    router.exact("/mcp-list", Arc::new(CmdMcpList));
    router.exact("/tools", Arc::new(CmdTools));
    router.exact("/workspace", Arc::new(CmdWorkspace));
    router.exact("/cleanup", Arc::new(CmdCleanup));
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::bus::events::InboundMessage;
    use chrono::Utc;

    fn stop_ctx(agent_loop: Option<Arc<crate::agent::agent_loop::AgentLoop>>) -> CommandContext {
        CommandContext::with_options(
            InboundMessage {
                channel: "cli".into(),
                sender_id: "user".into(),
                chat_id: "direct".into(),
                content: "/stop".into(),
                timestamp: Utc::now(),
                media: vec![],
                metadata: Default::default(),
                session_key_override: None,
            },
            None,
            "stop",
            "/stop",
            "",
            agent_loop,
        )
    }

    #[tokio::test]
    async fn handle_without_agent_loop_reports_no_agent() {
        let out = CmdStop.handle(&stop_ctx(None)).await;
        assert_eq!(out.content, "No agent available to execute command: /stop.");
    }

    #[tokio::test]
    async fn dream_without_agent_loop_reports_no_agent() {
        let ctx = CommandContext::with_options(
            InboundMessage {
                channel: "cli".into(),
                sender_id: "user".into(),
                chat_id: "direct".into(),
                content: "/dream".into(),
                timestamp: Utc::now(),
                media: vec![],
                metadata: Default::default(),
                session_key_override: None,
            },
            None,
            "dream",
            "/dream",
            "",
            None,
        );
        let out = CmdDream.handle(&ctx).await;
        assert_eq!(out.content, "No agent available to execute command: /dream.");
    }

    #[tokio::test]
    async fn model_command_reports_current_model() {
        use crate::providers::base::{GenerationSettings, LLMProviderDyn, LLMResponse};

        struct TestProvider;
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
                static SETTINGS: std::sync::OnceLock<GenerationSettings> =
                    std::sync::OnceLock::new();
                SETTINGS.get_or_init(GenerationSettings::new)
            }
            fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
                unimplemented!()
            }
            fn spec(&self) -> Option<&crate::providers::registry::ProviderSpec> {
                None
            }
            fn get_default_model(&self) -> String {
                "test".into()
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

        let bus = Arc::new(crate::bus::queue::MessageBus::new());
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(TestProvider);
        let loop_ = Arc::new(crate::agent::agent_loop::AgentLoop::new(
            bus,
            provider,
            std::env::temp_dir(),
            Some("claude-sonnet-5".into()),
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
            None,
            None,
            None,
            None,
        ));
        let ctx = CommandContext::with_options(
            InboundMessage {
                channel: "cli".into(),
                sender_id: "user".into(),
                chat_id: "direct".into(),
                content: "/model".into(),
                timestamp: Utc::now(),
                media: vec![],
                metadata: Default::default(),
                session_key_override: None,
            },
            None,
            "model",
            "/model",
            "",
            Some(loop_),
        );
        let out = CmdModel.handle(&ctx).await;
        assert_eq!(out.content, "Model: claude-sonnet-5");
    }

    #[tokio::test]
    async fn handle_with_agent_loop_and_no_tasks_reports_none_active() {
        use crate::providers::base::{GenerationSettings, LLMProviderDyn, LLMResponse};

        struct TestProvider;
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
                static SETTINGS: std::sync::OnceLock<GenerationSettings> =
                    std::sync::OnceLock::new();
                SETTINGS.get_or_init(GenerationSettings::new)
            }
            fn generation_settings_mut(&mut self) -> &mut GenerationSettings {
                unimplemented!()
            }
            fn spec(&self) -> Option<&crate::providers::registry::ProviderSpec> {
                None
            }
            fn get_default_model(&self) -> String {
                "test".into()
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

        let bus = Arc::new(crate::bus::queue::MessageBus::new());
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(TestProvider);
        let loop_ = Arc::new(crate::agent::agent_loop::AgentLoop::new(
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
            None
        ));
        let out = CmdStop.handle(&stop_ctx(Some(loop_))).await;
        assert_eq!(out.content, "No active task to stop.");
    }

    #[test]
    fn format_dream_log_content_latest_with_diff() {
        let commit = CommitInfo {
            sha: "abcd1234".into(),
            message: "dream: test".into(),
            timestamp: "2026-04-04 12:00".into(),
        };
        let diff = "diff --git a/SOUL.md b/SOUL.md\n--- a/SOUL.md\n+++ b/SOUL.md\n@@ -1 +1 @@\n-old\n+new\n";
        let content = CmdDreamLog::format_dream_log_content(&commit, diff, None);

        assert!(content.contains("## Dream Update"));
        assert!(content.contains("Here is the latest Dream memory change."));
        assert!(content.contains("- Commit: `abcd1234`"));
        assert!(content.contains("- Changed files: `SOUL.md`"));
        assert!(content.contains("Use `/dream-restore abcd1234` to undo this change."));
        assert!(content.contains("```diff"));
        assert!(content.contains("+new"));
    }

    #[test]
    fn format_dream_log_content_selected_without_diff() {
        let commit = CommitInfo {
            sha: "abcd1234".into(),
            message: "dream: test".into(),
            timestamp: "2026-04-04 12:00".into(),
        };
        let content = CmdDreamLog::format_dream_log_content(&commit, "", Some("abcd1234"));

        assert!(content.contains("Here is the selected Dream memory change."));
        assert!(content.contains("No tracked memory files changed."));
        assert!(content.contains("Dream recorded this version, but there is no file diff to display."));
        assert!(!content.contains("```diff"));
    }

    #[test]
    fn format_restore_list_includes_commits_and_next_steps() {
        let commits = vec![
            CommitInfo {
                sha: "abcd1234".into(),
                message: "dream: latest\nextra".into(),
                timestamp: "2026-04-04 12:00".into(),
            },
            CommitInfo {
                sha: "bbbb2222".into(),
                message: "dream: older".into(),
                timestamp: "2026-04-04 08:00".into(),
            },
        ];
        let content = CmdDreamRestore::format_restore_list(&commits);

        assert!(content.contains("## Dream Restore"));
        assert!(content.contains("- `abcd1234` 2026-04-04 12:00 - dream: latest"));
        assert!(content.contains("- `bbbb2222` 2026-04-04 08:00 - dream: older"));
        assert!(content.contains("Preview a version with `/dream-log <sha>`"));
        assert!(content.contains("Restore a version with `/dream-restore <sha>`"));
    }

    #[test]
    fn format_mcp_list_empty() {
        let content = format_mcp_list(&[]);
        assert_eq!(content, "MCP servers (0 connected):");
    }

    #[test]
    fn format_mcp_list_single_url_server() {
        let servers = vec![(
            "ems".to_string(),
            "https://ems.example.org/mcp".to_string(),
        )];
        let content = format_mcp_list(&servers);
        assert_eq!(
            content,
            "MCP servers (1 connected):\n- ems — https://ems.example.org/mcp"
        );
    }

    #[test]
    fn format_mcp_list_multiple_servers_including_stdio() {
        let servers = vec![
            (
                "ems".to_string(),
                "https://ems.example.org/mcp".to_string(),
            ),
            (
                "filesystem".to_string(),
                "npx -y @modelcontextprotocol/server-filesystem".to_string(),
            ),
        ];
        let content = format_mcp_list(&servers);
        assert!(content.contains("MCP servers (2 connected):"));
        assert!(content.contains("- ems — https://ems.example.org/mcp"));
        assert!(content.contains(
            "- filesystem — npx -y @modelcontextprotocol/server-filesystem"
        ));
    }

    #[tokio::test]
    async fn mcp_list_without_agent_loop_returns_no_loop_message() {
        let ctx = CommandContext::with_options(
            InboundMessage {
                channel: "cli".into(),
                sender_id: "user".into(),
                chat_id: "direct".into(),
                content: "/mcp-list".into(),
                timestamp: Utc::now(),
                media: vec![],
                metadata: Default::default(),
                session_key_override: None,
            },
            None,
            "mcp-list",
            "/mcp-list",
            "",
            None,
        );
        let out = CmdMcpList.handle(&ctx).await;
        assert!(out.content.contains("No agent available to execute command: /mcp-list"));
    }

    #[test]
    fn cleanup_skips_protected_dirs_and_bootstrap_files() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let root = workspace.path();

        fs::create_dir_all(root.join("memory")).unwrap();
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::create_dir_all(root.join(".rust-bot/tool-results/default")).unwrap();
        fs::write(root.join("memory/MEMORY.md"), "keep").unwrap();
        fs::write(root.join("sessions/cli.json"), "keep").unwrap();
        fs::write(root.join("AGENTS.md"), "keep").unwrap();
        fs::write(root.join("HEARTBEAT.md"), "keep").unwrap();
        fs::write(root.join("scratch.txt"), "remove").unwrap();
        fs::write(root.join(".rust-bot/tool-results/default/abc.txt"), "remove").unwrap();

        let cleanable = CmdCleanup::list_cleanable_files(root, root).unwrap();
        let rel_paths: Vec<String> = cleanable
            .iter()
            .map(|path| {
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        let mut expected = vec![
            "scratch.txt".to_string(),
            ".rust-bot/tool-results/default/abc.txt".to_string(),
        ];
        let mut actual = rel_paths;
        expected.sort();
        actual.sort();
        assert_eq!(actual, expected);
    }
}
