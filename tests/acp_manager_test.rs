//! A parent rust-bot running a child **rust-bot process** over ACP.
//!
//! The child is the real `rust-bot acp` binary (started by the manager exactly as
//! in production: argv array, absolute paths, derived environment). Its LLM is a
//! mock HTTP server on localhost, so the whole round trip needs no network.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rust_bot::agent::acp::client::{ActivityStatus, ProgressSink, TurnEnd};
use rust_bot::agent::acp::manager::{
    AcpManager, AcpManagerSettings, ManagerTimings, RunError, RunRequest, RunResult,
};
use rust_bot::agent::acp::permission::{PermissionResponder, PolicyResponder};
use rust_bot::agent::acp::store::{NewAgent, Overrides};
use rust_bot::agent::acp::workspace_lock::acquire;
use rust_bot::config::schema::{AcpConfig, AcpPermissionPolicy};
use serde_json::{Value, json};

mod support;

use support::mock_llm::{MockLlm, MockReply};
use support::{long_running_command, long_running_process_name, running_process_count, wait_until};

const AGENT: &str = "code-review";
const PARENT_SESSION: &str = "cli:direct";

// ── a parent in a temp folder ───────────────────────────────────────────────

struct Parent {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    config_path: PathBuf,
    project: PathBuf,
    llm: MockLlm,
    manager: Arc<AcpManager>,
}

/// Options for building a [`Parent`].
struct ParentOptions {
    script: Vec<MockReply>,
    /// Merged into the parent config (`{"tools": {...}}`, `{"agents": {...}}`, ...).
    config_patch: Value,
    extra_env: Vec<(&'static str, &'static str)>,
    timings: ManagerTimings,
}

impl ParentOptions {
    fn new(script: Vec<MockReply>) -> Self {
        Self {
            script,
            config_patch: json!({}),
            extra_env: Vec::new(),
            timings: ManagerTimings::default(),
        }
    }
}

fn parent(options: ParentOptions) -> Parent {
    let root = tempfile::tempdir().unwrap();
    let base = std::path::absolute(root.path()).unwrap();
    let workspace = base.join("parent-home");
    let project = base.join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    rust_bot::utils::helpers::sync_workspace_templates(&workspace, true);

    let llm = MockLlm::start(options.script);
    let mut config = json!({
        "agents": {"provider": "openai", "model": "gpt-4o-mini", "workspace": workspace},
        "providers": {"openai": {"apiKey": "sk-test", "apiBase": llm.api_base()}},
        "tools": {"acp": {"enabled": true, "shutdownGraceSecs": 60, "defaultTimeoutSecs": 120}}
    });
    merge(&mut config, &options.config_patch);
    let config_path = base.join("config.json");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    let mut parent_env: HashMap<String, String> = std::env::vars().collect();
    parent_env.extend(
        options
            .extra_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    let manager = Arc::new(AcpManager::new(AcpManagerSettings {
        parent_workspace: workspace.clone(),
        config_path: config_path.clone(),
        inherited_overlays: Vec::new(),
        depth: 0,
        executable_override: Some(PathBuf::from(env!("CARGO_BIN_EXE_rust-bot"))),
        parent_env,
        timings: options.timings,
    }));
    Parent {
        _root: root,
        workspace,
        config_path,
        project,
        llm,
        manager,
    }
}

/// RFC 7386-style shallow-recursive merge, enough for building test configs.
fn merge(target: &mut Value, patch: &Value) {
    if let (Some(target_map), Some(patch_map)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch_map {
            merge(target_map.entry(key.clone()).or_insert(Value::Null), value);
        }
    } else {
        *target = patch.clone();
    }
}

impl Parent {
    /// Create the child agent `AGENT` with `overlay`.
    fn create_agent(&self, overlay: Value) {
        let parent_config = self.manager.load_parent_config().unwrap();
        self.manager
            .store()
            .create(
                NewAgent {
                    name: AGENT.to_string(),
                    purpose: "reviews code".to_string(),
                    preset: "rustbot".to_string(),
                    overlay,
                    overrides: Overrides::default(),
                    cwd: None,
                    created_by: PARENT_SESSION.to_string(),
                    depth: 1,
                },
                &parent_config,
            )
            .expect("create the child agent");
    }

    fn child_home(&self) -> PathBuf {
        self.manager.store().agent_dir(AGENT)
    }

    fn run_request(&self, prompt: &str, policy: AcpPermissionPolicy) -> RunRequest {
        self.run_request_for(AGENT, prompt, PARENT_SESSION, policy)
    }

    fn run_request_for(
        &self,
        agent: &str,
        prompt: &str,
        parent_session: &str,
        policy: AcpPermissionPolicy,
    ) -> RunRequest {
        let responder: Arc<dyn PermissionResponder> = Arc::new(PolicyResponder::new(policy, None));
        let acp = AcpConfig {
            enabled: true,
            shutdown_grace_secs: 60,
            default_timeout_secs: 120,
            ..AcpConfig::default()
        };
        RunRequest {
            agent: agent.to_string(),
            prompt: prompt.to_string(),
            cwd: Some(self.project.to_string_lossy().into_owned()),
            parent_session_key: parent_session.to_string(),
            parent_project: self.workspace.clone(),
            parent_restricted: false,
            acp,
            timeout: None,
            responder,
            progress: None,
        }
    }

    fn config_bytes(&self) -> Vec<u8> {
        std::fs::read(&self.config_path).unwrap()
    }
}

/// Wait for the child process of a finished run to end, and return how it ended.
async fn wait_exit(result: &mut RunResult) -> rust_bot::agent::acp::transport::ExitReport {
    tokio::time::timeout(Duration::from_secs(120), &mut result.exit)
        .await
        .expect("the child exited in time")
        .expect("the reaper task joined")
}

/// Recording progress sink.
#[derive(Default)]
struct Tools(std::sync::Mutex<Vec<String>>);

impl ProgressSink for Tools {
    fn tool_started(&self, title: &str) {
        self.0.lock().unwrap().push(title.to_string());
    }

    fn still_working(&self, _elapsed: Duration) {}
}

/// Every file under `dir` except the `acp/` folder (the children's homes) with
/// its contents: what a child must never change in its parent.
fn snapshot_without_children(dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, into: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if relative.starts_with("acp") {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, into);
            } else {
                into.insert(relative, std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    walk(dir, dir, &mut files);
    files
}

fn write_notes(parent: &Parent) {
    std::fs::write(parent.project.join("notes.txt"), "hello from the project").unwrap();
}

fn read_notes_script(final_text: &str) -> Vec<MockReply> {
    vec![
        MockReply::tool_call("call-1", "read_file", json!({"path": "notes.txt"})),
        MockReply::text(final_text),
    ]
}

// ── a run ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_run_launches_the_real_child_and_returns_its_reply() {
    let parent = parent(ParentOptions::new(read_notes_script(
        "The file says: hello",
    )));
    parent.create_agent(json!({}));
    write_notes(&parent);
    let config_before = parent.config_bytes();
    let parent_files_before = snapshot_without_children(&parent.workspace);
    let progress = Arc::new(Tools::default());
    let mut request = parent.run_request(
        "what is in notes.txt?",
        AcpPermissionPolicy::AutoApproveRead,
    );
    request.progress = Some(progress.clone());

    let mut result = parent.manager.run(request).await.expect("the run succeeds");

    assert_eq!(result.reply, "The file says: hello");
    assert_eq!(result.end, TurnEnd::Finished);
    assert!(!result.resumed);
    assert_eq!(result.activity.len(), 1, "{:?}", result.activity);
    assert!(result.activity[0].title.contains("notes.txt"));
    assert_eq!(result.activity[0].status, ActivityStatus::Completed);
    assert_eq!(progress.0.lock().unwrap().len(), 1);

    // After the reply the child is stopped by closing its input: it exits by itself.
    let report = wait_exit(&mut result).await;
    assert!(report.exited_cleanly(), "{report:?}");

    // Its lock is free again once it is gone.
    drop(
        acquire(
            &parent.child_home(),
            Duration::ZERO,
            Duration::from_millis(50),
        )
        .await
        .expect("the child released its workspace lock"),
    );

    // The child kept its memory in its own home ...
    let sessions: Vec<_> = std::fs::read_dir(parent.child_home().join("sessions"))
        .unwrap()
        .flatten()
        .collect();
    assert!(
        !sessions.is_empty(),
        "the child saved its session in its own home"
    );
    // ... and nothing of it reached the parent: no sessions, config untouched.
    assert_eq!(
        parent.config_bytes(),
        config_before,
        "the parent config is untouched"
    );
    assert_eq!(
        snapshot_without_children(&parent.workspace),
        parent_files_before,
        "the parent's own memory, sessions and files are untouched"
    );
}

#[tokio::test]
async fn the_second_run_resumes_the_conversation_in_a_new_process() {
    let parent = parent(ParentOptions::new(read_notes_script("first answer")));
    parent.create_agent(json!({}));
    write_notes(&parent);

    let mut first = parent
        .manager
        .run(parent.run_request(
            "what is in notes.txt?",
            AcpPermissionPolicy::AutoApproveRead,
        ))
        .await
        .unwrap();
    let first_pid_report = wait_exit(&mut first).await;
    assert!(first_pid_report.exited_cleanly());
    parent.llm.push(vec![MockReply::text("second answer")]);

    let mut second = parent
        .manager
        .run(parent.run_request(
            "what did I ask before?",
            AcpPermissionPolicy::AutoApproveRead,
        ))
        .await
        .unwrap();

    assert!(second.resumed);
    assert_eq!(second.session_id, first.session_id);
    assert_eq!(
        second.reply, "second answer",
        "replayed history must not leak into the reply"
    );
    assert!(second.activity.is_empty(), "{:?}", second.activity);
    // The child's model saw the first turn.
    assert!(
        parent
            .llm
            .everything_seen()
            .contains("what is in notes.txt?")
    );
    wait_exit(&mut second).await;
}

#[tokio::test]
async fn different_parent_sessions_get_different_child_conversations() {
    let parent = parent(ParentOptions::new(vec![
        MockReply::text("answer for alice"),
        MockReply::text("answer for bob"),
    ]));
    parent.create_agent(json!({}));

    let mut alice = parent
        .manager
        .run(parent.run_request_for(
            AGENT,
            "hi",
            "ws:alice",
            AcpPermissionPolicy::AutoApproveRead,
        ))
        .await
        .unwrap();
    wait_exit(&mut alice).await;
    let mut bob = parent
        .manager
        .run(parent.run_request_for(AGENT, "hi", "ws:bob", AcpPermissionPolicy::AutoApproveRead))
        .await
        .unwrap();
    wait_exit(&mut bob).await;

    assert_ne!(alice.session_id, bob.session_id);
    assert!(!bob.resumed, "bob must not continue alice's thread");
    let remembered = parent.manager.index().entries().unwrap();
    assert_eq!(remembered.len(), 2, "{remembered:?}");
}

// ── environment ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_variable_the_config_references_reaches_the_child_and_is_expanded_there() {
    // The model name is a placeholder in the *parent's file*; only a child that
    // received the variable can expand it, and the mock LLM sees the result.
    // (That the other variables stay behind is covered where the environment is
    // built: `launch` and `transport` unit tests; the shell tool builds its own
    // environment, so it cannot be used to look.)
    let mut options = ParentOptions::new(vec![MockReply::text("done")]);
    options.config_patch = json!({"agents": {"model": "${ACP_E2E_MODEL}"}});
    options.extra_env = vec![("ACP_E2E_MODEL", "model-from-the-environment")];
    let parent = parent(options);
    parent.create_agent(json!({}));

    let mut result = parent
        .manager
        .run(parent.run_request("hello", AcpPermissionPolicy::AutoApproveRead))
        .await
        .unwrap();
    wait_exit(&mut result).await;

    let models: Vec<String> = parent
        .llm
        .requests()
        .iter()
        .filter_map(|body| body["model"].as_str().map(str::to_string))
        .collect();
    assert!(
        models
            .iter()
            .any(|model| model == "model-from-the-environment"),
        "the child must have expanded the variable: {models:?}"
    );
}

#[tokio::test]
async fn an_unset_referenced_variable_fails_in_the_parent_and_starts_nothing() {
    let mut options = ParentOptions::new(Vec::new());
    options.config_patch =
        json!({"tools": {"web": {"search": {"apiKey": "${ACP_E2E_NOT_SET_ANYWHERE}"}}}});
    let parent = parent(options);
    parent.create_agent(json!({}));

    let error = parent
        .manager
        .run(parent.run_request("hi", AcpPermissionPolicy::AutoApproveRead))
        .await
        .err()
        .expect("the run is refused");

    assert!(matches!(error, RunError::Launch(_)), "{error}");
    assert!(
        error.to_string().contains("ACP_E2E_NOT_SET_ANYWHERE"),
        "{error}"
    );
    assert!(
        !parent.child_home().join(".acp.lock").exists(),
        "no child process was started, so nothing took its workspace lock"
    );
}

// ── refused before anything starts ──────────────────────────────────────────

#[tokio::test]
async fn an_unknown_agent_lists_the_known_ones() {
    let parent = parent(ParentOptions::new(Vec::new()));
    parent.create_agent(json!({}));

    let error = parent
        .manager
        .run(parent.run_request_for(
            "code-reviw",
            "hi",
            PARENT_SESSION,
            AcpPermissionPolicy::AutoApproveRead,
        ))
        .await
        .err()
        .unwrap();

    assert!(matches!(error, RunError::UnknownAgent { .. }));
    assert!(error.to_string().contains(AGENT), "{error}");
}

#[tokio::test]
async fn the_parents_own_home_is_refused_as_the_project_folder() {
    let parent = parent(ParentOptions::new(Vec::new()));
    parent.create_agent(json!({}));
    let mut request = parent.run_request("hi", AcpPermissionPolicy::AutoApproveRead);
    request.cwd = Some(parent.workspace.to_string_lossy().into_owned());

    let error = parent.manager.run(request).await.err().unwrap();

    assert!(matches!(error, RunError::Project(_)), "{error}");
}

#[tokio::test]
async fn an_agent_with_another_preset_is_refused() {
    let parent = parent(ParentOptions::new(Vec::new()));
    parent.create_agent(json!({}));
    // Pretend an agent was created with a preset this version cannot launch.
    let meta_path = parent.child_home().join("agent.json");
    let mut meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    meta["preset"] = json!("gemini");
    std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

    let error = parent
        .manager
        .run(parent.run_request("hi", AcpPermissionPolicy::AutoApproveRead))
        .await
        .err()
        .unwrap();

    assert!(
        matches!(error, RunError::UnsupportedPreset { .. }),
        "{error}"
    );
}

// ── a child that never starts ───────────────────────────────────────────────

/// What the stand-in child recorded when it was started.
struct StandInRecord {
    dir: PathBuf,
}

impl StandInRecord {
    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Wait for the stand-in to have written `name`, and return its non-empty lines.
    async fn lines(&self, name: &str) -> Vec<String> {
        let path = self.file(name);
        wait_until(
            || path.exists(),
            &format!("the stand-in child to write {name}"),
        )
        .await;
        // The writer may still be flushing; give it a moment.
        tokio::time::sleep(Duration::from_millis(400)).await;
        std::fs::read_to_string(&path)
            .unwrap()
            .trim_start_matches('\u{feff}')
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }
}

/// A "child" that ignores everything and sleeps. First it records its pid, its
/// arguments, its working directory, the names of its environment variables and
/// its depth variable, so tests can check how it was started.
fn write_unresponsive_child(dir: &Path) -> (Value, StandInRecord) {
    let record = StandInRecord {
        dir: dir.to_path_buf(),
    };
    let pid = record.file("pid.txt");
    let args = record.file("args.txt");
    let cwd = record.file("cwd.txt");
    let env_names = record.file("env_names.txt");
    let depth = record.file("depth.txt");
    if cfg!(windows) {
        let script = dir.join("unresponsive.ps1");
        let text = [
            format!("$PID | Out-File -Encoding ascii '{}'", pid.display()),
            format!("$args | Out-File -Encoding ascii '{}'", args.display()),
            format!(
                "(Get-Location).Path | Out-File -Encoding ascii '{}'",
                cwd.display()
            ),
            format!(
                "Get-ChildItem Env: | ForEach-Object {{ $_.Name }} | Out-File -Encoding ascii '{}'",
                env_names.display()
            ),
            format!(
                "$env:RUST_BOT_ACP_DEPTH | Out-File -Encoding ascii '{}'",
                depth.display()
            ),
            "Start-Sleep -Seconds 120".to_string(),
        ]
        .join("\n");
        std::fs::write(&script, text).unwrap();
        (
            json!({"command": ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script]}),
            record,
        )
    } else {
        let script = dir.join("unresponsive.sh");
        let text = [
            format!("echo $$ > '{}'", pid.display()),
            format!("printf '%s\\n' \"$@\" > '{}'", args.display()),
            format!("pwd > '{}'", cwd.display()),
            format!("env | cut -d= -f1 > '{}'", env_names.display()),
            format!("echo \"$RUST_BOT_ACP_DEPTH\" > '{}'", depth.display()),
            "sleep 120".to_string(),
        ]
        .join("\n");
        std::fs::write(&script, text).unwrap();
        (json!({"command": ["sh", script]}), record)
    }
}

fn process_is_alive(pid: u32) -> bool {
    if cfg!(windows) {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
    } else {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

#[tokio::test]
async fn a_child_that_never_answers_initialize_is_reported_and_killed() {
    let scratch = tempfile::tempdir().unwrap();
    let (preset, record) = write_unresponsive_child(scratch.path());
    let mut options = ParentOptions::new(Vec::new());
    options.config_patch =
        json!({"tools": {"acp": {"shutdownGraceSecs": 1, "launchPresets": {"rustbot": preset}}}});
    options.timings = ManagerTimings {
        startup_slack: Duration::from_millis(1500),
        ..ManagerTimings::default()
    };
    let parent = parent(options);
    parent.create_agent(json!({}));
    let mut request = parent.run_request("hi", AcpPermissionPolicy::AutoApproveRead);
    request.acp.shutdown_grace_secs = 1;

    let started = std::time::Instant::now();
    let error = parent
        .manager
        .run(request)
        .await
        .err()
        .expect("the run fails");

    assert!(matches!(error, RunError::Child { .. }), "{error}");
    assert!(error.to_string().contains("did not start"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "must not wait for the 120s sleep"
    );

    // The process the manager started is gone, not left behind.
    let pid: u32 = record.lines("pid.txt").await[0].parse().expect("a pid");
    wait_until(
        || !process_is_alive(pid),
        "the unresponsive child to be killed",
    )
    .await;
}

// ── stopping the parent's turn ──────────────────────────────────────────────

#[tokio::test]
async fn dropping_the_run_mid_turn_stops_the_child_and_frees_the_agent() {
    let parent = parent(ParentOptions::new(vec![
        MockReply::tool_call(
            "long-1",
            "shell",
            json!({"command": long_running_command()}),
        ),
        MockReply::text("never reached"),
    ]));
    parent.create_agent(json!({}));
    let baseline = running_process_count(long_running_process_name());

    let manager = Arc::clone(&parent.manager);
    let request = parent.run_request("run the long command", AcpPermissionPolicy::AllowAll);
    let running = tokio::spawn(async move { manager.run(request).await.map(|_| ()) });

    wait_until(
        || running_process_count(long_running_process_name()) > baseline,
        "the child's long shell command to start",
    )
    .await;
    // What `/stop` does to the parent's turn: the task is aborted, the future dropped.
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());

    // The child is told to stop, kills its shell tree and exits, releasing its lock.
    wait_until(
        || running_process_count(long_running_process_name()) <= baseline,
        "the child's shell command to be killed",
    )
    .await;
    let lock = acquire(
        &parent.child_home(),
        Duration::from_secs(90),
        Duration::from_millis(200),
    )
    .await
    .expect("the child exits and releases its workspace lock");
    drop(lock);

    // The next run for the same agent works and does not have to wait behind the old one.
    parent.llm.push(vec![MockReply::text("back again")]);
    let started = std::time::Instant::now();
    let mut next = parent
        .manager
        .run(parent.run_request("are you there?", AcpPermissionPolicy::AutoApproveRead))
        .await
        .expect("the next run works");
    assert!(
        next.reply.contains("back again") || next.reply.contains("never reached"),
        "{}",
        next.reply
    );
    assert!(started.elapsed() < Duration::from_secs(60));
    wait_exit(&mut next).await;
}

// ── two runs of the same child at once ──────────────────────────────────────

#[tokio::test]
async fn two_runs_of_the_same_child_are_serialized_and_never_collide_on_its_lock() {
    let parent = parent(ParentOptions::new(vec![
        MockReply::text("first reply"),
        MockReply::text("second reply"),
    ]));
    parent.create_agent(json!({}));

    let (a, b) = tokio::join!(
        parent.manager.run(parent.run_request_for(
            AGENT,
            "a",
            "ws:a",
            AcpPermissionPolicy::AutoApproveRead
        )),
        parent.manager.run(parent.run_request_for(
            AGENT,
            "b",
            "ws:b",
            AcpPermissionPolicy::AutoApproveRead
        )),
    );
    let (mut a, mut b) = (a.expect("run a"), b.expect("run b"));

    let mut replies = vec![a.reply.clone(), b.reply.clone()];
    replies.sort();
    assert_eq!(replies, vec!["first reply", "second reply"]);
    // The second child only started once the first was gone: it never had to
    // wait on the workspace lock (which it would say on stderr).
    for report in [wait_exit(&mut a).await, wait_exit(&mut b).await] {
        assert!(report.exited_cleanly(), "{report:?}");
        assert!(
            !report.stderr_tail.contains("is in use by"),
            "{}",
            report.stderr_tail
        );
    }
}

#[tokio::test]
async fn a_child_is_started_with_absolute_paths_the_parents_cwd_and_a_derived_environment() {
    let scratch = tempfile::tempdir().unwrap();
    let (preset, record) = write_unresponsive_child(scratch.path());
    let mut options = ParentOptions::new(Vec::new());
    options.config_patch =
        json!({"tools": {"acp": {"shutdownGraceSecs": 1, "launchPresets": {"rustbot": preset}}}});
    options.extra_env = vec![("ACP_E2E_UNRELATED", "must-not-be-passed")];
    options.timings = ManagerTimings {
        startup_slack: Duration::from_millis(1500),
        ..ManagerTimings::default()
    };
    let parent = parent(options);
    parent.create_agent(json!({}));
    let mut request = parent.run_request("hi", AcpPermissionPolicy::AutoApproveRead);
    request.acp.shutdown_grace_secs = 1;

    // The stand-in never speaks ACP, so the run fails; what matters is how it was started.
    let _ = parent.manager.run(request).await;

    // Arguments: the config, the overlay and the home, all absolute, plus a lock wait.
    let args = record.lines("args.txt").await;
    let value_after = |flag: &str| {
        let index = args
            .iter()
            .position(|arg| arg == flag)
            .unwrap_or_else(|| panic!("{flag} in {args:?}"));
        PathBuf::from(&args[index + 1])
    };
    let config = value_after("--config");
    let overlay = value_after("--overlay");
    let home = value_after("--workspace");
    for path in [&config, &overlay, &home] {
        assert!(path.is_absolute(), "{path:?} in {args:?}");
    }
    assert_eq!(config, parent.config_path);
    assert_eq!(overlay, parent.manager.store().overlay_path(AGENT));
    assert_eq!(home, parent.child_home());
    assert!(args.iter().any(|arg| arg == "--lock-wait-secs"), "{args:?}");
    assert!(
        args.iter().any(|arg| arg == "--deny-path"),
        "the parent's home is denied: {args:?}"
    );

    // Working directory: the parent's own, not the child's home or the project.
    let cwd = record.lines("cwd.txt").await;
    assert_eq!(
        std::fs::canonicalize(&cwd[0]).unwrap(),
        std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap()
    );

    // Environment: derived (depth, PATH), not the parent's whole environment.
    let names = record.lines("env_names.txt").await;
    let has = |wanted: &str| names.iter().any(|name| name.eq_ignore_ascii_case(wanted));
    assert!(has("RUST_BOT_ACP_DEPTH") && has("PATH"), "{names:?}");
    assert!(
        !has("ACP_E2E_UNRELATED"),
        "an unreferenced variable leaked: {names:?}"
    );
    assert_eq!(record.lines("depth.txt").await, vec!["1"]);
}
