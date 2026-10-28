//! BM25 ranking of deferred tools for `tool_search`.
//!
//! Pure functions, no I/O: a document per tool is built from its name, description,
//! parameter schema and MCP server name, and a query is ranked against them with Okapi
//! BM25. Ported from pi's `tool-search/tool.ts` (tokenizer, stemming, scoring and the
//! "a query naming a tool ranks it first" rule).

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::agent::tools::registry::{DeferredToolDefinition, MCP_TOOL_PREFIX};

/// BM25 term-frequency saturation.
const BM25_K1: f64 = 1.2;
/// BM25 document-length normalisation.
const BM25_B: f64 = 0.75;

/// Words too common to tell tools apart.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

/// A tool as the ranker sees it: its name and its searchable text.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSearchDocument {
    pub name: String,
    /// MCP server of the tool, used to recognise a query naming the tool without its
    /// `mcp_{server}_` prefix.
    pub server_name: Option<String>,
    pub text: String,
}

/// One ranked tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSearchMatch {
    pub name: String,
    pub score: f64,
    /// The query names this tool (full name, or the name after `mcp_{server}_`).
    pub named: bool,
}

/// Naive singular form, so `issues` matches `issue` and `searches` matches `search`.
fn stem(term: &str) -> String {
    let length = term.len();
    if length > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..length - 3]);
    }
    let sibilant_plural = ["ches", "shes", "sses", "xes", "zes"]
        .iter()
        .any(|suffix| term.ends_with(suffix));
    if length > 4 && sibilant_plural {
        return term[..length - 2].to_string();
    }
    if length > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..length - 1].to_string();
    }
    term.to_string()
}

/// Insert a space at camelCase boundaries: `listIssues` → `list Issues`,
/// `HTTPServer` → `HTTP Server`.
fn split_camel_case(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut split = String::with_capacity(text.len() + 8);
    for (index, &character) in characters.iter().enumerate() {
        if index > 0 && character.is_ascii_uppercase() {
            let previous = characters[index - 1];
            let next_is_lowercase = characters
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_lowercase());
            let lower_to_upper = previous.is_ascii_lowercase() || previous.is_ascii_digit();
            let acronym_end = previous.is_ascii_uppercase() && next_is_lowercase;
            if lower_to_upper || acronym_end {
                split.push(' ');
            }
        }
        split.push(character);
    }
    split
}

/// Lowercase terms, split at camelCase boundaries and non-alphanumerics, without stop
/// words, each reduced to a naive singular.
pub fn tokenize(text: &str) -> Vec<String> {
    split_camel_case(text)
        .to_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty() && !STOP_WORDS.contains(term))
        .map(stem)
        .collect()
}

/// Schema descriptions and property names, recursively, appended to `parts`.
fn collect_schema_text(schema: &Value, parts: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else {
        return;
    };
    if let Some(description) = schema.get("description").and_then(Value::as_str) {
        parts.push(description.to_string());
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (property_name, property_schema) in properties {
            parts.push(property_name.clone());
            collect_schema_text(property_schema, parts);
        }
    }
    if let Some(items) = schema.get("items") {
        collect_schema_text(items, parts);
    }
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = schema.get(combinator).and_then(Value::as_array) {
            for variant in variants {
                collect_schema_text(variant, parts);
            }
        }
    }
}

/// Searchable text of a tool: its name, the name with `_` as spaces, its description,
/// schema descriptions and property names, and its MCP server name.
pub fn create_tool_search_document(tool: &DeferredToolDefinition) -> ToolSearchDocument {
    let mut parts = vec![
        tool.name.clone(),
        tool.name.replace('_', " "),
        tool.description.clone(),
    ];
    collect_schema_text(&tool.parameters, &mut parts);
    if let Some(server_name) = &tool.server_name {
        parts.push(server_name.clone());
    }
    let text = parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    ToolSearchDocument {
        name: tool.name.clone(),
        server_name: tool.server_name.clone(),
        text,
    }
}

/// Lowercase, with characters other than ASCII letters, digits and `_` as `_`, so
/// `dev-radius` matches `dev_radius`.
fn normalize_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether the normalised `query` names the tool: its full name, or the tool name
/// after its `mcp_{server}_` prefix.
fn is_name_query(normalized_query: &str, document: &ToolSearchDocument) -> bool {
    if normalize_name(&document.name) == normalized_query {
        return true;
    }
    document.server_name.as_ref().is_some_and(|server_name| {
        let prefixed = format!("{MCP_TOOL_PREFIX}{server_name}_{normalized_query}");
        normalize_name(&document.name) == normalize_name(&prefixed)
    })
}

/// Rank `documents` against `query` with Okapi BM25 and return at most `limit` matches
/// with a positive score, best first. A query that names a tool ranks that tool first,
/// because length normalisation could otherwise put a short description sharing the
/// name's words above it. Ties keep document order.
pub fn rank_tools(
    query: &str,
    documents: &[ToolSearchDocument],
    limit: usize,
) -> Vec<ToolSearchMatch> {
    let mut seen_terms = HashSet::new();
    let query_terms: Vec<String> = tokenize(query)
        .into_iter()
        .filter(|term| seen_terms.insert(term.clone()))
        .collect();
    if query_terms.is_empty() || documents.is_empty() || limit == 0 {
        return Vec::new();
    }

    let term_counts: Vec<HashMap<String, usize>> = documents
        .iter()
        .map(|document| {
            let mut counts = HashMap::new();
            for term in tokenize(&document.text) {
                *counts.entry(term).or_insert(0) += 1;
            }
            counts
        })
        .collect();
    let lengths: Vec<f64> = term_counts
        .iter()
        .map(|counts| counts.values().sum::<usize>() as f64)
        .collect();
    let document_count = documents.len() as f64;
    let average_length = match lengths.iter().sum::<f64>() / document_count {
        average if average > 0.0 => average,
        _ => 1.0,
    };
    let inverse_document_frequency: HashMap<&str, f64> = query_terms
        .iter()
        .map(|term| {
            let frequency = term_counts
                .iter()
                .filter(|counts| counts.contains_key(term))
                .count() as f64;
            let idf = (1.0 + (document_count - frequency + 0.5) / (frequency + 0.5)).ln();
            (term.as_str(), idf)
        })
        .collect();

    let normalized_query = normalize_name(query);
    let mut matches: Vec<ToolSearchMatch> = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let length_norm = BM25_K1 * (1.0 - BM25_B + BM25_B * lengths[index] / average_length);
        let score: f64 = query_terms
            .iter()
            .filter_map(|term| {
                let count = *term_counts[index].get(term)? as f64;
                let idf = inverse_document_frequency.get(term.as_str())?;
                Some(idf * (count * (BM25_K1 + 1.0)) / (count + length_norm))
            })
            .sum();
        if score > 0.0 {
            matches.push(ToolSearchMatch {
                name: document.name.clone(),
                score,
                named: is_name_query(&normalized_query, document),
            });
        }
    }
    // Stable sort: equal scores keep document order.
    matches.sort_by(|a, b| {
        b.named
            .cmp(&a.named)
            .then_with(|| b.score.total_cmp(&a.score))
    });
    matches.truncate(limit);
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(name: &str, server_name: Option<&str>, text: &str) -> ToolSearchDocument {
        ToolSearchDocument {
            name: name.to_string(),
            server_name: server_name.map(str::to_string),
            text: text.to_string(),
        }
    }

    fn ranked_names(query: &str, documents: &[ToolSearchDocument], limit: usize) -> Vec<String> {
        rank_tools(query, documents, limit)
            .into_iter()
            .map(|ranked| ranked.name)
            .collect()
    }

    // ── tokenizer ─────────────────────────────────────────────────────────────

    #[test]
    fn tokenize_splits_camel_case_and_acronyms() {
        assert_eq!(tokenize("listIssues"), vec!["list", "issue"]);
        assert_eq!(tokenize("HTTPServer"), vec!["http", "server"]);
    }

    #[test]
    fn tokenize_splits_underscores_and_punctuation() {
        assert_eq!(
            tokenize("mcp_jira_search-issues"),
            vec!["mcp", "jira", "search", "issue"]
        );
    }

    #[test]
    fn tokenize_drops_stop_words() {
        assert_eq!(
            tokenize("Search for the issues of a project"),
            vec!["search", "issue", "project"]
        );
    }

    #[test]
    fn stem_reduces_common_plurals_only() {
        assert_eq!(stem("queries"), "query");
        assert_eq!(stem("searches"), "search");
        assert_eq!(stem("boxes"), "box");
        assert_eq!(stem("issues"), "issue");
        assert_eq!(stem("class"), "class");
        assert_eq!(stem("its"), "its");
    }

    // ── document ──────────────────────────────────────────────────────────────

    #[test]
    fn document_includes_schema_properties_and_server_name() {
        let tool = DeferredToolDefinition {
            name: "mcp_strapi_find_entries".to_string(),
            description: "Find content entries.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "collection": {"type": "string", "description": "Collection slug"},
                    "filters": {
                        "type": "array",
                        "items": {"type": "object", "properties": {"locale": {"type": "string"}}}
                    }
                }
            }),
            server_name: Some("strapi".to_string()),
            loaded: false,
        };

        let document = create_tool_search_document(&tool);

        for expected in [
            "mcp strapi find entries",
            "collection",
            "Collection slug",
            "locale",
        ] {
            assert!(document.text.contains(expected), "missing {expected}");
        }
        assert!(document.text.ends_with("strapi"));
    }

    // ── ranking ───────────────────────────────────────────────────────────────

    #[test]
    fn rank_orders_by_relevance() {
        let documents = vec![
            document("send_email", None, "send email message to a recipient"),
            document("search_issues", None, "search jira issues by query"),
            document("create_issue", None, "create a jira issue"),
        ];

        assert_eq!(
            ranked_names("search issues", &documents, 8),
            vec!["search_issues", "create_issue"]
        );
    }

    #[test]
    fn exact_name_query_wins_over_a_shorter_description() {
        let documents = vec![
            document("issue", None, "issue"),
            document(
                "mcp_jira_list_issues",
                Some("jira"),
                "mcp_jira_list_issues list issues of a jira board and more words to lengthen",
            ),
        ];

        assert_eq!(
            ranked_names("mcp_jira_list_issues", &documents, 8)[0],
            "mcp_jira_list_issues"
        );
        assert_eq!(
            ranked_names("list_issues", &documents, 8)[0],
            "mcp_jira_list_issues"
        );
    }

    #[test]
    fn matches_report_whether_the_query_names_the_tool() {
        let documents = vec![
            document("mcp_cms_list_new", Some("cms"), "list new"),
            document("mcp_cms_list_tag", Some("cms"), "list tag"),
        ];

        let matches = rank_tools("list_new", &documents, 8);

        assert_eq!(matches[0].name, "mcp_cms_list_new");
        assert!(matches[0].named);
        assert!(matches.iter().skip(1).all(|ranked| !ranked.named));
    }

    #[test]
    fn ties_keep_document_order() {
        let documents = vec![
            document("beta", None, "export report"),
            document("alpha", None, "export report"),
        ];

        assert_eq!(ranked_names("export", &documents, 8), vec!["beta", "alpha"]);
    }

    #[test]
    fn query_matching_only_a_schema_property_finds_the_tool() {
        let tool = DeferredToolDefinition {
            name: "convert".to_string(),
            description: "Convert a document.".to_string(),
            parameters: json!({"type": "object", "properties": {"docx_path": {"type": "string"}}}),
            server_name: None,
            loaded: false,
        };
        let documents = vec![
            create_tool_search_document(&tool),
            document("other", None, "unrelated words"),
        ];

        assert_eq!(ranked_names("docx", &documents, 8), vec!["convert"]);
    }

    #[test]
    fn limit_caps_the_number_of_matches() {
        let documents: Vec<ToolSearchDocument> = (0..5)
            .map(|index| document(&format!("tool_{index}"), None, "report export"))
            .collect();

        assert_eq!(rank_tools("report", &documents, 2).len(), 2);
    }

    #[test]
    fn empty_query_zero_limit_and_empty_corpus_return_nothing() {
        let documents = vec![document("search", None, "search things")];

        assert!(rank_tools("", &documents, 8).is_empty());
        assert!(rank_tools("the of", &documents, 8).is_empty());
        assert!(rank_tools("search", &documents, 0).is_empty());
        assert!(rank_tools("search", &[], 8).is_empty());
    }

    #[test]
    fn unmatched_query_returns_nothing() {
        let documents = vec![document("search", None, "search things")];

        assert!(rank_tools("weather", &documents, 8).is_empty());
    }
}
