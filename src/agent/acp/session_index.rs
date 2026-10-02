//! Which ACP session of a child belongs to which parent session.
//!
//! `<parent workspace>/acp/sessions.json` maps `(agent name, parent session key)`
//! to the child's ACP session id, so a later run can `session/load` the same
//! conversation. Several parent processes (gateway, CLI, a second gateway) may
//! update it, so every change is a read-modify-write under an exclusive lock on a
//! separate `sessions.json.lock` file, and the file itself is replaced atomically.
//! The lock is a different file because on Windows a locked file cannot be read
//! or replaced by anyone else, including the holder's own atomic rename.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::utils::fs::write_atomic;

/// One remembered child session.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    pub agent: String,
    /// The parent's session key; empty for a session shared by all parent sessions.
    pub parent_session_key: String,
    pub acp_session_id: String,
    pub last_used: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct IndexFile {
    #[serde(default)]
    entries: Vec<SessionEntry>,
}

/// The index of one parent workspace.
#[derive(Debug, Clone)]
pub struct ChildSessionIndex {
    file_path: PathBuf,
    lock_path: PathBuf,
}

impl ChildSessionIndex {
    pub fn new(parent_workspace: impl Into<PathBuf>) -> Self {
        let folder = parent_workspace.into().join("acp");
        Self {
            file_path: folder.join("sessions.json"),
            lock_path: folder.join("sessions.json.lock"),
        }
    }

    /// The remembered ACP session id, if any.
    pub fn get(&self, agent: &str, parent_session_key: &str) -> io::Result<Option<String>> {
        let index = self.locked(|index| {
            let found = index
                .entries
                .iter()
                .find(|entry| {
                    entry.agent == agent && entry.parent_session_key == parent_session_key
                })
                .map(|entry| entry.acp_session_id.clone());
            (found, false)
        })?;
        Ok(index)
    }

    /// Remember `acp_session_id` for this pair, replacing an earlier one.
    pub fn set(
        &self,
        agent: &str,
        parent_session_key: &str,
        acp_session_id: &str,
    ) -> io::Result<()> {
        self.locked(|index| {
            let entry = SessionEntry {
                agent: agent.to_string(),
                parent_session_key: parent_session_key.to_string(),
                acp_session_id: acp_session_id.to_string(),
                last_used: Utc::now().to_rfc3339(),
            };
            match index.entries.iter_mut().find(|existing| {
                existing.agent == agent && existing.parent_session_key == parent_session_key
            }) {
                Some(existing) => *existing = entry,
                None => index.entries.push(entry),
            }
            ((), true)
        })
    }

    /// Forget this pair (e.g. the child no longer knows the session).
    pub fn remove(&self, agent: &str, parent_session_key: &str) -> io::Result<()> {
        self.locked(|index| {
            let before = index.entries.len();
            index.entries.retain(|entry| {
                !(entry.agent == agent && entry.parent_session_key == parent_session_key)
            });
            ((), index.entries.len() != before)
        })
    }

    /// Every remembered session, for tests and diagnostics.
    pub fn entries(&self) -> io::Result<Vec<SessionEntry>> {
        self.locked(|index| (index.entries.clone(), false))
    }

    /// Run `action` on the index under the exclusive lock. `action` returns its
    /// result and whether the index changed (and must be saved).
    fn locked<T>(&self, action: impl FnOnce(&mut IndexFile) -> (T, bool)) -> io::Result<T> {
        if let Some(folder) = self.file_path.parent() {
            fs::create_dir_all(folder)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)?;
        lock.lock()?;
        let result = (|| {
            let mut index = self.read()?;
            let (result, changed) = action(&mut index);
            if changed {
                let bytes = serde_json::to_vec_pretty(&index).map_err(io::Error::other)?;
                write_atomic(&self.file_path, &bytes)?;
            }
            Ok(result)
        })();
        let _ = lock.unlock();
        result
    }

    /// The index as stored; a missing file is an empty index. A damaged file is
    /// treated as empty too, so a lost mapping costs one `session/new`, never a
    /// stuck agent.
    fn read(&self) -> io::Result<IndexFile> {
        match fs::read_to_string(&self.file_path) {
            Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_else(|error| {
                log::warn!(
                    "{} is damaged ({error}); starting from an empty index",
                    self.file_path.display()
                );
                IndexFile::default()
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(IndexFile::default()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn nothing_is_remembered_at_first() {
        let dir = tempfile::tempdir().unwrap();
        let index = ChildSessionIndex::new(dir.path());
        assert_eq!(index.get("code-review", "cli:direct").unwrap(), None);
    }

    #[test]
    fn a_session_is_remembered_per_agent_and_parent_session() {
        let dir = tempfile::tempdir().unwrap();
        let index = ChildSessionIndex::new(dir.path());
        index.set("code-review", "ws:alice", "sess-1").unwrap();
        index.set("code-review", "ws:bob", "sess-2").unwrap();
        index.set("docs", "ws:alice", "sess-3").unwrap();

        assert_eq!(
            index.get("code-review", "ws:alice").unwrap().as_deref(),
            Some("sess-1")
        );
        assert_eq!(
            index.get("code-review", "ws:bob").unwrap().as_deref(),
            Some("sess-2")
        );
        assert_eq!(
            index.get("docs", "ws:alice").unwrap().as_deref(),
            Some("sess-3")
        );
        assert_eq!(index.get("docs", "ws:bob").unwrap(), None);
    }

    #[test]
    fn setting_again_replaces_instead_of_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let index = ChildSessionIndex::new(dir.path());
        index.set("a", "k", "old").unwrap();
        index.set("a", "k", "new").unwrap();
        assert_eq!(index.get("a", "k").unwrap().as_deref(), Some("new"));
        assert_eq!(index.entries().unwrap().len(), 1);
    }

    #[test]
    fn a_removed_mapping_is_gone_and_others_stay() {
        let dir = tempfile::tempdir().unwrap();
        let index = ChildSessionIndex::new(dir.path());
        index.set("a", "k1", "s1").unwrap();
        index.set("a", "k2", "s2").unwrap();
        index.remove("a", "k1").unwrap();
        assert_eq!(index.get("a", "k1").unwrap(), None);
        assert_eq!(index.get("a", "k2").unwrap().as_deref(), Some("s2"));
        // Removing something that is not there is fine.
        index.remove("a", "k1").unwrap();
    }

    #[test]
    fn the_mapping_survives_a_new_instance() {
        let dir = tempfile::tempdir().unwrap();
        ChildSessionIndex::new(dir.path())
            .set("a", "k", "s")
            .unwrap();
        assert_eq!(
            ChildSessionIndex::new(dir.path())
                .get("a", "k")
                .unwrap()
                .as_deref(),
            Some("s")
        );
    }

    #[test]
    fn a_damaged_file_reads_as_empty_and_can_be_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let index = ChildSessionIndex::new(dir.path());
        index.set("a", "k", "s").unwrap();
        fs::write(dir.path().join("acp").join("sessions.json"), "{broken").unwrap();

        assert_eq!(index.get("a", "k").unwrap(), None);
        index.set("a", "k", "again").unwrap();
        assert_eq!(index.get("a", "k").unwrap().as_deref(), Some("again"));
    }

    #[test]
    fn concurrent_writers_never_lose_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let index = Arc::new(ChildSessionIndex::new(dir.path()));
        let threads: Vec<_> = (0..8)
            .map(|worker| {
                // Each thread has its own instance: they only share the files,
                // like separate processes do.
                let index = ChildSessionIndex::new(dir.path());
                std::thread::spawn(move || {
                    for round in 0..10 {
                        index
                            .set(&format!("agent-{worker}"), &format!("key-{round}"), "s")
                            .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(index.entries().unwrap().len(), 80);
    }
}
