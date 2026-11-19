//! The `skill_manage` tool: lets the agent record what it learnt as a skill
//! (procedural memory) and refine it later. The filesystem work lives in
//! [`SkillStore`]; this module parses the call and phrases the answer.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::modes::AgentMode;
use crate::agent::skill_store::{MAX_SKILL_NAME_CHARS, SkillChange, SkillStore};
use crate::agent::tools::base::{Tool, ToolResult};
use crate::agent::tools::registry::ToolRegistry;
use crate::utils::prompt_templates::render_template;

/// Name the model calls the tool by.
pub const SKILL_MANAGE_TOOL_NAME: &str = "skill_manage";

/// Template telling the model when and how to learn skills.
const SKILL_LEARNING_TEMPLATE: &str = "agent/skill_learning.md";

/// The "Learning Skills" system-prompt section, telling the model when to call
/// `skill_manage`.
///
/// Present only when `skill_manage` is registered in `registry` and `mode`
/// exposes it, so the prompt never points at a tool the model does not have
/// (e.g. one listed in `tools.disabledTools`). `skills_dir` is where the skills
/// are written.
pub fn skill_learning_prompt_section(
    registry: &ToolRegistry,
    mode: AgentMode,
    skills_dir: &Path,
) -> Option<String> {
    if !(mode.allows_tool(SKILL_MANAGE_TOOL_NAME) && registry.has(SKILL_MANAGE_TOOL_NAME)) {
        return None;
    }
    let mut context = tera::Context::new();
    context.insert("skills_dir", &skills_dir.display().to_string());
    match render_template(SKILL_LEARNING_TEMPLATE, &context, true) {
        Ok(section) if !section.is_empty() => Some(section),
        Ok(_) => None,
        Err(error) => {
            log::warn!("skill learning prompt section not rendered: {error}");
            None
        }
    }
}

/// Creates, patches and deletes the agent's own skills.
pub struct SkillManageTool {
    store: Arc<SkillStore>,
}

impl SkillManageTool {
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self { store }
    }

    /// Run one call: check its fields for the action, then hand it to the store.
    fn run(&self, params: &Value) -> Result<SkillChange, String> {
        let action = required_text(params, "action")?;
        let name = required_text(params, "name")?;
        match action {
            "create" => self.store.create(name, required_text(params, "content")?),
            "patch" => self.patch(name, params),
            "write_file" => self.store.write_file(
                name,
                required_text(params, "file_path")?,
                required_text_allow_empty(params, "file_content")?,
            ),
            "remove_file" => self
                .store
                .remove_file(name, required_text(params, "file_path")?),
            "delete" => self.store.delete(name),
            other => Err(format!(
                "Error: unknown action '{other}'. Use create, patch, write_file, \
                 remove_file or delete."
            )),
        }
    }

    /// `patch` has two shapes: `old_string` + `new_string` for a targeted
    /// replacement, or `content` alone for a full `SKILL.md` rewrite.
    fn patch(&self, name: &str, params: &Value) -> Result<SkillChange, String> {
        let content = optional_text(params, "content");
        let has_replacement_fields =
            params.get("old_string").is_some() || params.get("new_string").is_some();
        if let Some(content) = content {
            if has_replacement_fields {
                return Err(
                    "Error: patch takes either old_string/new_string or content \
                            (full rewrite), not both."
                        .to_string(),
                );
            }
            return self.store.rewrite(name, content);
        }
        let replace_all = params
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.store.patch(
            name,
            required_text(params, "old_string")?,
            required_text_allow_empty(params, "new_string")?,
            optional_text(params, "file_path"),
            replace_all,
        )
    }
}

/// The non-empty string field `key`, or an error naming it.
fn required_text<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
    optional_text(params, key).ok_or_else(|| format!("Error: missing required parameter '{key}'."))
}

/// The string field `key`, which may be empty (e.g. a `new_string` that
/// deletes the match), or an error when it is absent.
fn required_text_allow_empty<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Error: missing required parameter '{key}'."))
}

/// The string field `key` when present and not empty.
fn optional_text<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// The answer to the model for a successful change. Except after a delete it
/// says how to use the skill before the skill index lists it next turn.
fn describe_change(change: &SkillChange, action: &str) -> String {
    let path = change.path.display();
    if action == "delete" {
        return format!("{}\nRemoved: {path}", change.message);
    }
    format!(
        "{}\nPath: {path}\nThe skill index lists it from the next turn; read_file this \
         path to use the new version now.",
        change.message
    )
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> String {
        SKILL_MANAGE_TOOL_NAME.to_string()
    }

    fn description(&self) -> String {
        "Create, update or delete skills: your procedural memory for recurring tasks. \
         Record a skill after you work out a non-trivial reusable workflow, find the \
         working approach after errors or dead ends, or are corrected by the user. \
         Actions: create (full SKILL.md: frontmatter with name and description, then \
         the instructions), patch (old_string/new_string targeted fix, preferred; or \
         content alone to rewrite SKILL.md), write_file/remove_file (supporting files \
         under references/, templates/, scripts/ or assets/), delete. Prefer patching an \
         existing skill over creating a near-duplicate. Description: one sentence, \
         'Use when <trigger>. <behaviour>.' Write lessons, not logs: imperative rules \
         and why, no dates, ticket numbers or incident narration."
            .to_string()
    }

    fn read_only(&self) -> bool {
        false
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "patch", "write_file", "remove_file", "delete"],
                    "description": "What to do."
                },
                "name": {
                    "type": "string",
                    "description": format!(
                        "Skill name: lowercase letters, digits, '.', '_' and '-', \
                         at most {MAX_SKILL_NAME_CHARS} characters. It is the skill's \
                         folder name and must equal the frontmatter name."
                    )
                },
                "content": {
                    "type": "string",
                    "description": "create, or patch as a full rewrite: the whole SKILL.md \
                        (--- frontmatter with name and description ---, then markdown)."
                },
                "old_string": {
                    "type": "string",
                    "description": "patch: text to find (must match once unless replace_all)."
                },
                "new_string": {
                    "type": "string",
                    "description": "patch: replacement; an empty string deletes the match."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "patch: replace every occurrence (default false)."
                },
                "file_path": {
                    "type": "string",
                    "description": "Supporting file relative to the skill folder, e.g. \
                        'references/api.md'. Required for write_file and remove_file; \
                        optional for patch (default SKILL.md)."
                },
                "file_content": {
                    "type": "string",
                    "description": "write_file: the whole text of the supporting file."
                }
            },
            "required": ["action", "name"]
        })
    }

    async fn execute(&self, params: &Value) -> String {
        self.execute_result(params).await.content
    }

    async fn execute_result(&self, params: &Value) -> ToolResult {
        let action = params.get("action").and_then(Value::as_str).unwrap_or("");
        match self.run(params) {
            Ok(change) => ToolResult::ok(describe_change(&change, action)),
            Err(message) => ToolResult::error(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill_store::{SKILL_FILE_NAME, SkillLimits};
    use serde_json::json;
    use tempfile::TempDir;

    /// A tool over an empty workspace and an empty bundled-skills folder.
    fn tool_in(workspace: &TempDir, builtin: &TempDir) -> SkillManageTool {
        SkillManageTool::new(Arc::new(SkillStore::new(
            workspace.path(),
            Some(builtin.path().to_path_buf()),
            SkillLimits::default(),
        )))
    }

    fn document(name: &str) -> String {
        format!("---\nname: {name}\ndescription: Use when testing.\n---\n\nStep one.\n")
    }

    #[test]
    fn the_tool_is_named_skill_manage_and_writes() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let tool = tool_in(&workspace, &builtin);
        assert_eq!(tool.name(), "skill_manage");
        assert!(!tool.read_only());
        assert!(!tool.concurrency_safe());
    }

    #[test]
    fn parameters_require_action_and_name_and_list_every_action() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let schema = tool_in(&workspace, &builtin).parameters();
        assert_eq!(schema["required"], json!(["action", "name"]));
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["create", "patch", "write_file", "remove_file", "delete"])
        );
    }

    #[tokio::test]
    async fn create_answers_with_the_path_and_how_to_use_it_now() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let tool = tool_in(&workspace, &builtin);
        let result = tool
            .execute_result(
                &json!({"action": "create", "name": "demo", "content": document("demo")}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.starts_with("Skill 'demo' created."));
        assert!(result.content.contains(SKILL_FILE_NAME));
        assert!(result.content.contains("read_file"));
        assert!(workspace.path().join("skills/demo/SKILL.md").is_file());
    }

    #[tokio::test]
    async fn missing_fields_are_errors_naming_the_field() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let tool = tool_in(&workspace, &builtin);
        let cases = [
            (json!({"name": "demo"}), "'action'"),
            (json!({"action": "create"}), "'name'"),
            (json!({"action": "create", "name": "demo"}), "'content'"),
            (
                json!({"action": "write_file", "name": "demo", "file_content": "x"}),
                "'file_path'",
            ),
            (
                json!({"action": "write_file", "name": "demo", "file_path": "references/a.md"}),
                "'file_content'",
            ),
            (
                json!({"action": "remove_file", "name": "demo"}),
                "'file_path'",
            ),
            (
                json!({"action": "patch", "name": "demo", "new_string": "x"}),
                "'old_string'",
            ),
            (
                json!({"action": "patch", "name": "demo", "old_string": "x"}),
                "'new_string'",
            ),
        ];
        for (params, field) in cases {
            let result = tool.execute_result(&params).await;
            assert!(result.is_error, "{params}");
            assert!(
                result.content.contains(field),
                "{params}: {}",
                result.content
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_action_is_an_error() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let result = tool_in(&workspace, &builtin)
            .execute_result(&json!({"action": "rename", "name": "demo"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown action 'rename'"));
    }

    #[tokio::test]
    async fn patch_with_content_rewrites_and_with_both_shapes_is_refused() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let tool = tool_in(&workspace, &builtin);
        tool.execute_result(
            &json!({"action": "create", "name": "demo", "content": document("demo")}),
        )
        .await;

        let new_document = document("demo").replace("Step one.", "Step two.");
        let rewrite = tool
            .execute_result(&json!({"action": "patch", "name": "demo", "content": new_document}))
            .await;
        assert!(!rewrite.is_error, "{}", rewrite.content);
        assert!(rewrite.content.contains("rewritten"));

        let both = tool
            .execute_result(&json!({"action": "patch", "name": "demo",
                                    "content": new_document, "old_string": "a", "new_string": "b"}))
            .await;
        assert!(both.is_error);
        assert!(both.content.contains("not both"));
    }

    #[tokio::test]
    async fn patch_with_an_empty_new_string_deletes_the_match() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let tool = tool_in(&workspace, &builtin);
        let content = document("demo").replace("Step one.", "Step one. Obsolete note.");
        tool.execute_result(&json!({"action": "create", "name": "demo", "content": content}))
            .await;
        let result = tool
            .execute_result(&json!({"action": "patch", "name": "demo",
                                    "old_string": " Obsolete note.", "new_string": ""}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        let text = std::fs::read_to_string(workspace.path().join("skills/demo/SKILL.md")).unwrap();
        assert!(!text.contains("Obsolete"));
    }

    #[tokio::test]
    async fn store_errors_come_back_as_failed_calls() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let result = tool_in(&workspace, &builtin)
            .execute_result(&json!({"action": "delete", "name": "ghost"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    // ── skill_learning_prompt_section ─────────────────────────────────────

    #[test]
    fn learning_section_is_present_when_skill_manage_is_registered() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(tool_in(&workspace, &builtin)));
        let skills_dir = workspace.path().join("skills");

        let section =
            skill_learning_prompt_section(&registry, AgentMode::Standard, &skills_dir).unwrap();

        assert!(section.contains("# Learning Skills"));
        assert!(section.contains("skill_manage"));
        assert!(section.contains("dead ends"));
        assert!(section.contains("corrected by the user"));
        assert!(section.contains(&skills_dir.display().to_string()));
    }

    #[test]
    fn learning_section_is_absent_without_the_tool() {
        let registry = ToolRegistry::new();
        assert!(
            skill_learning_prompt_section(&registry, AgentMode::Standard, Path::new("skills"))
                .is_none()
        );
    }

    #[test]
    fn learning_section_is_absent_once_the_tool_is_disabled() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(tool_in(&workspace, &builtin)));
        registry.disable(SKILL_MANAGE_TOOL_NAME);
        assert!(
            skill_learning_prompt_section(&registry, AgentMode::Standard, Path::new("skills"))
                .is_none()
        );
    }

    #[test]
    fn learning_section_is_absent_in_a_mode_that_hides_the_tool() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(tool_in(&workspace, &builtin)));
        assert!(
            skill_learning_prompt_section(&registry, AgentMode::Minimal, Path::new("skills"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn delete_answers_with_the_removed_folder() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let tool = tool_in(&workspace, &builtin);
        tool.execute_result(
            &json!({"action": "create", "name": "demo", "content": document("demo")}),
        )
        .await;
        let result = tool
            .execute_result(&json!({"action": "delete", "name": "demo"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("Removed:"));
        assert!(!result.content.contains("read_file"));
    }
}
