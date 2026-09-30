//! Tests that launch the real `rust-bot acp` binary, as an ACP client would.
//!
//! They cover what in-process tests cannot: the process-level stdout redirect,
//! startup failures reaching the client as JSON-RPC errors, and a clean exit
//! when the client closes stdin.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CHILD_ENV: &str = "RUST_BOT_ACP_REDIRECT_CHILD";
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);

/// A config that builds a provider without any network access.
fn write_valid_config(dir: &Path) -> PathBuf {
    let config = json!({
        "agents": {
            "provider": "openai",
            "model": "gpt-4o-mini",
            "workspace": dir.join("workspace").to_string_lossy()
        },
        "providers": { "openai": { "apiKey": "sk-not-used" } }
    });
    let path = dir.join("config.json");
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    path
}

fn initialize_request() -> String {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": 1, "clientCapabilities": {} }
    })
    .to_string()
}

fn new_session_request(cwd: &Path) -> String {
    json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": { "cwd": cwd.to_string_lossy(), "mcpServers": [] }
    })
    .to_string()
}

struct RunningAgent {
    child: Child,
    stdout_lines: mpsc::Receiver<String>,
    stderr_text: mpsc::Receiver<String>,
}

/// Launch `rust-bot acp` from `working_dir` with the given extra arguments.
fn launch(working_dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> RunningAgent {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rust-bot"));
    command
        .arg("acp")
        .args(args)
        .current_dir(working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("launch rust-bot acp");

    let stdout = child.stdout.take().unwrap();
    let (line_tx, stdout_lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stderr = child.stderr.take().unwrap();
    let (text_tx, stderr_text) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        let _ = text_tx.send(text);
    });

    RunningAgent {
        child,
        stdout_lines,
        stderr_text,
    }
}

impl RunningAgent {
    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }

    /// Next stdout line, failing the test if none arrives in time.
    fn next_stdout_line(&self) -> String {
        self.stdout_lines
            .recv_timeout(EXIT_TIMEOUT)
            .expect("a line on stdout")
    }

    /// Wait for the process to exit; kills it (and fails) on timeout.
    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("rust-bot acp did not exit within {EXIT_TIMEOUT:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Everything the process wrote to stdout after exit.
    fn remaining_stdout(&self) -> Vec<String> {
        self.stdout_lines.try_iter().collect()
    }

    fn stderr(&self) -> String {
        self.stderr_text
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_default()
    }
}

fn parse_frame(line: &str) -> Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line:?}"))
}

#[test]
fn stdout_carries_only_json_rpc_frames_even_with_debug_logging() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_valid_config(dir.path());
    let mut agent = launch(
        dir.path(),
        &["--config", config.to_str().unwrap(), "--logs"],
        &[("RUST_LOG", "debug"), ("RUST_LOG_FILE", "")],
    );

    agent.send(&initialize_request());
    let response = parse_frame(&agent.next_stdout_line());
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response.get("result").is_some(), "{response}");
    assert_eq!(response["result"]["agentInfo"]["name"], "rust-bot");

    agent.close_stdin();
    let status = agent.wait_for_exit();
    assert!(status.success(), "exit status: {status:?}");

    // Nothing but the one response ever reached stdout ...
    let extra = agent.remaining_stdout();
    assert!(extra.is_empty(), "unexpected stdout lines: {extra:?}");
    // ... while the logs went to stderr instead.
    let stderr = agent.stderr();
    assert!(
        !stderr.trim().is_empty(),
        "debug logging should appear on stderr"
    );
}

#[test]
fn stray_prints_land_on_stderr_not_stdout() {
    // Self-exec: this test re-runs itself as a child that claims the protocol
    // stdout, prints a stray line the way any dependency might, and writes one
    // protocol frame to the claimed handle.
    if std::env::var_os(CHILD_ENV).is_some() {
        let mut protocol_out =
            rust_bot::utils::stdio_redirect::claim_protocol_stdout().expect("claim stdout");
        println!("STRAY-PRINT-FROM-A-DEPENDENCY");
        writeln!(protocol_out, "PROTOCOL-FRAME").unwrap();
        protocol_out.flush().unwrap();
        std::process::exit(0);
    }

    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "stray_prints_land_on_stderr_not_stdout",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .expect("run child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(stdout.contains("PROTOCOL-FRAME"), "stdout: {stdout}");
    assert!(
        !stdout.contains("STRAY-PRINT-FROM-A-DEPENDENCY"),
        "a stray print reached stdout: {stdout}"
    );
    assert!(
        stderr.contains("STRAY-PRINT-FROM-A-DEPENDENCY"),
        "the stray print should be on stderr: {stderr}"
    );
}

#[test]
fn missing_config_reaches_the_client_as_a_json_rpc_error_and_a_failing_exit() {
    let dir = tempfile::tempdir().unwrap();
    // Relative path on purpose: it must not silently fall back to defaults.
    let mut agent = launch(dir.path(), &["--config", "does-not-exist.json"], &[]);

    agent.send(&initialize_request());
    let response = parse_frame(&agent.next_stdout_line());
    assert_eq!(response["id"], 1);
    assert!(response.get("result").is_none(), "{response}");
    let error = response["error"].to_string();
    assert!(error.contains("Config file not found"), "{error}");

    let status = agent.wait_for_exit();
    assert!(!status.success(), "must exit non-zero, got {status:?}");
}

#[test]
fn malformed_config_is_an_error_not_a_panic_crash() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    std::fs::write(&config, "{ this is not json").unwrap();
    let mut agent = launch(dir.path(), &["--config", config.to_str().unwrap()], &[]);

    agent.send(&initialize_request());
    let response = parse_frame(&agent.next_stdout_line());
    assert_eq!(response["id"], 1);
    assert!(response.get("error").is_some(), "{response}");

    let status = agent.wait_for_exit();
    assert!(!status.success());
}

#[test]
fn closing_stdin_stops_cleanly_and_leaves_valid_files() {
    let dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let config = write_valid_config(dir.path());
    let mut agent = launch(dir.path(), &["--config", config.to_str().unwrap()], &[]);

    agent.send(&initialize_request());
    agent.next_stdout_line();
    agent.send(&new_session_request(project.path()));
    let created = parse_frame(&agent.next_stdout_line());
    assert_eq!(created["id"], 2);
    let session_id = created["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    agent.close_stdin();
    let status = agent.wait_for_exit();
    assert!(status.success(), "exit status: {status:?}");

    // The memory file and the session file are intact.
    let workspace = dir.path().join("workspace");
    let memory = std::fs::read_to_string(workspace.join("memory").join("MEMORY.md"))
        .expect("MEMORY.md is readable text");
    assert!(!memory.contains('\u{FFFD}'));
    let sessions = std::fs::read_dir(workspace.join("sessions")).expect("sessions dir");
    let session_file = sessions
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(&session_id[..8])
        })
        .expect("the ACP session was persisted");
    for line in std::fs::read_to_string(session_file.path())
        .unwrap()
        .lines()
    {
        serde_json::from_str::<Value>(line).expect("every session line is valid JSON");
    }
}
