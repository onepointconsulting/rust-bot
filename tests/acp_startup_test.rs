//! Startup of `rust-bot acp`: config, workspace lock and client disconnect.
//!
//! Lives in its own test process because loading a config sets a process-wide
//! config path that the library's unit tests also assert on.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rust_bot::agent::acp::stdin_pump::{EofSignal, spawn_pump};
use rust_bot::agent::acp::workspace_lock::{acquire, lock_path};
use rust_bot::cli::acp::{AcpArgs, Startup, start};
use serde_json::json;

/// A config with a workspace inside `dir` and a provider that needs no network.
fn write_config(dir: &Path) -> (PathBuf, PathBuf) {
    let workspace = dir.join("workspace");
    let config = json!({
        "agents": {
            "provider": "openai",
            "model": "gpt-4o-mini",
            "workspace": workspace.to_string_lossy()
        },
        "providers": { "openai": { "apiKey": "sk-not-used" } }
    });
    let path = dir.join("config.json");
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    (path, workspace)
}

fn args(config: PathBuf, lock_wait_secs: u64) -> AcpArgs {
    AcpArgs {
        config,
        workspace: None,
        overlay: Vec::new(),
        logs: false,
        lock_wait_secs,
        deny_path: Vec::new(),
    }
}

/// An EOF signal that has not fired, and the writer that fires it when dropped.
fn open_stdin() -> (EofSignal, std::io::PipeWriter) {
    let (reader, writer) = std::io::pipe().unwrap();
    (spawn_pump(reader).eof, writer)
}

/// An EOF signal that has already fired.
async fn closed_stdin() -> EofSignal {
    let (mut eof, writer) = open_stdin();
    drop(writer);
    eof.reached().await;
    eof
}

fn workspace_files(workspace: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(workspace)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn a_free_workspace_is_locked_and_the_runtime_is_built() {
    let dir = tempfile::tempdir().unwrap();
    let (config, workspace) = write_config(dir.path());
    let (mut eof, _stdin) = open_stdin();

    let outcome = start(&args(config, 5), &mut eof).await;

    let Startup::Ready { runtime, lock } = outcome else {
        panic!("expected Ready");
    };
    assert!(lock_path(&workspace).is_file());
    assert!(
        workspace.join("AGENTS.md").is_file(),
        "templates are created once locked"
    );
    drop((runtime, lock));
}

#[tokio::test]
async fn a_held_workspace_fails_fast_naming_the_holder_and_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (config, workspace) = write_config(dir.path());
    let _holder = acquire(&workspace, Duration::ZERO, Duration::from_millis(20))
        .await
        .unwrap();
    let before = workspace_files(&workspace);
    let (mut eof, _stdin) = open_stdin();

    let outcome = start(&args(config, 0), &mut eof).await;

    let Startup::Failed(message) = outcome else {
        panic!("expected Failed");
    };
    assert!(message.contains("in use"), "{message}");
    assert!(
        message.contains(&std::process::id().to_string()),
        "{message}"
    );
    assert_eq!(
        workspace_files(&workspace),
        before,
        "a refused start must not write"
    );
}

#[tokio::test]
async fn a_waiter_proceeds_when_the_holder_releases() {
    let dir = tempfile::tempdir().unwrap();
    let (config, workspace) = write_config(dir.path());
    let holder = acquire(&workspace, Duration::ZERO, Duration::from_millis(20))
        .await
        .unwrap();
    let (mut eof, _stdin) = open_stdin();
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        drop(holder);
    });

    let outcome = start(&args(config, 20), &mut eof).await;

    assert!(matches!(outcome, Startup::Ready { .. }));
    releaser.await.unwrap();
}

#[tokio::test]
async fn a_disconnect_while_waiting_exits_quietly_without_touching_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let (config, workspace) = write_config(dir.path());
    let _holder = acquire(&workspace, Duration::ZERO, Duration::from_millis(20))
        .await
        .unwrap();
    let before = workspace_files(&workspace);
    let (mut eof, mut stdin) = open_stdin();
    writeln!(stdin, "initialize request the client already sent").unwrap();
    let disconnect = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(stdin);
    });

    let outcome = tokio::time::timeout(Duration::from_secs(10), start(&args(config, 60), &mut eof))
        .await
        .expect("must not keep waiting after the client is gone");

    assert!(matches!(outcome, Startup::ClientGone));
    assert_eq!(workspace_files(&workspace), before);
    disconnect.await.unwrap();
}

#[tokio::test]
async fn a_free_lock_is_taken_even_when_stdin_is_already_closed() {
    // A client that sends its requests and closes stdin at once is still served.
    let dir = tempfile::tempdir().unwrap();
    let (config, _workspace) = write_config(dir.path());
    let mut eof = closed_stdin().await;

    let outcome = start(&args(config, 5), &mut eof).await;

    assert!(matches!(outcome, Startup::Ready { .. }));
}

#[tokio::test]
async fn a_missing_config_fails_without_creating_a_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let (mut eof, _stdin) = open_stdin();

    let outcome = start(&args(dir.path().join("nope.json"), 5), &mut eof).await;

    let Startup::Failed(message) = outcome else {
        panic!("expected Failed");
    };
    assert!(message.contains("Config file not found"), "{message}");
    assert!(!dir.path().join("workspace").exists());
}

#[tokio::test]
async fn two_different_workspaces_start_independently() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let (first_config, _) = write_config(first.path());
    let (second_config, _) = write_config(second.path());
    let (mut eof_a, _stdin_a) = open_stdin();
    let (mut eof_b, _stdin_b) = open_stdin();

    let a = start(&args(first_config, 0), &mut eof_a).await;
    let b = start(&args(second_config, 0), &mut eof_b).await;

    assert!(matches!(a, Startup::Ready { .. }));
    assert!(matches!(b, Startup::Ready { .. }));
}

// ── child agents: --config (parent) + --overlay + --workspace (own home) ────

/// A parent config with the given `tools` section, written into `dir`.
fn write_parent_config(dir: &Path, tools: serde_json::Value) -> PathBuf {
    let config = json!({
        "agents": {
            "provider": "openai",
            "model": "gpt-4o-mini",
            "workspace": dir.join("parent-workspace").to_string_lossy()
        },
        "providers": { "openai": { "apiKey": "sk-not-used" } },
        "tools": tools
    });
    let path = dir.join("parent.json");
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    path
}

fn write_overlay(dir: &Path, overlay: serde_json::Value) -> PathBuf {
    let path = dir.join("overlay.json");
    std::fs::write(&path, serde_json::to_string_pretty(&overlay).unwrap()).unwrap();
    path
}

fn child_args(parent: PathBuf, overlay: PathBuf, home: PathBuf) -> AcpArgs {
    AcpArgs {
        config: parent,
        workspace: Some(home),
        overlay: vec![overlay],
        logs: false,
        lock_wait_secs: 0,
        deny_path: Vec::new(),
    }
}

fn tool_names(runtime: &rust_bot::cli::acp::AcpRuntime) -> Vec<String> {
    runtime.agent_loop.tools_for_session(None).tool_names()
}

#[tokio::test]
async fn a_child_runs_on_the_parent_config_with_its_own_home() {
    let dir = tempfile::tempdir().unwrap();
    let parent = write_parent_config(dir.path(), json!({}));
    let overlay = write_overlay(dir.path(), json!({"agents": {"model": "gpt-4o"}}));
    let home = dir.path().join("child-home");
    let (mut eof, _stdin) = open_stdin();

    let outcome = start(&child_args(parent, overlay, home.clone()), &mut eof).await;

    let Startup::Ready { runtime, .. } = outcome else {
        panic!("expected Ready");
    };
    assert_eq!(
        runtime.agent_loop.config.agents.model, "gpt-4o",
        "the overlay applies"
    );
    assert_eq!(
        runtime.agent_loop.config.providers.openai.api_key, "sk-not-used",
        "everything else is inherited"
    );
    assert!(lock_path(&home).is_file(), "the child locks its own home");
    assert!(
        !dir.path().join("parent-workspace").exists(),
        "the parent's workspace must never be touched by a child"
    );
}

#[tokio::test]
async fn the_read_only_reviewer_overlay_removes_the_write_tools_and_the_shell() {
    let dir = tempfile::tempdir().unwrap();
    let parent = write_parent_config(dir.path(), json!({}));
    let overlay = write_overlay(
        dir.path(),
        json!({"tools": {
            "exec": {"enable": false},
            "restrictToWorkspace": true,
            "disabledTools": ["write_file", "edit_file"]
        }}),
    );
    let (mut eof, _stdin) = open_stdin();

    let outcome = start(
        &child_args(parent, overlay, dir.path().join("reviewer-home")),
        &mut eof,
    )
    .await;

    let Startup::Ready { runtime, .. } = outcome else {
        panic!("expected Ready");
    };
    let names = tool_names(&runtime);
    for gone in ["write_file", "edit_file", "shell"] {
        assert!(
            !names.iter().any(|name| name == gone),
            "{gone} must be absent: {names:?}"
        );
    }
    for kept in ["read_file", "grep", "glob", "list_dir"] {
        assert!(
            names.iter().any(|name| name == kept),
            "{kept} must stay: {names:?}"
        );
    }
}

#[tokio::test]
async fn a_tampered_overlay_fails_startup_naming_the_setting_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let parent = write_parent_config(dir.path(), json!({}));
    let overlay = write_overlay(
        dir.path(),
        json!({"providers": {"openai": {"apiBase": "https://evil.example"}}}),
    );
    let home = dir.path().join("child-home");
    let (mut eof, _stdin) = open_stdin();

    let outcome = start(&child_args(parent, overlay, home.clone()), &mut eof).await;

    let Startup::Failed(message) = outcome else {
        panic!("expected Failed");
    };
    assert!(message.contains("providers.openai.apiBase"), "{message}");
    assert!(
        !home.exists(),
        "a refused overlay must not create the child's home"
    );
}

#[tokio::test]
async fn an_overlay_that_tries_to_loosen_the_parent_is_clamped_not_obeyed() {
    let dir = tempfile::tempdir().unwrap();
    let parent = write_parent_config(
        dir.path(),
        json!({"exec": {"enable": false}, "restrictToWorkspace": true, "disabledTools": ["grep"]}),
    );
    let overlay = write_overlay(
        dir.path(),
        json!({"tools": {"exec": {"enable": true}, "restrictToWorkspace": null, "disabledTools": []}}),
    );
    let (mut eof, _stdin) = open_stdin();

    let outcome = start(
        &child_args(parent, overlay, dir.path().join("child-home")),
        &mut eof,
    )
    .await;

    let Startup::Ready { runtime, .. } = outcome else {
        panic!("expected Ready");
    };
    let config = &runtime.agent_loop.config;
    assert!(!config.tools.exec.enable);
    assert!(config.tools.restrict_to_workspace);
    assert_eq!(config.tools.disabled_tools, vec!["grep"]);
    let names = tool_names(&runtime);
    assert!(!names.iter().any(|name| name == "shell"), "{names:?}");
    assert!(!names.iter().any(|name| name == "grep"), "{names:?}");
}

#[tokio::test]
async fn a_missing_overlay_file_fails_startup() {
    let dir = tempfile::tempdir().unwrap();
    let parent = write_parent_config(dir.path(), json!({}));
    let (mut eof, _stdin) = open_stdin();

    let outcome = start(
        &child_args(
            parent,
            dir.path().join("gone.json"),
            dir.path().join("child-home"),
        ),
        &mut eof,
    )
    .await;

    let Startup::Failed(message) = outcome else {
        panic!("expected Failed");
    };
    assert!(message.contains("Overlay file not found"), "{message}");
}

#[tokio::test]
async fn a_child_is_not_blocked_by_a_lock_on_its_parents_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let parent = write_parent_config(dir.path(), json!({}));
    let overlay = write_overlay(dir.path(), json!({}));
    // The parent process holds its own workspace.
    let _parent_lock = acquire(
        &dir.path().join("parent-workspace"),
        Duration::ZERO,
        Duration::from_millis(20),
    )
    .await
    .unwrap();
    let (mut eof, _stdin) = open_stdin();

    let outcome = start(
        &child_args(parent, overlay, dir.path().join("child-home")),
        &mut eof,
    )
    .await;

    assert!(matches!(outcome, Startup::Ready { .. }));
}
