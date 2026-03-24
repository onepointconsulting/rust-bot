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
}