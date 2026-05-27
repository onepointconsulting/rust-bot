use std::{collections::{HashMap, HashSet}, path::PathBuf, sync::Arc};

use async_trait::async_trait;

use crate::{
    agent::{hook::{AgentHook, AgentHookContext}, runner::AgentRunner},
    bus::queue::MessageBus,
    config::schema::{ExecToolConfig, WebToolsConfig},
    providers::base::LLMProviderDyn,
};

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

struct SubagentManager {
    pub provider: Arc<dyn LLMProviderDyn>,
    pub workspace: PathBuf,
    pub bus: Arc<MessageBus>,
    pub max_tool_result_chars: usize,
    pub model: String,
    pub web_config: WebToolsConfig,
    pub exec_config: ExecToolConfig,
    pub restrict_to_workspace: bool,
    pub runner: AgentRunner,
    running_tasks: HashMap<String, tokio::task::JoinHandle<()>>,
    session_tasks: HashMap<String, HashSet<String>>,
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

        Self {
            provider: provider.clone(),
            workspace,
            bus,
            max_tool_result_chars,
            model: model.unwrap_or(provider.as_ref().get_default_model()),
            web_config: web_config.unwrap_or(WebToolsConfig::default()),
            exec_config: exec_config.unwrap_or(ExecToolConfig::default()),
            restrict_to_workspace: restrict_to_workspace.unwrap_or(false),
            runner: AgentRunner::new(provider.clone()),
            running_tasks: HashMap::new(),
            session_tasks: HashMap::new(),
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
    pub async fn spawn(
        &self,
        task: &str,
        label: Option<&str>,
        original_channel_option: Option<&str>,
        origin_chat_id_option: Option<&str>,
        session_key: Option<&str>,
    ) -> String {
        let original_channel = original_channel_option.unwrap_or("cli");
        let origin_chat_id = origin_chat_id_option.unwrap_or("direct");
        let task_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let display_label = label.unwrap_or( 
            format!("{}{}", task[..30].to_string(), if task.len() > 30 { "..." } else { "" }).as_str());
        let origin = serde_json::json!({
            "channel": original_channel,
            "chat_id": origin_chat_id,
        });
        "".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use super::SubagentHook;
    use crate::agent::hook::{AgentHook, AgentHookContext};
    use crate::providers::base::ToolCallRequest;

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
        ctx.tool_calls.push(make_tool_call("tool_a", HashMap::new()));
        ctx.tool_calls.push(make_tool_call("tool_b", HashMap::new()));
        ctx.tool_calls.push(make_tool_call("tool_c", HashMap::new()));
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
        ctx.tool_calls.push(make_tool_call("my_tool", HashMap::new()));
        let tool_count_before = ctx.tool_calls.len();
        hook.before_execute_tools(&mut ctx).await;
        assert_eq!(ctx.tool_calls.len(), tool_count_before, "hook must not modify tool_calls");
    }
}