use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use regex::Regex;
use crate::agent::context::{SOUL_FILE, USER_FILE};
use crate::utils::gitstore::GitStore;
use crate::utils::prompt_templates::render_template;
use crate::utils::helpers::{ensure_dir, estimate_message_tokens, estimate_prompt_tokens_chain, strip_think};


const DEFAULT_MAX_HISTORY: usize = 1000;

const MEMORY_FILE: &'static str = "MEMORY.md";
const HISTORY_FILE: &'static str = "history.jsonl";
const CURSOR_FILE: &'static str = "cursor.json";
const DREAM_CURSOR_FILE: &'static str = "dream_cursor.json";


static LEGACY_ENTRY_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[(\d{4}-\d{2}-\d{2}[^\]]*)\]\s*").unwrap()
});
static LEGACY_TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[(\d{4}-\d{2}-\d{2} \d{2}:\d{2})\]\s*").unwrap()
});
static LEGACY_RAW_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[\d{4}-\d{2}-\d{2}[^\]]*\]\s+[A-Z][A-Z0-9_]*(?:\s+\[tools:\s*[^\]]+\])?:")
        .unwrap()
});

pub struct MemoryStore {
    pub workspace: PathBuf,
    pub max_history_entries: usize,
    pub memory_dir: PathBuf,
    pub memory_file: PathBuf,
    pub history_file: PathBuf,
    pub legacy_history_file: PathBuf,
    pub soul_file: PathBuf,
    pub user_file: PathBuf,
    cursor_file: PathBuf,
    dream_cursor_file: PathBuf,
    git: GitStore
}

impl MemoryStore {
    pub fn new(workspace: PathBuf, max_history_entries: Option<usize>) -> Self {
        let cloned_workspace = workspace.clone();
        let memory_dir = workspace.join("memory");
        ensure_dir(&memory_dir);
        let memory_file = memory_dir.join(MEMORY_FILE);
        let history_file = memory_dir.join(HISTORY_FILE);
        let legacy_history_file = memory_dir.join("HISTORY.md");
        let soul_file = workspace.join(SOUL_FILE);
        let user_file = workspace.join(USER_FILE);
        let cursor_file = memory_dir.join(CURSOR_FILE);
        let dream_cursor_file = memory_dir.join(DREAM_CURSOR_FILE);
        let git = GitStore::new(workspace, vec![]);
        Self {
            workspace: cloned_workspace,
            max_history_entries: max_history_entries.unwrap_or(DEFAULT_MAX_HISTORY),
            memory_dir,
            memory_file,
            history_file,
            legacy_history_file,
            soul_file,
            user_file,
            cursor_file,
            dream_cursor_file,
            git: git,
        }
    }

    /// Return the mtime of `legacy_history_file` formatted as `"YYYY-MM-DD HH:MM"`.
    ///
    /// Falls back to the current local time when the file metadata is
    /// unavailable (mirrors Python's `except OSError` branch).
    fn legacy_fallback_timestamp(&self) -> String {
        let mtime: SystemTime = self.legacy_history_file
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| SystemTime::now());

        DateTime::<Local>::from(mtime)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_store(tmp: &TempDir) -> MemoryStore {
        MemoryStore::new(tmp.path().to_path_buf(), None)
    }

    #[test]
    fn test_legacy_fallback_timestamp_format() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // File doesn't exist → falls back to now(); result must still match the pattern.
        let ts = store.legacy_fallback_timestamp();
        assert_eq!(ts.len(), 16, "expected 'YYYY-MM-DD HH:MM' (16 chars), got: {ts}");
        assert!(ts.chars().nth(4) == Some('-'), "expected dash at position 4");
        assert!(ts.chars().nth(7) == Some('-'), "expected dash at position 7");
        assert!(ts.chars().nth(10) == Some(' '), "expected space at position 10");
        assert!(ts.chars().nth(13) == Some(':'), "expected colon at position 13");
    }

    #[test]
    fn test_legacy_fallback_timestamp_uses_file_mtime() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);

        // Create the legacy file so metadata() succeeds
        fs::create_dir_all(&store.memory_dir).unwrap();
        fs::write(&store.legacy_history_file, b"test").unwrap();

        let ts = store.legacy_fallback_timestamp();
        // The timestamp should still match the format
        assert_eq!(ts.len(), 16);

        // The year should be plausible (>= 2024)
        let year: u32 = ts[..4].parse().unwrap();
        assert!(year >= 2024, "unexpected year: {year}");
    }

    #[test]
    fn test_legacy_fallback_timestamp_missing_file_returns_now() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        // legacy_history_file doesn't exist → falls back to SystemTime::now()
        assert!(!store.legacy_history_file.exists());
        let ts = store.legacy_fallback_timestamp();
        let year: u32 = ts[..4].parse().unwrap();
        assert!(year >= 2024);
    }
}