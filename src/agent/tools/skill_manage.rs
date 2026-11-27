//! The `skill_manage` tool: lets the agent record what it learnt as a skill
//! (procedural memory) and refine it later. The filesystem work lives in
//! [`SkillStore`]; the parameter checks in [`SkillOperation::from_params`];
//! this module decides what to do with a call and phrases the answer.
//!
//! The foreground agent uses the default tool: every action, applied at once.
//! Dream uses a restricted one ([`SkillManageTool::with_allowed_actions`]) and,
//! when `tools.skills.requireApproval` is on, one that stages proposals
//! ([`SkillWriteMode::Propose`]) instead of writing skills.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::modes::AgentMode;
use crate::agent::skill_operation::{SkillAction, SkillOperation, optional_text};
use crate::agent::skill_proposals::{SkillProposal, SkillProposalStore};
use crate::agent::skill_store::{MAX_SKILL_NAME_CHARS, SkillChange, SkillStore};
use crate::agent::tools::base::{Tool, ToolResult};
use crate::agent::tools::registry::ToolRegistry;
use crate::utils::prompt_templates::render_template;

/// Name the model calls the tool by.
pub const SKILL_MANAGE_TOOL_NAME: &str = "skill_manage";

/// Template telling the model when and how to learn skills.
const SKILL_LEARNING_TEMPLATE: &str = "agent/skill_learning.md";

/// Origin recorded on proposals staged by this tool.
const PROPOSAL_ORIGIN: &str = "dream";

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

/// What the tool does with a valid call.
#[derive(Clone)]
pub enum SkillWriteMode {
    /// Write the change to the skills folder at once (the default).
    Apply,
    /// Stage the change as a proposal the user accepts or rejects later.
    Propose(Arc<SkillProposalStore>),
}

/// Creates, patches and deletes the agent's own skills.
pub struct SkillManageTool {
    store: Arc<SkillStore>,
    mode: SkillWriteMode,
    /// Actions this instance accepts, in [`SkillAction::ALL`] order.
    allowed_actions: Vec<SkillAction>,
}

impl SkillManageTool {
    /// A tool applying every action directly.
    pub fn new(store: Arc<SkillStore>) -> Self {
        Self {
            store,
            mode: SkillWriteMode::Apply,
            allowed_actions: SkillAction::ALL.to_vec(),
        }
    }

    /// Use `mode` for valid calls.
    pub fn with_mode(mut self, mode: SkillWriteMode) -> Self {
        self.mode = mode;
        self
    }

    /// Accept only `actions`; the schema and the description list only these.
    pub fn with_allowed_actions(mut self, actions: &[SkillAction]) -> Self {
        self.allowed_actions = SkillAction::ALL
            .into_iter()
            .filter(|action| actions.contains(action))
            .collect();
        self
    }

    /// Whether valid calls are staged rather than applied.
    fn proposes(&self) -> bool {
        matches!(self.mode, SkillWriteMode::Propose(_))
    }

    /// Run one call: parse it, check the action is allowed, then apply or
    /// stage it. Returns the answer for the model.
    fn run(&self, params: &Value) -> Result<String, String> {
        let operation = SkillOperation::from_params(params)?;
        self.ensure_allowed(operation.action())?;
        match &self.mode {
            SkillWriteMode::Apply => {
                let change = self.store.apply(&operation)?;
                Ok(describe_change(&change, operation.action()))
            }
            SkillWriteMode::Propose(proposals) => {
                let reason = optional_text(params, "reason").map(str::to_string);
                let proposal = proposals.propose(operation, PROPOSAL_ORIGIN, reason)?;
                Ok(describe_proposal(&proposal))
            }
        }
    }

    /// An error unless this instance accepts `action`.
    fn ensure_allowed(&self, action: SkillAction) -> Result<(), String> {
        if self.allowed_actions.contains(&action) {
            return Ok(());
        }
        Err(format!(
            "Error: action '{}' is not allowed here. Use {}.",
            action.as_str(),
            self.allowed_action_names().join(", ")
        ))
    }

    /// The allowed action names, in schema order.
    fn allowed_action_names(&self) -> Vec<&'static str> {
        self.allowed_actions
            .iter()
            .map(|action| action.as_str())
            .collect()
    }

    /// The description's "Actions: …" list, for the allowed actions only.
    fn actions_description(&self) -> String {
        let parts: Vec<&str> = self
            .allowed_actions
            .iter()
            .map(|action| match action {
                SkillAction::Create => {
                    "create (full SKILL.md: frontmatter with name and description, then \
                     the instructions)"
                }
                SkillAction::Patch => {
                    "patch (old_string/new_string targeted fix, preferred; or content alone \
                     to rewrite SKILL.md)"
                }
                SkillAction::WriteFile => {
                    "write_file (a supporting file under references/, templates/, scripts/ \
                     or assets/)"
                }
                SkillAction::RemoveFile => "remove_file (a supporting file)",
                SkillAction::Delete => "delete (a whole skill)",
            })
            .collect();
        parts.join(", ")
    }
}

/// The answer to the model for a successful change. Except after a delete it
/// says how to use the skill before the skill index lists it next turn.
fn describe_change(change: &SkillChange, action: SkillAction) -> String {
    let path = change.path.display();
    if action == SkillAction::Delete {
        return format!("{}\nRemoved: {path}", change.message);
    }
    format!(
        "{}\nPath: {path}\nThe skill index lists it from the next turn; read_file this \
         path to use the new version now.",
        change.message
    )
}

/// The answer to the model for a staged change.
fn describe_proposal(proposal: &SkillProposal) -> String {
    format!(
        "Proposal {} staged: {} skill '{}'. The user reviews it with /skills-review; \
         it takes effect only once accepted.",
        proposal.id,
        proposal.operation.label(),
        proposal.operation.skill_name()
    )
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> String {
        SKILL_MANAGE_TOOL_NAME.to_string()
    }

    fn description(&self) -> String {
        let verbs = if self.allowed_actions.contains(&SkillAction::Delete) {
            "Create, update or delete"
        } else {
            "Create or update"
        };
        let mut description = format!(
            "{verbs} skills: your procedural memory for recurring tasks. \
             Record a skill after you work out a non-trivial reusable workflow, find the \
             working approach after errors or dead ends, or are corrected by the user. \
             Actions: {}. Prefer patching an existing skill over creating a \
             near-duplicate. Description: one sentence, 'Use when <trigger>. \
             <behaviour>.' Write lessons, not logs: imperative rules and why, no dates, \
             ticket numbers or incident narration.",
            self.actions_description()
        );
        if self.proposes() {
            description.push_str(
                " Changes are staged as proposals for the user to review, not written: \
                 each call must be valid on its own against the skills as they are now, \
                 so put a new skill's complete text into one create.",
            );
        }
        description
    }

    fn read_only(&self) -> bool {
        false
    }

    fn parameters(&self) -> Value {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": self.allowed_action_names(),
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
        });
        if self.proposes() {
            schema["properties"]["reason"] = serde_json::json!({
                "type": "string",
                "description": "The lesson this change records, shown to the user \
                    reviewing the proposal."
            });
        }
        schema
    }

    async fn execute(&self, params: &Value) -> String {
        self.execute_result(params).await.content
    }

    async fn execute_result(&self, params: &Value) -> ToolResult {
        match self.run(params) {
            Ok(answer) => ToolResult::ok(answer),
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
    async fn a_missing_field_is_an_error_naming_it() {
        // The per-action field checks live in `SkillOperation::from_params`.
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let result = tool_in(&workspace, &builtin)
            .execute_result(&json!({"action": "create", "name": "demo"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("'content'"), "{}", result.content);
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

    // ── allowed actions and write modes ───────────────────────────────────

    fn store_in(workspace: &TempDir, builtin: &TempDir) -> Arc<SkillStore> {
        Arc::new(SkillStore::new(
            workspace.path(),
            Some(builtin.path().to_path_buf()),
            SkillLimits::default(),
        ))
    }

    #[test]
    fn a_restricted_tool_lists_only_its_actions() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let tool = SkillManageTool::new(store_in(&workspace, &builtin))
            .with_allowed_actions(&SkillAction::NON_DESTRUCTIVE);
        assert_eq!(
            tool.parameters()["properties"]["action"]["enum"],
            json!(["create", "patch", "write_file"])
        );
        let description = tool.description();
        assert!(!description.contains("delete"), "{description}");
        assert!(!description.contains("remove_file"), "{description}");
        assert!(description.starts_with("Create or update skills"));
        assert!(
            tool_in(&workspace, &builtin)
                .description()
                .starts_with("Create, update or delete")
        );
    }

    #[tokio::test]
    async fn a_restricted_tool_refuses_other_actions_and_keeps_the_skill() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = store_in(&workspace, &builtin);
        store.create("demo", &document("demo")).unwrap();
        let tool = SkillManageTool::new(store).with_allowed_actions(&SkillAction::NON_DESTRUCTIVE);
        let result = tool
            .execute_result(&json!({"action": "delete", "name": "demo"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not allowed"), "{}", result.content);
        assert!(result.content.contains("create, patch, write_file"));
        assert!(workspace.path().join("skills/demo/SKILL.md").is_file());
    }

    #[tokio::test]
    async fn propose_mode_stages_the_change_instead_of_writing_it() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = store_in(&workspace, &builtin);
        let proposals = Arc::new(SkillProposalStore::new(
            workspace.path(),
            Arc::clone(&store),
        ));
        let tool =
            SkillManageTool::new(store).with_mode(SkillWriteMode::Propose(Arc::clone(&proposals)));
        assert!(tool.parameters()["properties"]["reason"].is_object());
        assert!(tool.description().contains("staged as proposals"));

        let result = tool
            .execute_result(&json!({"action": "create", "name": "demo",
                                    "content": document("demo"), "reason": "a lesson"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("Proposal 1 staged"),
            "{}",
            result.content
        );
        assert!(!workspace.path().join("skills/demo").exists());
        let pending = proposals.list();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].reason.as_deref(), Some("a lesson"));
    }

    #[tokio::test]
    async fn propose_mode_refuses_an_invalid_change() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = store_in(&workspace, &builtin);
        let proposals = Arc::new(SkillProposalStore::new(
            workspace.path(),
            Arc::clone(&store),
        ));
        let tool =
            SkillManageTool::new(store).with_mode(SkillWriteMode::Propose(Arc::clone(&proposals)));
        let result = tool
            .execute_result(&json!({"action": "patch", "name": "ghost",
                                    "old_string": "a", "new_string": "b"}))
            .await;
        assert!(result.is_error);
        assert!(proposals.list().is_empty());
    }

    #[test]
    fn the_default_tool_has_no_reason_parameter() {
        let (workspace, builtin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        assert!(tool_in(&workspace, &builtin).parameters()["properties"]["reason"].is_null());
    }
}
