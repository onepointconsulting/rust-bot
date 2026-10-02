//! Helpers for the processes rust-bot starts.

use std::process::Command;

/// Forcefully terminate a process and its descendants by PID.
///
/// * **Windows:** `taskkill /F /T /PID`, because the real work is often done by
///   a grandchild (`cmd.exe /c ...`, `npx` shims) that killing the direct child
///   would leave running.
/// * **Unix:** the process group first (a process started with its own group,
///   as ACP children are, takes its whole tree with it), then the process
///   itself for a process that is not a group leader.
///
/// Blocking: it waits for the helper command, so call it from `spawn_blocking`
/// in async code unless it is a drop path that cannot await.
pub fn kill_process_tree_sync(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .output();
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// A process that would run for a minute unless it is killed.
    fn long_running_process() -> std::process::Child {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("ping");
            command.args(["-n", "60", "127.0.0.1"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sleep");
            command.arg("60");
            command
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a long-running process")
    }

    #[test]
    fn a_running_process_is_killed_and_reaped() {
        let mut child = long_running_process();
        assert!(child.try_wait().unwrap().is_none(), "still running");

        kill_process_tree_sync(child.id());

        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the process survived the kill"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(!status.success());
    }

    #[test]
    fn killing_a_process_that_is_already_gone_is_harmless() {
        let mut child = long_running_process();
        let pid = child.id();
        kill_process_tree_sync(pid);
        let _ = child.wait();
        kill_process_tree_sync(pid);
    }
}
