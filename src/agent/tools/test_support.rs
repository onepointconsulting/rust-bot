//! Fake tools shared by the unit tests of the runner and the tool pipeline.

use std::time::Duration;

use async_trait::async_trait;

use crate::agent::runner::AgentRunSpec;
use crate::agent::tools::base::{Tool, ToolResult};
use crate::agent::tools::registry::ToolRegistry;

/// Tool `scripted` that requires a `path` argument and answers with a fixed reply.
pub(crate) struct ScriptedTool {
    pub reply: String,
}

#[async_trait]
impl Tool for ScriptedTool {
    fn name(&self) -> String {
        "scripted".into()
    }
    fn description(&self) -> String {
        "scripted".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        })
    }
    async fn execute(&self, _: &serde_json::Value) -> String {
        self.reply.clone()
    }
}

/// Tool `slow` that sleeps for `delay` and then answers `"slow done"`.
pub(crate) struct SlowTool {
    pub delay: Duration,
    /// Reported as `exempt_from_execute_timeout`.
    pub exempt: bool,
}

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> String {
        "slow".into()
    }
    fn description(&self) -> String {
        "slow".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn exempt_from_execute_timeout(&self) -> bool {
        self.exempt
    }
    async fn execute(&self, _: &serde_json::Value) -> String {
        tokio::time::sleep(self.delay).await;
        "slow done".into()
    }
}

/// Tool `panicky` whose `execute` panics with `"boom"`.
pub(crate) struct PanickingTool {
    /// Panic with a `String` payload (`panic!("{}", ..)`) instead of a `&str` one.
    pub string_payload: bool,
}

#[async_trait]
impl Tool for PanickingTool {
    fn name(&self) -> String {
        "panicky".into()
    }
    fn description(&self) -> String {
        "panicky".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: &serde_json::Value) -> String {
        if self.string_payload {
            panic!("{}", String::from("boom"));
        }
        panic!("boom");
    }
}

/// Tool `flagged` that answers `content` and says by flag whether it failed.
///
/// Its `execute` text deliberately disagrees with the flag's legacy reading
/// only through `execute_result`, which is what the pipeline calls.
pub(crate) struct FlaggedTool {
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
impl Tool for FlaggedTool {
    fn name(&self) -> String {
        "flagged".into()
    }
    fn description(&self) -> String {
        "flagged".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: &serde_json::Value) -> String {
        self.content.clone()
    }
    async fn execute_result(&self, _: &serde_json::Value) -> ToolResult {
        ToolResult {
            content: self.content.clone(),
            is_error: self.is_error,
        }
    }
}

/// Tool called `name` that accepts any arguments and answers `"ok"`.
pub(crate) struct NamedTool {
    pub name: String,
}

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn description(&self) -> String {
        self.name.clone()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: &serde_json::Value) -> String {
        "ok".into()
    }
}

/// A run spec whose registry holds only `tool`.
pub(crate) fn spec_with_tool(tool: impl Tool + 'static) -> AgentRunSpec {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(tool));
    AgentRunSpec {
        tools,
        ..Default::default()
    }
}
