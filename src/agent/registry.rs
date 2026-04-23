use async_trait::async_trait;
use crate::agent::tools::base::Tool;
use std::collections::HashMap;

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }

    /// Register a tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name();
        self.tools.insert(name, tool);
    }

    /// Unregister a tool by name.
    pub fn unregister(&mut self, name: &str) {
        self.tools.remove(name);
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<&Box<dyn Tool>> {
        self.tools.get(name)
    }

    /// Check if a tool is registered.
    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Get all tool definitions in OpenAI format with stable ordering for
    /// cache-friendly prompts.
    ///
    /// Built-in tools (no `mcp_` prefix) are sorted and placed first; MCP
    /// tools are sorted and appended — mirroring the Python implementation.
    pub fn get_definitions(&self) -> Vec<serde_json::Value> {
        let schema_name = |s: &serde_json::Value| -> String {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string()
        };

        let mut builtins: Vec<serde_json::Value> = Vec::new();
        let mut mcp_tools: Vec<serde_json::Value> = Vec::new();

        for tool in self.tools.values() {
            let schema = tool.to_schema();
            if schema_name(&schema).starts_with("mcp_") {
                mcp_tools.push(schema);
            } else {
                builtins.push(schema);
            }
        }

        builtins.sort_by(|a, b| schema_name(a).cmp(&schema_name(b)));
        mcp_tools.sort_by(|a, b| schema_name(a).cmp(&schema_name(b)));
        builtins.extend(mcp_tools);
        builtins
    }

    pub async fn execute(&self, name: &str, input: String) -> String {
        let tool_res = self.tools.get(name);
        if tool_res.is_none() {
            return format!("Tool {} not found. Available tools: {:?}", name, self.tools.keys());
        }
        let tool = tool_res.unwrap();
        let args: serde_json::Value = match serde_json::from_str(&input) {
            Ok(v) => v,
            Err(_) => return format!("Error: invalid JSON input: {}", input),
        };
        tool.cast_params(&args);
        let errors = tool.validate_params(&args);
        let l_hint = "\n\n[Analyze the error above and try a different approach.]";
        if !errors.is_empty() {
            return format!("Error: invalid parameters for tool {}: {:?}{l_hint}", name, errors);
        }
        let result = tool.execute(&args).await;
        if result.starts_with("Error:") {
            return format!("{result}{l_hint}");
        }
        return result;
    }

    /// Get list of registered tool names.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Returns the number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Checks if a tool with the given name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Resolve, cast, and validate one tool call
    pub fn prepare_call(
        &self,
        name: &str,
        params: &serde_json::Value,
    ) -> (Option<&dyn Tool>, serde_json::Value, Option<String>) {
        
        if let Some(tool) = self.tools.get(name) {
            let cast_params = tool.cast_params(params);
            let errors = tool.validate_params(&cast_params);
            if !errors.is_empty() {
                return (Some(tool.as_ref()), cast_params, Option::Some(format!("Error: Invalid parameters for tool '{}': {}", name, errors.join("; "))));
            }
            return (Some(tool.as_ref()), cast_params, Option::None);
        }
        return (Option::None, params.clone(), Option::Some(format!("Error: Tool '{}' not found. Available: {}", name, self.tool_names().join(", "))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::base::Tool;

    struct SampleTool;

    #[async_trait]
    impl Tool for SampleTool {
        fn name(&self) -> String {
            "sample".to_string()
        }

        fn description(&self) -> String {
            "sample tool".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 2 },
                },
                "required": ["query"],
            })
        }
        
        async fn execute(&self, _params: &serde_json::Value) -> String {
            "ok".to_string()
        }
    }

    #[tokio::test]
    async fn test_registry_returns_validation_ok() {
        let tool = SampleTool;
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(tool));
        let result = registry.execute("sample", "{\"query\": \"hello\"}".to_string()).await;
        println!("result: {}", result);
        assert_eq!(result, "ok");
    }

    #[tokio::test]
    async fn test_registry_returns_validation_error() {
        let tool = SampleTool;
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(tool));
        let result = registry.execute("sample", "{\"name\": \"john\"}".to_string()).await;
        println!("result: {}", result);
        assert!(result.trim().contains("Error: invalid parameters for tool sample: [\"missing required query\"]"));
    }
}

    