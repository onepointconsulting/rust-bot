/// Tool registry for dynamic tool management.
use std::collections::HashMap;
use std::sync::Arc;

use crate::agent::tools::base::Tool;

const HINT: &str = "\n\n[Analyze the error above and try a different approach.]";

// ── ToolRegistry ──────────────────────────────────────────────────────────────
#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    // ── lifecycle ─────────────────────────────────────────────────────────────

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name();
        self.tools.insert(name, Arc::from(tool));
    }

    pub fn unregister(&mut self, name: &str) {
        self.tools.remove(name);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    // ── schema export ─────────────────────────────────────────────────────────

    /// Get tool definitions with stable ordering for cache-friendly prompts.
    ///
    /// Built-in tools (alphabetical) come first; MCP tools (`mcp_` prefix,
    /// also alphabetical) are appended.
    pub fn get_definitions(&self) -> Vec<serde_json::Value> {
        let mut builtins: Vec<serde_json::Value> = Vec::new();
        let mut mcp_tools: Vec<serde_json::Value> = Vec::new();

        for schema in self.tools.values().map(|t| t.to_schema()) {
            if Self::schema_name(&schema).starts_with("mcp_") {
                mcp_tools.push(schema);
            } else {
                builtins.push(schema);
            }
        }

        builtins.sort_by_key(|s| Self::schema_name(s));
        mcp_tools.sort_by_key(|s| Self::schema_name(s));
        builtins.extend(mcp_tools);
        builtins
    }

    // ── execution ─────────────────────────────────────────────────────────────

    /// Resolve, cast, and validate one tool call.
    ///
    /// Returns `(tool, cast_params, error)`. When `error` is `Some` the call
    /// should not proceed; when it is `None` the tool is ready to execute.
    pub fn prepare_call(
        &self,
        name: &str,
        params: &serde_json::Value,
    ) -> (Option<Arc<dyn Tool>>, serde_json::Value, Option<String>) {
        let tool = match self.tools.get(name) {
            Some(t) => t.clone(),
            None => {
                let available = self.tool_names().join(", ");
                return (
                    None,
                    params.clone(),
                    Some(format!(
                        "Error: Tool '{}' not found. Available: {}",
                        name, available
                    )),
                );
            }
        };

        let cast_params = tool.cast_params(params);
        let errors = tool.validate_params(&cast_params);
        if !errors.is_empty() {
            return (
                Some(tool),
                cast_params,
                Some(format!(
                    "Error: Invalid parameters for tool '{}': {}",
                    name,
                    errors.join("; ")
                )),
            );
        }

        (Some(tool), cast_params, None)
    }

    /// Execute a tool by name with the given parameters.
    ///
    /// Any error (unknown tool, invalid params, or a result beginning with
    /// `"Error"`) has `HINT` appended to nudge the LLM to self-correct.
    pub async fn execute(&self, name: &str, params: &serde_json::Value) -> String {
        let (tool, cast_params, error) = self.prepare_call(name, params);
        if let Some(err) = error {
            return err + HINT;
        }

        // prepare_call guarantees tool is Some when error is None
        let result = tool.unwrap().as_ref().execute(&cast_params).await;
        if result.starts_with("Error") {
            result + HINT
        } else {
            result
        }
    }

    // ── helpers / introspection ───────────────────────────────────────────────

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Return a registry that exposes only `allow`, or `self` cloned when
    /// `allow` is `None` (Standard: every registered tool stays visible).
    pub fn restrict(&self, allow: Option<&[&str]>) -> ToolRegistry {
        let Some(allow) = allow else {
            return self.clone();
        };
        let mut tools = HashMap::new();
        for name in allow {
            if let Some(tool) = self.tools.get(*name) {
                tools.insert((*name).to_string(), tool.clone());
            }
        }
        ToolRegistry { tools }
    }

    /// Extract a normalised tool name from either OpenAI-wrapped or flat schemas.
    fn schema_name(schema: &serde_json::Value) -> String {
        if let Some(name) = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
        {
            return name.to_string();
        }
        schema
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::base::Tool;
    use async_trait::async_trait;

    // ── test fixtures ─────────────────────────────────────────────────────────

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> String {
            "echo".to_string()
        }
        fn description(&self) -> String {
            "Echoes the input message.".to_string()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            })
        }
        async fn execute(&self, params: &serde_json::Value) -> String {
            params["message"].as_str().unwrap_or("").to_string()
        }
    }

    struct AddTool;

    #[async_trait]
    impl Tool for AddTool {
        fn name(&self) -> String {
            "add".to_string()
        }
        fn description(&self) -> String {
            "Adds two integers.".to_string()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "a": { "type": "integer" },
                    "b": { "type": "integer" }
                },
                "required": ["a", "b"]
            })
        }
        async fn execute(&self, params: &serde_json::Value) -> String {
            let a = params["a"].as_i64().unwrap_or(0);
            let b = params["b"].as_i64().unwrap_or(0);
            (a + b).to_string()
        }
    }

    /// A tool whose name starts with `mcp_` to test schema ordering.
    struct McpSearchTool;

    #[async_trait]
    impl Tool for McpSearchTool {
        fn name(&self) -> String {
            "mcp_search".to_string()
        }
        fn description(&self) -> String {
            "MCP search tool.".to_string()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {}, "required": [] })
        }
        async fn execute(&self, _params: &serde_json::Value) -> String {
            "results".to_string()
        }
    }

    /// A tool that always returns an error string.
    struct FailTool;

    #[async_trait]
    impl Tool for FailTool {
        fn name(&self) -> String {
            "fail".to_string()
        }
        fn description(&self) -> String {
            "Always errors.".to_string()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {}, "required": [] })
        }
        async fn execute(&self, _params: &serde_json::Value) -> String {
            "Error: something went wrong".to_string()
        }
    }

    fn registry_with_defaults() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool));
        reg.register(Box::new(AddTool));
        reg
    }

    // ── lifecycle tests ───────────────────────────────────────────────────────

    #[test]
    fn test_register_and_has() {
        let reg = registry_with_defaults();
        assert!(reg.has("echo"));
        assert!(reg.has("add"));
        assert!(!reg.has("missing"));
    }

    #[test]
    fn test_get_returns_tool() {
        let reg = registry_with_defaults();
        assert!(reg.get("echo").is_some());
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn test_unregister_removes_tool() {
        let mut reg = registry_with_defaults();
        reg.unregister("echo");
        assert!(!reg.has("echo"));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_unregister_nonexistent_is_noop() {
        let mut reg = registry_with_defaults();
        reg.unregister("does_not_exist"); // must not panic
        assert_eq!(reg.len(), 2);
    }

    // ── introspection tests ───────────────────────────────────────────────────

    #[test]
    fn test_len_and_is_empty() {
        let mut reg = ToolRegistry::new();
        assert!(reg.is_empty());
        reg.register(Box::new(EchoTool));
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn test_contains() {
        let reg = registry_with_defaults();
        assert!(reg.contains("echo"));
        assert!(!reg.contains("xyz"));
    }

    #[test]
    fn test_tool_names_contains_registered() {
        let reg = registry_with_defaults();
        let mut names = reg.tool_names();
        names.sort();
        assert_eq!(names, vec!["add", "echo"]);
    }

    #[test]
    fn restrict_none_clones_the_full_catalog() {
        let reg = registry_with_defaults();
        let view = reg.restrict(None);
        let mut names = view.tool_names();
        names.sort();
        assert_eq!(names, vec!["add", "echo"]);
    }

    #[test]
    fn restrict_keep_listed_tools_and_drops_the_rest() {
        let mut reg = registry_with_defaults();
        reg.register(Box::new(McpSearchTool));
        let view = reg.restrict(Some(&["echo"]));
        assert!(view.has("echo"));
        assert!(!view.has("add"));
        assert!(!view.has("mcp_search"));
        let (tool, _, error) = view.prepare_call("add", &serde_json::json!({}));
        assert!(tool.is_none());
        assert!(error.unwrap().contains("not found"));
    }

    // ── get_definitions ordering tests ───────────────────────────────────────

    #[test]
    fn test_get_definitions_builtins_before_mcp() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(McpSearchTool)); // mcp_ prefix
        reg.register(Box::new(EchoTool)); // builtin
        reg.register(Box::new(AddTool)); // builtin

        let defs = reg.get_definitions();
        assert_eq!(defs.len(), 3);

        // First two must be the builtins (alphabetically sorted)
        assert_eq!(defs[0]["function"]["name"], "add");
        assert_eq!(defs[1]["function"]["name"], "echo");
        // Last one is the mcp_ tool
        assert_eq!(defs[2]["function"]["name"], "mcp_search");
    }

    #[test]
    fn test_get_definitions_schema_shape() {
        let reg = registry_with_defaults();
        for def in reg.get_definitions() {
            assert_eq!(def["type"], "function");
            assert!(def["function"]["name"].is_string());
            assert!(def["function"]["description"].is_string());
        }
    }

    // ── prepare_call tests ────────────────────────────────────────────────────

    #[test]
    fn test_prepare_call_unknown_tool() {
        let reg = registry_with_defaults();
        let (tool, _, error) = reg.prepare_call("ghost", &serde_json::json!({}));
        assert!(tool.is_none());
        let err = error.unwrap();
        assert!(err.contains("Tool 'ghost' not found"));
        assert!(err.contains("Available:"));
    }

    #[test]
    fn test_prepare_call_missing_required_param() {
        let reg = registry_with_defaults();
        let (tool, _, error) = reg.prepare_call("echo", &serde_json::json!({}));
        assert!(tool.is_some());
        let err = error.unwrap();
        assert!(err.contains("Invalid parameters"));
        assert!(err.contains("missing required message"));
    }

    #[test]
    fn test_prepare_call_valid_params() {
        let reg = registry_with_defaults();
        let params = serde_json::json!({ "message": "hello" });
        let (tool, cast, error) = reg.prepare_call("echo", &params);
        assert!(tool.is_some());
        assert!(error.is_none());
        assert_eq!(cast["message"], "hello");
    }

    // ── execute tests ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_execute_unknown_tool_appends_hint() {
        let reg = registry_with_defaults();
        let result = reg.execute("ghost", &serde_json::json!({})).await;
        assert!(result.contains("Tool 'ghost' not found"));
        assert!(result.contains("[Analyze the error above"));
    }

    #[tokio::test]
    async fn test_execute_invalid_params_appends_hint() {
        let reg = registry_with_defaults();
        let result = reg.execute("echo", &serde_json::json!({})).await;
        assert!(result.contains("Invalid parameters"));
        assert!(result.contains("[Analyze the error above"));
    }

    #[tokio::test]
    async fn test_execute_success() {
        let reg = registry_with_defaults();
        let result = reg
            .execute("echo", &serde_json::json!({ "message": "hi" }))
            .await;
        assert_eq!(result, "hi");
    }

    #[tokio::test]
    async fn test_execute_add_tool() {
        let reg = registry_with_defaults();
        let result = reg
            .execute("add", &serde_json::json!({ "a": 3, "b": 7 }))
            .await;
        assert_eq!(result, "10");
    }

    #[tokio::test]
    async fn test_execute_tool_error_result_appends_hint() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(FailTool));
        let result = reg.execute("fail", &serde_json::json!({})).await;
        assert!(result.starts_with("Error: something went wrong"));
        assert!(result.contains("[Analyze the error above"));
    }
}
