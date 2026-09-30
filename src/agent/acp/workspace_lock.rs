//! One `rust-bot acp` process per workspace.
//!
//! The workspace's sessions, memory files and cursors are rewritten in place,
//! so two processes working on the same workspace would overwrite each other.
//! The process that writes the files holds an exclusive OS lock on
//! `<workspace>/.acp.lock` for its whole life. The OS releases it when the
//! process exits, however it exits (crash and kill included), so a lock can
//! never be left stale.
//!
//! On Windows a held lock is mandatory: other processes cannot even *read* the
//! locked file. Who holds it is therefore recorded in a separate, unlocked
//! sidecar file, `.acp.lock.owner`, purely for diagnostics; ownership is decided
//! by the lock, never by that file.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::utils::fs::write_atomic;

/// Name of the lock file inside the workspace.
pub const LOCK_FILE_NAME: &str = ".acp.lock";
/// Name of the sidecar file that records the holder.
pub const OWNER_FILE_NAME: &str = ".acp.lock.owner";
/// How often a waiting process retries the lock.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Who holds a workspace, as recorded by the holder itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockOwner {
    pub pid: u32,
    /// RFC 3339 time the holder took the lock.
    pub since: String,
}

/// Why the lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds it and did not let go within the wait.
    HeldBy {
        /// What the holder recorded; `None` when the record is missing or unreadable.
        owner: Option<LockOwner>,
        workspace: PathBuf,
        waited: Duration,
    },
    /// The lock file could not be opened or locked at all.
    Io(io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::HeldBy {
                owner,
                workspace,
                waited,
            } => {
                let who = match owner {
                    Some(owner) => {
                        format!("rust-bot acp process {} (since {})", owner.pid, owner.since)
                    }
                    None => "another rust-bot acp process".to_string(),
                };
                write!(
                    f,
                    "workspace {} is in use by {who}; waited {}s for it to finish. \
                     Close that thread or process, or use a different workspace.",
                    workspace.display(),
                    waited.as_secs()
                )
            }
            LockError::Io(error) => write!(f, "cannot lock the workspace: {error}"),
        }
    }
}

impl std::error::Error for LockError {}

/// An exclusive hold on a workspace. Dropping it (or exiting the process) releases it.
#[derive(Debug)]
pub struct WorkspaceLock {
    file: File,
    owner_path: PathBuf,
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.owner_path);
        let _ = self.file.unlock();
    }
}

/// Path of the lock file for `workspace`.
pub fn lock_path(workspace: &Path) -> PathBuf {
    workspace.join(LOCK_FILE_NAME)
}

/// Path of the owner record for `workspace`.
pub fn owner_path(workspace: &Path) -> PathBuf {
    workspace.join(OWNER_FILE_NAME)
}

/// The recorded holder of `workspace`, if a readable record exists.
pub fn read_owner(workspace: &Path) -> Option<LockOwner> {
    let text = fs::read_to_string(owner_path(workspace)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Take the exclusive lock on `workspace`, waiting up to `wait`.
///
/// Creates the workspace folder and the lock file when missing. Retries every
/// `poll` without blocking the async runtime. A `wait` of zero tries exactly once.
pub async fn acquire(
    workspace: &Path,
    wait: Duration,
    poll: Duration,
) -> Result<WorkspaceLock, LockError> {
    fs::create_dir_all(workspace).map_err(LockError::Io)?;
    // A relative workspace (e.g. `.\.rust-bot\workspace` in a config) resolves
    // against the client's working directory; name the real folder in messages.
    let shown = std::path::absolute(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path(workspace))
        .map_err(LockError::Io)?;

    let started = tokio::time::Instant::now();
    let mut announced = false;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::Error(error)) => return Err(LockError::Io(error)),
            Err(TryLockError::WouldBlock) => {
                if !announced && !wait.is_zero() {
                    announced = true;
                    // stderr is safe in protocol mode; a client that shows agent
                    // logs will explain the pause.
                    eprintln!(
                        "rust-bot acp: workspace {} is in use by {}; waiting up to {}s for it",
                        shown.display(),
                        read_owner(workspace)
                            .map(|owner| format!("process {}", owner.pid))
                            .unwrap_or_else(|| "another process".to_string()),
                        wait.as_secs()
                    );
                }
                let waited = started.elapsed();
                if waited >= wait {
                    return Err(LockError::HeldBy {
                        owner: read_owner(workspace),
                        workspace: shown,
                        waited,
                    });
                }
                tokio::time::sleep(poll.min(wait - waited)).await;
            }
        }
    }

    let owner = LockOwner {
        pid: std::process::id(),
        since: chrono::Utc::now().to_rfc3339(),
    };
    // Diagnostics only: failing to record the owner must not fail the lock.
    let _ = serde_json::to_vec(&owner)
        .map_err(io::Error::other)
        .and_then(|bytes| write_atomic(&owner_path(workspace), &bytes));

    Ok(WorkspaceLock {
        file,
        owner_path: owner_path(workspace),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(20);

    #[tokio::test]
    async fn a_free_workspace_is_locked_and_the_owner_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let lock = acquire(dir.path(), Duration::ZERO, SHORT).await.unwrap();
        assert!(lock_path(dir.path()).is_file());
        let owner = read_owner(dir.path()).expect("owner recorded");
        assert_eq!(owner.pid, std::process::id());
        drop(lock);
    }

    #[tokio::test]
    async fn a_held_workspace_is_refused_and_names_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let _held = acquire(dir.path(), Duration::ZERO, SHORT).await.unwrap();

        let error = acquire(dir.path(), Duration::ZERO, SHORT)
            .await
            .unwrap_err();

        match &error {
            LockError::HeldBy { owner, .. } => {
                assert_eq!(owner.as_ref().map(|o| o.pid), Some(std::process::id()));
            }
            other => panic!("expected HeldBy, got {other:?}"),
        }
        let message = error.to_string();
        assert!(
            message.contains(&std::process::id().to_string()),
            "{message}"
        );
        assert!(message.contains("in use"), "{message}");
    }

    #[tokio::test]
    async fn a_waiter_gets_the_lock_once_the_holder_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let held = acquire(dir.path(), Duration::ZERO, SHORT).await.unwrap();
        let releaser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(held);
        });

        let started = std::time::Instant::now();
        let second = acquire(dir.path(), Duration::from_secs(10), SHORT).await;

        assert!(second.is_ok(), "the waiter must get the lock");
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "it must actually have waited"
        );
        releaser.await.unwrap();
    }

    #[tokio::test]
    async fn the_error_names_the_absolute_workspace_even_for_a_relative_path() {
        // Relative on purpose: it resolves against the working directory, which
        // is exactly what made the original message ambiguous.
        let relative =
            PathBuf::from("target").join(format!("acp-lock-relative-{}", std::process::id()));
        let _held = acquire(&relative, Duration::ZERO, SHORT).await.unwrap();

        let error = acquire(&relative, Duration::ZERO, SHORT).await.unwrap_err();

        let LockError::HeldBy { workspace, .. } = &error else {
            panic!("expected HeldBy, got {error:?}");
        };
        assert!(workspace.is_absolute(), "{workspace:?}");
        let message = error.to_string();
        assert!(
            message.contains(&workspace.display().to_string()),
            "{message}"
        );
        drop(_held);
        let _ = fs::remove_dir_all(&relative);
    }

    #[tokio::test]
    async fn a_failed_wait_reports_how_long_it_waited() {
        let dir = tempfile::tempdir().unwrap();
        let _held = acquire(dir.path(), Duration::ZERO, SHORT).await.unwrap();
        let error = acquire(dir.path(), Duration::from_millis(150), SHORT)
            .await
            .unwrap_err();
        match error {
            LockError::HeldBy { waited, .. } => assert!(waited >= Duration::from_millis(150)),
            other => panic!("expected HeldBy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn different_workspaces_do_not_block_each_other() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let _a = acquire(first.path(), Duration::ZERO, SHORT).await.unwrap();
        assert!(acquire(second.path(), Duration::ZERO, SHORT).await.is_ok());
    }

    #[tokio::test]
    async fn dropping_the_lock_removes_the_owner_record_and_frees_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        drop(acquire(dir.path(), Duration::ZERO, SHORT).await.unwrap());
        assert!(read_owner(dir.path()).is_none());
        assert!(acquire(dir.path(), Duration::ZERO, SHORT).await.is_ok());
    }

    #[tokio::test]
    async fn the_workspace_folder_is_created_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("not").join("yet");
        assert!(acquire(&workspace, Duration::ZERO, SHORT).await.is_ok());
        assert!(workspace.is_dir());
    }

    #[test]
    fn a_missing_or_garbled_owner_record_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_owner(dir.path()).is_none());
        fs::write(owner_path(dir.path()), "not json at all").unwrap();
        assert!(read_owner(dir.path()).is_none());
    }

    #[test]
    fn the_message_copes_with_an_unknown_holder() {
        let error = LockError::HeldBy {
            owner: None,
            workspace: PathBuf::from("/w"),
            waited: Duration::from_secs(30),
        };
        let message = error.to_string();
        assert!(
            message.contains("another rust-bot acp process"),
            "{message}"
        );
        assert!(message.contains("30s"), "{message}");
    }
}
