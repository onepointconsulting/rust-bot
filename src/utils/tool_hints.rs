use std::collections::HashMap;
use std::sync::LazyLock;

use crate::providers::base::ToolCallRequest;
use crate::utils::path::abbreviate_path;

/// Format metadata for a tool: argument keys to extract, display template, and value kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolFormat {
    pub key_args: &'static [&'static str],
    pub template: &'static str,
    pub is_path: bool,
    pub is_command: bool,
}

struct ToolCallGroup {
    name: String,
    count: usize,
    first: ToolCallRequest,
}

/// Registry: tool_name -> (key_args, template, is_path, is_command)
pub static TOOL_FORMATS: LazyLock<HashMap<&'static str, ToolFormat>> = LazyLock::new(|| {
    HashMap::from([
        (
            "read_file",
            ToolFormat {
                key_args: &["path", "file_path"],
                template: "read {}",
                is_path: true,
                is_command: false,
            },
        ),
        (
            "write_file",
            ToolFormat {
                key_args: &["path", "file_path"],
                template: "write {}",
                is_path: true,
                is_command: false,
            },
        ),
        (
            "edit",
            ToolFormat {
                key_args: &["file_path", "path"],
                template: "edit {}",
                is_path: true,
                is_command: false,
            },
        ),
        (
            "glob",
            ToolFormat {
                key_args: &["pattern"],
                template: "glob \"{}\"",
                is_path: false,
                is_command: false,
            },
        ),
        (
            "grep",
            ToolFormat {
                key_args: &["pattern"],
                template: "grep \"{}\"",
                is_path: false,
                is_command: false,
            },
        ),
        (
            "exec",
            ToolFormat {
                key_args: &["command"],
                template: "$ {}",
                is_path: false,
                is_command: true,
            },
        ),
        (
            "web_search",
            ToolFormat {
                key_args: &["query"],
                template: "search \"{}\"",
                is_path: false,
                is_command: false,
            },
        ),
        (
            "web_fetch",
            ToolFormat {
                key_args: &["url"],
                template: "fetch {}",
                is_path: true,
                is_command: false,
            },
        ),
        (
            "list_dir",
            ToolFormat {
                key_args: &["path"],
                template: "ls {}",
                is_path: true,
                is_command: false,
            },
        ),
    ])
});

pub fn get_tool_format(name: &str) -> Option<&'static ToolFormat> {
    TOOL_FORMATS.get(name)
}

const HINT_MAX_LEN: usize = 40;
const ELLIPSIS: &str = "\u{2026}";

/// Format tool calls as concise hints with smart abbreviation.
pub fn format_tool_hints(tool_calls: Vec<ToolCallRequest>) -> String {
    if tool_calls.is_empty() {
        return "".to_string();
    }

    let mut hints: Vec<String> = Vec::new();
    for group in group_consecutive(tool_calls) {
        let fmt = TOOL_FORMATS.get(group.name.as_str());
        let mut hint: String;
        if let Some(fmt) = fmt {
            hint = fmt_known(&group.first, fmt);
        }
        else if group.name.starts_with("mcp_") {
            hint = fmt_mcp(&group.first);
        }
        else {
            hint = fmt_fallback(&group.first);
        }
        if group.count > 1 {
            hint = format!("{hint} \u{00d7} {}", group.count);
        }
        hints.push(hint);
    }

    return hints.join(", ");
}

/// Group consecutive calls to the same tool: [(name, count, first), ...].
fn group_consecutive(calls: Vec<ToolCallRequest>) -> Vec<ToolCallGroup> {
    let mut groups: Vec<ToolCallGroup> = Vec::new();
    for tc in calls {
        if groups.len() > 0 && groups.last().unwrap().name == tc.name {
            let last_index = groups.len() - 1;
            groups[last_index] = ToolCallGroup {
                name: tc.name.clone(),
                count: groups.last().unwrap().count + 1,
                first: groups.last().unwrap().first.clone(),
            }
        }
        else {
            groups.push(ToolCallGroup { name: tc.name.clone(), count: 1, first: tc });
        }
    }
    return groups
}

/// Format a registered tool using its template.
fn fmt_known(tc: &ToolCallRequest, fmt: &ToolFormat) -> String {
    let Some(val) = extract_arg(tc, fmt.key_args) else {
        return tc.name.clone();
    };

    if fmt.is_path {
        let value = abbreviate_path(&val, HINT_MAX_LEN);
        return fmt.template.replace("{}", &value);
    }

    if fmt.is_command {
        let truncated: String = val.chars().take(HINT_MAX_LEN).collect();
        let value = if val.chars().count() > HINT_MAX_LEN {
            format!("{truncated}{ELLIPSIS}")
        } else {
            truncated
        };
        return fmt.template.replace("{}", &value);
    }

    fmt.template.replace("{}", &val)
}

/// Extract the first available value from preferred key names.
fn extract_arg(tc: &ToolCallRequest, key_args: &[&str]) -> Option<String> {
    let args = tc.arguments.clone();
    for key in key_args {
        let val_option = args.get(*key);
        if let Some(val) = val_option && val.is_string() {
            if let Some(val_str) = val.as_str() {
                if !val_str.is_empty() {
                    return Some(val_str.to_string());
                }
            }
        }
    }
    for val in args.values() {
        if val.is_string() {
            if let Some(val_str) = val.as_str() {
                if !val_str.is_empty() {
                    return Some(val_str.to_string());
                }
            }
        }
    }
    None
}

/// Normalize tool-call arguments (already parsed on [`ToolCallRequest`]).
fn get_args(tc: &ToolCallRequest) -> &HashMap<String, serde_json::Value> {
    &tc.arguments
}

/// First non-empty string value from tool arguments.
fn first_string_arg(args: &HashMap<String, serde_json::Value>) -> Option<&str> {
    for val in args.values() {
        if let Some(s) = val.as_str() {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Parse an MCP tool name into `(server, tool)`.
fn parse_mcp_name(name: &str) -> (String, String) {
    if let Some((server_part, tool)) = name.split_once("__") {
        let server = server_part.strip_prefix("mcp_").unwrap_or(server_part);
        return (server.to_string(), tool.to_string());
    }

    let rest = name.strip_prefix("mcp_").unwrap_or(name);
    match rest.split_once('_') {
        Some((server, tool)) => (server.to_string(), tool.to_string()),
        None => (rest.to_string(), String::new()),
    }
}

/// Format MCP tool as `server::tool` or `server::tool("arg")`.
fn fmt_mcp(tc: &ToolCallRequest) -> String {
    let (server, tool) = parse_mcp_name(&tc.name);
    if tool.is_empty() {
        return tc.name.clone();
    }

    let args = get_args(tc);
    match first_string_arg(args) {
        None => format!("{server}::{tool}"),
        Some(val) => {
            let abbreviated = abbreviate_path(val, HINT_MAX_LEN);
            format!("{server}::{tool}(\"{abbreviated}\")")
        }
    }
}

/// Original formatting logic for unregistered tools.
fn fmt_fallback(tc: &ToolCallRequest) -> String {
    let args = get_args(tc);
    let Some(val) = args.values().next() else {
        return tc.name.clone();
    };
    let Some(s) = val.as_str() else {
        return tc.name.clone();
    };

    let display = if s.chars().count() > HINT_MAX_LEN {
        abbreviate_path(s, HINT_MAX_LEN)
    } else {
        s.to_string()
    };
    format!("{}(\"{display}\")", tc.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use serde_json::json;

    fn tool_call(name: &str, args: HashMap<String, serde_json::Value>) -> ToolCallRequest {
        tool_call_with_id("call_1", name, args)
    }

    fn tool_call_with_id(
        id: &str,
        name: &str,
        args: HashMap<String, serde_json::Value>,
    ) -> ToolCallRequest {
        ToolCallRequest {
            id: id.into(),
            name: name.into(),
            arguments: args,
            extra_content: None,
            provider_specific_fields: None,
            function_provider_specific_fields: None,
        }
    }

    #[test]
    fn test_group_consecutive_empty_input() {
        assert!(group_consecutive(vec![]).is_empty());
    }

    #[test]
    fn test_group_consecutive_single_call() {
        let tc = tool_call_with_id("call_a", "read_file", HashMap::new());
        let groups = group_consecutive(vec![tc.clone()]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "read_file");
        assert_eq!(groups[0].count, 1);
        assert_eq!(groups[0].first.id, "call_a");
    }

    #[test]
    fn test_group_consecutive_merges_consecutive_same_name() {
        let calls = vec![
            tool_call_with_id("call_1", "read_file", HashMap::new()),
            tool_call_with_id("call_2", "read_file", HashMap::new()),
            tool_call_with_id("call_3", "read_file", HashMap::new()),
        ];
        let groups = group_consecutive(calls);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "read_file");
        assert_eq!(groups[0].count, 3);
        assert_eq!(groups[0].first.id, "call_1");
    }

    #[test]
    fn test_group_consecutive_splits_different_tools() {
        let calls = vec![
            tool_call_with_id("call_1", "read_file", HashMap::new()),
            tool_call_with_id("call_2", "grep", HashMap::new()),
            tool_call_with_id("call_3", "exec", HashMap::new()),
        ];
        let groups = group_consecutive(calls);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].count, 1);
        assert_eq!(groups[0].name, "read_file");
        assert_eq!(groups[0].first.id, "call_1");
        assert_eq!(groups[1].count, 1);
        assert_eq!(groups[1].name, "grep");
        assert_eq!(groups[1].first.id, "call_2");
        assert_eq!(groups[2].count, 1);
        assert_eq!(groups[2].name, "exec");
        assert_eq!(groups[2].first.id, "call_3");
    }

    #[test]
    fn test_group_consecutive_same_tool_non_consecutive_creates_new_groups() {
        let calls = vec![
            tool_call_with_id("call_1", "read_file", HashMap::new()),
            tool_call_with_id("call_2", "read_file", HashMap::new()),
            tool_call_with_id("call_3", "grep", HashMap::new()),
            tool_call_with_id("call_4", "read_file", HashMap::new()),
        ];
        let groups = group_consecutive(calls);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].name, "read_file");
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].first.id, "call_1");
        assert_eq!(groups[1].name, "grep");
        assert_eq!(groups[1].count, 1);
        assert_eq!(groups[1].first.id, "call_3");
        assert_eq!(groups[2].name, "read_file");
        assert_eq!(groups[2].count, 1);
        assert_eq!(groups[2].first.id, "call_4");
    }

    #[test]
    fn test_group_consecutive_mixed_runs() {
        let calls = vec![
            tool_call_with_id("a1", "glob", HashMap::new()),
            tool_call_with_id("a2", "glob", HashMap::new()),
            tool_call_with_id("b1", "grep", HashMap::new()),
            tool_call_with_id("a3", "glob", HashMap::new()),
            tool_call_with_id("a4", "glob", HashMap::new()),
            tool_call_with_id("a5", "glob", HashMap::new()),
        ];
        let groups = group_consecutive(calls);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].name, "glob");
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].first.id, "a1");
        assert_eq!(groups[1].name, "grep");
        assert_eq!(groups[1].count, 1);
        assert_eq!(groups[1].first.id, "b1");
        assert_eq!(groups[2].name, "glob");
        assert_eq!(groups[2].count, 3);
        assert_eq!(groups[2].first.id, "a3");
    }

    #[test]
    fn test_extract_arg_returns_first_preferred_key() {
        let tc = tool_call(
            "read_file",
            HashMap::from([
                ("path".into(), json!("src/main.rs")),
                ("file_path".into(), json!("other.rs")),
            ]),
        );
        assert_eq!(
            extract_arg(&tc, &["path", "file_path"]),
            Some("src/main.rs".into())
        );
    }

    #[test]
    fn test_extract_arg_falls_back_to_second_preferred_key() {
        let tc = tool_call(
            "read_file",
            HashMap::from([
                ("path".into(), json!("")),
                ("file_path".into(), json!("/tmp/foo.txt")),
            ]),
        );
        assert_eq!(
            extract_arg(&tc, &["path", "file_path"]),
            Some("/tmp/foo.txt".into())
        );
    }

    #[test]
    fn test_extract_arg_uses_fallback_value_when_no_preferred_key() {
        let tc = tool_call("glob", HashMap::from([("pattern".into(), json!("*.rs"))]));
        assert_eq!(
            extract_arg(&tc, &["path", "file_path"]),
            Some("*.rs".into())
        );
    }

    #[test]
    fn test_extract_arg_skips_non_string_values() {
        let tc = tool_call(
            "read_file",
            HashMap::from([
                ("path".into(), json!(123)),
                ("file_path".into(), json!("/ok")),
            ]),
        );
        assert_eq!(
            extract_arg(&tc, &["path", "file_path"]),
            Some("/ok".into())
        );
    }

    #[test]
    fn test_extract_arg_returns_none_for_empty_args() {
        let tc = tool_call("read_file", HashMap::new());
        assert_eq!(extract_arg(&tc, &["path"]), None);
    }

    #[test]
    fn test_extract_arg_returns_none_when_only_empty_strings() {
        let tc = tool_call(
            "read_file",
            HashMap::from([
                ("path".into(), json!("")),
                ("other".into(), json!("")),
            ]),
        );
        assert_eq!(extract_arg(&tc, &["path"]), None);
    }

    #[test]
    fn test_fmt_known_returns_tool_name_when_no_args() {
        let tc = tool_call("read_file", HashMap::new());
        let fmt = get_tool_format("read_file").unwrap();
        assert_eq!(fmt_known(&tc, fmt), "read_file");
    }

    #[test]
    fn test_fmt_known_path_short() {
        let tc = tool_call(
            "read_file",
            HashMap::from([("path".into(), json!("src/main.rs"))]),
        );
        let fmt = get_tool_format("read_file").unwrap();
        assert_eq!(fmt_known(&tc, fmt), "read src/main.rs");
    }

    #[test]
    fn test_fmt_known_path_abbreviates_long_path() {
        let tc = tool_call(
            "read_file",
            HashMap::from([(
                "path".into(),
                json!("very/long/nested/directory/structure/file.txt"),
            )]),
        );
        let fmt = get_tool_format("read_file").unwrap();
        let result = fmt_known(&tc, fmt);
        assert!(result.starts_with("read "));
        assert!(result.contains(ELLIPSIS));
        assert!(result.ends_with("file.txt"));
    }

    #[test]
    fn test_fmt_known_command_short() {
        let tc = tool_call(
            "exec",
            HashMap::from([("command".into(), json!("cargo test"))]),
        );
        let fmt = get_tool_format("exec").unwrap();
        assert_eq!(fmt_known(&tc, fmt), "$ cargo test");
    }

    #[test]
    fn test_fmt_known_command_truncates_long_command() {
        let command = "a".repeat(50);
        let tc = tool_call("exec", HashMap::from([("command".into(), json!(command))]));
        let fmt = get_tool_format("exec").unwrap();
        let result = fmt_known(&tc, fmt);
        assert!(result.starts_with("$ "));
        assert!(result.ends_with(ELLIPSIS));
        assert_eq!(result.chars().count(), HINT_MAX_LEN + 1 + 2); // "$ " prefix + 40 + ellipsis
    }

    #[test]
    fn test_fmt_known_command_uses_char_count_for_unicode() {
        let command: String = "é".repeat(41);
        let tc = tool_call("exec", HashMap::from([("command".into(), json!(command))]));
        let fmt = get_tool_format("exec").unwrap();
        let result = fmt_known(&tc, fmt);
        assert!(result.ends_with(ELLIPSIS));
        let body = result.strip_prefix("$ ").unwrap();
        assert_eq!(body.chars().count(), HINT_MAX_LEN + 1);
    }

    #[test]
    fn test_fmt_known_default_template() {
        let tc = tool_call("glob", HashMap::from([("pattern".into(), json!("*.rs"))]));
        let fmt = get_tool_format("glob").unwrap();
        assert_eq!(fmt_known(&tc, fmt), "glob \"*.rs\"");
    }

    #[test]
    fn test_fmt_known_web_fetch_abbreviates_url() {
        let tc = tool_call(
            "web_fetch",
            HashMap::from([(
                "url".into(),
                json!("https://example.com/api/v2/deep/nested/resource.json"),
            )]),
        );
        let fmt = get_tool_format("web_fetch").unwrap();
        let result = fmt_known(&tc, fmt);
        assert!(result.starts_with("fetch example.com/"));
        assert!(result.contains(ELLIPSIS));
        assert!(result.ends_with("resource.json"));
    }

    #[test]
    fn test_fmt_mcp_double_underscore_no_args() {
        let tc = tool_call("mcp_github__search", HashMap::new());
        assert_eq!(fmt_mcp(&tc), "github::search");
    }

    #[test]
    fn test_fmt_mcp_single_underscore_no_args() {
        let tc = tool_call("mcp_github_search", HashMap::new());
        assert_eq!(fmt_mcp(&tc), "github::search");
    }

    #[test]
    fn test_fmt_mcp_with_string_arg() {
        let tc = tool_call(
            "mcp_github__read_file",
            HashMap::from([("path".into(), json!("src/main.rs"))]),
        );
        assert_eq!(fmt_mcp(&tc), "github::read_file(\"src/main.rs\")");
    }

    #[test]
    fn test_fmt_mcp_abbreviates_long_path_arg() {
        let tc = tool_call(
            "mcp_fs__read",
            HashMap::from([(
                "path".into(),
                json!("very/long/nested/directory/structure/file.txt"),
            )]),
        );
        let result = fmt_mcp(&tc);
        assert!(result.starts_with("fs::read(\""));
        assert!(result.contains(ELLIPSIS));
        assert!(result.ends_with("file.txt\")"));
    }

    #[test]
    fn test_fmt_mcp_returns_name_when_tool_empty() {
        let tc = tool_call("mcp_github", HashMap::new());
        assert_eq!(fmt_mcp(&tc), "mcp_github");
    }

    #[test]
    fn test_fmt_mcp_skips_empty_and_non_string_args() {
        let tc = tool_call(
            "mcp_server__tool",
            HashMap::from([
                ("count".into(), json!(5)),
                ("empty".into(), json!("")),
                ("query".into(), json!("hello")),
            ]),
        );
        assert_eq!(fmt_mcp(&tc), "server::tool(\"hello\")");
    }

    #[test]
    fn test_parse_mcp_name_without_mcp_prefix() {
        assert_eq!(parse_mcp_name("custom__tool"), ("custom".into(), "tool".into()));
    }

    #[test]
    fn test_fmt_fallback_no_args_returns_name() {
        let tc = tool_call("custom_tool", HashMap::new());
        assert_eq!(fmt_fallback(&tc), "custom_tool");
    }

    #[test]
    fn test_fmt_fallback_non_string_first_value_returns_name() {
        let tc = tool_call("custom_tool", HashMap::from([("count".into(), json!(5))]));
        assert_eq!(fmt_fallback(&tc), "custom_tool");
    }

    #[test]
    fn test_fmt_fallback_short_string() {
        let tc = tool_call(
            "custom_tool",
            HashMap::from([("query".into(), json!("hello"))]),
        );
        assert_eq!(fmt_fallback(&tc), "custom_tool(\"hello\")");
    }

    #[test]
    fn test_fmt_fallback_exactly_max_len_not_abbreviated() {
        let value = "a".repeat(HINT_MAX_LEN);
        let tc = tool_call("custom_tool", HashMap::from([("query".into(), json!(value.clone()))]));
        assert_eq!(fmt_fallback(&tc), format!("custom_tool(\"{value}\")"));
    }

    #[test]
    fn test_fmt_fallback_long_string_abbreviated() {
        let value = "very/long/nested/directory/structure/file.txt";
        let tc = tool_call("custom_tool", HashMap::from([("path".into(), json!(value))]));
        let result = fmt_fallback(&tc);
        assert!(result.starts_with("custom_tool(\""));
        assert!(result.contains(ELLIPSIS));
        assert!(result.ends_with("file.txt\")"));
    }

    #[test]
    fn test_fmt_fallback_empty_string_value() {
        let tc = tool_call("custom_tool", HashMap::from([("query".into(), json!(""))]));
        assert_eq!(fmt_fallback(&tc), "custom_tool(\"\")");
    }

    #[test]
    fn test_format_tool_hints_empty_returns_empty_string() {
        assert_eq!(format_tool_hints(vec![]), "");
    }

    #[test]
    fn test_format_tool_hints_known_tool() {
        let calls = vec![tool_call_with_id(
            "c1",
            "read_file",
            HashMap::from([("path".into(), json!("src/main.rs"))]),
        )];
        assert_eq!(format_tool_hints(calls), "read src/main.rs");
    }

    #[test]
    fn test_format_tool_hints_consecutive_same_tool_adds_count_suffix() {
        let calls = vec![
            tool_call_with_id(
                "c1",
                "read_file",
                HashMap::from([("path".into(), json!("a.rs"))]),
            ),
            tool_call_with_id(
                "c2",
                "read_file",
                HashMap::from([("path".into(), json!("b.rs"))]),
            ),
            tool_call_with_id(
                "c3",
                "read_file",
                HashMap::from([("path".into(), json!("c.rs"))]),
            ),
        ];
        assert_eq!(format_tool_hints(calls), "read a.rs \u{00d7} 3");
    }

    #[test]
    fn test_format_tool_hints_mixed_tools_joined_with_comma() {
        let calls = vec![
            tool_call_with_id(
                "c1",
                "read_file",
                HashMap::from([("path".into(), json!("a.rs"))]),
            ),
            tool_call_with_id(
                "c2",
                "grep",
                HashMap::from([("pattern".into(), json!("foo"))]),
            ),
        ];
        assert_eq!(format_tool_hints(calls), "read a.rs, grep \"foo\"");
    }

    #[test]
    fn test_format_tool_hints_mcp_tool() {
        let calls = vec![tool_call("mcp_github__search", HashMap::new())];
        assert_eq!(format_tool_hints(calls), "github::search");
    }

    #[test]
    fn test_format_tool_hints_fallback_tool() {
        let calls = vec![tool_call(
            "custom_tool",
            HashMap::from([("query".into(), json!("hello"))]),
        )];
        assert_eq!(format_tool_hints(calls), "custom_tool(\"hello\")");
    }

    #[test]
    fn test_format_tool_hints_non_consecutive_same_tool_separate_hints() {
        let calls = vec![
            tool_call_with_id(
                "c1",
                "read_file",
                HashMap::from([("path".into(), json!("a.rs"))]),
            ),
            tool_call_with_id(
                "c2",
                "read_file",
                HashMap::from([("path".into(), json!("ignored.rs"))]),
            ),
            tool_call_with_id(
                "c3",
                "grep",
                HashMap::from([("pattern".into(), json!("x"))]),
            ),
            tool_call_with_id(
                "c4",
                "read_file",
                HashMap::from([("path".into(), json!("b.rs"))]),
            ),
        ];
        assert_eq!(
            format_tool_hints(calls),
            "read a.rs \u{00d7} 2, grep \"x\", read b.rs"
        );
    }

    #[test]
    fn test_format_tool_hints_single_call_no_count_suffix() {
        let calls = vec![tool_call_with_id(
            "c1",
            "glob",
            HashMap::from([("pattern".into(), json!("*.rs"))]),
        )];
        let result = format_tool_hints(calls);
        assert_eq!(result, "glob \"*.rs\"");
        assert!(!result.contains('\u{00d7}'));
    }

    #[test]
    fn test_tool_formats_contains_expected_tools() {
        assert_eq!(TOOL_FORMATS.len(), 9);
        assert!(TOOL_FORMATS.contains_key("read_file"));
        assert!(TOOL_FORMATS.contains_key("exec"));
    }

    #[test]
    fn test_get_tool_format_read_file() {
        let fmt = get_tool_format("read_file").unwrap();
        assert_eq!(fmt.key_args, &["path", "file_path"]);
        assert_eq!(fmt.template, "read {}");
        assert!(fmt.is_path);
        assert!(!fmt.is_command);
    }

    #[test]
    fn test_get_tool_format_exec() {
        let fmt = get_tool_format("exec").unwrap();
        assert_eq!(fmt.key_args, &["command"]);
        assert_eq!(fmt.template, "$ {}");
        assert!(!fmt.is_path);
        assert!(fmt.is_command);
    }

    #[test]
    fn test_get_tool_format_unknown() {
        assert!(get_tool_format("unknown_tool").is_none());
    }
}
