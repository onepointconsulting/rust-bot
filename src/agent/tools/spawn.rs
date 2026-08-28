use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::{subagent::SubagentManager, tools::base::Tool};

/// Tool to spawn a subagent for background task execution.
pub struct SpawnTool {
    manager: Arc<SubagentManager>,
    origin_channel: Mutex<String>,
    origin_chat_id: Mutex<String>,
    session_key: Mutex<String>,
}

impl SpawnTool {
    pub fn new(manager: Arc<SubagentManager>) -> Self {
        Self {
            manager,
            origin_channel: Mutex::new("cli".to_string()),
            origin_chat_id: Mutex::new("direct".to_string()),
            session_key: Mutex::new("cli:direct".to_string()),
        }
    }

    /// Set the origin context for subagent announcements.
    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self.origin_channel.lock().unwrap() = channel.to_string();
        *self.origin_chat_id.lock().unwrap() = chat_id.to_string();
        *self.session_key.lock().unwrap() = format!("{channel}:{chat_id}");
    }
}

#[async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> String {
        "spawn".to_string()
    }

    fn description(&self) -> String {
        "Spawn a subagent to handle a task in the background. \
         Use this for complex or time-consuming tasks that can run independently. \
         The subagent will complete the task and report back when done. \
         For deliverables or existing projects, inspect the workspace first \
         and use a dedicated subdirectory when helpful."
            .to_string()
    }

    fn set_tool_context(&self, channel: &str, chat_id: &str, _message_id: Option<&str>) {
        self.set_context(channel, chat_id);
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for the subagent to complete",
                },
                "label": {
                    "type": "string",
                    "description": "Optional short label for the task (for display)",
                },
            },
            "required": ["task"],
        })
    }

    async fn execute(&self, params: &Value) -> String {
        let task = params.get("task").and_then(Value::as_str).unwrap_or("");
        if task.is_empty() {
            return "Error: missing required parameter 'task'".to_string();
        }
        let label = params.get("label").and_then(Value::as_str);

        let origin_channel = self.origin_channel.lock().unwrap().clone();
        let origin_chat_id = self.origin_chat_id.lock().unwrap().clone();
        let session_key = self.session_key.lock().unwrap().clone();

        Arc::clone(&self.manager).spawn(
            task,
            label,
            Some(origin_channel.as_str()),
            Some(origin_chat_id.as_str()),
            Some(session_key.as_str()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::tools::base::Tool,
        bus::queue::MessageBus,
        providers::base::{GenerationSettings, LLMProviderDyn, LLMResponse},
    };
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tempfile::TempDir;

    struct TestProvider {
        settings: GenerationSettings,
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
        ) -> LLMResponse {
            LLMResponse::new()
        }
    }

    #[test]
    fn set_context_updates_session_key() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(SubagentManager::new_simple(
            Arc::new(TestProvider {
                settings: GenerationSettings::new(),
            }),
            tmp.path().to_path_buf(),
            Arc::new(MessageBus::new()),
            4096,
        ));
        let tool = SpawnTool::new(manager);
        tool.set_context("telegram", "chat-42");

        assert_eq!(*tool.origin_channel.lock().unwrap(), "telegram");
        assert_eq!(*tool.origin_chat_id.lock().unwrap(), "chat-42");
        assert_eq!(*tool.session_key.lock().unwrap(), "telegram:chat-42");
    }

    #[test]
    fn set_tool_context_via_trait_delegates_to_set_context() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(SubagentManager::new_simple(
            Arc::new(TestProvider {
                settings: GenerationSettings::new(),
            }),
            tmp.path().to_path_buf(),
            Arc::new(MessageBus::new()),
            4096,
        ));
        let tool = SpawnTool::new(manager);
        Tool::set_tool_context(&tool, "telegram", "chat-42", Some("msg-1"));

        assert_eq!(*tool.origin_channel.lock().unwrap(), "telegram");
        assert_eq!(*tool.origin_chat_id.lock().unwrap(), "chat-42");
        assert_eq!(*tool.session_key.lock().unwrap(), "telegram:chat-42");
    }

    #[tokio::test]
    async fn execute_missing_task_returns_error() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(SubagentManager::new_simple(
            Arc::new(TestProvider {
                settings: GenerationSettings::new(),
            }),
            tmp.path().to_path_buf(),
            Arc::new(MessageBus::new()),
            4096,
        ));
        let tool = SpawnTool::new(manager);
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.starts_with("Error: missing required parameter 'task'"));
    }

    #[tokio::test]
    async fn execute_spawns_subagent_and_returns_ack() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(SubagentManager::new_simple(
            Arc::new(TestProvider {
                settings: GenerationSettings::new(),
            }),
            tmp.path().to_path_buf(),
            Arc::new(MessageBus::new()),
            4096,
        ));
        let tool = SpawnTool::new(Arc::clone(&manager));
        tool.set_context("cli", "direct");

        let result = tool
            .execute(&serde_json::json!({
                "task": "summarise logs",
                "label": "worker-1",
            }))
            .await;

        assert!(result.contains("worker-1"));
        assert!(result.contains("started"));
    }
}
