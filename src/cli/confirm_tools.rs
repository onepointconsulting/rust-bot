use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::lock::Mutex;
use inquire::Confirm;

use crate::agent::hook::{AgentHook, AgentHookContext, ToolHookDecision};
use crate::cli::cancel::pause_for_prompt;
use crate::cli::stream::StreamRenderer;
use crate::providers::base::ToolCallRequest;

const ARG_PREVIEW_LIMIT: usize = 120;

/// CLI-only hook that asks before each tool call when stdin is a terminal.
pub struct CliAskHook {
    renderer: StdMutex<Option<Arc<Mutex<StreamRenderer>>>>,
}

impl CliAskHook {
    pub fn new() -> Self {
        Self {
            renderer: StdMutex::new(None),
        }
    }

    pub fn set_renderer(&self, renderer: Arc<Mutex<StreamRenderer>>) {
        *self.renderer.lock().unwrap_or_else(|e| e.into_inner()) = Some(renderer);
    }

    pub fn clear_renderer(&self) {
        *self.renderer.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn renderer(&self) -> Option<Arc<Mutex<StreamRenderer>>> {
        self.renderer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

fn preview_arguments(arguments: &HashMap<String, serde_json::Value>) -> String {
    let args_str = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string());
    let mut chars = args_str.chars();
    let preview: String = chars.by_ref().take(ARG_PREVIEW_LIMIT).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn prompt_for_tool(name: &str, args_preview: &str) -> Result<bool, inquire::InquireError> {
    let message = if args_preview == "{}" {
        format!("Execute tool `{name}`?")
    } else {
        format!("Execute tool `{name}` ({args_preview})?")
    };
    Confirm::new(&message)
        .with_default(true)
        .with_help_message("y to run this call, n to skip it")
        .prompt()
}

fn prompt_tool_calls(tool_calls: &[ToolCallRequest]) -> Vec<String> {
    let mut denied_ids = Vec::new();
    for tool_call in tool_calls {
        let args_preview = preview_arguments(&tool_call.arguments);
        let approved = prompt_for_tool(&tool_call.name, &args_preview);
        if !matches!(approved, Ok(true)) {
            denied_ids.push(tool_call.id.clone());
        }
    }
    denied_ids
}

fn decision_from_denied_ids(ids: Vec<String>) -> ToolHookDecision {
    if ids.is_empty() {
        ToolHookDecision::Continue
    } else {
        ToolHookDecision::DenyCalls {
            ids,
            reason: "User denied tool execution".into(),
        }
    }
}

#[async_trait]
impl AgentHook for CliAskHook {
    async fn before_execute_tools(&self, context: &mut AgentHookContext) -> ToolHookDecision {
        if context.tool_calls.is_empty() {
            return ToolHookDecision::Continue;
        }

        if !std::io::stdin().is_terminal() {
            log::warn!(
                "tools.confirmBeforeExecute is enabled but stdin is not a terminal; executing tools without prompts"
            );
            return ToolHookDecision::Continue;
        }

        if let Some(renderer) = self.renderer() {
            renderer.lock().await.stop_for_input();
        }
        let _pause = pause_for_prompt();
        let tool_calls = context.tool_calls.clone();
        let denied_ids = tokio::task::spawn_blocking(move || prompt_tool_calls(&tool_calls))
            .await
            .unwrap_or_else(|_| {
                context
                    .tool_calls
                    .iter()
                    .map(|tool_call| tool_call.id.clone())
                    .collect()
            });
        drop(_pause);
        if let Some(renderer) = self.renderer() {
            renderer.lock().await.resume_after_input();
        }
        decision_from_denied_ids(denied_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_call(
        id: &str,
        name: &str,
        arguments: HashMap<String, serde_json::Value>,
    ) -> ToolCallRequest {
        ToolCallRequest {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }
    }

    #[test]
    fn preview_arguments_truncates_long_payloads() {
        let mut arguments = HashMap::new();
        arguments.insert(
            "content".to_string(),
            serde_json::Value::String("x".repeat(200)),
        );
        let preview = preview_arguments(&arguments);
        assert!(preview.ends_with("..."));
        assert!(preview.chars().count() <= ARG_PREVIEW_LIMIT + 3);
    }

    #[test]
    fn preview_arguments_keeps_short_payloads() {
        let mut arguments = HashMap::new();
        arguments.insert("path".to_string(), serde_json::json!("/tmp/foo.txt"));
        assert_eq!(
            preview_arguments(&arguments),
            serde_json::to_string(&arguments).unwrap()
        );
    }

    #[test]
    fn decision_from_denied_ids_continue_when_empty() {
        assert_eq!(
            decision_from_denied_ids(Vec::new()),
            ToolHookDecision::Continue
        );
    }

    #[test]
    fn decision_from_denied_ids_collects_only_declined_calls() {
        assert_eq!(
            decision_from_denied_ids(vec!["call_2".into(), "call_3".into()]),
            ToolHookDecision::DenyCalls {
                ids: vec!["call_2".into(), "call_3".into()],
                reason: "User denied tool execution".into(),
            }
        );
    }

    #[tokio::test]
    async fn skips_prompts_when_stdin_is_not_a_terminal() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let hook = CliAskHook::new();
        let mut ctx = AgentHookContext::new(0, vec![]);
        ctx.tool_calls.push(make_tool_call(
            "call_1",
            "read_file",
            HashMap::from([("path".to_string(), serde_json::json!("/tmp/foo.txt"))]),
        ));
        let decision = hook.before_execute_tools(&mut ctx).await;
        assert_eq!(decision, ToolHookDecision::Continue);
        assert_eq!(ctx.tool_calls.len(), 1);
    }
}
