use crate::{
    PKG_VERSION,
    agent::context::BOOTSTRAP_FILES,
    bus::events::OutboundMessage,
    command::{CommandContext, CommandHandler, CommandRouter, types::ChatCommand},
    security::workspace_access::WorkspaceAccessMode,
    session::goal_state::{self, GoalUpdateAction},
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
use std::{
    fs, io,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

/// Build an outbound reply addressed back to the inbound message's channel/chat.
fn reply(ctx: &CommandContext, content: impl Into<String>) -> OutboundMessage {
    OutboundMessage {
        channel: ctx.msg.channel.clone(),
        chat_id: ctx.msg.chat_id.clone(),
        content: content.into(),
        reply_to: None,
        media: vec![],
        metadata: ctx.msg.metadata.clone(),
        event: None,
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
        event: None,
    }
}

fn reply_as_markdown(ctx: &CommandContext, content: impl Into<String>) -> OutboundMessage {
    let mut metadata = ctx.msg.metadata.clone();
    metadata.insert("render_as".to_string(), "markdown".into());
    OutboundMessage {
        channel: ctx.msg.channel.clone(),
        chat_id: ctx.msg.chat_id.clone(),
        content: content.into(),
        reply_to: None,
        media: vec![],
        metadata,
        event: None,
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
                    event: None,
                });
            }
        });
        reply(ctx, "Restarting...")
    }
}

pub struct CmdNew;

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
        session.metadata.remove(goal_state::GOAL_STATE_KEY);
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

        let current_model = agent_loop.model();
        OutboundMessage {
            channel: ctx.msg.channel.clone(),
            chat_id: ctx.msg.chat_id.clone(),
            content: build_status_content(
                PKG_VERSION,
                current_model.as_str(),
                start_time_secs,
                &last_usage,
                agent_loop.context_window_tokens(),
                session_msg_count,
                ctx_est,
                Some(search_usage_text.as_str()),
            ),
            reply_to: None,
            media: vec![],
            metadata,
            event: None,
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
                event: None,
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

#[async_trait]
impl CommandHandler for CmdModel {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/model");
        };
        let requested = ctx.args.trim();
        if requested.is_empty() {
            let model = agent_loop.runtime_resolver.get_model();
            return reply_as_text(ctx, format!("Model: {}", model));
        }
        let runtime = agent_loop.set_runtime_model(requested);
        match runtime {
            Ok(runtime) => reply_as_text(ctx, format!("Model: {}", runtime.model)),
            Err(e) => reply_as_text(ctx, format!("Error: {e}")),
        }
    }
}

struct CmdModelPreset;

/// Show or switch the LLM model preset used by this chat session.
///
/// `/model-preset` (no args) reports the session's currently active
/// model/preset. `/model-preset <name>` switches this session (only) to that
/// named preset for future turns; `/model-preset default` clears back to the
/// process-wide default. The switch is session-scoped — it never touches the
/// process-wide default, and other sessions are unaffected.
#[async_trait]
impl CommandHandler for CmdModelPreset {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/model-preset");
        };
        let requested = ctx.args.trim();

        if requested.is_empty() {
            let (_session_manager, session) = ctx.lock_session_manager_and_session(agent_loop);
            let runtime = agent_loop.runtime_for_session(Some(&session));
            let provider_name = runtime
                .provider
                .api_base()
                .map(|base| base.to_string())
                .unwrap_or_else(|| "Unknown".to_string());
            return reply_as_text(
                ctx,
                format!(
                    "Model: {} (preset: {})\nProvider base url: {}",
                    runtime.model, runtime.preset_name, provider_name
                ),
            );
        }

        let (mut session_manager, session) = ctx.lock_session_manager_and_session(agent_loop);
        let session_key = session.key.clone();
        match agent_loop.set_session_model_preset(&mut session_manager, &session_key, requested) {
            Ok(runtime) => reply_as_text(
                ctx,
                format!(
                    "Model preset set to '{}' for this session (model: {}).",
                    runtime.preset_name, runtime.model
                ),
            ),
            Err(_) => {
                let available = agent_loop.available_model_presets().join(", ");
                reply_as_text(
                    ctx,
                    format!("Unknown model preset '{requested}'. Available presets: {available}"),
                )
            }
        }
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

    fn format_dream_log_content(
        commit: &CommitInfo,
        diff: &str,
        requested_sha: Option<&str>,
    ) -> String {
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
                Some((commit, diff)) => Self::format_dream_log_content(&commit, &diff, Some(sha)),
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
        let registry = agent_loop.tools.lock().unwrap_or_else(|e| e.into_inner());
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
                        .map(|tool| format!("* **{name}** — {}", tool.description()))
                })
                .collect();
            format!("Tools ({} available):\n{}", lines.len(), lines.join("\n"))
        };
        reply_as_text(ctx, convert_text_to_markdown(&content))
    }
}

struct CmdWorkspace;

/// Show or switch the workspace scope (project directory + access mode)
/// used by filesystem/shell tools during this chat session.
///
/// `/workspace` (no args) reports the session's currently effective scope.
/// `/workspace <path> [restricted|full]` switches this session (only) to
/// that project directory for future turns (`restricted` is the default
/// access mode when omitted). `/workspace default` clears back to the
/// process-wide default. The switch is session-scoped — it never touches
/// the process-wide default, and other sessions are unaffected.
#[async_trait]
impl CommandHandler for CmdWorkspace {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/workspace");
        };
        let requested = ctx.args.trim();

        if requested.is_empty() {
            let (_session_manager, session) = ctx.lock_session_manager_and_session(agent_loop);
            let scope = agent_loop.workspace_scope_for_session(Some(&session));
            return reply_as_text(
                ctx,
                format!(
                    "Workspace: {} (access: {})",
                    scope.project_path.display(),
                    scope.access_mode.as_str()
                ),
            );
        }

        if requested.eq_ignore_ascii_case("default") {
            let (mut session_manager, session) = ctx.lock_session_manager_and_session(agent_loop);
            agent_loop.clear_session_workspace_scope(&mut session_manager, &session.key);
            return reply_as_text(ctx, "Workspace override cleared; using the process default.");
        }

        let mut parts = requested.splitn(2, char::is_whitespace);
        let path_arg = parts.next().unwrap_or("");
        let mode_arg = parts
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("restricted");
        let access_mode = match mode_arg.parse::<WorkspaceAccessMode>() {
            Ok(mode) => mode,
            Err(e) => return reply_as_text(ctx, format!("Error: {e}")),
        };

        let (mut session_manager, session) = ctx.lock_session_manager_and_session(agent_loop);
        let project_path = PathBuf::from(path_arg);
        if !project_path.exists() {
            let create_result = std::fs::create_dir_all(&project_path);
            if let Err(e) = create_result {
                return reply_as_text(ctx, format!("Error creating workspace directory: {e}"));
            }
        }
        match agent_loop.set_session_workspace_scope(
            &mut session_manager,
            &session.key,
            &project_path,
            access_mode,
        ) {
            Ok(scope) => reply_as_text(
                ctx,
                format!(
                    "Workspace set to {} ({}) for this session.",
                    scope.project_path.display(),
                    scope.access_mode.as_str()
                ),
            ),
            Err(e) => reply_as_text(ctx, format!("Error: {e}")),
        }
    }
}

struct CmdGoal;

/// Start, check, or cancel a sustained goal — an objective the agent tracks
/// across many turns (persisted in session metadata, echoed back into the
/// model's own prompt context every turn; see `session::goal_state`).
///
/// `/goal <objective>` starts a new goal for this session immediately (this
/// executes directly rather than nanobot's tag-and-let-the-model-decide
/// design — see the port plan). `/goal` with no args reports the active
/// goal's status, if any. `/goal cancel` (or `/goal clear`) force-clears an
/// active goal — the manual escape hatch for when the model never calls its
/// own `update_goal` tool. The model can also complete/cancel/block/replace
/// the goal itself via that tool during later turns.
#[async_trait]
impl CommandHandler for CmdGoal {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/goal");
        };
        let requested = ctx.args.trim();

        if requested.is_empty() {
            let (_session_manager, session) = ctx.lock_session_manager_and_session(agent_loop);
            if !goal_state::sustained_goal_active(&session.metadata) {
                return reply_as_text(
                    ctx,
                    "Usage: /goal <long-running task description> (or /goal cancel to clear an active goal)",
                );
            }
            let objective = session
                .metadata
                .get(goal_state::GOAL_STATE_KEY)
                .and_then(|g| g.get("objective"))
                .and_then(|v| v.as_str())
                .unwrap_or("(no objective text stored)");
            return reply_as_text(ctx, format!("Goal (active): {objective}"));
        }

        if requested.eq_ignore_ascii_case("cancel") || requested.eq_ignore_ascii_case("clear") {
            let (mut session_manager, session) = ctx.lock_session_manager_and_session(agent_loop);
            return match agent_loop.update_session_goal(
                &mut session_manager,
                &session.key,
                GoalUpdateAction::Cancel,
                Some("Cancelled via /goal cancel"),
                None,
                None,
            ) {
                Ok(message) => reply_as_text(ctx, message),
                Err(e) => reply_as_text(ctx, format!("Error: {e}")),
            };
        }

        let (mut session_manager, session) = ctx.lock_session_manager_and_session(agent_loop);
        match agent_loop.create_session_goal(&mut session_manager, &session.key, requested, None) {
            Ok(()) => reply_as_text(ctx, format!("Goal started: {requested}")),
            Err(e) => reply_as_text(ctx, format!("Error: {e}")),
        }
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

struct CmdListSessions;

#[async_trait]
impl CommandHandler for CmdListSessions {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/list-sessions");
        };
        let session_manager = agent_loop
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let sessions = session_manager.list_sessions();
        let content =
            crate::session::manager::format_sessions_list(&sessions, Some(ctx.key.as_str()));
        reply_as_text(ctx, content)
    }
}

struct CmdExamplePrompts;

#[async_trait]
impl CommandHandler for CmdExamplePrompts {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/example-prompts");
        };
        let config = agent_loop.config.clone();
        let prompts = config.example_prompts.join("\n");
        reply_as_text(ctx, format!("Example prompts:\n{}", prompts))
    }
}

struct CmdModelPresets;

#[async_trait]
impl CommandHandler for CmdModelPresets {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
        let Some(agent_loop) = &ctx.agent_loop else {
            return reply_no_loop(ctx, "/model-presets");
        };
        let config = agent_loop.config.clone();
        let presets = config.model_presets.clone();
        let content = format!(
            "Model presets:\n{}",
            presets
                .iter()
                .map(|(name, preset)| format!("* **{name}** — {preset:?}"))
                .collect::<Vec<String>>()
                .join("\n")
        );
        reply_as_markdown(ctx, &content)
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
        "/model — Show the current model",
        "/model-preset — Show the current preset's model and provider or switch to a different preset",
        "/model-presets — List available model presets",
        "/dream — Manually trigger Dream consolidation",
        "/dream-log — Show what the last Dream changed",
        "/dream-restore — Revert memory to a previous state",
        "/help — Show available commands",
        "/mcp-list — List available MCP servers",
        "/tools — List available tools",
        "/workspace — Show the session's workspace scope, or switch it: /workspace <path> [restricted|full], /workspace default to clear",
        "/goal <task> — Start a sustained goal for this session; /goal to check status, /goal cancel to clear it",
        "/cleanup — Remove stray files from the workspace (keeps memory, sessions, skills, etc.)",
        "/list-sessions — List available sessions in current workspace",
        "/example-prompts — List example prompts",
    ];
    lines.join("\n")
}

pub fn register_builtin_commands(router: &mut CommandRouter) {
    router.priority(ChatCommand::Stop.to_string(), Arc::new(CmdStop));
    router.priority(ChatCommand::Restart.to_string(), Arc::new(CmdRestart));
    router.priority(ChatCommand::Status.to_string(), Arc::new(CmdStatus));
    router.exact(ChatCommand::New.to_string(), Arc::new(CmdNew));
    router.exact(ChatCommand::Dream.to_string(), Arc::new(CmdDream));
    router.exact(ChatCommand::DreamLog.to_string(), Arc::new(CmdDreamLog));
    router.exact(
        ChatCommand::DreamRestore.to_string(),
        Arc::new(CmdDreamRestore),
    );
    router.exact(ChatCommand::Help.to_string(), Arc::new(CmdHelp));
    router.prefix(ChatCommand::Model.to_string(), Arc::new(CmdModel));
    router.prefix(
        ChatCommand::ModelPreset.to_string(),
        Arc::new(CmdModelPreset),
    );
    router.exact(
        ChatCommand::ModelPresets.to_string(),
        Arc::new(CmdModelPresets),
    );
    router.exact(ChatCommand::McpList.to_string(), Arc::new(CmdMcpList));
    router.exact(ChatCommand::Tools.to_string(), Arc::new(CmdTools));
    router.prefix(ChatCommand::Workspace.to_string(), Arc::new(CmdWorkspace));
    router.prefix(ChatCommand::Goal.to_string(), Arc::new(CmdGoal));
    router.exact(ChatCommand::Cleanup.to_string(), Arc::new(CmdCleanup));
    router.exact(
        ChatCommand::ListSessions.to_string(),
        Arc::new(CmdListSessions),
    );
    router.exact(
        ChatCommand::ExamplePrompts.to_string(),
        Arc::new(CmdExamplePrompts),
    );
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
        assert_eq!(
            out.content,
            "No agent available to execute command: /dream."
        );
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
        let mut config = crate::config::schema::Config::default();
        config.agents.model = "claude-sonnet-5".into();
        let loop_ = Arc::new(crate::agent::agent_loop::AgentLoop::new(
            bus,
            provider,
            std::env::temp_dir(),
            config,
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
        let out = CmdModelPreset.handle(&ctx).await;
        assert!(out.content.contains("Model: "));
        assert!(
            out.content.contains("(preset: default)"),
            "expected default preset label, got: {}",
            out.content
        );
    }

    use crate::providers::base::LLMProviderDyn;

    struct ModelCmdTestProvider;
    #[async_trait]
    impl LLMProviderDyn for ModelCmdTestProvider {
        fn api_key(&self) -> Option<String> {
            None
        }
        fn api_base(&self) -> Option<String> {
            None
        }
        fn extra_headers(&self) -> Option<HashMap<String, String>> {
            None
        }
        fn generation_settings(&self) -> &crate::providers::base::GenerationSettings {
            static SETTINGS: std::sync::OnceLock<crate::providers::base::GenerationSettings> =
                std::sync::OnceLock::new();
            SETTINGS.get_or_init(crate::providers::base::GenerationSettings::new)
        }
        fn generation_settings_mut(&mut self) -> &mut crate::providers::base::GenerationSettings {
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
        ) -> crate::providers::base::LLMResponse {
            crate::providers::base::LLMResponse::new()
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
        ) -> crate::providers::base::LLMResponse {
            crate::providers::base::LLMResponse::new()
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
        ) -> crate::providers::base::LLMResponse {
            crate::providers::base::LLMResponse::new()
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
        ) -> crate::providers::base::LLMResponse {
            crate::providers::base::LLMResponse::new()
        }
    }

    fn model_cmd_test_loop_with_preset() -> Arc<crate::agent::agent_loop::AgentLoop> {
        let bus = Arc::new(crate::bus::queue::MessageBus::new());
        let provider: Arc<dyn LLMProviderDyn> = Arc::new(ModelCmdTestProvider);
        let mut config = crate::config::schema::Config::default();
        config.agents.model = "claude-sonnet-5".into();
        config.providers.anthropic.api_key = "test-key".to_string();
        config.model_presets.insert(
            "fast".to_string(),
            crate::config::schema::ModelPresetConfig {
                model: "claude-haiku".to_string(),
                provider: "anthropic".to_string(),
                ..Default::default()
            },
        );
        Arc::new(crate::agent::agent_loop::AgentLoop::new(
            bus,
            provider,
            std::env::temp_dir(),
            config,
            None,
            None,
            None,
        ))
    }

    fn model_cmd_ctx(
        agent_loop: Arc<crate::agent::agent_loop::AgentLoop>,
        args: &str,
    ) -> CommandContext {
        CommandContext::with_options(
            InboundMessage {
                channel: "cli".into(),
                sender_id: "user".into(),
                chat_id: "direct".into(),
                content: format!("/model {args}").trim().to_string(),
                timestamp: Utc::now(),
                media: vec![],
                metadata: Default::default(),
                session_key_override: None,
            },
            None,
            "model-cmd-session",
            "/model",
            args,
            Some(agent_loop),
        )
    }

    #[tokio::test]
    async fn model_command_with_valid_preset_name_confirms_switch() {
        let loop_ = model_cmd_test_loop_with_preset();
        let ctx = model_cmd_ctx(loop_, "fast");
        let out = CmdModelPreset.handle(&ctx).await;
        assert!(out.content.contains("fast"), "got: {}", out.content);
        assert!(out.content.contains("claude-haiku"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn model_command_with_invalid_preset_name_lists_available() {
        let loop_ = model_cmd_test_loop_with_preset();
        let ctx = model_cmd_ctx(loop_, "not-a-real-preset");
        let out = CmdModelPreset.handle(&ctx).await;
        assert!(
            out.content.contains("Unknown model preset"),
            "got: {}",
            out.content
        );
        assert!(out.content.contains("default"), "got: {}", out.content);
        assert!(out.content.contains("fast"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn model_command_default_keyword_clears_session_override() {
        let loop_ = model_cmd_test_loop_with_preset();

        // First switch to "fast" ...
        let switch_ctx = model_cmd_ctx(loop_.clone(), "fast");
        let switched = CmdModelPreset.handle(&switch_ctx).await;
        assert!(switched.content.contains("fast"));

        // ... then confirm the no-args view reflects it before clearing.
        let show_ctx = model_cmd_ctx(loop_.clone(), "");
        let shown = CmdModelPreset.handle(&show_ctx).await;
        assert!(
            shown.content.contains("(preset: fast)"),
            "got: {}",
            shown.content
        );

        // ... then clear back to "default" for the same session.
        let default_ctx = model_cmd_ctx(loop_.clone(), "default");
        let cleared = CmdModelPreset.handle(&default_ctx).await;
        assert!(
            cleared.content.contains("default"),
            "got: {}",
            cleared.content
        );

        let show_after_ctx = model_cmd_ctx(loop_, "");
        let shown_after = CmdModelPreset.handle(&show_after_ctx).await;
        assert!(
            shown_after.content.contains("(preset: default)"),
            "got: {}",
            shown_after.content
        );
    }

    fn workspace_cmd_ctx(
        agent_loop: Arc<crate::agent::agent_loop::AgentLoop>,
        args: &str,
    ) -> CommandContext {
        CommandContext::with_options(
            InboundMessage {
                channel: "cli".into(),
                sender_id: "user".into(),
                chat_id: "direct".into(),
                content: format!("/workspace {args}").trim().to_string(),
                timestamp: Utc::now(),
                media: vec![],
                metadata: Default::default(),
                session_key_override: None,
            },
            None,
            "workspace-cmd-session",
            "/workspace",
            args,
            Some(agent_loop),
        )
    }

    #[tokio::test]
    async fn cmd_workspace_reports_process_default_with_no_args() {
        let loop_ = model_cmd_test_loop_with_preset();
        let ctx = workspace_cmd_ctx(loop_, "");
        let out = CmdWorkspace.handle(&ctx).await;
        assert!(out.content.starts_with("Workspace: "), "got: {}", out.content);
        assert!(out.content.contains("(access: "), "got: {}", out.content);
    }

    #[tokio::test]
    async fn cmd_workspace_switches_session_scope_and_reports_it_back() {
        let loop_ = model_cmd_test_loop_with_preset();
        let dir = tempfile::tempdir().unwrap();

        let switch_ctx = workspace_cmd_ctx(loop_.clone(), dir.path().to_str().unwrap());
        let switched = CmdWorkspace.handle(&switch_ctx).await;
        assert!(
            switched.content.contains(dir.path().to_str().unwrap()),
            "got: {}",
            switched.content
        );
        assert!(switched.content.contains("(restricted)"), "got: {}", switched.content);

        let show_ctx = workspace_cmd_ctx(loop_, "");
        let shown = CmdWorkspace.handle(&show_ctx).await;
        assert!(
            shown.content.contains(dir.path().to_str().unwrap()),
            "got: {}",
            shown.content
        );
    }

    #[tokio::test]
    async fn cmd_workspace_default_clears_override() {
        let loop_ = model_cmd_test_loop_with_preset();
        let dir = tempfile::tempdir().unwrap();

        let switch_ctx = workspace_cmd_ctx(loop_.clone(), dir.path().to_str().unwrap());
        CmdWorkspace.handle(&switch_ctx).await;

        let clear_ctx = workspace_cmd_ctx(loop_.clone(), "default");
        let cleared = CmdWorkspace.handle(&clear_ctx).await;
        assert!(cleared.content.contains("cleared"), "got: {}", cleared.content);

        let show_ctx = workspace_cmd_ctx(loop_, "");
        let shown = CmdWorkspace.handle(&show_ctx).await;
        assert!(
            !shown.content.contains(dir.path().to_str().unwrap()),
            "got: {}",
            shown.content
        );
    }

    #[tokio::test]
    async fn cmd_workspace_rejects_relative_or_missing_path_with_error_reply() {
        let loop_ = model_cmd_test_loop_with_preset();
        let ctx = workspace_cmd_ctx(loop_, "relative/dir");
        let out = CmdWorkspace.handle(&ctx).await;
        assert!(out.content.starts_with("Error:"), "got: {}", out.content);
    }

    fn goal_cmd_ctx(
        agent_loop: Arc<crate::agent::agent_loop::AgentLoop>,
        args: &str,
    ) -> CommandContext {
        CommandContext::with_options(
            InboundMessage {
                channel: "cli".into(),
                sender_id: "user".into(),
                chat_id: "direct".into(),
                content: format!("/goal {args}").trim().to_string(),
                timestamp: Utc::now(),
                media: vec![],
                metadata: Default::default(),
                session_key_override: None,
            },
            None,
            "goal-cmd-session",
            "/goal",
            args,
            Some(agent_loop),
        )
    }

    #[tokio::test]
    async fn cmd_goal_reports_usage_with_no_active_goal_and_no_args() {
        let loop_ = model_cmd_test_loop_with_preset();
        let ctx = goal_cmd_ctx(loop_, "");
        let out = CmdGoal.handle(&ctx).await;
        assert!(out.content.starts_with("Usage:"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn cmd_goal_start_status_and_cancel_round_trip() {
        let loop_ = model_cmd_test_loop_with_preset();

        let start_ctx = goal_cmd_ctx(loop_.clone(), "ship the feature");
        let started = CmdGoal.handle(&start_ctx).await;
        assert!(started.content.contains("ship the feature"), "got: {}", started.content);

        let status_ctx = goal_cmd_ctx(loop_.clone(), "");
        let status = CmdGoal.handle(&status_ctx).await;
        assert!(status.content.contains("ship the feature"), "got: {}", status.content);

        let cancel_ctx = goal_cmd_ctx(loop_.clone(), "cancel");
        let cancelled = CmdGoal.handle(&cancel_ctx).await;
        assert!(cancelled.content.contains("cancelled"), "got: {}", cancelled.content);

        let after_ctx = goal_cmd_ctx(loop_, "");
        let after = CmdGoal.handle(&after_ctx).await;
        assert!(after.content.starts_with("Usage:"), "got: {}", after.content);
    }

    #[tokio::test]
    async fn cmd_goal_refuses_second_goal_while_one_is_active() {
        let loop_ = model_cmd_test_loop_with_preset();
        CmdGoal.handle(&goal_cmd_ctx(loop_.clone(), "first objective")).await;
        let out = CmdGoal.handle(&goal_cmd_ctx(loop_, "second objective")).await;
        assert!(out.content.starts_with("Error:"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn cmd_new_clears_active_goal_but_keeps_other_metadata() {
        let loop_ = model_cmd_test_loop_with_preset();
        let session_key = "new-cmd-goal-session";
        let new_ctx = |args: &str| {
            CommandContext::with_options(
                InboundMessage {
                    channel: "cli".into(),
                    sender_id: "user".into(),
                    chat_id: "direct".into(),
                    content: "/new".to_string(),
                    timestamp: Utc::now(),
                    media: vec![],
                    metadata: Default::default(),
                    session_key_override: None,
                },
                None,
                session_key,
                "/new",
                args,
                Some(loop_.clone()),
            )
        };

        CmdGoal.handle(&goal_cmd_ctx_with_key(loop_.clone(), session_key, "an active goal")).await;
        {
            let mut session_manager = loop_.session_manager.lock().unwrap();
            let session = session_manager.get_or_create_session(session_key);
            session.metadata.insert("unrelated".to_string(), serde_json::json!("keep-me"));
            let snapshot = session.clone();
            session_manager.save(snapshot).unwrap();
        }
        assert!(
            crate::session::goal_state::sustained_goal_active(
                &loop_.session_manager.lock().unwrap().get_or_create_session(session_key).metadata
            ),
            "goal should be active before /new"
        );

        CmdNew.handle(&new_ctx("")).await;

        let mut session_manager = loop_.session_manager.lock().unwrap();
        let session = session_manager.get_or_create_session(session_key);
        assert!(
            !crate::session::goal_state::sustained_goal_active(&session.metadata),
            "goal should be cleared by /new"
        );
        assert_eq!(session.metadata.get("unrelated"), Some(&serde_json::json!("keep-me")));
    }

    fn goal_cmd_ctx_with_key(
        agent_loop: Arc<crate::agent::agent_loop::AgentLoop>,
        session_key: &str,
        args: &str,
    ) -> CommandContext {
        CommandContext::with_options(
            InboundMessage {
                channel: "cli".into(),
                sender_id: "user".into(),
                chat_id: "direct".into(),
                content: format!("/goal {args}").trim().to_string(),
                timestamp: Utc::now(),
                media: vec![],
                metadata: Default::default(),
                session_key_override: None,
            },
            None,
            session_key,
            "/goal",
            args,
            Some(agent_loop),
        )
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
            crate::config::schema::Config::default(),
            None,
            None,
            None,
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
        assert!(
            content.contains("Dream recorded this version, but there is no file diff to display.")
        );
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
        let servers = vec![("ems".to_string(), "https://ems.example.org/mcp".to_string())];
        let content = format_mcp_list(&servers);
        assert_eq!(
            content,
            "MCP servers (1 connected):\n- ems — https://ems.example.org/mcp"
        );
    }

    #[test]
    fn format_mcp_list_multiple_servers_including_stdio() {
        let servers = vec![
            ("ems".to_string(), "https://ems.example.org/mcp".to_string()),
            (
                "filesystem".to_string(),
                "npx -y @modelcontextprotocol/server-filesystem".to_string(),
            ),
        ];
        let content = format_mcp_list(&servers);
        assert!(content.contains("MCP servers (2 connected):"));
        assert!(content.contains("- ems — https://ems.example.org/mcp"));
        assert!(content.contains("- filesystem — npx -y @modelcontextprotocol/server-filesystem"));
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
        assert!(
            out.content
                .contains("No agent available to execute command: /mcp-list")
        );
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
        fs::write(
            root.join(".rust-bot/tool-results/default/abc.txt"),
            "remove",
        )
        .unwrap();

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
