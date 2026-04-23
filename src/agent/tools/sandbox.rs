//! Sandbox backends for shell command execution.
//!
//! To add a new backend, implement a function with the signature:
//!     fn wrap_<name>(command: &str, workspace: &str, cwd: &str, media_dir: Option<&Path>) -> String
//! and register it in `wrap_command`.

use std::path::{Path, PathBuf};

/// Shell-quote a single argument, matching Python's `shlex.quote` behaviour.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(|c| {
        c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '%' | '+' | '=')
    }) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Join a slice of arguments into a single shell command string, matching
/// Python's `shlex.join`.
fn shell_join(args: &[String]) -> String {
    args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

/// Wrap `command` in a bubblewrap sandbox (requires `bwrap` on the host).
///
/// Only the workspace is bind-mounted read-write; its parent dir (which holds
/// config.json) is hidden behind a fresh tmpfs.  If `media_dir` is provided it
/// is bind-mounted read-only so exec commands can read uploaded attachments.
fn bwrap(command: &str, workspace: &str, cwd: &str, media_dir: Option<&Path>) -> String {
    let ws = Path::new(workspace)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(workspace));

    let sandbox_cwd = Path::new(cwd)
        .canonicalize()
        .ok()
        .and_then(|abs| abs.strip_prefix(&ws).ok().map(|rel| ws.join(rel)))
        .unwrap_or_else(|| ws.clone());

    let required = ["/usr"];
    let optional = [
        "/bin",
        "/lib",
        "/lib64",
        "/etc/alternatives",
        "/etc/ssl/certs",
        "/etc/resolv.conf",
        "/etc/ld.so.cache",
    ];

    let ws_str = ws.to_string_lossy().into_owned();
    let ws_parent = ws
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ws_str.clone());
    let sandbox_cwd_str = sandbox_cwd.to_string_lossy().into_owned();

    let mut args: Vec<String> = vec![
        "bwrap".into(),
        "--new-session".into(),
        "--die-with-parent".into(),
    ];

    for p in &required {
        args.extend(["--ro-bind".into(), p.to_string(), p.to_string()]);
    }
    for p in &optional {
        args.extend(["--ro-bind-try".into(), p.to_string(), p.to_string()]);
    }

    args.extend([
        "--proc".into(), "/proc".into(),
        "--dev".into(),  "/dev".into(),
        "--tmpfs".into(), "/tmp".into(),
        "--tmpfs".into(), ws_parent,
        "--dir".into(),   ws_str.clone(),
        "--bind".into(),  ws_str.clone(), ws_str,
    ]);

    if let Some(media) = media_dir {
        let media_str = media
            .canonicalize()
            .unwrap_or_else(|_| media.to_path_buf())
            .to_string_lossy()
            .into_owned();
        args.extend(["--ro-bind-try".into(), media_str.clone(), media_str]);
    }

    args.extend([
        "--chdir".into(), sandbox_cwd_str,
        "--".into(),
        "sh".into(), "-c".into(), command.into(),
    ]);

    shell_join(&args)
}

/// Wrap `command` using the named sandbox backend.
///
/// Returns the wrapped command string, or an error if the backend is unknown.
pub fn wrap_command(
    sandbox: &str,
    command: &str,
    workspace: &str,
    cwd: &str,
    media_dir: Option<&Path>,
) -> Result<String, String> {
    match sandbox {
        "bwrap" => Ok(bwrap(command, workspace, cwd, media_dir)),
        other => Err(format!(
            "Unknown sandbox backend {:?}. Available: [\"bwrap\"]",
            other
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_quote_safe_string() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("/usr/bin"), "/usr/bin");
        assert_eq!(shell_quote("--flag"), "--flag");
    }

    #[test]
    fn test_shell_quote_needs_quoting() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_wrap_command_unknown_backend() {
        let result = wrap_command("docker", "ls", "/workspace", "/workspace", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown sandbox backend"));
    }

    #[test]
    #[cfg(unix)]
    fn test_bwrap_contains_key_flags() {
        let result = wrap_command("bwrap", "ls -la", "/tmp/workspace", "/tmp/workspace", None);
        assert!(result.is_ok());
        let cmd = result.unwrap();
        assert!(cmd.contains("bwrap"));
        assert!(cmd.contains("--new-session"));
        assert!(cmd.contains("--die-with-parent"));
        assert!(cmd.contains("--ro-bind"));
        assert!(cmd.contains("sh"));
        assert!(cmd.contains("-c"));
    }

    #[test]
    #[cfg(unix)]
    fn test_bwrap_with_media_dir() {
        let media = Path::new("/tmp/media");
        let result = wrap_command("bwrap", "ls", "/tmp/workspace", "/tmp/workspace", Some(media));
        assert!(result.is_ok());
        let cmd = result.unwrap();
        assert!(cmd.contains("/tmp/media"));
        assert!(cmd.contains("--ro-bind-try"));
    }
}
