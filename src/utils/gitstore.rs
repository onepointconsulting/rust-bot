//! Git-backed version control for memory files, using git2 (libgit2).
//!
//! A tracked path is either a file (`memory/MEMORY.md`) or a directory entry
//! ending in `/` (`skills/`), which tracks every file below it, including new
//! and deleted ones.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::TimeZone;
use git2::{DiffFormat, IndexAddOption, Repository, Signature, build::CheckoutBuilder};

/// Workspace paths versioned by the memory store: the files Dream edits and
/// the `skills/` folder Dream and `skill_manage` write to. Entries ending in
/// `/` are directories.
pub const MEMORY_TRACKED_PATHS: &[&str] = &["SOUL.md", "USER.md", "memory/MEMORY.md", "skills/"];

/// [`MEMORY_TRACKED_PATHS`] as owned strings, ready for [`GitStore::new`].
pub fn memory_tracked_paths() -> Vec<String> {
    MEMORY_TRACKED_PATHS
        .iter()
        .map(|path| path.to_string())
        .collect()
}

/// Whether the tracked `path` names a directory (it ends in `/`).
fn is_directory_entry(path: &str) -> bool {
    path.ends_with('/')
}

// ── CommitInfo ────────────────────────────────────────────────────────────────

pub struct CommitInfo {
    pub sha: String, // Short SHA (8 chars)
    pub message: String,
    pub timestamp: String, // Formatted datetime
}

impl CommitInfo {
    /// Format this commit for display, optionally with a diff.
    pub fn format(&self, diff: &str) -> String {
        let first_line = self.message.lines().next().unwrap_or("");
        let header = format!("## {}\n`{}` — {}\n", first_line, self.sha, self.timestamp);
        if !diff.is_empty() {
            format!("{}\n```diff\n{}\n```", header, diff)
        } else {
            format!("{}\n(no file changes)", header)
        }
    }
}

// ── GitStore ──────────────────────────────────────────────────────────────────

pub struct GitStore {
    workspace: PathBuf,
    tracked_files: Vec<String>,
}

impl GitStore {
    pub fn new(workspace: PathBuf, tracked_files: Vec<String>) -> Self {
        Self {
            workspace,
            tracked_files,
        }
    }

    pub fn is_initialized(&self) -> bool {
        self.workspace.join(".git").is_dir()
    }

    // ── init ──────────────────────────────────────────────────────────────────

    /// Initialize a git repo if not already initialized.
    ///
    /// Creates `.gitignore` and makes an initial commit.
    /// Returns `true` if a new repo was created, `false` if already exists.
    pub fn init(&self) -> bool {
        if self.is_initialized() {
            return false;
        }

        let repo = match Repository::init(&self.workspace) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("Git store init failed for {:?}: {}", self.workspace, e);
                return false;
            }
        };

        // Write .gitignore
        let gitignore_path = self.workspace.join(".gitignore");
        if let Err(e) = std::fs::write(&gitignore_path, self.build_gitignore()) {
            log::warn!("Git store: failed to write .gitignore: {}", e);
            return false;
        }

        // Touch tracked files (and create tracked directories) so the initial
        // commit has something to track
        for rel in &self.tracked_files {
            let p = self.workspace.join(rel);
            if is_directory_entry(rel) {
                let _ = std::fs::create_dir_all(&p);
                continue;
            }
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if !p.exists() {
                let _ = std::fs::write(&p, "");
            }
        }

        // Stage .gitignore + tracked files
        let mut index = match repo.index() {
            Ok(i) => i,
            Err(e) => {
                log::warn!("Git store: index error: {}", e);
                return false;
            }
        };

        let _ = index.add_path(Path::new(".gitignore"));
        self.stage_tracked(&mut index);
        let _ = index.write();

        let tree_oid = match index.write_tree() {
            Ok(oid) => oid,
            Err(e) => {
                log::warn!("Git store: write_tree error: {}", e);
                return false;
            }
        };
        let tree = match repo.find_tree(tree_oid) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("Git store: find_tree error: {}", e);
                return false;
            }
        };

        let sig = Self::signature();
        match repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "init: rust-bot memory store",
            &tree,
            &[],
        ) {
            Ok(_) => {
                log::info!("Git store initialized at {:?}", self.workspace);
                true
            }
            Err(e) => {
                log::warn!("Git store: commit error: {}", e);
                false
            }
        }
    }

    // ── daily operations ──────────────────────────────────────────────────────

    /// Stage tracked memory files and commit if there are changes.
    ///
    /// Only tracked paths count: the commit is skipped when the staged tree
    /// equals `HEAD`'s, whatever else changed in the workspace.
    /// Returns the short commit SHA, or `None` if nothing to commit.
    pub fn auto_commit(&self, message: &str) -> Option<String> {
        if !self.is_initialized() {
            return None;
        }

        let repo = Repository::open(&self.workspace)
            .map_err(|e| log::warn!("Git auto-commit: open failed: {}", e))
            .ok()?;

        let mut index = repo
            .index()
            .map_err(|e| log::warn!("Git auto-commit: index error: {}", e))
            .ok()?;

        self.stage_tracked(&mut index);
        index
            .write()
            .map_err(|e| log::warn!("Git auto-commit: index write error: {}", e))
            .ok()?;

        let tree_oid = index
            .write_tree()
            .map_err(|e| log::warn!("Git auto-commit: write_tree error: {}", e))
            .ok()?;
        if Self::head_tree_id(&repo) == Some(tree_oid) {
            return None;
        }
        let tree = repo
            .find_tree(tree_oid)
            .map_err(|e| log::warn!("Git auto-commit: find_tree error: {}", e))
            .ok()?;

        let sig = Self::signature();
        let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent_commit.iter().collect();

        let sha_oid = repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
            .map_err(|e| log::warn!("Git auto-commit: commit error: {}", e))
            .ok()?;

        let sha = sha_oid.to_string()[..8].to_string();
        log::debug!("Git auto-commit: {} ({})", sha, message);
        Some(sha)
    }

    // ── query ─────────────────────────────────────────────────────────────────

    /// Return a simplified commit log (newest first).
    pub fn log(&self, max_entries: usize) -> Vec<CommitInfo> {
        if !self.is_initialized() {
            return vec![];
        }

        let repo = match Repository::open(&self.workspace) {
            Ok(r) => r,
            Err(_) => return vec![],
        };

        let head_commit = match repo.head().and_then(|h| h.peel_to_commit()) {
            Ok(c) => c,
            Err(_) => return vec![],
        };

        let mut revwalk = match repo.revwalk() {
            Ok(r) => r,
            Err(_) => return vec![],
        };
        let _ = revwalk.push(head_commit.id());

        let mut entries = Vec::new();
        for oid in revwalk.take(max_entries) {
            let oid = match oid {
                Ok(o) => o,
                Err(_) => break,
            };
            let commit = match repo.find_commit(oid) {
                Ok(c) => c,
                Err(_) => break,
            };

            let ts = chrono::Local
                .timestamp_opt(commit.time().seconds(), 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();

            let msg = commit.message().unwrap_or("").trim().to_string();
            entries.push(CommitInfo {
                sha: oid.to_string()[..8].to_string(),
                message: msg,
                timestamp: ts,
            });
        }

        entries
    }

    /// Show a unified diff between two commits identified by short SHA.
    pub fn diff_commits(&self, sha1: &str, sha2: &str) -> String {
        if !self.is_initialized() {
            return String::new();
        }

        let repo = match Repository::open(&self.workspace) {
            Ok(r) => r,
            Err(_) => return String::new(),
        };

        let resolve = |short: &str| -> Option<git2::Oid> {
            let mut revwalk = repo.revwalk().ok()?;
            revwalk.push_head().ok()?;
            revwalk
                .filter_map(|r| r.ok())
                .find(|oid| oid.to_string().starts_with(short))
        };

        let oid1 = match resolve(sha1) {
            Some(o) => o,
            None => return String::new(),
        };
        let oid2 = match resolve(sha2) {
            Some(o) => o,
            None => return String::new(),
        };

        let tree1 = match repo.find_commit(oid1).and_then(|c| c.tree()) {
            Ok(t) => t,
            Err(_) => return String::new(),
        };
        let tree2 = match repo.find_commit(oid2).and_then(|c| c.tree()) {
            Ok(t) => t,
            Err(_) => return String::new(),
        };

        let diff = match repo.diff_tree_to_tree(Some(&tree1), Some(&tree2), None) {
            Ok(d) => d,
            Err(_) => return String::new(),
        };

        let mut out = String::new();
        let _ = diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
            match line.origin() {
                '+' | '-' | ' ' => out.push(line.origin()),
                _ => {}
            }
            if let Ok(s) = std::str::from_utf8(line.content()) {
                out.push_str(s);
            }
            true
        });

        out
    }

    /// Find a commit by short SHA prefix.
    pub fn find_commit(&self, short_sha: &str) -> Option<CommitInfo> {
        self.log(20)
            .into_iter()
            .find(|c| c.sha.starts_with(short_sha))
    }

    /// Find a commit and return it paired with its diff vs. the parent.
    pub fn show_commit_diff(&self, short_sha: &str) -> Option<(CommitInfo, String)> {
        let commits = self.log(20);
        for (i, c) in commits.iter().enumerate() {
            if c.sha.starts_with(short_sha) {
                let diff = if i + 1 < commits.len() {
                    self.diff_commits(&commits[i + 1].sha, &c.sha)
                } else {
                    String::new()
                };
                return Some((
                    CommitInfo {
                        sha: c.sha.clone(),
                        message: c.message.clone(),
                        timestamp: c.timestamp.clone(),
                    },
                    diff,
                ));
            }
        }
        None
    }

    // ── restore ───────────────────────────────────────────────────────────────

    /// Revert (undo) the changes introduced by the given commit.
    ///
    /// Restores all tracked memory files (and every file in tracked
    /// directories) to the state at the commit's parent, then creates a new
    /// commit recording the revert. Returns the new commit SHA, or `None` on
    /// failure or when the restore changed nothing.
    pub fn revert(&self, commit: &str) -> Option<String> {
        if !self.is_initialized() {
            return None;
        }

        let repo = Repository::open(&self.workspace)
            .map_err(|e| log::warn!("Git revert: open failed: {}", e))
            .ok()?;

        // Resolve the short SHA
        let mut revwalk = repo
            .revwalk()
            .map_err(|e| log::warn!("Git revert: revwalk error: {}", e))
            .ok()?;
        revwalk
            .push_head()
            .map_err(|e| log::warn!("Git revert: push_head error: {}", e))
            .ok()?;

        let target_oid = revwalk
            .filter_map(|r| r.ok())
            .find(|oid| oid.to_string().starts_with(commit))?;

        let commit_obj = repo
            .find_commit(target_oid)
            .map_err(|e| log::warn!("Git revert: find_commit error: {}", e))
            .ok()?;

        if commit_obj.parent_count() == 0 {
            log::warn!("Git revert: cannot revert root commit {}", commit);
            return None;
        }

        let parent = commit_obj
            .parent(0)
            .map_err(|e| log::warn!("Git revert: parent error: {}", e))
            .ok()?;
        let parent_tree = parent
            .tree()
            .map_err(|e| log::warn!("Git revert: parent tree error: {}", e))
            .ok()?;

        // Make every tracked path match the parent's tree: changed files are
        // restored and tracked files the parent did not have are removed.
        let mut checkout = CheckoutBuilder::new();
        checkout.force();
        for pathspec in self.tracked_pathspecs() {
            checkout.path(pathspec);
        }
        repo.checkout_tree(parent_tree.as_object(), Some(&mut checkout))
            .map_err(|e| log::warn!("Git revert: checkout error: {}", e))
            .ok()?;

        self.auto_commit(&format!("revert: undo {}", commit))
    }

    // ── internal helpers ──────────────────────────────────────────────────────

    fn signature() -> Signature<'static> {
        Signature::now("rust-bot", "rust-bot@dream").expect("valid hardcoded signature")
    }

    /// Add any `.gitignore` line the tracked paths need but the file lacks.
    ///
    /// Workspaces initialised before a path was tracked (e.g. `skills/`) keep
    /// ignoring it otherwise. Existing lines are left untouched, so running
    /// this again is a no-op. Does nothing when the repo is not initialised.
    pub fn ensure_gitignore(&self) {
        if !self.is_initialized() {
            return;
        }
        let gitignore_path = self.workspace.join(".gitignore");
        let existing = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
        let merged = merge_gitignore(&existing, &self.build_gitignore());
        if merged != existing
            && let Err(e) = std::fs::write(&gitignore_path, merged)
        {
            log::warn!("Git store: failed to update .gitignore: {}", e);
        }
    }

    /// Stage every tracked path in `index`: files are added, or removed from
    /// the index once deleted; directories have their new, changed and
    /// deleted files staged.
    fn stage_tracked(&self, index: &mut git2::Index) {
        for rel in &self.tracked_files {
            if is_directory_entry(rel) {
                let pathspec = [rel.trim_end_matches('/')];
                if let Err(e) = index.add_all(pathspec, IndexAddOption::DEFAULT, None) {
                    log::warn!("Git store: staging {} failed: {}", rel, e);
                }
                if let Err(e) = index.update_all(pathspec, None) {
                    log::warn!("Git store: staging deletions in {} failed: {}", rel, e);
                }
            } else if self.workspace.join(rel).is_file() {
                let _ = index.add_path(Path::new(rel));
            } else {
                let _ = index.remove_path(Path::new(rel));
            }
        }
    }

    /// The tracked paths as git pathspecs (directories without the trailing `/`).
    fn tracked_pathspecs(&self) -> Vec<&str> {
        self.tracked_files
            .iter()
            .map(|rel| rel.trim_end_matches('/'))
            .collect()
    }

    /// The tree id of `HEAD`, or `None` before the first commit.
    fn head_tree_id(repo: &Repository) -> Option<git2::Oid> {
        repo.head()
            .ok()?
            .peel_to_commit()
            .ok()
            .map(|commit| commit.tree_id())
    }

    /// The `.gitignore` that ignores the whole workspace except the tracked
    /// paths (and the folders holding tracked files).
    fn build_gitignore(&self) -> String {
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for f in self.tracked_files.iter().filter(|f| !is_directory_entry(f)) {
            let p = Path::new(f);
            if let Some(parent) = p.parent() {
                let s = parent.to_string_lossy();
                if !s.is_empty() && s != "." {
                    dirs.insert(s.into_owned());
                }
            }
        }
        let mut lines = vec!["/*".to_string()];
        for d in &dirs {
            lines.push(format!("!{}/", d));
        }
        for f in &self.tracked_files {
            lines.push(format!("!{}", f));
            if is_directory_entry(f) {
                lines.push(format!("!{}**", f));
            }
        }
        lines.push("!.gitignore".to_string());
        lines.join("\n") + "\n"
    }
}

/// `existing` `.gitignore` text with every line of `wanted` it lacks appended,
/// in `wanted`'s order. Returns `existing` unchanged when nothing is missing.
fn merge_gitignore(existing: &str, wanted: &str) -> String {
    let present: BTreeSet<&str> = existing.lines().map(str::trim).collect();
    let missing: Vec<&str> = wanted
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !present.contains(line))
        .collect();
    if missing.is_empty() {
        return existing.to_string();
    }
    let mut merged = existing.to_string();
    if !merged.is_empty() && !merged.ends_with('\n') {
        merged.push('\n');
    }
    for line in missing {
        merged.push_str(line);
        merged.push('\n');
    }
    merged
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store(dir: &Path) -> GitStore {
        GitStore::new(
            dir.to_path_buf(),
            vec![
                "memory/MEMORY.md".to_string(),
                "memory/HISTORY.md".to_string(),
            ],
        )
    }

    #[test]
    fn test_init_creates_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_store(tmp.path());

        assert!(!store.is_initialized());
        assert!(store.init());
        assert!(store.is_initialized());

        // Second init is a no-op
        assert!(!store.init());
    }

    #[test]
    fn test_auto_commit_no_changes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_store(tmp.path());
        store.init();

        // Nothing changed after init → no commit
        assert!(store.auto_commit("should be nothing").is_none());
    }

    #[test]
    fn test_auto_commit_with_change() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_store(tmp.path());
        store.init();

        std::fs::write(tmp.path().join("memory/MEMORY.md"), "# Hello").unwrap();
        let sha = store.auto_commit("update memory");
        assert!(sha.is_some());
        assert_eq!(sha.unwrap().len(), 8);
    }

    #[test]
    fn test_log_and_find_commit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_store(tmp.path());
        store.init();

        std::fs::write(tmp.path().join("memory/MEMORY.md"), "v1").unwrap();
        store.auto_commit("first update");

        let log = store.log(10);
        assert!(log.len() >= 2); // init + first update

        let sha = &log[0].sha;
        let found = store.find_commit(sha);
        assert!(found.is_some());
        assert_eq!(found.unwrap().sha, *sha);
    }

    #[test]
    fn test_revert() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_store(tmp.path());
        store.init();

        std::fs::write(tmp.path().join("memory/MEMORY.md"), "v1").unwrap();
        store.auto_commit("write v1");

        std::fs::write(tmp.path().join("memory/MEMORY.md"), "v2").unwrap();
        let sha_v2 = store.auto_commit("write v2").unwrap();

        // Revert the v2 commit → file should go back to v1
        let revert_sha = store.revert(&sha_v2);
        assert!(revert_sha.is_some());

        let content = std::fs::read_to_string(tmp.path().join("memory/MEMORY.md")).unwrap();
        assert_eq!(content, "v1");
    }

    // ── directory tracking (skills/) ──────────────────────────────────────────

    fn make_memory_store(dir: &Path) -> GitStore {
        GitStore::new(dir.to_path_buf(), memory_tracked_paths())
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn files_in_diff(diff_text: &str) -> Vec<String> {
        diff_text
            .lines()
            .filter_map(|line| line.strip_prefix("diff --git a/"))
            .map(|rest| rest.split(' ').next().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn memory_tracked_paths_include_the_skills_folder() {
        assert!(MEMORY_TRACKED_PATHS.contains(&"skills/"));
        assert!(MEMORY_TRACKED_PATHS.contains(&"memory/MEMORY.md"));
        assert_eq!(memory_tracked_paths().len(), MEMORY_TRACKED_PATHS.len());
    }

    #[test]
    fn gitignore_unignores_tracked_directories_and_their_contents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let gitignore = make_memory_store(tmp.path()).build_gitignore();
        assert!(gitignore.lines().any(|l| l == "!skills/"));
        assert!(gitignore.lines().any(|l| l == "!skills/**"));
        assert!(gitignore.lines().any(|l| l == "!memory/"));
        assert!(gitignore.starts_with("/*\n"));
    }

    #[test]
    fn a_store_without_tracked_paths_never_commits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = GitStore::new(tmp.path().to_path_buf(), vec![]);
        store.init();
        write(tmp.path(), "memory/MEMORY.md", "# changed");
        assert!(store.auto_commit("nothing tracked").is_none());
    }

    #[test]
    fn untracked_workspace_files_do_not_cause_empty_commits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_memory_store(tmp.path());
        store.init();
        write(tmp.path(), "notes.txt", "scratch");
        write(tmp.path(), "memory/history.jsonl", "{}");
        assert!(store.auto_commit("should be nothing").is_none());
    }

    #[test]
    fn memory_file_changes_are_committed_with_a_diff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_memory_store(tmp.path());
        store.init();
        write(tmp.path(), "memory/MEMORY.md", "- fact\n");
        let sha = store.auto_commit("dream: memory").expect("commit");
        let (_, diff) = store.show_commit_diff(&sha).expect("diff");
        assert_eq!(files_in_diff(&diff), vec!["memory/MEMORY.md".to_string()]);
    }

    #[test]
    fn new_and_deleted_skill_files_are_committed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_memory_store(tmp.path());
        store.init();

        write(tmp.path(), "skills/deploy/SKILL.md", "v1");
        write(tmp.path(), "skills/deploy/references/api.md", "ref");
        let added = store.auto_commit("add skill").expect("commit");
        let (_, diff) = store.show_commit_diff(&added).unwrap();
        let mut files = files_in_diff(&diff);
        files.sort();
        assert_eq!(
            files,
            vec![
                "skills/deploy/SKILL.md".to_string(),
                "skills/deploy/references/api.md".to_string()
            ]
        );

        std::fs::remove_file(tmp.path().join("skills/deploy/references/api.md")).unwrap();
        let removed = store.auto_commit("remove reference").expect("commit");
        let (_, diff) = store.show_commit_diff(&removed).unwrap();
        assert_eq!(
            files_in_diff(&diff),
            vec!["skills/deploy/references/api.md".to_string()]
        );
    }

    #[test]
    fn revert_restores_a_deleted_skill_and_removes_an_added_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = make_memory_store(tmp.path());
        store.init();
        write(tmp.path(), "skills/keep/SKILL.md", "keep me");
        store.auto_commit("add keep").unwrap();

        std::fs::remove_dir_all(tmp.path().join("skills/keep")).unwrap();
        write(tmp.path(), "skills/fresh/SKILL.md", "new skill");
        let change = store.auto_commit("swap skills").unwrap();

        assert!(store.revert(&change).is_some());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("skills/keep/SKILL.md")).unwrap(),
            "keep me"
        );
        assert!(!tmp.path().join("skills/fresh/SKILL.md").exists());
        // The revert is itself committed: nothing is left to commit.
        assert!(store.auto_commit("after revert").is_none());
    }

    #[test]
    fn ensure_gitignore_adds_missing_lines_once_and_keeps_existing_ones() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A workspace initialised before skills/ was tracked.
        let old_store = GitStore::new(
            tmp.path().to_path_buf(),
            vec![
                "SOUL.md".to_string(),
                "USER.md".to_string(),
                "memory/MEMORY.md".to_string(),
            ],
        );
        old_store.init();
        let gitignore_path = tmp.path().join(".gitignore");
        let mut old_text = std::fs::read_to_string(&gitignore_path).unwrap();
        old_text.push_str("# user line\n");
        std::fs::write(&gitignore_path, &old_text).unwrap();

        let store = make_memory_store(tmp.path());
        store.ensure_gitignore();
        let updated = std::fs::read_to_string(&gitignore_path).unwrap();
        assert!(updated.starts_with(&old_text));
        assert!(updated.lines().any(|l| l == "!skills/"));
        assert!(updated.lines().any(|l| l == "!skills/**"));

        store.ensure_gitignore();
        assert_eq!(std::fs::read_to_string(&gitignore_path).unwrap(), updated);

        // The old workspace now versions its skills.
        write(tmp.path(), "skills/demo/SKILL.md", "demo");
        let sha = store.auto_commit("skill").expect("commit");
        let (_, diff) = store.show_commit_diff(&sha).unwrap();
        assert!(files_in_diff(&diff).contains(&"skills/demo/SKILL.md".to_string()));
    }

    #[test]
    fn ensure_gitignore_is_a_no_op_without_a_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        make_memory_store(tmp.path()).ensure_gitignore();
        assert!(!tmp.path().join(".gitignore").exists());
    }

    #[test]
    fn merge_gitignore_appends_only_missing_lines() {
        assert_eq!(merge_gitignore("/*\n!a\n", "/*\n!a\n"), "/*\n!a\n");
        assert_eq!(merge_gitignore("/*\n!a", "/*\n!a\n!b\n"), "/*\n!a\n!b\n");
        assert_eq!(merge_gitignore("", "/*\n"), "/*\n");
    }
}
