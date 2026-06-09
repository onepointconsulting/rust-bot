use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::agent::agent_loop::AgentLoop;
use crate::bus::events::{InboundMessage, OutboundMessage};
use crate::session::manager::Session;
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
            }
        }
    }

    #[tokio::test]
    async fn dispatch_priority_matches_raw_case_insensitive() {
        let mut router = CommandRouter::new();
        router.priority(
            "/stop",
            Arc::new(StaticReplyHandler("stopped")),
        );

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
}

