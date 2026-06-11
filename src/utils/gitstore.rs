/// Git-backed version control for memory files, using git2 (libgit2).
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::TimeZone;
use git2::{DiffFormat, Repository, Signature};

// ── CommitInfo ────────────────────────────────────────────────────────────────

pub struct CommitInfo {
    pub sha: String,       // Short SHA (8 chars)
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
        Self { workspace, tracked_files }
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

        // Touch tracked files so the initial commit has something to track
        for rel in &self.tracked_files {
            let p = self.workspace.join(rel);
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
        for rel in &self.tracked_files {
            let _ = index.add_path(Path::new(rel));
        }
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
        match repo.commit(Some("HEAD"), &sig, &sig, "init: rust-bot memory store", &tree, &[]) {
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
    /// Returns the short commit SHA, or `None` if nothing to commit.
    pub fn auto_commit(&self, message: &str) -> Option<String> {
        if !self.is_initialized() {
            return None;
        }

        let repo = Repository::open(&self.workspace)
            .map_err(|e| log::warn!("Git auto-commit: open failed: {}", e))
            .ok()?;

        // Check for any changes among tracked files
        let statuses = repo.statuses(None)
            .map_err(|e| log::warn!("Git auto-commit: status failed: {}", e))
            .ok()?;

        let has_changes = statuses.iter().any(|s| s.status() != git2::Status::CURRENT);
        if !has_changes {
            return None;
        }

        let mut index = repo.index()
            .map_err(|e| log::warn!("Git auto-commit: index error: {}", e))
            .ok()?;

        for rel in &self.tracked_files {
            let _ = index.add_path(Path::new(rel));
        }
        index.write()
            .map_err(|e| log::warn!("Git auto-commit: index write error: {}", e))
            .ok()?;

        let tree_oid = index.write_tree()
            .map_err(|e| log::warn!("Git auto-commit: write_tree error: {}", e))
            .ok()?;
        let tree = repo.find_tree(tree_oid)
            .map_err(|e| log::warn!("Git auto-commit: find_tree error: {}", e))
            .ok()?;

        let sig = Self::signature();
        let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent_commit.iter().collect();

        let sha_oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
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
            revwalk.filter_map(|r| r.ok()).find(|oid| oid.to_string().starts_with(short))
        };

        let oid1 = match resolve(sha1) { Some(o) => o, None => return String::new() };
        let oid2 = match resolve(sha2) { Some(o) => o, None => return String::new() };

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
        self.log(20).into_iter().find(|c| c.sha.starts_with(short_sha))
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
    /// Restores all tracked memory files to the state at the commit's parent,
    /// then creates a new commit recording the revert.
    /// Returns the new commit SHA, or `None` on failure.
    pub fn revert(&self, commit: &str) -> Option<String> {
        if !self.is_initialized() {
            return None;
        }

        let repo = Repository::open(&self.workspace)
            .map_err(|e| log::warn!("Git revert: open failed: {}", e))
            .ok()?;

        // Resolve the short SHA
        let mut revwalk = repo.revwalk()
            .map_err(|e| log::warn!("Git revert: revwalk error: {}", e))
            .ok()?;
        revwalk.push_head()
            .map_err(|e| log::warn!("Git revert: push_head error: {}", e))
            .ok()?;

        let target_oid = revwalk
            .filter_map(|r| r.ok())
            .find(|oid| oid.to_string().starts_with(commit))?;

        let commit_obj = repo.find_commit(target_oid)
            .map_err(|e| log::warn!("Git revert: find_commit error: {}", e))
            .ok()?;

        if commit_obj.parent_count() == 0 {
            log::warn!("Git revert: cannot revert root commit {}", commit);
            return None;
        }

        let parent = commit_obj.parent(0)
            .map_err(|e| log::warn!("Git revert: parent error: {}", e))
            .ok()?;
        let parent_tree = parent.tree()
            .map_err(|e| log::warn!("Git revert: parent tree error: {}", e))
            .ok()?;

        // Restore tracked files from the parent's tree
        let mut restored = false;
        for rel in &self.tracked_files {
            let entry = match parent_tree.get_path(Path::new(rel)) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let blob = match repo.find_blob(entry.id()) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let content = String::from_utf8_lossy(blob.content()).into_owned();
            let dest = self.workspace.join(rel);
            if std::fs::write(&dest, content).is_ok() {
                restored = true;
            }
        }

        if !restored {
            return None;
        }

        self.auto_commit(&format!("revert: undo {}", commit))
    }

    // ── internal helpers ──────────────────────────────────────────────────────

    fn signature() -> Signature<'static> {
        Signature::now("rust-bot", "rust-bot@dream")
            .expect("valid hardcoded signature")
    }

    fn build_gitignore(&self) -> String {
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for f in &self.tracked_files {
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
        }
        lines.push("!.gitignore".to_string());
        lines.join("\n") + "\n"
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store(dir: &Path) -> GitStore {
        GitStore::new(
            dir.to_path_buf(),
            vec!["memory/MEMORY.md".to_string(), "memory/HISTORY.md".to_string()],
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
}
