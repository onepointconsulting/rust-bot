//! The parent's store of child agents.
//!
//! A child is a folder under `<parent workspace>/acp/agents/<name>/`:
//!
//! ```text
//! agent.json    who it is: purpose, preset, overrides, creation/resync times, depth
//! overlay.json  JSON merge patch over the parent config (checked by `config::overlay`)
//! SOUL.md  AGENTS.md  USER.md  TOOLS.md   snapshots of the parent's files
//! memory/  sessions/                      the child's own long-term memory and threads
//! ```
//!
//! The folder is the child's *home*, never the folder it works on. The directory
//! is the registry: children are found by scanning for `agent.json`, so there is
//! no index to keep in step.
//!
//! The bootstrap files are a snapshot taken at creation, on purpose: the child's
//! own Dream edits `SOUL.md` and `USER.md` over time, and reading them live from
//! the parent would either lose those edits or overwrite the parent's. The parent
//! refreshes them explicitly (`resync_from_parent`), and [`ChildAgentStore::list`]
//! reports which files the parent has changed since.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::context::{AGENTS_FILE, BOOTSTRAP_FILES, SOUL_FILE, USER_FILE};
use crate::config::overlay::{OverlayError, validate_overlay};
use crate::config::schema::Config;
use crate::utils::fs::write_atomic;

/// Folder, below the parent's workspace, that holds every child.
const AGENTS_ROOT: [&str; 2] = ["acp", "agents"];
/// Longest agent name.
const MAX_NAME_LENGTH: usize = 48;
/// Names Windows reserves for devices; a folder with such a name misbehaves.
const WINDOWS_RESERVED_NAMES: [&str; 6] = ["con", "prn", "aux", "nul", "com1", "lpt1"];

/// How an override combines with the parent's file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OverrideMode {
    /// Add the text after the parent's file, keeping the inherited rules.
    #[default]
    Append,
    /// Use only the given text.
    Replace,
}

/// A change to one of the child's bootstrap files, applied on top of the parent's.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FileOverride {
    #[serde(default)]
    pub mode: OverrideMode,
    pub text: String,
}

impl FileOverride {
    /// `base` (the parent's file, when it has one) with this override applied.
    fn apply(&self, base: Option<&str>) -> String {
        match (self.mode, base) {
            (OverrideMode::Replace, _) | (OverrideMode::Append, None) => self.text.clone(),
            (OverrideMode::Append, Some(base)) if base.trim().is_empty() => self.text.clone(),
            (OverrideMode::Append, Some(base)) => {
                format!("{}\n\n{}", base.trim_end(), self.text)
            }
        }
    }
}

/// The overrides of the three files an agent's author may change.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Overrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soul: Option<FileOverride>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<FileOverride>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<FileOverride>,
}

impl Overrides {
    /// The override that belongs to bootstrap file `file_name` (`TOOLS.md` has none).
    pub fn for_file(&self, file_name: &str) -> Option<&FileOverride> {
        match file_name {
            SOUL_FILE => self.soul.as_ref(),
            AGENTS_FILE => self.agents.as_ref(),
            USER_FILE => self.user.as_ref(),
            _ => None,
        }
    }

    /// Replace the overrides that `changes` sets; the others stay.
    fn merge(&mut self, changes: &Overrides) {
        if changes.soul.is_some() {
            self.soul = changes.soul.clone();
        }
        if changes.agents.is_some() {
            self.agents = changes.agents.clone();
        }
        if changes.user.is_some() {
            self.user = changes.user.clone();
        }
    }

    /// Bootstrap files whose override is set.
    fn files_with_overrides(&self) -> Vec<&'static str> {
        [SOUL_FILE, AGENTS_FILE, USER_FILE]
            .into_iter()
            .filter(|file| self.for_file(file).is_some())
            .collect()
    }
}

/// `agent.json`: what is known about a child besides its files.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMeta {
    pub name: String,
    /// One line on what the agent is for; shown to the model so it routes questions.
    pub purpose: String,
    /// Launch preset key. Only `rustbot` is launched in this milestone.
    pub preset: String,
    pub created_at: String,
    /// Session key of the parent session that created it.
    pub created_by: String,
    /// Depth of this child: its parent's depth plus one.
    pub depth: u32,
    /// Default project folder for runs that do not pass one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub overrides: Overrides,
    /// When the bootstrap files were last refreshed from the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resynced_at: Option<String>,
}

/// A child as listed: its metadata plus which parent files changed since the
/// snapshot (by file modification time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    pub meta: AgentMeta,
    pub drifted_files: Vec<String>,
}

/// Everything needed to create a child.
#[derive(Debug, Clone)]
pub struct NewAgent {
    pub name: String,
    pub purpose: String,
    pub preset: String,
    /// Merge patch over the parent config; `{}` for "inherit everything".
    pub overlay: Value,
    pub overrides: Overrides,
    pub cwd: Option<String>,
    pub created_by: String,
    pub depth: u32,
}

/// Changes to an existing child. A file with a changed override is re-copied from
/// the parent first, so the override is always relative to the parent's text.
#[derive(Debug, Clone, Default)]
pub struct AgentChanges {
    pub purpose: Option<String>,
    /// Replaces the stored overlay as a whole.
    pub overlay: Option<Value>,
    pub overrides: Overrides,
    pub cwd: Option<String>,
    /// Bootstrap files to re-copy from the parent (the stored overrides are re-applied).
    pub resync_from_parent: Vec<String>,
}

/// What an update did, for the tool result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateReport {
    /// Files re-copied from the parent; the child's own edits to them are gone.
    pub resynced_files: Vec<String>,
}

/// Why a store operation failed.
#[derive(Debug)]
pub enum StoreError {
    InvalidName(String),
    AlreadyExists(String),
    NotFound(String),
    Overlay(OverlayError),
    /// A request that makes no sense, e.g. resyncing a file that is not a bootstrap file.
    Invalid(String),
    Io(io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::InvalidName(reason) => write!(f, "invalid agent name: {reason}"),
            StoreError::AlreadyExists(name) => write!(
                f,
                "agent '{name}' already exists; use acp_update_agent to change it or acp_run_agent to use it"
            ),
            StoreError::NotFound(name) => write!(f, "agent '{name}' does not exist"),
            StoreError::Overlay(error) => write!(f, "{error}"),
            StoreError::Invalid(reason) => write!(f, "{reason}"),
            StoreError::Io(error) => write!(f, "agent store I/O error: {error}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        StoreError::Io(error)
    }
}

impl From<OverlayError> for StoreError {
    fn from(error: OverlayError) -> Self {
        StoreError::Overlay(error)
    }
}

/// Check an agent name: it becomes a folder name, so it must be plain.
pub fn validate_agent_name(name: &str) -> Result<(), StoreError> {
    let invalid = |reason: &str| Err(StoreError::InvalidName(format!("'{name}': {reason}")));
    if name.is_empty() {
        return invalid("it is empty");
    }
    if name.len() > MAX_NAME_LENGTH {
        return invalid(&format!("it is longer than {MAX_NAME_LENGTH} characters"));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or(' ');
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return invalid("it must start with a lowercase letter or a digit");
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return invalid("use only lowercase letters, digits, '-' and '_'");
    }
    if WINDOWS_RESERVED_NAMES.contains(&name) {
        return invalid("it is a reserved device name on Windows");
    }
    Ok(())
}

/// Read, write and scan the children of one parent workspace.
#[derive(Debug, Clone)]
pub struct ChildAgentStore {
    parent_workspace: PathBuf,
}

impl ChildAgentStore {
    pub fn new(parent_workspace: impl Into<PathBuf>) -> Self {
        Self {
            parent_workspace: parent_workspace.into(),
        }
    }

    /// The folder that holds all children.
    pub fn agents_root(&self) -> PathBuf {
        AGENTS_ROOT
            .iter()
            .fold(self.parent_workspace.clone(), |path, part| path.join(part))
    }

    /// A child's home folder.
    pub fn agent_dir(&self, name: &str) -> PathBuf {
        self.agents_root().join(name)
    }

    /// Path of a child's `overlay.json`.
    pub fn overlay_path(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("overlay.json")
    }

    fn meta_path(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("agent.json")
    }

    /// Create a child: validate, then lay out the folder.
    ///
    /// Fails for an existing name, so a repeated create can never wipe a child's
    /// memory. A failure part-way removes the half-built folder.
    pub fn create(
        &self,
        request: NewAgent,
        parent_config: &Config,
    ) -> Result<AgentMeta, StoreError> {
        validate_agent_name(&request.name)?;
        validate_overlay(parent_config, &request.overlay)?;

        fs::create_dir_all(self.agents_root())?;
        let dir = self.agent_dir(&request.name);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(StoreError::AlreadyExists(request.name));
            }
            Err(error) => return Err(error.into()),
        }

        let result = self.populate(&dir, &request);
        if result.is_err() {
            let _ = fs::remove_dir_all(&dir);
        }
        result
    }

    fn populate(&self, dir: &Path, request: &NewAgent) -> Result<AgentMeta, StoreError> {
        fs::create_dir_all(dir.join("memory"))?;
        fs::create_dir_all(dir.join("sessions"))?;
        for file in BOOTSTRAP_FILES {
            self.write_bootstrap_file(dir, file, &request.overrides)?;
        }
        let meta = AgentMeta {
            name: request.name.clone(),
            purpose: request.purpose.clone(),
            preset: request.preset.clone(),
            created_at: Utc::now().to_rfc3339(),
            created_by: request.created_by.clone(),
            depth: request.depth,
            cwd: request.cwd.clone(),
            overrides: request.overrides.clone(),
            resynced_at: None,
        };
        write_json(&dir.join("overlay.json"), &request.overlay)?;
        write_json(&dir.join("agent.json"), &meta)?;
        Ok(meta)
    }

    /// Copy one bootstrap file from the parent and apply the override for it.
    /// A file the parent does not have is created only when an override gives text;
    /// otherwise the child's own startup fills in the default.
    fn write_bootstrap_file(
        &self,
        dir: &Path,
        file: &str,
        overrides: &Overrides,
    ) -> Result<(), StoreError> {
        let parent_text = fs::read_to_string(self.parent_workspace.join(file)).ok();
        let text = match overrides.for_file(file) {
            Some(file_override) => Some(file_override.apply(parent_text.as_deref())),
            None => parent_text,
        };
        if let Some(text) = text {
            write_atomic(&dir.join(file), text.as_bytes())?;
        }
        Ok(())
    }

    /// A child's metadata.
    pub fn get(&self, name: &str) -> Result<AgentMeta, StoreError> {
        validate_agent_name(name)?;
        let text = match fs::read_to_string(self.meta_path(name)) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(name.to_string()));
            }
            Err(error) => return Err(error.into()),
        };
        serde_json::from_str(&text).map_err(|e| {
            StoreError::Invalid(format!("agent '{name}' has a damaged agent.json: {e}"))
        })
    }

    /// A child's stored overlay.
    pub fn read_overlay(&self, name: &str) -> Result<Value, StoreError> {
        validate_agent_name(name)?;
        crate::config::overlay::read_overlay(&self.overlay_path(name)).map_err(Into::into)
    }

    /// Every child, by name, with the parent files that changed since each snapshot.
    /// Folders without a readable `agent.json` are skipped (and logged).
    pub fn list(&self) -> Vec<AgentSummary> {
        let Ok(entries) = fs::read_dir(self.agents_root()) else {
            return Vec::new();
        };
        let mut summaries: Vec<AgentSummary> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                match self.get(&name) {
                    Ok(meta) => Some(AgentSummary {
                        drifted_files: self.drifted_files(&meta),
                        meta,
                    }),
                    Err(error) => {
                        log::warn!("skipping agent folder '{name}': {error}");
                        None
                    }
                }
            })
            .collect();
        summaries.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        summaries
    }

    /// Bootstrap files the parent modified after the child's snapshot.
    fn drifted_files(&self, meta: &AgentMeta) -> Vec<String> {
        let reference = meta
            .resynced_at
            .as_deref()
            .unwrap_or(&meta.created_at)
            .parse::<DateTime<Utc>>();
        let Ok(reference) = reference else {
            return Vec::new();
        };
        BOOTSTRAP_FILES
            .iter()
            .filter(|file| {
                fs::metadata(self.parent_workspace.join(file))
                    .and_then(|metadata| metadata.modified())
                    .is_ok_and(|modified| system_time_is_after(modified, reference))
            })
            .map(|file| file.to_string())
            .collect()
    }

    /// Apply `changes` to an existing child.
    ///
    /// The overlay is validated before anything is written. A file whose override
    /// changes, or that is listed in `resync_from_parent`, is re-copied from the
    /// parent and the stored overrides are applied again.
    pub fn update(
        &self,
        name: &str,
        changes: AgentChanges,
        parent_config: &Config,
    ) -> Result<UpdateReport, StoreError> {
        let mut meta = self.get(name)?;
        if let Some(overlay) = &changes.overlay {
            validate_overlay(parent_config, overlay)?;
        }
        let mut files_to_resync: Vec<&'static str> = Vec::new();
        for requested in &changes.resync_from_parent {
            let Some(file) = BOOTSTRAP_FILES
                .iter()
                .find(|file| **file == requested.as_str())
            else {
                return Err(StoreError::Invalid(format!(
                    "'{requested}' cannot be resynced; the files are {}",
                    BOOTSTRAP_FILES.join(", ")
                )));
            };
            files_to_resync.push(file);
        }

        meta.overrides.merge(&changes.overrides);
        for file in changes.overrides.files_with_overrides() {
            if !files_to_resync.contains(&file) {
                files_to_resync.push(file);
            }
        }
        if let Some(purpose) = changes.purpose {
            meta.purpose = purpose;
        }
        if let Some(cwd) = changes.cwd {
            meta.cwd = if cwd.trim().is_empty() {
                None
            } else {
                Some(cwd)
            };
        }

        let dir = self.agent_dir(name);
        if let Some(overlay) = &changes.overlay {
            write_json(&dir.join("overlay.json"), overlay)?;
        }
        for file in &files_to_resync {
            self.write_bootstrap_file(&dir, file, &meta.overrides)?;
        }
        if !files_to_resync.is_empty() {
            meta.resynced_at = Some(Utc::now().to_rfc3339());
        }
        write_json(&dir.join("agent.json"), &meta)?;

        Ok(UpdateReport {
            resynced_files: files_to_resync
                .iter()
                .map(|file| file.to_string())
                .collect(),
        })
    }
}

fn system_time_is_after(time: SystemTime, reference: DateTime<Utc>) -> bool {
    DateTime::<Utc>::from(time) > reference
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::TOOLS_FILE;
    use serde_json::json;

    /// A parent workspace with all four bootstrap files.
    fn parent_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(SOUL_FILE), "parent soul").unwrap();
        fs::write(dir.path().join(AGENTS_FILE), "parent agents").unwrap();
        fs::write(dir.path().join(USER_FILE), "parent user").unwrap();
        fs::write(dir.path().join(TOOLS_FILE), "parent tools").unwrap();
        dir
    }

    fn request(name: &str) -> NewAgent {
        NewAgent {
            name: name.to_string(),
            purpose: "reviews code".to_string(),
            preset: "rustbot".to_string(),
            overlay: json!({}),
            overrides: Overrides::default(),
            cwd: None,
            created_by: "cli:direct".to_string(),
            depth: 1,
        }
    }

    fn append(text: &str) -> Option<FileOverride> {
        Some(FileOverride {
            mode: OverrideMode::Append,
            text: text.to_string(),
        })
    }

    fn replace(text: &str) -> Option<FileOverride> {
        Some(FileOverride {
            mode: OverrideMode::Replace,
            text: text.to_string(),
        })
    }

    fn read(store: &ChildAgentStore, name: &str, file: &str) -> String {
        fs::read_to_string(store.agent_dir(name).join(file)).unwrap()
    }

    // ── names ───────────────────────────────────────────────────────────────

    #[test]
    fn plain_names_are_accepted() {
        for name in ["code-review", "a", "reviewer_2", "0day"] {
            assert!(validate_agent_name(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn names_that_could_escape_or_confuse_the_filesystem_are_rejected() {
        let too_long = "a".repeat(MAX_NAME_LENGTH + 1);
        for name in [
            "", "..", ".", "../x", "a/b", "a\\b", "a b", "-lead", "_lead", "Upper", "dot.dot",
            "con", "nul", &too_long, "ünï",
        ] {
            assert!(
                matches!(validate_agent_name(name), Err(StoreError::InvalidName(_))),
                "'{name}' should be rejected"
            );
        }
    }

    // ── create ──────────────────────────────────────────────────────────────

    #[test]
    fn create_lays_out_the_home_and_copies_the_bootstrap_files() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());

        let meta = store
            .create(request("code-review"), &Config::default())
            .unwrap();

        let home = store.agent_dir("code-review");
        assert!(home.join("memory").is_dir());
        assert!(home.join("sessions").is_dir());
        assert_eq!(read(&store, "code-review", SOUL_FILE), "parent soul");
        assert_eq!(read(&store, "code-review", AGENTS_FILE), "parent agents");
        assert_eq!(read(&store, "code-review", USER_FILE), "parent user");
        assert_eq!(read(&store, "code-review", TOOLS_FILE), "parent tools");
        assert_eq!(store.get("code-review").unwrap(), meta);
        assert_eq!(store.read_overlay("code-review").unwrap(), json!({}));
        assert_eq!(meta.depth, 1);
        assert!(meta.resynced_at.is_none());
    }

    #[test]
    fn overrides_are_applied_and_persisted() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        let mut new_agent = request("reviewer");
        new_agent.overrides.agents = append("You are a code review specialist.");
        new_agent.overrides.soul = replace("A terse reviewer.");

        let meta = store.create(new_agent, &Config::default()).unwrap();

        assert_eq!(
            read(&store, "reviewer", AGENTS_FILE),
            "parent agents\n\nYou are a code review specialist."
        );
        assert_eq!(read(&store, "reviewer", SOUL_FILE), "A terse reviewer.");
        assert_eq!(read(&store, "reviewer", USER_FILE), "parent user");
        // Persisted, so a later resync can apply them again.
        assert_eq!(store.get("reviewer").unwrap().overrides, meta.overrides);
        assert!(meta.overrides.agents.is_some() && meta.overrides.soul.is_some());
    }

    #[test]
    fn appending_to_a_missing_parent_file_writes_just_the_text() {
        let parent = tempfile::tempdir().unwrap();
        let store = ChildAgentStore::new(parent.path());
        let mut new_agent = request("solo");
        new_agent.overrides.user = append("Gil likes brevity.");

        store.create(new_agent, &Config::default()).unwrap();

        assert_eq!(read(&store, "solo", USER_FILE), "Gil likes brevity.");
        // No parent file and no override: nothing is written, the child's own
        // startup supplies the default.
        assert!(!store.agent_dir("solo").join(SOUL_FILE).exists());
    }

    #[test]
    fn creating_an_existing_agent_fails_and_keeps_it_intact() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        store
            .create(request("code-review"), &Config::default())
            .unwrap();
        let memory = store
            .agent_dir("code-review")
            .join("memory")
            .join("MEMORY.md");
        fs::write(&memory, "what the child learned").unwrap();

        let error = store
            .create(request("code-review"), &Config::default())
            .unwrap_err();

        assert!(matches!(error, StoreError::AlreadyExists(_)), "{error}");
        assert!(error.to_string().contains("acp_update_agent"));
        assert_eq!(
            fs::read_to_string(memory).unwrap(),
            "what the child learned"
        );
    }

    #[test]
    fn an_invalid_name_or_overlay_creates_nothing() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());

        assert!(
            store
                .create(request("../escape"), &Config::default())
                .is_err()
        );
        let mut widening = request("sneaky");
        widening.overlay = json!({"providers": {"openai": {"apiBase": "https://evil.example"}}});
        let error = store.create(widening, &Config::default()).unwrap_err();

        assert!(matches!(error, StoreError::Overlay(_)), "{error}");
        assert!(
            error.to_string().contains("providers.openai.apiBase"),
            "{error}"
        );
        assert!(!store.agent_dir("sneaky").exists());
        assert!(!parent.path().join("escape").exists());
    }

    #[test]
    fn a_stray_file_with_the_agents_name_counts_as_existing_and_is_kept() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        let blocked = store.agent_dir("broken");
        fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        fs::write(&blocked, "not a folder").unwrap();

        let error = store
            .create(request("broken"), &Config::default())
            .unwrap_err();

        assert!(matches!(error, StoreError::AlreadyExists(_)), "{error}");
        // The stray file is not ours to delete.
        assert!(blocked.is_file());
    }

    // ── get / list ──────────────────────────────────────────────────────────

    #[test]
    fn an_unknown_agent_is_not_found() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        assert!(matches!(store.get("ghost"), Err(StoreError::NotFound(_))));
        assert!(matches!(store.get("../x"), Err(StoreError::InvalidName(_))));
    }

    #[test]
    fn list_scans_the_directory_without_any_index() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        assert!(store.list().is_empty());
        store.create(request("zeta"), &Config::default()).unwrap();
        store.create(request("alpha"), &Config::default()).unwrap();
        // A folder that is not an agent is ignored.
        fs::create_dir_all(store.agents_root().join("stray")).unwrap();

        let names: Vec<String> = store.list().into_iter().map(|s| s.meta.name).collect();

        assert_eq!(names, vec!["alpha", "zeta"]);
        // A second store on the same folder finds them: the folder is the registry.
        let fresh = ChildAgentStore::new(parent.path());
        assert_eq!(fresh.list().len(), 2);
    }

    #[test]
    fn a_damaged_agent_json_is_skipped_by_list_and_reported_by_get() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        store.create(request("good"), &Config::default()).unwrap();
        store.create(request("bad"), &Config::default()).unwrap();
        fs::write(store.agent_dir("bad").join("agent.json"), "{not json").unwrap();

        let names: Vec<String> = store.list().into_iter().map(|s| s.meta.name).collect();

        assert_eq!(names, vec!["good"]);
        assert!(
            store
                .get("bad")
                .unwrap_err()
                .to_string()
                .contains("damaged")
        );
    }

    // ── update ──────────────────────────────────────────────────────────────

    #[test]
    fn update_edits_purpose_cwd_and_overlay() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        store
            .create(request("code-review"), &Config::default())
            .unwrap();

        store
            .update(
                "code-review",
                AgentChanges {
                    purpose: Some("reviews Rust code".to_string()),
                    cwd: Some("C:/repo".to_string()),
                    overlay: Some(json!({"tools": {"exec": {"enable": false}}})),
                    ..AgentChanges::default()
                },
                &Config::default(),
            )
            .unwrap();

        let meta = store.get("code-review").unwrap();
        assert_eq!(meta.purpose, "reviews Rust code");
        assert_eq!(meta.cwd.as_deref(), Some("C:/repo"));
        assert_eq!(
            store.read_overlay("code-review").unwrap(),
            json!({"tools": {"exec": {"enable": false}}})
        );
        // An empty cwd clears the default.
        store
            .update(
                "code-review",
                AgentChanges {
                    cwd: Some(String::new()),
                    ..AgentChanges::default()
                },
                &Config::default(),
            )
            .unwrap();
        assert!(store.get("code-review").unwrap().cwd.is_none());
    }

    #[test]
    fn an_invalid_overlay_update_changes_nothing() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        store
            .create(request("code-review"), &Config::default())
            .unwrap();

        let error = store
            .update(
                "code-review",
                AgentChanges {
                    purpose: Some("changed".to_string()),
                    overlay: Some(json!({"channels": {}})),
                    ..AgentChanges::default()
                },
                &Config::default(),
            )
            .unwrap_err();

        assert!(matches!(error, StoreError::Overlay(_)), "{error}");
        assert_eq!(store.get("code-review").unwrap().purpose, "reviews code");
        assert_eq!(store.read_overlay("code-review").unwrap(), json!({}));
    }

    #[test]
    fn changing_an_override_recopies_that_file_from_the_parent() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        store
            .create(request("reviewer"), &Config::default())
            .unwrap();
        // The child's own Dream edits its file.
        fs::write(store.agent_dir("reviewer").join(AGENTS_FILE), "child edits").unwrap();
        fs::write(
            store.agent_dir("reviewer").join(USER_FILE),
            "child user edits",
        )
        .unwrap();

        let report = store
            .update(
                "reviewer",
                AgentChanges {
                    overrides: Overrides {
                        agents: append("Focus on security."),
                        ..Overrides::default()
                    },
                    ..AgentChanges::default()
                },
                &Config::default(),
            )
            .unwrap();

        assert_eq!(report.resynced_files, vec![AGENTS_FILE]);
        assert_eq!(
            read(&store, "reviewer", AGENTS_FILE),
            "parent agents\n\nFocus on security."
        );
        // Untouched files keep the child's edits.
        assert_eq!(read(&store, "reviewer", USER_FILE), "child user edits");
        assert!(store.get("reviewer").unwrap().resynced_at.is_some());
    }

    #[test]
    fn resync_copies_the_parents_file_and_reapplies_the_stored_override() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        let mut new_agent = request("reviewer");
        new_agent.overrides.user = append("Prefers bullet points.");
        store.create(new_agent, &Config::default()).unwrap();
        fs::write(parent.path().join(USER_FILE), "parent learned more").unwrap();

        let report = store
            .update(
                "reviewer",
                AgentChanges {
                    resync_from_parent: vec![USER_FILE.to_string()],
                    ..AgentChanges::default()
                },
                &Config::default(),
            )
            .unwrap();

        assert_eq!(report.resynced_files, vec![USER_FILE]);
        assert_eq!(
            read(&store, "reviewer", USER_FILE),
            "parent learned more\n\nPrefers bullet points."
        );
    }

    #[test]
    fn resync_rejects_files_that_are_not_bootstrap_files() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        store
            .create(request("reviewer"), &Config::default())
            .unwrap();
        for bad in [
            "memory/MEMORY.md",
            "../USER.md",
            "agent.json",
            "overlay.json",
        ] {
            let error = store
                .update(
                    "reviewer",
                    AgentChanges {
                        resync_from_parent: vec![bad.to_string()],
                        ..AgentChanges::default()
                    },
                    &Config::default(),
                )
                .unwrap_err();
            assert!(matches!(error, StoreError::Invalid(_)), "{bad}: {error}");
        }
    }

    #[test]
    fn updating_an_unknown_agent_fails() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        let error = store
            .update("ghost", AgentChanges::default(), &Config::default())
            .unwrap_err();
        assert!(matches!(error, StoreError::NotFound(_)));
    }

    // ── drift ───────────────────────────────────────────────────────────────

    #[test]
    fn drift_is_reported_after_a_parent_edit_and_cleared_by_a_resync() {
        let parent = parent_workspace();
        let store = ChildAgentStore::new(parent.path());
        store
            .create(request("reviewer"), &Config::default())
            .unwrap();
        assert!(store.list()[0].drifted_files.is_empty());

        // The parent edits USER.md after the snapshot (file times have coarse
        // resolution on some filesystems, so step past the snapshot time).
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(parent.path().join(USER_FILE), "parent learned more").unwrap();
        assert_eq!(store.list()[0].drifted_files, vec![USER_FILE]);

        store
            .update(
                "reviewer",
                AgentChanges {
                    resync_from_parent: vec![USER_FILE.to_string()],
                    ..AgentChanges::default()
                },
                &Config::default(),
            )
            .unwrap();

        assert!(store.list()[0].drifted_files.is_empty());
        assert_eq!(read(&store, "reviewer", USER_FILE), "parent learned more");
    }

    #[test]
    fn override_text_replaces_or_appends() {
        let append_override = FileOverride {
            mode: OverrideMode::Append,
            text: "extra".to_string(),
        };
        assert_eq!(append_override.apply(Some("base\n\n")), "base\n\nextra");
        assert_eq!(append_override.apply(Some("  \n")), "extra");
        assert_eq!(append_override.apply(None), "extra");
        let replace_override = FileOverride {
            mode: OverrideMode::Replace,
            text: "only".to_string(),
        };
        assert_eq!(replace_override.apply(Some("base")), "only");
    }

    #[test]
    fn an_override_without_a_mode_appends() {
        let parsed: FileOverride = serde_json::from_str(r#"{"text": "hello"}"#).unwrap();
        assert_eq!(parsed.mode, OverrideMode::Append);
    }
}
