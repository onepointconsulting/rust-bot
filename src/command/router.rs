use std::collections::HashMap;
use std::sync::{Arc, LazyLock, MutexGuard};

use async_trait::async_trait;
use regex::Regex;

use crate::agent::agent_loop::AgentLoop;
use crate::bus::events::{InboundMessage, OutboundMessage};
use crate::session::manager::{Session, SessionManager};

/// Telegram / Discord bot mention suffix on the command token (`/cmd@BotName`).
static BOT_SUFFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_]+$").expect("valid BOT_SUFFIX_RE"));

/// Normalize slash-command transport variants before routing.
///
/// Telegram and Discord-style command dispatch can produce `/cmd@bot args`.
/// The bot suffix belongs to the transport, not the command name, so strip it
/// once at the router boundary while preserving user arguments verbatim.
pub fn normalize_command_text(text: &str) -> String {
    let stripped = text.trim();
    if !stripped.starts_with('/') {
        return stripped.to_string();
    }
    let (first, sep, rest) = match stripped.split_once(' ') {
        Some((first, rest)) => (first, " ", rest),
        None => (stripped, "", ""),
    };
    if !first.contains('@') {
        return stripped.to_string();
    }
    let Some((command, suffix)) = first.rsplit_once('@') else {
        return stripped.to_string();
    };
    if !command.is_empty() && !suffix.is_empty() && BOT_SUFFIX_RE.is_match(suffix) {
        if sep.is_empty() {
            command.to_string()
        } else {
            format!("{command}{sep}{rest}")
        }
    } else {
        stripped.to_string()
    }
}

#[async_trait]
pub trait CommandHandler: Send + Sync {
    async fn handle(&self, ctx: &CommandContext) -> OutboundMessage;
}

/// Everything a command handler needs to produce a response.
pub struct CommandContext {
    pub msg: InboundMessage,
    pub session: Option<Session>,
    pub key: String,
    pub raw: String,
    pub args: String,
    pub agent_loop: Option<Arc<AgentLoop>>,
}

impl CommandContext {
    /// Create a context with default `args` (`""`) and no agent loop (Python `loop=None`).
    pub fn new(
        msg: InboundMessage,
        session: Option<Session>,
        key: impl Into<String>,
        raw: impl Into<String>,
    ) -> Self {
        Self {
            msg,
            session,
            key: key.into(),
            raw: raw.into(),
            args: String::new(),
            agent_loop: None,
        }
    }

    /// Create a context with explicit optional fields (Python dataclass with all kwargs).
    pub fn with_options(
        msg: InboundMessage,
        session: Option<Session>,
        key: impl Into<String>,
        raw: impl Into<String>,
        args: impl Into<String>,
        agent_loop: Option<Arc<AgentLoop>>,
    ) -> Self {
        Self {
            msg,
            session,
            key: key.into(),
            raw: raw.into(),
            args: args.into(),
            agent_loop,
        }
    }

    /// Lock the agent loop's session manager and resolve the command session.
    ///
    /// Uses [`Self::session`] when set; otherwise loads or creates the session
    /// for [`Self::key`] from the manager.
    pub fn lock_session_manager_and_session<'a>(
        &'a self,
        agent_loop: &'a AgentLoop,
    ) -> (MutexGuard<'a, SessionManager>, Session) {
        let mut session_manager = agent_loop
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let session = match &self.session {
            Some(session) => session.clone(),
            None => session_manager.get_or_create_session(&self.key).clone(),
        };
        (session_manager, session)
    }
}

/// Pure dict-based command dispatch (Python `CommandRouter`).
///
/// Tiers checked in order:
/// 1. **priority** — exact-match commands handled before the dispatch lock (e.g. `/stop`, `/restart`).
/// 2. **exact** — exact-match commands handled inside the dispatch lock.
/// 3. **prefix** — longest-prefix-first match (e.g. `/team `).
/// 4. **interceptors** — fallback handlers (e.g. team-mode active check).
pub struct CommandRouter {
    priority: HashMap<String, Arc<dyn CommandHandler>>,
    exact: HashMap<String, Arc<dyn CommandHandler>>,
    prefix: Vec<(String, Arc<dyn CommandHandler>)>,
    interceptors: Vec<Arc<dyn CommandHandler>>,
}

impl CommandRouter {
    pub fn new() -> Self {
        Self {
            priority: HashMap::new(),
            exact: HashMap::new(),
            prefix: Vec::new(),
            interceptors: Vec::new(),
        }
    }

    pub fn priority(&mut self, cmd: impl Into<String>, handler: Arc<dyn CommandHandler>) {
        self.priority.insert(cmd.into(), handler);
    }

    pub fn exact(&mut self, cmd: impl Into<String>, handler: Arc<dyn CommandHandler>) {
        self.exact.insert(cmd.into(), handler);
    }

    /// Registers a prefix handler; entries are kept longest-prefix-first.
    pub fn prefix(&mut self, prefix: impl Into<String>, handler: Arc<dyn CommandHandler>) {
        self.prefix.push((prefix.into(), handler));
        self.prefix.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    pub fn intercept(&mut self, handler: Arc<dyn CommandHandler>) {
        self.interceptors.push(handler);
    }

    pub fn is_priority(&self, text: &str) -> bool {
        let key = text.trim().to_lowercase();
        self.priority.contains_key(&key)
    }

    /// Dispatch a priority command. Called from `run()` without the lock.
    pub async fn dispatch_priority(&self, ctx: &CommandContext) -> Option<OutboundMessage> {
        let key = ctx.raw.to_lowercase();
        let handler = self.priority.get(&key)?;
        Some(handler.handle(ctx).await)
    }

    /// Try exact, prefix, then interceptors. Returns None if no route matched.
    pub async fn dispatch(&self, ctx: &mut CommandContext) -> Option<OutboundMessage> {
        let cmd = ctx.raw.to_lowercase();
        let cmd = cmd.trim();

        if let Some(handler) = self.exact.get(cmd) {
            return Some(handler.handle(ctx).await);
        }

        for (pfx, handler) in &self.prefix {
            if cmd.starts_with(pfx) {
                ctx.args = ctx.raw[pfx.len()..].to_string();
                return Some(handler.handle(ctx).await);
            }
        }

        if let Some(handler) = self.interceptors.first() {
            return Some(handler.handle(ctx).await);
        }

        None
    }
}

impl Default for CommandRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn normalize_strips_whitespace() {
        assert_eq!(normalize_command_text("  /help  "), "/help");
        assert_eq!(normalize_command_text("  hello  "), "hello");
    }

    #[test]
    fn normalize_leaves_non_slash_text() {
        assert_eq!(normalize_command_text("help"), "help");
        assert_eq!(normalize_command_text("help@bot"), "help@bot");
    }

    #[test]
    fn normalize_leaves_commands_without_bot_suffix() {
        assert_eq!(normalize_command_text("/help"), "/help");
        assert_eq!(
            normalize_command_text("/model claude-fast"),
            "/model claude-fast"
        );
    }

    #[test]
    fn normalize_strips_telegram_style_bot_suffix() {
        assert_eq!(normalize_command_text("/help@MyBot"), "/help");
        assert_eq!(normalize_command_text("/stop@nanobot_bot"), "/stop");
        assert_eq!(normalize_command_text("/dream@MyBot log"), "/dream log");
    }

    #[test]
    fn normalize_preserves_arguments_verbatim() {
        assert_eq!(
            normalize_command_text("/model@Bot Claude Fast"),
            "/model Claude Fast"
        );
        // partition only on the first space — leading spaces in the rest stay
        assert_eq!(normalize_command_text("/cmd@Bot  spaced"), "/cmd  spaced");
    }

    #[test]
    fn normalize_rejects_invalid_bot_suffixes() {
        // hyphen is not in [A-Za-z0-9_]
        assert_eq!(normalize_command_text("/help@my-bot"), "/help@my-bot");
        // empty suffix
        assert_eq!(normalize_command_text("/help@"), "/help@");
        // command before @ is `/` (truthy) so the suffix still strips
        assert_eq!(normalize_command_text("/@bot"), "/");
    }

    #[test]
    fn normalize_uses_rightmost_at_for_suffix() {
        assert_eq!(normalize_command_text("/cmd@foo@Bot"), "/cmd@foo");
    }

    fn sample_msg() -> InboundMessage {
        InboundMessage {
            channel: "cli".into(),
            sender_id: "user".into(),
            chat_id: "direct".into(),
            content: "/help".into(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: Default::default(),
            session_key_override: None,
        }
    }

    #[test]
    fn new_defaults_args_and_agent_loop() {
        let ctx = CommandContext::new(sample_msg(), None, "help", "/help");
        assert_eq!(ctx.key, "help");
        assert_eq!(ctx.raw, "/help");
        assert!(ctx.args.is_empty());
        assert!(ctx.agent_loop.is_none());
    }

    #[test]
    fn with_options_sets_optional_fields() {
        let session = Session::new("cli:direct".into());
        let ctx = CommandContext::with_options(
            sample_msg(),
            Some(session),
            "help",
            "/help",
            "extra",
            None,
        );
        assert_eq!(ctx.args, "extra");
        assert!(ctx.session.is_some());
    }

    struct StaticReplyHandler(&'static str);

    #[async_trait]
    impl CommandHandler for StaticReplyHandler {
        async fn handle(&self, _ctx: &CommandContext) -> OutboundMessage {
            OutboundMessage {
                channel: "cli".into(),
                chat_id: "direct".into(),
                content: self.0.into(),
                reply_to: None,
                media: vec![],
                metadata: Default::default(),
                event: None,
            }
        }
    }

    struct ArgsEchoHandler;

    #[async_trait]
    impl CommandHandler for ArgsEchoHandler {
        async fn handle(&self, ctx: &CommandContext) -> OutboundMessage {
            OutboundMessage {
                channel: ctx.msg.channel.clone(),
                chat_id: ctx.msg.chat_id.clone(),
                content: ctx.args.clone(),
                reply_to: None,
                media: vec![],
                metadata: Default::default(),
                event: None,
            }
        }
    }

    #[tokio::test]
    async fn dispatch_priority_matches_raw_case_insensitive() {
        let mut router = CommandRouter::new();
        router.priority("/stop", Arc::new(StaticReplyHandler("stopped")));

        let ctx = CommandContext::new(sample_msg(), None, "stop", "/STOP");
        let out = router.dispatch_priority(&ctx).await;
        assert_eq!(out.as_ref().map(|m| m.content.as_str()), Some("stopped"));

        let ctx = CommandContext::new(sample_msg(), None, "help", "/help");
        assert!(router.dispatch_priority(&ctx).await.is_none());
    }

    #[tokio::test]
    async fn dispatch_prefix_sets_args_from_raw_suffix() {
        let mut router = CommandRouter::new();
        router.prefix("/team ", Arc::new(ArgsEchoHandler));

        let mut ctx = CommandContext::new(sample_msg(), None, "team", "/team hello");
        let out = router.dispatch(&mut ctx).await.unwrap();
        assert_eq!(ctx.args, "hello");
        assert_eq!(out.content, "hello");
    }

    #[tokio::test]
    async fn dispatch_model_prefix_sets_args_from_raw_suffix() {
        // `/model` must be registered via `prefix`, not `exact` — `exact` only
        // matches when the whole trimmed/lowercased message equals the
        // registered string, so `/model <preset>` would never reach the
        // handler and no argument would ever be parsed.
        let mut router = CommandRouter::new();
        router.prefix("/model", Arc::new(ArgsEchoHandler));

        let mut ctx = CommandContext::new(sample_msg(), None, "model", "/model claude-fast");
        let out = router.dispatch(&mut ctx).await.unwrap();
        assert_eq!(ctx.args.trim(), "claude-fast");
        assert_eq!(out.content.trim(), "claude-fast");

        let mut bare_ctx = CommandContext::new(sample_msg(), None, "model", "/model");
        let bare_out = router.dispatch(&mut bare_ctx).await.unwrap();
        assert_eq!(bare_ctx.args.trim(), "");
        assert_eq!(bare_out.content.trim(), "");
    }
}
