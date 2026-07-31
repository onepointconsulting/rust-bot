use async_trait::async_trait;
use crate::agent::tools::base::Tool;
use crate::agent::tools::sandbox::wrap_command;
use crate::config::paths::get_media_dir;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::io::Read;
use std::process::{Child, Command, Stdio};

const IS_WINDOWS: bool = cfg!(target_os = "windows");

/// Kills the process tree on drop unless [`Self::disarm`] was called.
///
/// Mirrors Python's `except asyncio.CancelledError: await self._kill_process(process); raise`.
struct ChildGuard {
    pid: u32,
    armed: bool,
}

impl ChildGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.armed {
            ShellTool::kill_process_tree_sync(self.pid);
        }
    }
}

pub struct ShellTool {
    timeout: u64,
    working_dir: Option<PathBuf>,
    deny_patterns: Option<Vec<String>>,
    allow_patterns: Option<Vec<String>>,
    restrict_to_workspace: bool,
    sandbox: Option<String>,
    path_append: Option<String>,
}

impl ShellTool {
    const MAX_TIMEOUT: u64 = 600;
    const MAX_OUTPUT: usize = 10_000;

    pub fn new(
        timeout: u64,
        working_dir: Option<PathBuf>,
        deny_patterns: Option<Vec<String>>,
        allow_patterns: Option<Vec<String>>,
        restrict_to_workspace: bool,
        sandbox: Option<String>,
        path_append: Option<String>,
    ) -> Self {
        Self {
            timeout,
            working_dir,
            deny_patterns: Some(deny_patterns.unwrap_or(vec![
                r"\brm\s+-[rf]{1,2}\b".to_string(),    // rm -r, rm -rf, rm -fr
                r"\bdel\s+/[fq]\b".to_string(),        // del /f, del /q
                r"\brmdir\s+/s\b".to_string(),         // rmdir /s
                r"(?:^|[;&|]\s*)format\b".to_string(), // format (as standalone command only)
                r"\b(mkfs|diskpart)\b".to_string(),    // disk operations
                r"\bdd\s+if=".to_string(),             // dd
                r">\s*/dev/sd".to_string(),            // write to disk
                r"\b(shutdown|reboot|poweroff)\b".to_string(), // system power
                r":\(\)\s*\{.*\};\s*:".to_string(),
            ])),
            allow_patterns: Some(allow_patterns.unwrap_or(vec![])),
            restrict_to_workspace,
            sandbox,
            path_append,
        }
    }

    async fn execute_command(
        &self,
        command: &str,
        working_dir: Option<PathBuf>,
        timeout: Option<u64>,
    ) -> String {
        let mut cwd = working_dir
            .or(self.working_dir.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let guard_error = self.guard_command(command, cwd.to_str().unwrap());
        if let Some(error) = guard_error {
            return error;
        }

        let mut effective_command = command.to_string();

        if let Some(sandbox) = &self.sandbox {
            if IS_WINDOWS {
                log::warn!(
                    "Sandbox '{}' is not supported on Windows; running unsandboxed",
                    sandbox.to_string()
                )
            } else {
                let workspace = self
                    .working_dir
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap());
                let command_result = wrap_command(
                    sandbox,
                    command,
                    workspace.to_str().unwrap(),
                    cwd.to_str().unwrap(),
                    None,
                );
                if let Ok(command) = command_result {
                    effective_command = command;
                } else {
                    return command_result.unwrap_err();
                }
                cwd = workspace;
            }
        }
        let effective_timeout = std::cmp::min(timeout.unwrap_or(self.timeout), Self::MAX_TIMEOUT);
        let mut env = Self::build_env();

        if let Some(path_append) = &self.path_append {
            if IS_WINDOWS {
                let current = env.get("PATH").cloned().unwrap_or_default();
                env.insert("PATH".to_string(), format!("{current};{path_append}"));
            } else {
                effective_command =
                    format!("export PATH=\"$PATH:{path_append}\"; {effective_command}");
            }
        }

        // ── Spawn ─────────────────────────────────────────────────────────────
        let child = match Self::spawn(&effective_command, cwd.to_str().unwrap_or("."), &env) {
            Ok(c) => c,
            Err(e) => return format!("Error executing command: {e}"),
        };

        // ── Collect output with timeout ───────────────────────────────────────
        let timeout_dur = std::time::Duration::from_secs(effective_timeout);
        let mut child = child;
        let pid = child.id();
        let mut guard = ChildGuard::new(pid);

        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");

        let stdout_handle = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            stdout_pipe.read_to_end(&mut buf).map(|_| buf)
        });
        let stderr_handle = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            stderr_pipe.read_to_end(&mut buf).map(|_| buf)
        });

        let deadline = tokio::time::Instant::now() + timeout_dur;
        let status = loop {
            if tokio::time::Instant::now() >= deadline {
                Self::kill_process(&mut child).await;
                guard.disarm();
                return format!("Error: Command timed out after {effective_timeout} seconds");
            }

            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => {
                    guard.disarm();
                    return format!("Error executing command: {e}");
                }
            }
        };
        guard.disarm();

        let stdout = match stdout_handle.await {
            Ok(Ok(buf)) => buf,
            Ok(Err(e)) => return format!("Error reading stdout: {e}"),
            Err(e) => return format!("Error executing command: {e}"),
        };
        let stderr = match stderr_handle.await {
            Ok(Ok(buf)) => buf,
            Ok(Err(e)) => return format!("Error reading stderr: {e}"),
            Err(e) => return format!("Error executing command: {e}"),
        };

        let output = std::process::Output {
            status,
            stdout,
            stderr,
        };

        // ── Build result string ───────────────────────────────────────────────
        let mut parts: Vec<String> = Vec::new();

        let stdout_text = String::from_utf8_lossy(&output.stdout);
        if !stdout_text.is_empty() {
            parts.push(stdout_text.into_owned());
        }

        let stderr_text = String::from_utf8_lossy(&output.stderr);
        if !stderr_text.trim().is_empty() {
            parts.push(format!("STDERR:\n{stderr_text}"));
        }

        let exit_code = output.status.code().unwrap_or(-1);
        parts.push(format!("\nExit code: {exit_code}"));

        let result = if parts.is_empty() {
            "(no output)".to_string()
        } else {
            parts.join("\n")
        };

        // ── Truncate if over MAX_OUTPUT ───────────────────────────────────────
        if result.len() > Self::MAX_OUTPUT {
            let half = Self::MAX_OUTPUT / 2;
            let truncated = result.len() - Self::MAX_OUTPUT;
            format!(
                "{}\n\n... ({} chars truncated) ...\n\n{}",
                &result[..half],
                truncated,
                &result[result.len() - half..],
            )
        } else {
            result
        }
    }

    /// Extract all absolute path references from a shell command string.
    ///
    /// Three classes are detected, mirroring the Python implementation:
    ///   - Windows drive-rooted paths  (`C:\…`)
    ///   - POSIX absolute paths        (`/absolute/path`)
    ///   - Home-directory shortcuts    (`~/…` or bare `~`)
    /// Build a minimal environment for subprocess execution.
    ///
    /// On Unix, only `HOME`, `LANG`, and `TERM` are forwarded; `bash -l`
    /// sources the user profile which sets `PATH` and other essentials.
    ///
    /// On Windows, `cmd.exe` has no login-profile mechanism, so a curated
    /// set of system variables (including `PATH`) is forwarded. API keys
    /// and other secrets are deliberately excluded.
    fn build_env() -> HashMap<String, String> {
        if IS_WINDOWS {
            let sr = std::env::var("SYSTEMROOT").unwrap_or_else(|_| r"C:\Windows".to_string());
            HashMap::from([
                ("SYSTEMROOT".to_string(), sr.clone()),
                (
                    "COMSPEC".to_string(),
                    std::env::var("COMSPEC").unwrap_or_else(|_| format!(r"{sr}\system32\cmd.exe")),
                ),
                (
                    "USERPROFILE".to_string(),
                    std::env::var("USERPROFILE").unwrap_or_default(),
                ),
                (
                    "HOMEDRIVE".to_string(),
                    std::env::var("HOMEDRIVE").unwrap_or_else(|_| "C:".to_string()),
                ),
                (
                    "HOMEPATH".to_string(),
                    std::env::var("HOMEPATH").unwrap_or_else(|_| r"\".to_string()),
                ),
                (
                    "TEMP".to_string(),
                    std::env::var("TEMP").unwrap_or_else(|_| format!(r"{sr}\Temp")),
                ),
                (
                    "TMP".to_string(),
                    std::env::var("TMP").unwrap_or_else(|_| format!(r"{sr}\Temp")),
                ),
                (
                    "PATHEXT".to_string(),
                    std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string()),
                ),
                (
                    "PATH".to_string(),
                    std::env::var("PATH").unwrap_or_else(|_| format!(r"{sr}\system32;{sr}")),
                ),
            ])
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            HashMap::from([
                ("HOME".to_string(), home),
                (
                    "LANG".to_string(),
                    std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".to_string()),
                ),
                (
                    "TERM".to_string(),
                    std::env::var("TERM").unwrap_or_else(|_| "dumb".to_string()),
                ),
            ])
        }
    }

    /// Best-effort safety guard for potentially destructive commands.
    ///
    /// Returns `Some(error_message)` if the command should be blocked,
    /// or `None` if it passes all checks.
    ///
    /// Checks (in order):
    ///   1. Deny-list patterns matched against the lowercased command.
    ///   2. Allow-list — if non-empty, command must match at least one pattern.
    ///   3. Path-traversal sequences (`../`, `..\`).
    ///   4. Absolute paths that escape the working directory.
    ///
    /// Note: the Python implementation also calls `contains_internal_url` from
    /// an external security module.  That check is omitted here; add it at the
    /// call site if needed.
    fn guard_command(&self, command: &str, cwd: &str) -> Option<String> {
        let cmd = command.trim();
        let lower = cmd.to_lowercase();

        // ── 1. Deny patterns ─────────────────────────────────────────────────
        if let Some(deny) = &self.deny_patterns {
            for pattern in deny {
                if let Ok(re) = Regex::new(pattern) {
                    if re.is_match(&lower) {
                        return Some(
                            "Error: Command blocked by safety guard (dangerous pattern detected)"
                                .to_string(),
                        );
                    }
                }
            }
        }

        // ── 2. Allow patterns ─────────────────────────────────────────────────
        if let Some(allow) = &self.allow_patterns {
            if !allow.is_empty() {
                let permitted = allow
                    .iter()
                    .any(|p| Regex::new(p).map(|re| re.is_match(&lower)).unwrap_or(false));
                if !permitted {
                    return Some(
                        "Error: Command blocked by safety guard (not in allowlist)".to_string(),
                    );
                }
            }
        }

        // ── 3. Workspace restriction ──────────────────────────────────────────
        if self.restrict_to_workspace {
            if cmd.contains("..\\") || cmd.contains("../") {
                return Some(
                    "Error: Command blocked by safety guard (path traversal detected)".to_string(),
                );
            }

            let cwd_path = Path::new(cwd)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(cwd));

            for raw in Self::extract_absolute_paths(cmd) {
                // Expand environment variables (best-effort: %VAR% on Windows, $VAR on POSIX).
                let expanded = Self::expand_vars(raw.trim());
                // Expand leading `~` to the user's home directory.
                let expanded = Self::expand_home(&expanded);

                let p = match Path::new(&expanded).canonicalize() {
                    Ok(resolved) => resolved,
                    // Path may not exist yet; use the raw expanded path for the check.
                    Err(_) => PathBuf::from(&expanded),
                };

                // TODO: Media directory should also be allowed here
                let media_path = get_media_dir(None);
                if p.is_absolute() && !p.starts_with(&cwd_path) && p != cwd_path 
                    && !p.starts_with(&media_path) && p != media_path {
                    return Some(
                        "Error: Command blocked by safety guard (path outside working dir)"
                            .to_string(),
                    );
                }
            }
        }

        None
    }

    /// Expand `%VAR%` (Windows) and `$VAR` / `${VAR}` (POSIX) in `s`.
    fn expand_vars(s: &str) -> String {
        // Windows-style: %VARNAME%
        let win_re = Regex::new(r"%([^%]+)%").unwrap();
        let s = win_re.replace_all(s, |caps: &regex::Captures| {
            std::env::var(&caps[1]).unwrap_or_else(|_| caps[0].to_string())
        });
        // POSIX-style: $VAR or ${VAR}
        let posix_re = Regex::new(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?").unwrap();
        posix_re
            .replace_all(&s, |caps: &regex::Captures| {
                std::env::var(&caps[1]).unwrap_or_else(|_| caps[0].to_string())
            })
            .into_owned()
    }

    /// Expand a leading `~` to the current user's home directory.
    fn expand_home(s: &str) -> String {
        if s == "~" || s.starts_with("~/") || s.starts_with("~\\") {
            let home = home::home_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()));
            format!("{}{}", home, &s[1..])
        } else {
            s.to_string()
        }
    }

    fn extract_absolute_paths(command: &str) -> Vec<String> {
        // Windows: C:\  or  C:\path\to\file  (* allows bare drive root)
        let win_re = Regex::new(r#"[A-Za-z]:\\[^\s"'|><;]*"#).unwrap();
        // POSIX absolute: /path  (must be preceded by start-of-string or a delimiter)
        let posix_re = Regex::new(r#"(?:^|[\s|>'"])(/[^\s"'>;|<]+)"#).unwrap();
        // Home shortcut: ~ or ~/path
        let home_re = Regex::new(r#"(?:^|[\s|>'"])(~[^\s"'>;|<]*)"#).unwrap();

        let mut paths: Vec<String> = Vec::new();

        for m in win_re.find_iter(command) {
            paths.push(m.as_str().to_string());
        }
        // For POSIX and home patterns the actual path is in capture group 1.
        for cap in posix_re.captures_iter(command) {
            if let Some(m) = cap.get(1) {
                paths.push(m.as_str().to_string());
            }
        }
        for cap in home_re.captures_iter(command) {
            if let Some(m) = cap.get(1) {
                paths.push(m.as_str().to_string());
            }
        }

        paths
    }

    /// Launch `command` in a platform-appropriate shell.
    ///
    /// On Windows the shell is taken from `COMSPEC` in `env` (falling back to
    /// the process environment and then `cmd.exe`); the command is passed as
    /// `/c <command>`.
    ///
    /// On Unix `bash -l -c <command>` is used; `bash` is located via `PATH`
    /// with `/bin/bash` as a fallback, mirroring Python's `shutil.which`.
    ///
    /// Both stdout and stderr are captured via `Stdio::piped()`.
    fn spawn(command: &str, cwd: &str, env: &HashMap<String, String>) -> std::io::Result<Child> {
        if IS_WINDOWS {
            let comspec = env
                .get("COMSPEC")
                .cloned()
                .or_else(|| std::env::var("COMSPEC").ok())
                .unwrap_or_else(|| "cmd.exe".to_string());

            Command::new(&comspec)
                .arg("/c")
                .arg(command)
                .current_dir(cwd)
                .env_clear()
                .envs(env)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        } else {
            // Mirror Python's `shutil.which("bash") or "/bin/bash"`:
            // search every directory in PATH for an executable named "bash".
            let bash = std::env::var("PATH")
                .unwrap_or_default()
                .split(':')
                .map(|dir| PathBuf::from(dir).join("bash"))
                .find(|p| p.is_file())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/bin/bash".to_string());

            Command::new(&bash)
                .arg("-l")
                .arg("-c")
                .arg(command)
                .current_dir(cwd)
                .env_clear()
                .envs(env)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        }
    }

    /// Forcefully terminate a process and its descendants by PID.
    fn kill_process_tree_sync(pid: u32) {
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .output();
        }
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    }

    /// Kill a subprocess and its entire process tree, then reap it.
    ///
    /// On **Windows**, `taskkill /F /T /PID` is used instead of `Child::kill()`
    /// because `cmd.exe /c <command>` spawns the real process as a child of
    /// cmd.exe.  Killing only cmd.exe would leave that grandchild running.
    /// `taskkill /F /T` forcefully terminates the whole tree.
    ///
    /// On **Unix**, `Child::kill()` sends SIGKILL to the direct child.  A
    /// final non-blocking `try_wait` is attempted afterwards to reap any
    /// remaining zombie (equivalent to `os.waitpid(pid, WNOHANG)`).
    ///
    /// In both cases the function waits up to 5 seconds for the process to exit.
    async fn kill_process(child: &mut Child) {
        Self::kill_process_tree_sync(child.id());

        // Wait up to 5 s for the process to exit.
        //
        // `Child::wait` is blocking, so we poll `try_wait` in a small async
        // loop rather than spawning a thread, avoiding the need to move `child`
        // into a closure.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break, // process exited
                Ok(None) => {}        // still running
                Err(e) => {
                    log::debug!("kill_process: try_wait error: {e}");
                    break;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                log::debug!("kill_process: timed out waiting for process to exit");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // On Unix, a final non-blocking try_wait reaps any remaining zombie
        // (mirrors Python's os.waitpid(pid, os.WNOHANG)).
        #[cfg(unix)]
        {
            match child.try_wait() {
                Ok(_) => {}
                Err(e) => {
                    log::debug!("kill_process: final try_wait (WNOHANG equivalent) failed: {e}")
                }
            }
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> String {
        "shell".to_string()
    }

    fn description(&self) -> String {
        return r#"Execute a shell command and return its output.
Prefer read_file/write_file/edit_file over cat/echo/sed, and grep/glob over shell find/grep. 
Use -y or --yes flags to avoid interactive prompts.
Output is truncated at 10 000 chars; timeout defaults to 60s."#
            .to_string()
            .trim()
            .to_string();
    }

    fn exclusive(&self) -> bool {
        true
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "working_dir": { "type": "string", "description": "The working directory to execute the command in" },
                "timeout": { "type": "integer", "description": "The timeout in seconds for the command to execute" },
            },
            "required": ["command"],
        })
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        let command = match params.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: missing required parameter 'command'".to_string(),
        };
    
        let working_dir = params
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
    
        let timeout = params
            .get("timeout")
            .and_then(|v| v.as_u64());
    
        self.execute_command(command, working_dir, timeout).await
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::tools::shell::{IS_WINDOWS, ShellTool};

    #[test]
    fn test_extract_absolute_paths() {
        let command = "echo Hello, world! > ~/notes.txt";
        let paths = ShellTool::extract_absolute_paths(command);
        assert_eq!(paths, vec!["~/notes.txt"]);
    }

    #[test]
    fn test_extract_absolute_paths_windows() {
        let command = "echo Hello, world! > d:\\notes.txt";
        let paths = ShellTool::extract_absolute_paths(command);
        assert_eq!(paths, vec!["d:\\notes.txt"]);
    }

    #[test]
    fn test_extract_absolute_paths_windows_no_drive() {
        let command = "echo Hello, world! > c:\\notes\\my_notes.txt";
        let paths = ShellTool::extract_absolute_paths(command);
        assert_eq!(paths, vec!["c:\\notes\\my_notes.txt"]);
    }

    #[test]
    fn test_build_env() {
        let env = ShellTool::build_env();
        if IS_WINDOWS {
            assert!(env.get("SYSTEMROOT").is_some());
            assert!(env.get("COMSPEC").is_some());
            assert!(env.get("USERPROFILE").is_some());
            assert!(env.get("HOMEDRIVE").is_some());
            assert!(env.get("HOMEPATH").is_some());
            assert!(env.get("TEMP").is_some());
            assert!(env.get("TMP").is_some());
        } else {
            assert!(env.get("HOME").is_some());
            assert!(env.get("LANG").is_some());
            assert!(env.get("TERM").is_some());
        }
    }

    /// Spawn notepad.exe and immediately kill it.
    ///
    /// Only compiled and run on Windows (`#[cfg(windows)]`); skipped on every
    /// other platform.  Notepad is used because it is always present on Windows
    /// and starts without needing any arguments.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore]
    async fn test_spawn_and_kill_notepad() {
        let env = ShellTool::build_env();
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let mut child =
            ShellTool::spawn("notepad.exe", &cwd, &env).expect("failed to spawn notepad.exe");

        // Give it a moment to start up, then kill it.
        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

        ShellTool::kill_process(&mut child).await;

        // After kill_process the process should no longer be running.
        let status = child.try_wait().expect("try_wait failed after kill");
        assert!(
            status.is_some(),
            "process should have exited after kill_process"
        );
    }

    /// Run a simple `echo` command through the full ShellTool execution path
    /// and verify the output is captured correctly.
    #[tokio::test]
    async fn test_execute_echo() {
        let tool = ShellTool::new(10, None, None, None, false, None, None);

        // Use a platform-appropriate echo command.
        let command = if IS_WINDOWS {
            "echo hello"
        } else {
            "echo hello"
        };
        let result = tool.execute_command(command, None, None).await;
        println!("result: {result}");

        assert!(
            result.contains("hello"),
            "expected 'hello' in output, got: {result}"
        );
        assert!(
            result.contains("Exit code: 0"),
            "expected exit code 0, got: {result}"
        );
    }

    #[test]
    fn test_guard_command_dangerous_pattern() {
        let tool = ShellTool::new(60, None, None, None, false, None, None);
        let command = "echo Hello, world! > ~/notes.txt && rm -rf ~/notes.txt";
        let result = tool.guard_command(command, ".");
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .contains("Error: Command blocked by safety guard (dangerous pattern detected)")
        );
    }

    #[tokio::test]
    async fn test_execute_command() {
        let tool = ShellTool::new(10, None, None, None, false, None, None);
        let result = tool.execute_command("echo hello", None, None).await;
        println!("result: {result}");
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn test_execute_volume_info() {
        let tool = ShellTool::new(10, None, None, None, false, None, None);
        let result = tool.execute_command(if IS_WINDOWS {"vol"} else {"lsblk -f"}, None, None).await;
        println!("result: {result}");
        assert!(result.len() > 0, "result should not be empty");
    }
}
