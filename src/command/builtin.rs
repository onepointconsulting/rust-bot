use crate::{
    bus::events::OutboundMessage,
    command::{CommandContext, CommandHandler, CommandRouter}, utils::restart::restart_with_notice,
};
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};

struct CmdStop;

/// Cancel all active tasks and subagents for the session.
#[async_trait]
impl CommandHandler for CmdStop {
    async fn handle(&self, ctx: &CommandContext) -> Option<OutboundMessage> {
        if let Some(agent_loop) = &ctx.agent_loop {
            let agent_loop = Arc::clone(agent_loop);
            let msg = ctx.msg.clone();
            let session_key = msg.session_key();
            let tasks = agent_loop
                .active_tasks
                .lock()
                .await
                .remove(&session_key)
                .unwrap_or_default();
            let mut cancelled: u32 = 0;
            for handle in tasks {
                handle.abort();
                cancelled += 1;
            }
            let sub_cancelled = agent_loop
                .subagents
                .cancel_by_session(&session_key)
                .await;
            let total = cancelled + sub_cancelled;
            let content = if total > 0 {
                format!("Stopped {total} task(s).")
            } else {
                "No active task to stop.".to_string()
            };
            return Some(OutboundMessage {
                channel: ctx.msg.channel.clone(),
                chat_id: ctx.msg.chat_id.clone(),
                content,
                reply_to: None,
                media: vec![],
                metadata: if msg.metadata.is_empty() {
                    HashMap::new()
                } else {
                    msg.metadata.clone()
                },
            });
        }
        None
    }
}

struct CmdRestart;

/// Restart the process in-place via exec/spawn after a short delay.
#[async_trait]
impl CommandHandler for CmdRestart {
    async fn handle(&self, ctx: &CommandContext) -> Option<OutboundMessage> {
        let Some(agent_loop) = &ctx.agent_loop else {
            return None;
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
                    metadata: Default::default(),
                });
            }
        });
        Some(OutboundMessage {
            channel: msg.channel,
            chat_id: msg.chat_id,
            content: "Restarting...".to_string(),
            reply_to: None,
            media: vec![],
            metadata: Default::default(),
        })
    }
}

pub fn register_builtin_commands(router: &mut CommandRouter) {
    router.priority("/stop", Arc::new(CmdStop));
    router.priority("/restart", Arc::new(CmdRestart));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::bus::events::InboundMessage;

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
    async fn handle_without_agent_loop_returns_none() {
        let out = CmdStop.handle(&stop_ctx(None)).await;
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn handle_with_agent_loop_and_no_tasks_reports_none_active() {
        use crate::providers::base::{GenerationSettings, LLMProviderDyn, LLMResponse};

        struct TestProvider;
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
                static SETTINGS: std::sync::OnceLock<GenerationSettings> = std::sync::OnceLock::new();
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
        let out = CmdStop.handle(&stop_ctx(Some(loop_))).await.unwrap();
        assert_eq!(out.content, "No active task to stop.");
    }
}
