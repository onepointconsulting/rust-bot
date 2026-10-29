/// Tool registry for dynamic tool management.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::agent::tools::base::{Tool, ToolExposure};
use crate::providers::cache_control::CACHE_BOUNDARY_KEY;

/// Name prefix every MCP-provided tool is registered under.
pub const MCP_TOOL_PREFIX: &str = "mcp_";

const HINT: &str = "\n\n[Analyze the error above and try a different approach.]";

/// A deferred tool as `tool_search` ranks it: everything the model would see once it
/// is loaded, plus the MCP server it comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// `None` for a deferred built-in.
    pub server_name: Option<String>,
    /// Whether `tool_search` already loaded it in this view.
    pub loaded: bool,
}

/// Whether a listed tool can be called now, as shown in the tool lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolListExposure {
    /// Declared to the model on every request.
    Direct,
    /// Deferred and not loaded: `tool_search` can load it.
    Deferred,
    /// Deferred and loaded by `tool_search` in this session.
    Loaded,
}

impl ToolListExposure {
    /// Whether the model can call a tool with this exposure right now.
    pub fn is_callable(self) -> bool {
        self != Self::Deferred
    }
}

/// Display-oriented view of one registered tool (no parameter schema).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
    pub exposure: ToolListExposure,
}

/// One built-in tool as listed in the Settings dialog: its description and whether it
/// is active in this process (`false` when `tools.disabledTools` switched it off at
/// startup).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolCatalogueEntry {
    pub name: String,
    pub description: String,
    pub active: bool,
}

// ── ToolRegistry ──────────────────────────────────────────────────────────────
#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// Tools switched off by `tools.disabledTools`: not callable, but remembered (with
    /// their description) so the Settings dialog can still list and re-enable them.
    disabled: HashMap<String, ToolSummary>,
    /// Built-in tools deferred by `tools.deferredTools`. MCP tools carry their own
    /// exposure (see [`Tool::exposure`]).
    deferred_builtins: HashSet<String>,
    /// Deferred tools `tool_search` loaded in this view, in load order. `clone()` shares
    /// it, so the runner's copy and `tool_search`'s copy of one turn see the same loads;
    /// [`Self::restrict_by`] copies it, so a new view never writes into its source.
    loaded: Arc<Mutex<Vec<String>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            disabled: HashMap::new(),
            deferred_builtins: HashSet::new(),
            loaded: Arc::new(Mutex::new(Vec::new())),
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

    /// Switch the tool called `name` off: remove it from the callable tools but remember
    /// its summary for [`Self::catalogue`]. Does nothing for a tool that is not registered.
    pub fn disable(&mut self, name: &str) {
        if let Some(tool) = self.tools.remove(name) {
            self.disabled.insert(
                name.to_string(),
                ToolSummary {
                    name: tool.name(),
                    description: tool.description(),
                    exposure: ToolListExposure::Direct,
                },
            );
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    // ── deferred tools ────────────────────────────────────────────────────────

    /// Defer the built-in tools in `names` (`tools.deferredTools`): they stay registered
    /// and callable once loaded, but are only declared after `tool_search` loads them.
    pub fn defer_builtins(&mut self, names: impl IntoIterator<Item = String>) {
        self.deferred_builtins.extend(names);
    }

    /// Exposure of the registered tool `name`; `None` when no such tool is registered.
    pub fn exposure_of(&self, name: &str) -> Option<ToolExposure> {
        self.tools
            .get(name)
            .map(|tool| self.exposure_of_tool(name, tool))
    }

    /// Whether any registered tool is deferred, i.e. `tool_search` has something to find.
    pub fn has_deferred_tools(&self) -> bool {
        self.tools
            .iter()
            .any(|(name, tool)| self.exposure_of_tool(name, tool) == ToolExposure::Deferred)
    }

    /// Load the deferred tools in `names`, so they are declared from the next
    /// [`Self::get_definitions`] on. Names that are not registered, not deferred or
    /// already loaded are skipped. Returns the names that were newly loaded, in order.
    pub fn load_tools(&self, names: &[String]) -> Vec<String> {
        let mut loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        let mut newly_loaded = Vec::new();
        for name in names {
            let is_deferred = self.exposure_of(name) == Some(ToolExposure::Deferred);
            if is_deferred && !loaded.contains(name) {
                loaded.push(name.clone());
                newly_loaded.push(name.clone());
            }
        }
        newly_loaded
    }

    /// Deferred tools loaded in this view that are still registered, in load order.
    pub fn loaded_tools(&self) -> Vec<String> {
        self.loaded_names()
            .into_iter()
            .filter(|name| self.exposure_of(name) == Some(ToolExposure::Deferred))
            .collect()
    }

    /// Every registered deferred tool, loaded or not, sorted by name: the corpus
    /// `tool_search` ranks over.
    pub fn deferred_definitions(&self) -> Vec<DeferredToolDefinition> {
        let loaded = self.loaded_names();
        let mut definitions: Vec<DeferredToolDefinition> = self
            .tools
            .iter()
            .filter(|(name, tool)| self.exposure_of_tool(name, tool) == ToolExposure::Deferred)
            .map(|(name, tool)| DeferredToolDefinition {
                name: name.clone(),
                description: tool.description(),
                parameters: tool.parameters(),
                server_name: tool.mcp_server_name(),
                loaded: loaded.contains(name),
            })
            .collect();
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        definitions
    }

    /// Whether the registered tool `name` is declared to the model now: it is direct,
    /// or deferred and loaded.
    fn is_declared(&self, name: &str, tool: &Arc<dyn Tool>) -> bool {
        self.exposure_of_tool(name, tool) == ToolExposure::Direct
            || self.loaded_names().iter().any(|loaded| loaded == name)
    }

    fn exposure_of_tool(&self, name: &str, tool: &Arc<dyn Tool>) -> ToolExposure {
        if self.deferred_builtins.contains(name) {
            ToolExposure::Deferred
        } else {
            tool.exposure()
        }
    }

    fn loaded_names(&self) -> Vec<String> {
        self.loaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    // ── schema export ─────────────────────────────────────────────────────────

    /// Get the definitions of the tools declared to the model, ordered for prompt
    /// caching: direct built-ins (alphabetical), direct MCP tools (alphabetical), then
    /// loaded deferred tools in load order, so a load only ever appends to the tail.
    ///
    /// When the registry has deferred tools, the last direct built-in carries
    /// [`CACHE_BOUNDARY_KEY`] so the Anthropic provider keeps a cache breakpoint on the
    /// stable prefix (a loaded built-in at the tail would otherwise be mistaken for it).
    pub fn get_definitions(&self) -> Vec<serde_json::Value> {
        let mut builtins: Vec<serde_json::Value> = Vec::new();
        let mut mcp_tools: Vec<serde_json::Value> = Vec::new();

        for (name, tool) in &self.tools {
            if self.exposure_of_tool(name, tool) == ToolExposure::Deferred {
                continue;
            }
            let schema = tool.to_schema();
            if Self::is_mcp_tool_name(name) {
                mcp_tools.push(schema);
            } else {
                builtins.push(schema);
            }
        }

        builtins.sort_by_key(Self::schema_name);
        mcp_tools.sort_by_key(Self::schema_name);
        if self.has_deferred_tools()
            && let Some(last_builtin) = builtins.last_mut()
        {
            last_builtin[CACHE_BOUNDARY_KEY] = serde_json::Value::Bool(true);
        }
        builtins.extend(mcp_tools);
        builtins.extend(
            self.loaded_tools()
                .iter()
                .filter_map(|name| self.tools.get(name))
                .map(|tool| tool.to_schema()),
        );
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
            Some(t) if self.is_declared(name, t) => t.clone(),
            Some(_) => {
                return (
                    None,
                    params.clone(),
                    Some(format!(
                        "Error: Tool '{name}' is not loaded. Call tool_search first."
                    )),
                );
            }
            None => {
                // Only declared tools are listed, so the deferred catalogue does not leak.
                let available = self.declared_tool_names().join(", ");
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

    /// Names of the tools declared to the model now (direct, or deferred and loaded),
    /// sorted A–Z.
    pub fn declared_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .iter()
            .filter(|(name, tool)| self.is_declared(name, tool))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Name, description and exposure of every registered tool, sorted by name so the
    /// order is stable across calls (the backing map is unordered). A deferred tool is
    /// `Loaded` once loaded in this view (a session view is seeded with its loads).
    pub fn tool_summaries(&self) -> Vec<ToolSummary> {
        let loaded = self.loaded_names();
        let mut summaries: Vec<ToolSummary> = self
            .tools
            .iter()
            .map(|(name, tool)| ToolSummary {
                name: tool.name(),
                description: tool.description(),
                exposure: match self.exposure_of_tool(name, tool) {
                    ToolExposure::Direct => ToolListExposure::Direct,
                    ToolExposure::Deferred if loaded.contains(name) => ToolListExposure::Loaded,
                    ToolExposure::Deferred => ToolListExposure::Deferred,
                },
            })
            .collect();
        summaries.sort_by(|a, b| a.name.cmp(&b.name));
        summaries
    }

    /// Every built-in (non-MCP) tool this process knows about, active or switched off,
    /// sorted A–Z ignoring case. MCP tools are left out: servers are managed separately.
    pub fn catalogue(&self) -> Vec<ToolCatalogueEntry> {
        let active = self.tools.values().map(|tool| ToolCatalogueEntry {
            name: tool.name(),
            description: tool.description(),
            active: true,
        });
        let switched_off = self.disabled.values().map(|summary| ToolCatalogueEntry {
            name: summary.name.clone(),
            description: summary.description.clone(),
            active: false,
        });
        let mut entries: Vec<ToolCatalogueEntry> = active
            .chain(switched_off)
            .filter(|entry| !Self::is_mcp_tool_name(&entry.name))
            .collect();
        entries.sort_by_key(|entry| entry.name.to_lowercase());
        entries
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

    /// Return a registry exposing only the tools whose name satisfies `keep`.
    /// The original registry is untouched, so this is a per-call view.
    ///
    /// The deferred built-ins carry over. The loaded tools are copied (those that are
    /// kept) into a handle of the view's own, so loads in the view never reach the
    /// source: the process-wide registry is shared by every chat.
    pub fn restrict_by(&self, keep: impl Fn(&str) -> bool) -> ToolRegistry {
        let tools = self
            .tools
            .iter()
            .filter(|(name, _)| keep(name))
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect();
        let loaded = self
            .loaded_names()
            .into_iter()
            .filter(|name| keep(name))
            .collect();
        ToolRegistry {
            tools,
            disabled: HashMap::new(),
            deferred_builtins: self.deferred_builtins.clone(),
            loaded: Arc::new(Mutex::new(loaded)),
        }
    }

    /// Whether `name` is an MCP-provided tool (see [`MCP_TOOL_PREFIX`]).
    pub fn is_mcp_tool_name(name: &str) -> bool {
        name.starts_with(MCP_TOOL_PREFIX)
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

    /// A tool of the deferred MCP server `srv`.
    struct DeferredMcpTool(&'static str);

    #[async_trait]
    impl Tool for DeferredMcpTool {
        fn name(&self) -> String {
            self.0.to_string()
        }
        fn description(&self) -> String {
            format!("Deferred tool {}.", self.0)
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {}, "required": [] })
        }
        fn exposure(&self) -> ToolExposure {
            ToolExposure::Deferred
        }
        fn mcp_server_name(&self) -> Option<String> {
            Some("srv".to_string())
        }
        async fn execute(&self, _params: &serde_json::Value) -> String {
            "deferred result".to_string()
        }
    }

    fn registry_with_defaults() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool));
        reg.register(Box::new(AddTool));
        reg
    }

    /// Direct built-ins `add` and `echo`, direct MCP tool `mcp_search`, deferred MCP
    /// tools `mcp_srv_a` / `mcp_srv_b`, and the built-in `fail` deferred by config.
    fn registry_with_deferred() -> ToolRegistry {
        let mut reg = registry_with_defaults();
        reg.register(Box::new(McpSearchTool));
        reg.register(Box::new(DeferredMcpTool("mcp_srv_b")));
        reg.register(Box::new(DeferredMcpTool("mcp_srv_a")));
        reg.register(Box::new(FailTool));
        reg.defer_builtins(["fail".to_string()]);
        reg
    }

    fn definition_names(reg: &ToolRegistry) -> Vec<String> {
        reg.get_definitions()
            .iter()
            .map(ToolRegistry::schema_name)
            .collect()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
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
    fn test_tool_summaries_are_sorted_by_name_and_carry_descriptions() {
        let reg = registry_with_defaults();
        let summaries = reg.tool_summaries();
        let names: Vec<&str> = summaries.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["add", "echo"]);
        for summary in &summaries {
            let tool = reg.get(&summary.name).unwrap();
            assert_eq!(summary.description, tool.description());
            assert!(!summary.description.is_empty());
        }
    }

    #[test]
    fn test_tool_summaries_empty_registry() {
        assert!(ToolRegistry::new().tool_summaries().is_empty());
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
    fn restrict_by_keeping_everything_clones_the_full_catalog() {
        let reg = registry_with_defaults();
        let view = reg.restrict_by(|_| true);
        let mut names = view.tool_names();
        names.sort();
        assert_eq!(names, vec!["add", "echo"]);
    }

    #[test]
    fn restrict_by_can_drop_only_mcp_tools() {
        let mut reg = registry_with_defaults();
        reg.register(Box::new(McpSearchTool));
        let view = reg.restrict_by(|name| !ToolRegistry::is_mcp_tool_name(name));
        let mut names = view.tool_names();
        names.sort();
        assert_eq!(names, vec!["add", "echo"]);
        // The source registry still has the MCP tool.
        assert!(reg.has("mcp_search"));
    }

    #[test]
    fn is_mcp_tool_name_matches_the_prefix_only() {
        assert!(ToolRegistry::is_mcp_tool_name("mcp_search"));
        assert!(!ToolRegistry::is_mcp_tool_name("search_mcp"));
        assert!(!ToolRegistry::is_mcp_tool_name("echo"));
    }

    #[test]
    fn restrict_by_keeps_listed_tools_and_drops_the_rest() {
        let mut reg = registry_with_defaults();
        reg.register(Box::new(McpSearchTool));
        let view = reg.restrict_by(|name| name == "echo");
        assert!(view.has("echo"));
        assert!(!view.has("add"));
        assert!(!view.has("mcp_search"));
        let (tool, _, error) = view.prepare_call("add", &serde_json::json!({}));
        assert!(tool.is_none());
        assert!(error.unwrap().contains("not found"));
    }

    // ── catalogue tests ───────────────────────────────────────────────────────

    #[test]
    fn disable_removes_the_tool_but_keeps_it_in_the_catalogue() {
        let mut reg = registry_with_defaults();
        reg.disable("echo");

        assert!(!reg.has("echo"));
        let catalogue = reg.catalogue();
        let echo = catalogue.iter().find(|e| e.name == "echo").unwrap();
        assert!(!echo.active);
        assert_eq!(echo.description, "Echoes the input message.");
        assert!(catalogue.iter().find(|e| e.name == "add").unwrap().active);
    }

    #[test]
    fn disable_of_an_unknown_tool_is_a_noop() {
        let mut reg = registry_with_defaults();
        reg.disable("ghost");
        assert_eq!(reg.catalogue().len(), 2);
    }

    #[test]
    fn catalogue_excludes_mcp_tools_and_sorts_ignoring_case() {
        let mut reg = registry_with_defaults();
        reg.register(Box::new(McpSearchTool));
        reg.register(Box::new(FailTool));

        let names: Vec<String> = reg.catalogue().into_iter().map(|e| e.name).collect();

        assert_eq!(names, vec!["add", "echo", "fail"]);
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

    // ── deferred tools ────────────────────────────────────────────────────────

    #[test]
    fn exposure_comes_from_the_tool_or_the_deferred_builtins() {
        let reg = registry_with_deferred();
        assert_eq!(reg.exposure_of("echo"), Some(ToolExposure::Direct));
        assert_eq!(reg.exposure_of("mcp_srv_a"), Some(ToolExposure::Deferred));
        assert_eq!(reg.exposure_of("fail"), Some(ToolExposure::Deferred));
        assert_eq!(reg.exposure_of("ghost"), None);
        assert!(reg.has_deferred_tools());
        assert!(!registry_with_defaults().has_deferred_tools());
    }

    #[test]
    fn deferred_tools_are_not_declared_until_loaded() {
        let reg = registry_with_deferred();
        assert_eq!(
            definition_names(&reg),
            names(&["add", "echo", "mcp_search"])
        );
    }

    #[test]
    fn loaded_tools_are_appended_after_direct_mcp_in_load_order() {
        let reg = registry_with_deferred();
        reg.load_tools(&names(&["mcp_srv_b"]));
        reg.load_tools(&names(&["fail", "mcp_srv_a"]));

        assert_eq!(
            definition_names(&reg),
            names(&[
                "add",
                "echo",
                "mcp_search",
                "mcp_srv_b",
                "fail",
                "mcp_srv_a"
            ])
        );
    }

    #[test]
    fn load_tools_skips_unknown_direct_and_already_loaded_names() {
        let reg = registry_with_deferred();

        let newly_loaded = reg.load_tools(&names(&["mcp_srv_a", "echo", "ghost", "mcp_srv_a"]));

        assert_eq!(newly_loaded, names(&["mcp_srv_a"]));
        assert!(reg.load_tools(&names(&["mcp_srv_a"])).is_empty());
        assert_eq!(reg.loaded_tools(), names(&["mcp_srv_a"]));
    }

    #[test]
    fn loaded_tools_skips_a_name_that_is_no_longer_registered() {
        let mut reg = registry_with_deferred();
        reg.load_tools(&names(&["mcp_srv_a", "mcp_srv_b"]));

        reg.unregister("mcp_srv_a");

        assert_eq!(reg.loaded_tools(), names(&["mcp_srv_b"]));
        assert_eq!(
            definition_names(&reg),
            names(&["add", "echo", "mcp_search", "mcp_srv_b"])
        );
    }

    #[test]
    fn prepare_call_rejects_an_unloaded_deferred_tool_until_it_is_loaded() {
        let reg = registry_with_deferred();

        let (tool, _, error) = reg.prepare_call("mcp_srv_a", &serde_json::json!({}));
        assert!(tool.is_none());
        assert_eq!(
            error.unwrap(),
            "Error: Tool 'mcp_srv_a' is not loaded. Call tool_search first."
        );

        reg.load_tools(&names(&["mcp_srv_a"]));
        let (tool, _, error) = reg.prepare_call("mcp_srv_a", &serde_json::json!({}));
        assert!(tool.is_some());
        assert!(error.is_none());
    }

    #[test]
    fn unknown_tool_error_lists_declared_tools_only() {
        let reg = registry_with_deferred();

        let (_, _, error) = reg.prepare_call("ghost", &serde_json::json!({}));
        let error = error.unwrap();

        assert!(
            error.ends_with("Available: add, echo, mcp_search"),
            "{error}"
        );
    }

    #[test]
    fn clones_share_loads_and_restricted_views_copy_them() {
        let source = registry_with_deferred();
        let view = source.restrict_by(|_| true);
        let runner_copy = view.clone();

        runner_copy.load_tools(&names(&["mcp_srv_a"]));
        assert_eq!(view.loaded_tools(), names(&["mcp_srv_a"]));
        assert!(
            source.loaded_tools().is_empty(),
            "a view must never load into the process-wide registry"
        );

        let nested_view = view.restrict_by(|_| true);
        assert_eq!(nested_view.loaded_tools(), names(&["mcp_srv_a"]));
        nested_view.load_tools(&names(&["mcp_srv_b"]));
        assert_eq!(view.loaded_tools(), names(&["mcp_srv_a"]));
    }

    #[test]
    fn two_views_of_one_registry_do_not_see_each_others_loads() {
        let source = registry_with_deferred();
        let first_chat = source.restrict_by(|_| true);
        let second_chat = source.restrict_by(|_| true);

        first_chat.load_tools(&names(&["mcp_srv_a"]));

        assert!(second_chat.loaded_tools().is_empty());
    }

    #[test]
    fn restrict_by_carries_the_deferred_builtins() {
        let view = registry_with_deferred().restrict_by(|name| name != "add");
        assert_eq!(view.exposure_of("fail"), Some(ToolExposure::Deferred));
    }

    #[test]
    fn a_view_without_mcp_drops_loaded_mcp_tools_and_treats_them_as_unknown() {
        let view = registry_with_deferred().restrict_by(|_| true);
        view.load_tools(&names(&["mcp_srv_a", "fail"]));

        let no_mcp = view.restrict_by(|name| !ToolRegistry::is_mcp_tool_name(name));

        assert_eq!(no_mcp.loaded_tools(), names(&["fail"]));
        let (_, _, error) = no_mcp.prepare_call("mcp_srv_a", &serde_json::json!({}));
        assert!(error.unwrap().contains("Tool 'mcp_srv_a' not found"));
    }

    #[test]
    fn a_view_without_mcp_has_nothing_deferred_when_only_mcp_tools_were() {
        let mut reg = registry_with_defaults();
        reg.register(Box::new(DeferredMcpTool("mcp_srv_a")));

        let no_mcp = reg.restrict_by(|name| !ToolRegistry::is_mcp_tool_name(name));

        assert!(reg.has_deferred_tools());
        assert!(!no_mcp.has_deferred_tools());
    }

    #[test]
    fn deferred_definitions_are_sorted_and_carry_server_and_loaded_flag() {
        let reg = registry_with_deferred();
        reg.load_tools(&names(&["mcp_srv_b"]));

        let definitions = reg.deferred_definitions();

        let summary: Vec<(&str, Option<&str>, bool)> = definitions
            .iter()
            .map(|d| (d.name.as_str(), d.server_name.as_deref(), d.loaded))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("fail", None, false),
                ("mcp_srv_a", Some("srv"), false),
                ("mcp_srv_b", Some("srv"), true),
            ]
        );
        assert_eq!(definitions[1].description, "Deferred tool mcp_srv_a.");
        assert_eq!(definitions[1].parameters["type"], "object");
    }

    #[test]
    fn tool_summaries_carry_direct_deferred_and_loaded_exposure() {
        let reg = registry_with_deferred();
        reg.load_tools(&names(&["mcp_srv_a"]));

        let exposures: Vec<(String, ToolListExposure)> = reg
            .tool_summaries()
            .into_iter()
            .map(|summary| (summary.name, summary.exposure))
            .collect();

        assert_eq!(
            exposures,
            vec![
                ("add".to_string(), ToolListExposure::Direct),
                ("echo".to_string(), ToolListExposure::Direct),
                ("fail".to_string(), ToolListExposure::Deferred),
                ("mcp_search".to_string(), ToolListExposure::Direct),
                ("mcp_srv_a".to_string(), ToolListExposure::Loaded),
                ("mcp_srv_b".to_string(), ToolListExposure::Deferred),
            ]
        );
        assert!(ToolListExposure::Loaded.is_callable());
        assert!(!ToolListExposure::Deferred.is_callable());
    }

    #[test]
    fn declared_tool_names_are_direct_plus_loaded() {
        let reg = registry_with_deferred();
        reg.load_tools(&names(&["mcp_srv_a"]));
        assert_eq!(
            reg.declared_tool_names(),
            names(&["add", "echo", "mcp_search", "mcp_srv_a"])
        );
    }

    // ── cache boundary flag ───────────────────────────────────────────────────

    fn flagged_names(reg: &ToolRegistry) -> Vec<String> {
        reg.get_definitions()
            .iter()
            .filter(|definition| definition.get(CACHE_BOUNDARY_KEY).is_some())
            .map(ToolRegistry::schema_name)
            .collect()
    }

    #[test]
    fn cache_boundary_flags_only_the_last_direct_builtin() {
        let reg = registry_with_deferred();
        reg.load_tools(&names(&["fail"]));

        assert_eq!(flagged_names(&reg), names(&["echo"]));
        assert_eq!(reg.get_definitions()[1][CACHE_BOUNDARY_KEY], true);
    }

    #[test]
    fn cache_boundary_is_absent_without_deferred_tools_or_builtins() {
        assert!(flagged_names(&registry_with_defaults()).is_empty());

        let mut only_mcp = ToolRegistry::new();
        only_mcp.register(Box::new(McpSearchTool));
        only_mcp.register(Box::new(DeferredMcpTool("mcp_srv_a")));
        assert!(flagged_names(&only_mcp).is_empty());
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
