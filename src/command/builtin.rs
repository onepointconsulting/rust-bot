use crate::{
    PKG_VERSION, bus::events::OutboundMessage, command::{CommandContext, CommandHandler, CommandRouter}, utils::{
        helpers::build_status_content, restart::restart_with_notice,
        searchusage::fetch_search_usage,
    }
};
use async_trait::async_trait;
use futures::FutureExt;
use std::{panic::AssertUnwindSafe, sync::Arc, time::Instant};

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

fn reply_no_loop(ctx: &CommandContext, command: &str) -> OutboundMessage {
    reply(
        ctx,
        format!("No agent available to execute command: {command}."),
    )
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

pub fn register_builtin_commands(router: &mut CommandRouter) {
    router.priority("/stop", Arc::new(CmdStop));
    router.priority("/restart", Arc::new(CmdRestart));
    router.priority("/status", Arc::new(CmdStatus));
    router.exact("/new", Arc::new(CmdNew));
    router.exact("/dream", Arc::new(CmdDream));
    router.exact("/status", Arc::new(CmdStatus));
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
        ));
        let out = CmdStop.handle(&stop_ctx(Some(loop_))).await;
        assert_eq!(out.content, "No active task to stop.");
    }
}
