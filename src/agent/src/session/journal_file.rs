//! File-backed [`JournalStore`]: one `<safe key>.journal` file per session.
//!
//! The extension is deliberately **not** `jsonl`:
//! [`crate::session::manager::SessionStore::list_sessions`] scans for that
//! extension and must not report a journal as a session. The content is still
//! JSON lines, one record per line.
//!
//! An append is one `write_all` of a single line, so a process kill never
//! interleaves records and at worst leaves one torn trailing line, which
//! [`FileJournalStore::scan_path`] skips. A torn tail is terminated with a
//! newline before the next append so it stays one bad line.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::session::journal::{
    Durability, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalRecord, JournalScan, JournalStore,
};
use crate::utils::helpers::safe_filename;

/// File extension of a journal. Not `jsonl`, see the module docs.
pub const JOURNAL_EXTENSION: &str = "journal";

/// Next sequence number of one session; `None` until first needed (then read from disk).
type SeqCounter = Arc<Mutex<Option<u64>>>;

/// Journals stored as files in one directory.
pub struct FileJournalStore {
    dir: PathBuf,
    /// Per-session lock and sequence counter. Holding a session's lock
    /// serializes its appends, scans, rewrites and deletes.
    sessions: Mutex<HashMap<String, SeqCounter>>,
}

/// Process-wide registry, so every user of one directory shares one set of
/// sequence counters (two stores on one directory would hand out duplicates).
fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<FileJournalStore>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<FileJournalStore>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

impl FileJournalStore {
    /// The shared store for journals in `dir`. Nothing is created on disk until the first append.
    pub fn shared(dir: PathBuf) -> Arc<Self> {
        let mut known = registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = known.get(&dir).and_then(Weak::upgrade) {
            return existing;
        }
        known.retain(|_, weak| weak.strong_count() > 0);
        let store = Arc::new(Self {
            dir: dir.clone(),
            sessions: Mutex::new(HashMap::new()),
        });
        known.insert(dir, Arc::downgrade(&store));
        store
    }

    /// Directory the journals live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// File holding the journal of `session_key`.
    pub fn path_for(&self, session_key: &str) -> PathBuf {
        self.dir.join(format!(
            "{}.{JOURNAL_EXTENSION}",
            safe_filename(session_key)
        ))
    }

    fn counter(&self, session_key: &str) -> SeqCounter {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(sessions.entry(session_key.to_string()).or_default())
    }

    /// Read the journal file at `path`; a missing file is an empty journal.
    pub fn scan_path(path: &Path) -> JournalScan {
        let Ok(bytes) = fs::read(path) else {
            return JournalScan::default();
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut entries = Vec::new();
        let mut skipped_lines = 0;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            match Self::parse_line(line) {
                Some(entry) => entries.push(entry),
                None => skipped_lines += 1,
            }
        }
        if skipped_lines > 0 {
            log::warn!(
                "journal {}: skipped {skipped_lines} unreadable line(s)",
                path.display()
            );
        }
        JournalScan::from_entries(entries, skipped_lines)
    }

    /// Parse one line; `None` for torn/corrupt lines, unknown kinds and future versions.
    fn parse_line(line: &str) -> Option<JournalEntry> {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let version = value.get("v").and_then(serde_json::Value::as_u64)?;
        if version > u64::from(JOURNAL_SCHEMA_VERSION) {
            return None;
        }
        serde_json::from_value(value).ok()
    }

    /// Whether the file is empty or ends with a newline.
    fn ends_cleanly(path: &Path) -> bool {
        match fs::read(path) {
            Ok(bytes) => bytes.last().is_none_or(|last| *last == b'\n'),
            Err(_) => true,
        }
    }
}

impl JournalStore for FileJournalStore {
    fn append(
        &self,
        session_key: &str,
        record: JournalRecord,
        durability: Durability,
    ) -> io::Result<u64> {
        let path = self.path_for(session_key);
        let counter = self.counter(session_key);
        let mut next_seq = counter.lock().unwrap_or_else(|e| e.into_inner());
        let seq = match *next_seq {
            Some(seq) => seq,
            None => Self::scan_path(&path)
                .entries
                .last()
                .map_or(1, |entry| entry.seq + 1),
        };
        let mut line =
            serde_json::to_string(&JournalEntry::new(seq, record)).map_err(io::Error::other)?;
        line.push('\n');

        fs::create_dir_all(&self.dir)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        if !Self::ends_cleanly(&path) {
            // A torn tail from an earlier crash: terminate it so it stays one bad line.
            file.write_all(b"\n")?;
        }
        file.write_all(line.as_bytes())?;
        file.flush()?;
        if durability == Durability::Synced {
            file.sync_data()?;
        }
        *next_seq = Some(seq + 1);
        Ok(seq)
    }

    fn scan(&self, session_key: &str) -> JournalScan {
        let counter = self.counter(session_key);
        // Hold the lock so a scan never sees a half-written rewrite.
        let _guard = counter.lock().unwrap_or_else(|e| e.into_inner());
        Self::scan_path(&self.path_for(session_key))
    }

    fn scan_all(&self) -> Vec<JournalScan> {
        let Ok(listing) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = listing
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some(JOURNAL_EXTENSION))
            .collect();
        paths.sort();
        paths
            .iter()
            .map(|path| Self::scan_path(path))
            .filter(|scan| !scan.entries.is_empty() || scan.skipped_lines > 0)
            .collect()
    }

    fn rewrite(
        &self,
        session_key: &str,
        transform: &mut dyn FnMut(Vec<JournalEntry>) -> Vec<JournalEntry>,
    ) -> io::Result<()> {
        let path = self.path_for(session_key);
        let counter = self.counter(session_key);
        let mut next_seq = counter.lock().unwrap_or_else(|e| e.into_inner());
        let scan = Self::scan_path(&path);
        let highest = scan.entries.last().map_or(0, |entry| entry.seq);
        // Sequence numbers are never reused, even if the rewrite drops the newest records.
        *next_seq = Some(next_seq.unwrap_or(1).max(highest + 1));

        let before = scan.entries.clone();
        let after = transform(scan.entries);
        if after == before && scan.skipped_lines == 0 {
            return Ok(());
        }
        let mut contents = String::new();
        for entry in &after {
            contents.push_str(&serde_json::to_string(entry).map_err(io::Error::other)?);
            contents.push('\n');
        }
        crate::utils::fs::write_atomic(&path, contents.as_bytes())
    }

    fn delete(&self, session_key: &str) -> io::Result<()> {
        let counter = self.counter(session_key);
        let _guard = counter.lock().unwrap_or_else(|e| e.into_inner());
        match fs::remove_file(self.path_for(session_key)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::journal::{RunJournal, conformance};
    use serde_json::json;

    fn start(turn_id: &str) -> JournalRecord {
        JournalRecord::TurnStart {
            turn_id: turn_id.to_string(),
            session_key: "k".to_string(),
            request_id: None,
            user_message: Some(json!({"role": "user", "content": "hi"})),
            attempt: 0,
            channel: None,
            chat_id: None,
        }
    }

    /// A store over a fresh temp directory; the guard keeps the directory alive.
    fn store_in_tempdir() -> (Arc<FileJournalStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (FileJournalStore::shared(dir.path().to_path_buf()), dir)
    }

    #[test]
    fn the_file_store_conforms() {
        // Each store gets its own directory; keep them alive for the whole run.
        let dirs = Mutex::new(Vec::new());
        conformance::run(&|| {
            let (store, dir) = store_in_tempdir();
            dirs.lock().unwrap().push(dir);
            store
        });
    }

    #[test]
    fn journals_are_not_named_jsonl() {
        let (store, _dir) = store_in_tempdir();
        let path = store.path_for("cli:direct");
        assert_eq!(path.extension().unwrap(), JOURNAL_EXTENSION);
        assert_eq!(path.file_name().unwrap(), "cli_direct.journal");
    }

    #[test]
    fn nothing_is_created_before_the_first_append() {
        let (store, dir) = store_in_tempdir();
        assert!(store.scan("k").entries.is_empty());
        assert!(!store.path_for("k").exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_new_store_continues_the_sequence_found_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let first = FileJournalStore::shared(dir.path().to_path_buf());
        first.append("k", start("a"), Durability::Flushed).unwrap();
        drop(first);
        let second = FileJournalStore::shared(dir.path().to_path_buf());
        assert_eq!(
            second.append("k", start("b"), Durability::Flushed).unwrap(),
            2
        );
    }

    #[test]
    fn stores_for_one_directory_are_shared() {
        let dir = tempfile::tempdir().unwrap();
        let first = FileJournalStore::shared(dir.path().to_path_buf());
        let second = FileJournalStore::shared(dir.path().to_path_buf());
        assert_eq!(
            first.append("k", start("a"), Durability::Flushed).unwrap(),
            1
        );
        assert_eq!(
            second.append("k", start("b"), Durability::Flushed).unwrap(),
            2
        );
    }

    #[test]
    fn torn_trailing_line_is_skipped_and_the_next_append_stays_intact() {
        let (store, _dir) = store_in_tempdir();
        store.append("k", start("a"), Durability::Flushed).unwrap();
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.path_for("k"))
            .unwrap();
        file.write_all(br#"{"v":1,"seq":2,"ts":"x","kind":"tool_in"#)
            .unwrap();
        drop(file);

        let scan = store.scan("k");
        assert_eq!((scan.entries.len(), scan.skipped_lines), (1, 1));

        store.append("k", start("b"), Durability::Flushed).unwrap();
        let scan = store.scan("k");
        assert_eq!(
            scan.entries.len(),
            2,
            "append after a torn tail must be readable"
        );
        assert_eq!(scan.skipped_lines, 1);
    }

    #[test]
    fn garbage_unknown_kinds_and_future_versions_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let good = serde_json::to_string(&JournalEntry::new(1, start("t1"))).unwrap();
        let text = format!(
            "{good}\nnot json at all\n{{\"v\":1,\"seq\":2,\"ts\":\"t\",\"kind\":\"from_the_future\"}}\n{{\"v\":99,\"seq\":3,\"ts\":\"t\",\"kind\":\"turn_end\",\"turn_id\":\"t1\",\"outcome\":\"completed\"}}\n"
        );
        let path = dir.path().join("k.journal");
        fs::write(&path, text).unwrap();

        let scan = FileJournalStore::scan_path(&path);
        assert_eq!((scan.entries.len(), scan.skipped_lines), (1, 3));
    }

    #[test]
    fn an_unwritable_location_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the journal directory should be.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "x").unwrap();
        let store = FileJournalStore::shared(blocker.join("sessions"));
        assert!(store.append("k", start("a"), Durability::Flushed).is_err());
        // The handle used by the rest of the code reports it the same way.
        let journal = RunJournal::new(store, "k");
        assert!(journal.append(start("a")).is_err());
    }

    #[test]
    fn rewrite_is_atomic_on_disk_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileJournalStore::shared(dir.path().to_path_buf());
        for turn in ["a", "b"] {
            store.append("k", start(turn), Durability::Flushed).unwrap();
        }
        store
            .rewrite("k", &mut |entries| entries.into_iter().skip(1).collect())
            .unwrap();
        let on_disk = FileJournalStore::scan_path(&store.path_for("k"));
        assert_eq!(on_disk.entries.len(), 1);
        assert_eq!(on_disk.entries[0].seq, 2);
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");
    }
}
