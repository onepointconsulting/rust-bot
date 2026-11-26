//! One change to a skill, as a value: what the `skill_manage` tool asks for,
//! what [`crate::agent::skill_store::SkillStore`] applies or dry-runs, and what
//! a staged skill proposal stores on disk.
//!
//! [`SkillOperation::from_params`] holds the field checks for the tool's
//! parameters, so the tool, the store and the proposal store share one model.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An action name the `skill_manage` tool accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillAction {
    Create,
    Patch,
    WriteFile,
    RemoveFile,
    Delete,
}

impl SkillAction {
    /// Every action, in the order the tool lists them.
    pub const ALL: [SkillAction; 5] = [
        SkillAction::Create,
        SkillAction::Patch,
        SkillAction::WriteFile,
        SkillAction::RemoveFile,
        SkillAction::Delete,
    ];

    /// The actions a background agent (Dream) may use: it never removes or
    /// deletes anything.
    pub const NON_DESTRUCTIVE: [SkillAction; 3] = [
        SkillAction::Create,
        SkillAction::Patch,
        SkillAction::WriteFile,
    ];

    /// The name the model passes as `action`.
    pub fn as_str(self) -> &'static str {
        match self {
            SkillAction::Create => "create",
            SkillAction::Patch => "patch",
            SkillAction::WriteFile => "write_file",
            SkillAction::RemoveFile => "remove_file",
            SkillAction::Delete => "delete",
        }
    }

    /// The action named `name`, if any.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.as_str() == name)
    }
}

/// A single change to a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum SkillOperation {
    /// Create the skill `name` from the full `SKILL.md` text.
    Create { name: String, content: String },
    /// Replace the whole `SKILL.md` of an existing skill (`patch` with `content`).
    Rewrite { name: String, content: String },
    /// Replace `old_string` with `new_string` in `SKILL.md`, or in the
    /// supporting file `file_path` when given.
    Patch {
        name: String,
        old_string: String,
        new_string: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_path: Option<String>,
        #[serde(default)]
        replace_all: bool,
    },
    /// Add or overwrite a supporting file.
    WriteFile {
        name: String,
        file_path: String,
        file_content: String,
    },
    /// Remove a supporting file.
    RemoveFile { name: String, file_path: String },
    /// Delete a workspace skill.
    Delete { name: String },
}

impl SkillOperation {
    /// Build an operation from the `skill_manage` tool's parameters, checking
    /// that every field the action needs is present.
    ///
    /// `patch` has two shapes: `old_string` + `new_string` for a targeted
    /// replacement, or `content` alone for a full `SKILL.md` rewrite.
    pub fn from_params(params: &Value) -> Result<Self, String> {
        let action_name = required_text(params, "action")?;
        let name = required_text(params, "name")?.to_string();
        let action = SkillAction::parse(action_name).ok_or_else(|| {
            format!(
                "Error: unknown action '{action_name}'. Use create, patch, write_file, \
                 remove_file or delete."
            )
        })?;
        Ok(match action {
            SkillAction::Create => SkillOperation::Create {
                name,
                content: required_text(params, "content")?.to_string(),
            },
            SkillAction::Patch => Self::patch_from_params(name, params)?,
            SkillAction::WriteFile => SkillOperation::WriteFile {
                name,
                file_path: required_text(params, "file_path")?.to_string(),
                file_content: required_text_allow_empty(params, "file_content")?.to_string(),
            },
            SkillAction::RemoveFile => SkillOperation::RemoveFile {
                name,
                file_path: required_text(params, "file_path")?.to_string(),
            },
            SkillAction::Delete => SkillOperation::Delete { name },
        })
    }

    /// The `patch` action's operation: a rewrite when `content` is given,
    /// otherwise a find-and-replace.
    fn patch_from_params(name: String, params: &Value) -> Result<Self, String> {
        let has_replacement_fields =
            params.get("old_string").is_some() || params.get("new_string").is_some();
        if let Some(content) = optional_text(params, "content") {
            if has_replacement_fields {
                return Err(
                    "Error: patch takes either old_string/new_string or content \
                     (full rewrite), not both."
                        .to_string(),
                );
            }
            return Ok(SkillOperation::Rewrite {
                name,
                content: content.to_string(),
            });
        }
        Ok(SkillOperation::Patch {
            name,
            old_string: required_text(params, "old_string")?.to_string(),
            new_string: required_text_allow_empty(params, "new_string")?.to_string(),
            file_path: optional_text(params, "file_path").map(str::to_string),
            replace_all: params
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// The tool action this operation belongs to (a rewrite is a `patch`).
    pub fn action(&self) -> SkillAction {
        match self {
            SkillOperation::Create { .. } => SkillAction::Create,
            SkillOperation::Rewrite { .. } | SkillOperation::Patch { .. } => SkillAction::Patch,
            SkillOperation::WriteFile { .. } => SkillAction::WriteFile,
            SkillOperation::RemoveFile { .. } => SkillAction::RemoveFile,
            SkillOperation::Delete { .. } => SkillAction::Delete,
        }
    }

    /// The name of the skill this operation changes.
    pub fn skill_name(&self) -> &str {
        match self {
            SkillOperation::Create { name, .. }
            | SkillOperation::Rewrite { name, .. }
            | SkillOperation::Patch { name, .. }
            | SkillOperation::WriteFile { name, .. }
            | SkillOperation::RemoveFile { name, .. }
            | SkillOperation::Delete { name } => name,
        }
    }

    /// A short human label, e.g. `patch references/api.md`.
    pub fn label(&self) -> String {
        match self {
            SkillOperation::Create { .. } => "create".to_string(),
            SkillOperation::Rewrite { .. } => "rewrite SKILL.md".to_string(),
            SkillOperation::Patch { file_path, .. } => {
                format!("patch {}", file_path.as_deref().unwrap_or("SKILL.md"))
            }
            SkillOperation::WriteFile { file_path, .. } => format!("write_file {file_path}"),
            SkillOperation::RemoveFile { file_path, .. } => format!("remove_file {file_path}"),
            SkillOperation::Delete { .. } => "delete".to_string(),
        }
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
pub(crate) fn optional_text<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn actions_round_trip_through_their_names() {
        for action in SkillAction::ALL {
            assert_eq!(SkillAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(SkillAction::parse("rename"), None);
        assert!(!SkillAction::NON_DESTRUCTIVE.contains(&SkillAction::Delete));
        assert!(!SkillAction::NON_DESTRUCTIVE.contains(&SkillAction::RemoveFile));
    }

    #[test]
    fn missing_fields_are_errors_naming_the_field() {
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
            let error = SkillOperation::from_params(&params).unwrap_err();
            assert!(error.contains(field), "{params}: {error}");
        }
    }

    #[test]
    fn an_unknown_action_is_an_error() {
        let error =
            SkillOperation::from_params(&json!({"action": "rename", "name": "demo"})).unwrap_err();
        assert!(error.contains("unknown action 'rename'"));
    }

    #[test]
    fn patch_with_content_is_a_rewrite_and_with_both_shapes_is_refused() {
        let rewrite = SkillOperation::from_params(
            &json!({"action": "patch", "name": "demo", "content": "doc"}),
        )
        .unwrap();
        assert_eq!(
            rewrite,
            SkillOperation::Rewrite {
                name: "demo".into(),
                content: "doc".into()
            }
        );
        assert_eq!(rewrite.action(), SkillAction::Patch);

        let both = SkillOperation::from_params(&json!({"action": "patch", "name": "demo",
            "content": "doc", "old_string": "a", "new_string": "b"}))
        .unwrap_err();
        assert!(both.contains("not both"));
    }

    #[test]
    fn patch_reads_optional_fields_and_allows_an_empty_new_string() {
        let patch = SkillOperation::from_params(&json!({"action": "patch", "name": "demo",
            "old_string": "a", "new_string": "", "file_path": "references/x.md",
            "replace_all": true}))
        .unwrap();
        assert_eq!(
            patch,
            SkillOperation::Patch {
                name: "demo".into(),
                old_string: "a".into(),
                new_string: String::new(),
                file_path: Some("references/x.md".into()),
                replace_all: true,
            }
        );
        assert_eq!(patch.label(), "patch references/x.md");
        assert_eq!(patch.skill_name(), "demo");
    }

    #[test]
    fn operations_serialise_with_an_operation_tag_and_read_back() {
        let operation = SkillOperation::WriteFile {
            name: "demo".into(),
            file_path: "references/a.md".into(),
            file_content: "text".into(),
        };
        let json = serde_json::to_value(&operation).unwrap();
        assert_eq!(json["operation"], "write_file");
        let back: SkillOperation = serde_json::from_value(json).unwrap();
        assert_eq!(back, operation);
    }
}
