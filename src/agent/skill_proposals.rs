//! Staged skill changes waiting for the user's decision.
//!
//! When `tools.skills.requireApproval` is on, Dream does not write skills: each
//! change it wants becomes a proposal file under `memory/skill_proposals/`.
//! That folder is outside `skills/`, so [`crate::agent::skills::SkillsLoader`]
//! never loads unapproved content. The user lists, accepts or rejects
//! proposals with `/skills-review`.
//!
//! Only valid proposals are staged ([`SkillStore::check`] runs first), and
//! accepting re-checks, so a skill edited in the meantime fails cleanly
//! instead of being overwritten.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::agent::skill_operation::SkillOperation;
use crate::agent::skill_store::{SkillChange, SkillStore};
use crate::utils::fs::write_atomic;

/// Folder, relative to the workspace, holding pending proposals.
pub const SKILL_PROPOSALS_DIR: &str = "memory/skill_proposals";

/// Extension of a proposal file.
const PROPOSAL_EXTENSION: &str = "json";

/// One staged skill change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillProposal {
    /// Short numeric id the user types in `/skills-review`.
    pub id: String,
    /// When the proposal was staged (RFC 3339, local time).
    pub created_at: String,
    /// Who proposed it, e.g. `dream`.
    pub origin: String,
    /// The lesson the change records, as the proposer stated it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The change itself.
    pub operation: SkillOperation,
}

/// Stores, lists, accepts and rejects skill proposals.
pub struct SkillProposalStore {
    directory: PathBuf,
    skills: Arc<SkillStore>,
    /// Serialises id allocation so two proposals never share an id.
    id_lock: Mutex<()>,
}

impl SkillProposalStore {
    /// A store keeping proposals in `<workspace>/memory/skill_proposals` and
    /// applying accepted ones through `skills`.
    pub fn new(workspace: &Path, skills: Arc<SkillStore>) -> Self {
        Self {
            directory: workspace.join(SKILL_PROPOSALS_DIR),
            skills,
            id_lock: Mutex::new(()),
        }
    }

    /// The skill store accepted proposals are applied through.
    pub fn skills(&self) -> &Arc<SkillStore> {
        &self.skills
    }

    /// Stage `operation` after checking it would apply now.
    ///
    /// `origin` names the proposer (e.g. `dream`); `reason` is the lesson it
    /// records. Returns the stored proposal, or the validation error.
    pub fn propose(
        &self,
        operation: SkillOperation,
        origin: &str,
        reason: Option<String>,
    ) -> Result<SkillProposal, String> {
        self.skills.check(&operation)?;
        let _guard = self.id_lock.lock().unwrap_or_else(|e| e.into_inner());
        let proposal = SkillProposal {
            id: self.next_id().to_string(),
            created_at: chrono::Local::now().to_rfc3339(),
            origin: origin.to_string(),
            reason: reason.filter(|text| !text.trim().is_empty()),
            operation,
        };
        let json = serde_json::to_string_pretty(&proposal)
            .map_err(|error| format!("Error: could not encode the proposal: {error}"))?;
        write_atomic(&self.path_of(&proposal.id), json.as_bytes())
            .map_err(|error| format!("Error: could not stage the proposal: {error}"))?;
        Ok(proposal)
    }

    /// Every pending proposal, oldest first. Unreadable files are skipped.
    pub fn list(&self) -> Vec<SkillProposal> {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        let mut proposals: Vec<SkillProposal> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == PROPOSAL_EXTENSION))
            .filter_map(|path| read_proposal(&path))
            .collect();
        proposals.sort_by_key(|proposal| numeric_id(&proposal.id).unwrap_or(u64::MAX));
        proposals
    }

    /// The pending proposal `id`.
    pub fn get(&self, id: &str) -> Result<SkillProposal, String> {
        if numeric_id(id).is_none() {
            return Err(format!("No skill proposal `{id}`: ids are numbers."));
        }
        read_proposal(&self.path_of(id)).ok_or_else(|| format!("No pending skill proposal `{id}`."))
    }

    /// Apply the proposal `id` and remove it from the queue.
    ///
    /// [`SkillStore::apply`] validates the operation again against the skills
    /// as they are now: when the skill changed since it was proposed (e.g. a
    /// patch's text is gone, or a skill of that name now exists) nothing is
    /// written and the proposal stays pending.
    pub fn accept(&self, id: &str) -> Result<SkillChange, String> {
        let proposal = self.get(id)?;
        let change = self.skills.apply(&proposal.operation).map_err(|error| {
            format!(
                "Proposal `{id}` no longer applies; the skill has changed since it was \
                 proposed. {error}"
            )
        })?;
        self.remove_file(id)?;
        Ok(change)
    }

    /// Drop the proposal `id` without applying it.
    pub fn reject(&self, id: &str) -> Result<SkillProposal, String> {
        let proposal = self.get(id)?;
        self.remove_file(id)?;
        Ok(proposal)
    }

    /// The file of the proposal `id`.
    fn path_of(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{id}.{PROPOSAL_EXTENSION}"))
    }

    /// Delete the file of the proposal `id`.
    fn remove_file(&self, id: &str) -> Result<(), String> {
        fs::remove_file(self.path_of(id))
            .map_err(|error| format!("Error: could not remove proposal `{id}`: {error}"))
    }

    /// One more than the highest pending id (1 when the queue is empty).
    fn next_id(&self) -> u64 {
        let highest = fs::read_dir(&self.directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let stem = path.file_stem()?.to_str()?.to_string();
                numeric_id(&stem)
            })
            .max()
            .unwrap_or(0);
        highest + 1
    }
}

/// `id` as a number, when it is one (also guards file paths built from it).
fn numeric_id(id: &str) -> Option<u64> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    id.parse().ok()
}

/// The proposal stored at `path`, or `None` when it is missing or unreadable.
fn read_proposal(path: &Path) -> Option<SkillProposal> {
    let text = fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(proposal) => Some(proposal),
        Err(error) => {
            log::warn!(
                "Skipping unreadable skill proposal {}: {error}",
                path.display()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill_store::SkillLimits;
    use tempfile::TempDir;

    /// A workspace with an empty bundled-skills folder and a proposal store.
    struct Fixture {
        workspace: TempDir,
        _builtin: TempDir,
        proposals: SkillProposalStore,
    }

    impl Fixture {
        fn new() -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let builtin = tempfile::tempdir().unwrap();
            let skills = Arc::new(SkillStore::new(
                workspace.path(),
                Some(builtin.path().to_path_buf()),
                SkillLimits::default(),
            ));
            let proposals = SkillProposalStore::new(workspace.path(), skills);
            Self {
                workspace,
                _builtin: builtin,
                proposals,
            }
        }

        fn skill_file(&self, name: &str) -> PathBuf {
            self.workspace.path().join("skills").join(name).join("SKILL.md")
        }
    }

    fn document(name: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: Use when testing.\n---\n\n{body}\n")
    }

    fn create(name: &str) -> SkillOperation {
        SkillOperation::Create {
            name: name.into(),
            content: document(name, "Step one."),
        }
    }

    #[test]
    fn propose_stages_a_valid_operation_without_writing_the_skill() {
        let fixture = Fixture::new();
        let proposal = fixture
            .proposals
            .propose(create("demo"), "dream", Some("lesson".into()))
            .unwrap();
        assert_eq!(proposal.id, "1");
        assert_eq!(proposal.origin, "dream");
        assert_eq!(proposal.reason.as_deref(), Some("lesson"));
        assert!(!fixture.skill_file("demo").exists());
        assert!(
            fixture
                .workspace
                .path()
                .join(SKILL_PROPOSALS_DIR)
                .join("1.json")
                .is_file()
        );
    }

    #[test]
    fn propose_rejects_an_invalid_operation() {
        let fixture = Fixture::new();
        let invalid = SkillOperation::Create {
            name: "demo".into(),
            content: "no frontmatter".into(),
        };
        assert!(fixture.proposals.propose(invalid, "dream", None).is_err());
        let patch_of_missing = SkillOperation::Patch {
            name: "ghost".into(),
            old_string: "a".into(),
            new_string: "b".into(),
            file_path: None,
            replace_all: false,
        };
        assert!(
            fixture
                .proposals
                .propose(patch_of_missing, "dream", None)
                .is_err()
        );
        assert!(fixture.proposals.list().is_empty());
    }

    #[test]
    fn list_is_in_id_order_and_ids_keep_growing() {
        let fixture = Fixture::new();
        for index in 0..11 {
            fixture
                .proposals
                .propose(create(&format!("skill-{index}")), "dream", None)
                .unwrap();
        }
        let ids: Vec<String> = fixture
            .proposals
            .list()
            .into_iter()
            .map(|proposal| proposal.id)
            .collect();
        let expected: Vec<String> = (1..=11).map(|id| id.to_string()).collect();
        assert_eq!(ids, expected);

        fixture.proposals.reject("5").unwrap();
        let next = fixture
            .proposals
            .propose(create("another"), "dream", None)
            .unwrap();
        assert_eq!(next.id, "12");
    }

    #[test]
    fn accept_applies_the_operation_and_clears_the_proposal() {
        let fixture = Fixture::new();
        fixture
            .proposals
            .propose(create("demo"), "dream", None)
            .unwrap();
        let change = fixture.proposals.accept("1").unwrap();
        assert_eq!(change.path, fixture.skill_file("demo"));
        assert!(fixture.skill_file("demo").is_file());
        assert!(fixture.proposals.list().is_empty());
        assert!(fixture.proposals.accept("1").is_err());
    }

    #[test]
    fn accept_after_a_conflicting_edit_fails_cleanly_and_keeps_the_proposal() {
        let fixture = Fixture::new();
        let skills = Arc::clone(fixture.proposals.skills());
        skills
            .create("demo", &document("demo", "Run step one."))
            .unwrap();
        let patch = SkillOperation::Patch {
            name: "demo".into(),
            old_string: "Run step one.".into(),
            new_string: "Run step two.".into(),
            file_path: None,
            replace_all: false,
        };
        fixture.proposals.propose(patch, "dream", None).unwrap();

        // The user edits the skill before reviewing the proposal.
        skills
            .rewrite("demo", &document("demo", "Something else entirely."))
            .unwrap();

        let error = fixture.proposals.accept("1").unwrap_err();
        assert!(error.contains("no longer applies"), "{error}");
        let text = fs::read_to_string(fixture.skill_file("demo")).unwrap();
        assert!(text.contains("Something else entirely."));
        assert_eq!(fixture.proposals.list().len(), 1);
    }

    #[test]
    fn reject_deletes_the_proposal_without_applying_it() {
        let fixture = Fixture::new();
        fixture
            .proposals
            .propose(create("demo"), "dream", None)
            .unwrap();
        let rejected = fixture.proposals.reject("1").unwrap();
        assert_eq!(rejected.operation, create("demo"));
        assert!(fixture.proposals.list().is_empty());
        assert!(!fixture.skill_file("demo").exists());
    }

    #[test]
    fn ids_that_are_not_numbers_are_refused() {
        let fixture = Fixture::new();
        assert!(fixture.proposals.get("../../SOUL").is_err());
        assert!(fixture.proposals.reject("").is_err());
        assert!(fixture.proposals.accept("1a").is_err());
    }
}
