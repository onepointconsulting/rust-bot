use super::base::Tool;
use std::collections::HashMap;

pub struct RegistryTool {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl RegistryTool {
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

    /// Get all tool definitions in OpenAI format.
    pub fn get_definitions(&self) -> Vec<serde_json::Value> {
        self.tools
            .values()
            .map(|tool| {
                // Compose the OpenAI-format tool definition.
                serde_json::json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    "parameters": tool.parameters(),
                })
            })
            .collect()
    }

    pub fn execute(&self, name: &str, input: String) -> String {
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
        let result = tool.execute(&args);
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
}