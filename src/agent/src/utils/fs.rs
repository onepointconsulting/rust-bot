//! Crash-safe file writing.
//!
//! Plain `File::create` truncates the target first, so a reader (or a crash)
//! between the truncate and the last byte sees a half-written file. Several
//! rust-bot processes can share one config or workspace, so every rewrite of a
//! file that others read goes through [`write_atomic`] instead.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Attempts made to rename the temp file over the target before giving up.
const RENAME_ATTEMPTS: u32 = 6;
/// Pause after the first failed rename; it grows with every further attempt.
const RENAME_RETRY_DELAY: Duration = Duration::from_millis(20);

/// Distinguishes temp files of concurrent writers inside one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Serializes the rename step inside one process. On Windows several renames
/// replacing the same destination at once fail with "access denied" and can keep
/// colliding in lock-step, so writers in this process take turns; the retry in
/// [`write_atomic`] is then left for interference from outside, such as an
/// antivirus scanner or another rust-bot process.
static RENAME_LOCK: Mutex<()> = Mutex::new(());

/// Replace `path` with `contents` so that readers see either the old file or
/// the new one, never a partial write.
///
/// The bytes go to a temp file in the same folder (same volume, so the final
/// rename is atomic), are flushed to disk, and then renamed over the target.
/// Missing parent folders are created. A symlink target is written through, and
/// on Unix the existing file's permissions are kept (a config file holding
/// secrets may be `0600`). On Windows the rename is retried briefly, because an
/// antivirus scanner or indexer can hold the target open for a moment.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    // Write through a symlink instead of replacing the link itself.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let folder = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&folder)?;

    let temp = temp_path_for(&target, &folder);
    let written = write_temp_file(&temp, &target, contents).and_then(|()| {
        let _turn = RENAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        retry_transient(RENAME_ATTEMPTS, RENAME_RETRY_DELAY, || {
            fs::rename(&temp, &target)
        })
    });
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// Name of the temp file used while writing `target`: hidden, `.tmp`-suffixed
/// (so scans for `*.jsonl` or `*.json` ignore it) and unique per writer.
fn temp_path_for(target: &Path, folder: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    folder.join(format!(".{name}.{}.{unique}.tmp", std::process::id()))
}

/// Write and flush the temp file, giving it the target's permissions on Unix.
fn write_temp_file(temp: &Path, target: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = File::create_new(temp)?;
    #[cfg(unix)]
    if let Ok(metadata) = fs::metadata(target) {
        fs::set_permissions(temp, metadata.permissions())?;
    }
    #[cfg(not(unix))]
    let _ = target;
    file.write_all(contents)?;
    file.sync_all()
}

/// Run `operation`, retrying while it fails with `PermissionDenied`.
///
/// That is how Windows reports a file that another process has open for a
/// moment. Any other error, and a failure after the last attempt, is returned.
fn retry_transient<T>(
    attempts: u32,
    delay: Duration,
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut attempt = 1;
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && attempt < attempts => {
                std::thread::sleep(delay * attempt);
                attempt += 1;
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn leftover_temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn creates_a_new_file_and_missing_folders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("config.json");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn replaces_existing_content_completely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "a much longer previous content").unwrap();
        write_atomic(&path, b"short").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"short");
    }

    #[test]
    fn a_reader_that_opened_the_file_earlier_still_sees_the_old_complete_content() {
        // Truncate-and-rewrite would change what an already-open reader sees;
        // replacing the file by rename does not.
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        write_atomic(&path, b"old complete content").unwrap();
        let mut early_reader = File::open(&path).unwrap();

        write_atomic(&path, b"new").unwrap();

        let mut seen = String::new();
        early_reader.read_to_string(&mut seen).unwrap();
        assert_eq!(seen, "old complete content");
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert!(leftover_temp_files(dir.path()).is_empty());
    }

    #[test]
    fn a_failed_write_removes_its_temp_file_and_keeps_the_old_content() {
        let dir = tempfile::tempdir().unwrap();
        // The target is a directory: renaming a file over it fails.
        let target = dir.path().join("taken");
        fs::create_dir(&target).unwrap();
        assert!(write_atomic(&target, b"x").is_err());
        assert!(leftover_temp_files(dir.path()).is_empty());
        assert!(target.is_dir());
    }

    #[test]
    fn readers_never_see_a_partial_file_while_writers_replace_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.txt");
        let first = "A".repeat(200_000);
        let second = "B".repeat(150_000);
        write_atomic(&path, first.as_bytes()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (path, stop) = (path.clone(), Arc::clone(&stop));
            let (first, second) = (first.clone(), second.clone());
            std::thread::spawn(move || {
                let mut use_first = false;
                while !stop.load(Ordering::Relaxed) {
                    let text = if use_first { &first } else { &second };
                    write_atomic(&path, text.as_bytes()).expect("atomic write");
                    use_first = !use_first;
                }
            })
        };

        let mut complete_reads = 0;
        for _ in 0..300 {
            // A transient read error (Windows replace window) is not a partial read.
            if let Ok(content) = fs::read_to_string(&path) {
                assert!(
                    content == first || content == second,
                    "saw a partial or mixed file of {} bytes",
                    content.len()
                );
                complete_reads += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert!(complete_reads > 0);
        assert!(leftover_temp_files(dir.path()).is_empty());
    }

    #[test]
    fn concurrent_writers_do_not_collide_on_temp_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.txt");
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        write_atomic(&path, format!("writer-{index}").as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("writer-"));
        assert!(leftover_temp_files(dir.path()).is_empty());
    }

    #[test]
    fn retry_succeeds_after_transient_permission_denied() {
        let mut calls = 0;
        let result = retry_transient(5, Duration::from_millis(1), || {
            calls += 1;
            if calls < 3 {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                Ok("done")
            }
        });
        assert_eq!(result.unwrap(), "done");
        assert_eq!(calls, 3);
    }

    #[test]
    fn retry_gives_up_after_the_last_attempt() {
        let mut calls = 0;
        let result: io::Result<()> = retry_transient(3, Duration::from_millis(1), || {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert!(result.is_err());
        assert_eq!(calls, 3);
    }

    #[test]
    fn retry_does_not_retry_other_errors() {
        let mut calls = 0;
        let result: io::Result<()> = retry_transient(5, Duration::from_millis(1), || {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::NotFound))
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_permissions_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&path, b"new").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_a_symlink_instead_of_replacing_it() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        let link = dir.path().join("link.json");
        fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&real).unwrap(), b"new");
    }
}
