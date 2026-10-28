//! `tool_search`: find deferred tools with BM25 and load the matches, so they are
//! declared to the model from its next call.
//!
//! Registered per turn by the agent loop, and only when the turn's tool view has at
//! least one deferred tool. It holds a clone of that view, which shares the view's
//! loaded-tools handle (see [`ToolRegistry::load_tools`]): a load here is visible to the
//! runner's `get_definitions()` on the next iteration.
//!
//! There is no `ToolSearchConfig` enable flag: the `exposure` / `deferredTools` settings
//! already decide whether the tool exists. It never blocks on user input.

use std::collections::{BTreeMap, HashMap, HashSet};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::tools::base::Tool;
use crate::agent::tools::registry::{DeferredToolDefinition, MCP_TOOL_PREFIX, ToolRegistry};
use crate::agent::tools::tool_ranker::{create_tool_search_document, rank_tools};
pub use crate::config::schema::TOOL_SEARCH_TOOL_NAME;

/// Matches returned when the model gives no `limit`.
pub const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;
/// Upper bound on `limit`, so a broad query cannot load the whole catalogue.
pub const MAX_TOOL_SEARCH_LIMIT: usize = 20;
/// A match is loaded only if it scores at least this share of the best match (or the
/// query names it). Weaker matches are listed without loading, so a vague query on a
/// server with many look-alike tools does not fill the tools array with noise.
pub const LOAD_SCORE_RATIO: f64 = 0.5;
/// Example tool names shown per deferred server in the prompt section, so the model sees
/// the server's naming pattern and can search by name (an exact name ranks first).
const MAX_EXAMPLE_TOOL_NAMES: usize = 5;

/// Static on purpose: it lists no tools, so the declared `tools` array does not change
/// when MCP servers connect.
const TOOL_SEARCH_DESCRIPTION: &str = "Search for tools that are not loaded yet and load the \
     matches, so you can call them from your next step. Tools of some MCP servers and other \
     optional tools are not provided to you upfront; when a request needs a capability you do \
     not have, search for it here before saying you cannot do it. Describe the capability in a \
     few keywords (e.g. \"create jira issue\"), or give a tool's exact name.";

/// Opening of the system-prompt section that lists deferred tools. It is an instruction,
/// not only a label: models otherwise answer "I cannot" without ever searching.
const DEFERRED_TOOLS_SECTION_INTRO: &str = "## Optional tools\n\nThese tools are not loaded yet. \
     If a request needs a capability you do not have, call `tool_search` before saying you \
     cannot do it. Search with a few keywords, or with a tool name following the pattern of \
     the examples (an exact name is found first):";

/// System-prompt section naming what `tool_search` can load in `view` (the session's
/// mode view): one line per deferred MCP server, with its tool count, a few example tool
/// names and its configured description, and one line per deferred built-in, with its
/// first description line.
/// `None` when nothing is deferred, so a setup without deferral gets no extra text.
///
/// Only registered tools count, so a server is listed once it has connected.
/// `server_descriptions` maps server names to `mcpServers.<name>.description`.
pub fn deferred_tools_prompt_section(
    view: &ToolRegistry,
    server_descriptions: &HashMap<String, String>,
) -> Option<String> {
    let deferred = view.deferred_definitions();
    if deferred.is_empty() {
        return None;
    }
    // `deferred_definitions` is sorted by name, so each server's names are too.
    let mut tool_names_by_server: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut builtin_lines = Vec::new();
    for tool in &deferred {
        match &tool.server_name {
            Some(server_name) => tool_names_by_server
                .entry(server_name)
                .or_default()
                .push(&tool.name),
            None => builtin_lines.push(format!(
                "- Tool `{}`: {}",
                tool.name,
                first_line(&tool.description)
            )),
        }
    }
    let server_lines = tool_names_by_server
        .into_iter()
        .map(|(server_name, tool_names)| {
            let plural = if tool_names.len() == 1 { "" } else { "s" };
            let examples = example_tool_names(server_name, &tool_names)
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let summary = format!(
                "- MCP server `{server_name}` ({} tool{plural}, e.g. {examples})",
                tool_names.len()
            );
            match server_descriptions
                .get(server_name)
                .map(|description| description.trim())
                .filter(|description| !description.is_empty())
            {
                Some(description) => format!("{summary}: {description}"),
                None => summary,
            }
        });
    let lines: Vec<String> = server_lines.chain(builtin_lines).collect();
    Some(format!(
        "{DEFERRED_TOOLS_SECTION_INTRO}\n{}",
        lines.join("\n")
    ))
}

/// Up to [`MAX_EXAMPLE_TOOL_NAMES`] names of `server_name`'s tools, without the
/// `mcp_{server}_` prefix, one per distinct leading word (`list_x`, `get_x`, ...) so they
/// show the naming pattern rather than one family. `tool_names` must be sorted.
fn example_tool_names<'a>(server_name: &str, tool_names: &[&'a str]) -> Vec<&'a str> {
    let prefix = format!("{MCP_TOOL_PREFIX}{server_name}_");
    let mut leading_words_seen = HashSet::new();
    tool_names
        .iter()
        .map(|name| name.strip_prefix(prefix.as_str()).unwrap_or(name))
        .filter(|short_name| {
            let leading_word = short_name.split('_').next().unwrap_or(short_name);
            leading_words_seen.insert(leading_word.to_string())
        })
        .take(MAX_EXAMPLE_TOOL_NAMES)
        .collect()
}

/// First line of a tool description, trimmed.
fn first_line(description: &str) -> &str {
    description.trim().lines().next().unwrap_or("")
}

pub struct ToolSearchTool {
    /// The turn's tool view; shares its loaded-tools handle.
    tools: ToolRegistry,
}

/// Matches of one search, best first.
#[derive(Debug, Default)]
struct SearchOutcome {
    /// Loaded by this call (or earlier; see [`DeferredToolDefinition::loaded`]).
    loaded: Vec<DeferredToolDefinition>,
    /// Below [`LOAD_SCORE_RATIO`] of the best match: listed, not loaded.
    not_loaded: Vec<DeferredToolDefinition>,
}

impl ToolSearchTool {
    pub fn new(tools: ToolRegistry) -> Self {
        Self { tools }
    }

    /// Read `limit` from the arguments: absent means the default, a non-positive or
    /// non-integer value is an error, and anything above the cap is capped.
    fn requested_limit(params: &Value) -> Result<usize, String> {
        match params.get("limit") {
            None | Some(Value::Null) => Ok(DEFAULT_TOOL_SEARCH_LIMIT),
            Some(limit) => match limit.as_i64() {
                Some(limit) if limit > 0 => Ok((limit as usize).min(MAX_TOOL_SEARCH_LIMIT)),
                _ => Err("Error: limit must be a positive integer".to_string()),
            },
        }
    }

    /// Rank the deferred tools against `query` and load the strong matches: the ones the
    /// query names and the ones scoring at least [`LOAD_SCORE_RATIO`] of the best.
    /// Already loaded tools are ranked too, so a repeated call (for example a
    /// crash-recovery replay) gives the same matches.
    fn search_and_load(&self, query: &str, limit: usize) -> SearchOutcome {
        let candidates = self.tools.deferred_definitions();
        let documents: Vec<_> = candidates.iter().map(create_tool_search_document).collect();
        let ranked_matches = rank_tools(query, &documents, limit);
        let best_score = ranked_matches
            .iter()
            .map(|ranked| ranked.score)
            .fold(0.0, f64::max);
        let (strong_matches, weak_matches): (Vec<_>, Vec<_>) = ranked_matches
            .into_iter()
            .partition(|ranked| ranked.named || ranked.score >= best_score * LOAD_SCORE_RATIO);
        let strong_names: Vec<String> = strong_matches.into_iter().map(|m| m.name).collect();
        self.tools.load_tools(&strong_names);
        let definitions_of = |names: Vec<String>| -> Vec<DeferredToolDefinition> {
            names
                .iter()
                .filter_map(|name| candidates.iter().find(|tool| &tool.name == name))
                .cloned()
                .collect()
        };
        SearchOutcome {
            loaded: definitions_of(strong_names),
            not_loaded: definitions_of(weak_matches.into_iter().map(|m| m.name).collect()),
        }
    }

    /// The result text: the loaded matches, then the weaker ones that were not loaded,
    /// one line each with the first line of the description.
    fn format_result(outcome: &SearchOutcome) -> String {
        if outcome.loaded.is_empty() && outcome.not_loaded.is_empty() {
            return "No matching tools found.".to_string();
        }
        let mut sections = Vec::new();
        if !outcome.loaded.is_empty() {
            let plural = if outcome.loaded.len() == 1 { "" } else { "s" };
            sections.push(format!(
                "Loaded {} tool{plural}. They are available from your next call:\n{}",
                outcome.loaded.len(),
                Self::match_lines(&outcome.loaded)
            ));
        }
        if !outcome.not_loaded.is_empty() {
            sections.push(format!(
                "Weaker matches, not loaded (search for one by its exact name to load it):\n{}",
                Self::match_lines(&outcome.not_loaded)
            ));
        }
        sections.join("\n\n")
    }

    /// One line per match: name, first description line, and whether it was loaded before.
    fn match_lines(matches: &[DeferredToolDefinition]) -> String {
        matches
            .iter()
            .map(|tool| {
                let already_loaded = if tool.loaded { " (already loaded)" } else { "" };
                format!(
                    "- {}: {}{already_loaded}",
                    tool.name,
                    first_line(&tool.description)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> String {
        TOOL_SEARCH_TOOL_NAME.to_string()
    }

    fn description(&self) -> String {
        TOOL_SEARCH_DESCRIPTION.to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords describing the capability you need, or a tool's exact name."
                },
                "limit": {
                    "type": "integer",
                    "description": format!(
                        "Maximum number of tools to load. Defaults to {DEFAULT_TOOL_SEARCH_LIMIT}, at most {MAX_TOOL_SEARCH_LIMIT}."
                    )
                }
            },
            "required": ["query"]
        })
    }

    /// The same query loads the same tools and answers the same matches, and loading
    /// an already loaded tool changes nothing.
    fn replay_safe(&self) -> bool {
        true
    }

    async fn execute(&self, params: &Value) -> String {
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            return "Error: query must not be empty".to_string();
        }
        let limit = match Self::requested_limit(params) {
            Ok(limit) => limit,
            Err(error) => return error,
        };
        let outcome = self.search_and_load(query, limit);
        Self::format_result(&outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::base::ToolExposure;

    /// A tool with a fixed name, description and exposure.
    struct FakeTool {
        name: String,
        description: String,
        exposure: ToolExposure,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> String {
            self.name.clone()
        }
        fn description(&self) -> String {
            self.description.clone()
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn exposure(&self) -> ToolExposure {
            self.exposure
        }
        /// `mcp_{server}_{tool}` names belong to `{server}` (test names use no `_` in it).
        fn mcp_server_name(&self) -> Option<String> {
            self.name
                .strip_prefix("mcp_")
                .and_then(|rest| rest.split('_').next())
                .map(str::to_string)
        }
        async fn execute(&self, _params: &Value) -> String {
            "ok".to_string()
        }
    }

    fn fake_tool(name: &str, description: &str, exposure: ToolExposure) -> Box<dyn Tool> {
        Box::new(FakeTool {
            name: name.to_string(),
            description: description.to_string(),
            exposure,
        })
    }

    fn deferred(name: &str, description: &str) -> Box<dyn Tool> {
        fake_tool(name, description, ToolExposure::Deferred)
    }

    fn direct(name: &str, description: &str) -> Box<dyn Tool> {
        fake_tool(name, description, ToolExposure::Direct)
    }

    /// `count` deferred tools that all match the query "report".
    fn report_tools(count: usize) -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        for index in 0..count {
            tools.register(deferred(
                &format!("mcp_bulk_report_{index}"),
                "Export a report.",
            ));
        }
        tools
    }

    fn registry() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        tools.register(direct("read_file", "Read a file."));
        tools.register(deferred(
            "mcp_jira_create_issue",
            "Create a Jira issue.\nMore.",
        ));
        tools.register(deferred("mcp_jira_search_issues", "Search Jira issues."));
        tools.register(deferred("mcp_cms_find_entries", "Find CMS entries."));
        tools
    }

    fn declared_names(tools: &ToolRegistry) -> Vec<String> {
        tools
            .get_definitions()
            .iter()
            .map(|definition| definition["function"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn loads_matches_into_the_shared_view() {
        let tools = registry();
        let search = ToolSearchTool::new(tools.clone());

        let result = search.execute(&json!({"query": "jira issues"})).await;

        assert!(result.starts_with("Loaded 2 tools."), "{result}");
        assert!(result.contains("- mcp_jira_create_issue: Create a Jira issue."));
        assert!(!result.contains("More."), "only the first description line");
        let mut loaded = tools.loaded_tools();
        loaded.sort();
        assert_eq!(
            loaded,
            vec!["mcp_jira_create_issue", "mcp_jira_search_issues"]
        );
        assert_eq!(declared_names(&tools)[0], "read_file");
        assert_eq!(declared_names(&tools).len(), 3);
    }

    #[tokio::test]
    async fn marks_already_loaded_tools_and_is_idempotent() {
        let tools = registry();
        let search = ToolSearchTool::new(tools.clone());

        search.execute(&json!({"query": "cms entries"})).await;
        let repeated = search.execute(&json!({"query": "cms entries"})).await;

        assert!(repeated.contains("- mcp_cms_find_entries: Find CMS entries. (already loaded)"));
        assert_eq!(tools.loaded_tools(), vec!["mcp_cms_find_entries"]);
    }

    #[tokio::test]
    async fn caps_the_limit() {
        let tools = report_tools(30);
        let search = ToolSearchTool::new(tools.clone());

        search
            .execute(&json!({"query": "report", "limit": 100}))
            .await;

        assert_eq!(tools.loaded_tools().len(), MAX_TOOL_SEARCH_LIMIT);
    }

    #[tokio::test]
    async fn default_limit_applies_without_limit() {
        let tools = report_tools(12);
        let search = ToolSearchTool::new(tools.clone());

        search.execute(&json!({"query": "report"})).await;

        assert_eq!(tools.loaded_tools().len(), DEFAULT_TOOL_SEARCH_LIMIT);
    }

    #[tokio::test]
    async fn rejects_an_empty_query_and_a_non_positive_limit() {
        let tools = registry();
        let search = ToolSearchTool::new(tools.clone());

        assert_eq!(
            search.execute(&json!({"query": "  "})).await,
            "Error: query must not be empty"
        );
        for limit in [json!(0), json!(-3), json!("five")] {
            assert_eq!(
                search
                    .execute(&json!({"query": "jira", "limit": limit}))
                    .await,
                "Error: limit must be a positive integer"
            );
        }
        assert!(tools.loaded_tools().is_empty());
    }

    #[tokio::test]
    async fn reports_no_match_without_loading() {
        let tools = registry();
        let search = ToolSearchTool::new(tools.clone());

        let result = search.execute(&json!({"query": "weather forecast"})).await;

        assert_eq!(result, "No matching tools found.");
        assert!(tools.loaded_tools().is_empty());
    }

    #[test]
    fn direct_tools_are_never_search_results() {
        let tools = registry();
        let search = ToolSearchTool::new(tools.clone());

        let outcome = search.search_and_load("read file", 8);

        assert!(outcome.loaded.is_empty());
        assert!(outcome.not_loaded.is_empty());
    }

    #[test]
    fn is_replay_safe() {
        assert!(ToolSearchTool::new(ToolRegistry::new()).replay_safe());
    }

    // ── prompt section ────────────────────────────────────────────────────────

    /// `registry()` plus the built-in `docx`, deferred by config.
    fn registry_with_deferred_builtin() -> ToolRegistry {
        let mut tools = registry();
        tools.register(direct(
            "docx",
            "Convert Word documents.
Second line.",
        ));
        tools.defer_builtins(["docx".to_string()]);
        tools
    }

    #[test]
    fn prompt_section_lists_servers_then_builtins() {
        let descriptions = HashMap::from([("jira".to_string(), "Jira: issues".to_string())]);

        let section =
            deferred_tools_prompt_section(&registry_with_deferred_builtin(), &descriptions)
                .unwrap();

        let expected_lines = [
            DEFERRED_TOOLS_SECTION_INTRO,
            "- MCP server `cms` (1 tool, e.g. `find_entries`)",
            "- MCP server `jira` (2 tools, e.g. `create_issue`, `search_issues`): Jira: issues",
            "- Tool `docx`: Convert Word documents.",
        ];
        assert_eq!(section, expected_lines.join("\n"));
    }

    #[test]
    fn prompt_section_tells_the_model_to_search_before_giving_up() {
        let section = deferred_tools_prompt_section(&registry(), &HashMap::new()).unwrap();
        assert!(section.contains("call `tool_search` before saying you cannot do it"));
    }

    #[test]
    fn prompt_section_is_absent_when_nothing_is_deferred() {
        let mut tools = ToolRegistry::new();
        tools.register(direct("read_file", "Read a file."));

        assert_eq!(deferred_tools_prompt_section(&tools, &HashMap::new()), None);
    }

    #[test]
    fn prompt_section_leaves_out_mcp_servers_the_mode_hides() {
        let no_mcp =
            crate::agent::modes::AgentMode::NoMcp.restrict_tools(&registry_with_deferred_builtin());

        let section = deferred_tools_prompt_section(&no_mcp, &HashMap::new()).unwrap();

        assert!(!section.contains("MCP server"), "{section}");
        assert!(section.contains("- Tool `docx`"));
    }

    // ── score cut-off ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn weak_matches_are_listed_but_not_loaded() {
        let mut tools = ToolRegistry::new();
        tools.register(deferred("mcp_cms_list_articles", "List news articles."));
        tools.register(deferred(
            "mcp_cms_create_category",
            "Create a news category.",
        ));
        let search = ToolSearchTool::new(tools.clone());

        let result = search
            .execute(&json!({"query": "list news articles"}))
            .await;

        assert_eq!(tools.loaded_tools(), vec!["mcp_cms_list_articles"]);
        assert!(
            result.contains(
                "Weaker matches, not loaded (search for one by its exact name to load it):\n\
                 - mcp_cms_create_category: Create a news category."
            ),
            "{result}"
        );
    }

    #[tokio::test]
    async fn a_named_tool_is_loaded_even_when_its_score_is_weak() {
        let mut tools = ToolRegistry::new();
        tools.register(deferred("mcp_cms_list_new", "Content-manager list."));
        for collection in ["news_occasion", "news_tag", "news_author"] {
            tools.register(deferred(
                &format!("mcp_cms_list_{collection}"),
                "Content-manager list for news news news new new.",
            ));
        }
        let search = ToolSearchTool::new(tools.clone());

        search.execute(&json!({"query": "list_new"})).await;

        assert!(
            tools
                .loaded_tools()
                .contains(&"mcp_cms_list_new".to_string())
        );
    }

    // ── example tool names ────────────────────────────────────────────────────

    #[test]
    fn example_tool_names_show_one_tool_per_leading_word() {
        let tool_names = [
            "mcp_cms_create_author",
            "mcp_cms_create_new",
            "mcp_cms_delete_author",
            "mcp_cms_get_author",
            "mcp_cms_list_author",
            "mcp_cms_list_new",
            "mcp_cms_publish_author",
            "mcp_cms_update_author",
        ];

        assert_eq!(
            example_tool_names("cms", &tool_names),
            vec![
                "create_author",
                "delete_author",
                "get_author",
                "list_author",
                "publish_author"
            ]
        );
    }

    #[test]
    fn example_tool_names_handle_server_names_with_underscores_and_dashes() {
        let tool_names = ["mcp_our_yagya-cms_list_new"];
        assert_eq!(
            example_tool_names("our_yagya-cms", &tool_names),
            vec!["list_new"]
        );
    }
}
