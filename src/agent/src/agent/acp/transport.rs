//! A child ACP agent as an operating-system process.
//!
//! Three things the generic launcher in the protocol crate does not do, and a
//! parent rust-bot needs:
//!
//! * **A clean environment.** The child gets exactly the variables it is given,
//!   not the parent's whole environment (plan decision 19).
//! * **Closing its input is the stop signal.** ACP has no shutdown method; EOF on
//!   stdin is how a stdio agent is told to finish. The child then drains
//!   background work, dreams and exits. [`RunningChild`] keeps a handle that can
//!   close stdin on purpose while the connection owns the writer.
//! * **Graceful stop, then kill.** After the stop signal the process gets a grace
//!   period; if it has not exited by then its whole process tree is killed. The
//!   wait happens in the background, so the caller is never kept waiting, and
//!   whatever the caller passed as `hold` (the per-agent lock) is released only
//!   when the process is really gone.
//!
//! Dropping a [`RunningChild`] takes the same path, which is what makes an
//! aborted parent turn clean up its child: nothing is left running unattended.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use agent_client_protocol::ByteStreams;
use async_process::{Child, ChildStdin, ChildStdout};
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::task::JoinHandle;

use crate::utils::process::kill_process_tree_sync;

/// How much of the child's stderr is kept for failure reports.
const STDERR_CAPACITY: usize = 64 * 1024;
/// After a kill, how long to wait for the operating system to report the exit.
const POST_KILL_WAIT: Duration = Duration::from_secs(10);
/// Windows process creation flag: no console window for the child.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How to start a child. `env` is the child's **entire** environment.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: BTreeMap<String, String>,
    /// Working directory; `None` inherits the parent's (plan decision 20).
    pub current_dir: Option<PathBuf>,
}

/// How a child's life ended, for logs and tests.
#[derive(Debug, Clone)]
pub struct ExitReport {
    /// Exit status, when the operating system reported one.
    pub status: Option<ExitStatus>,
    /// The process did not exit within the grace period and was killed.
    pub killed: bool,
    /// The tail of what the child wrote to stderr.
    pub stderr_tail: String,
}

impl ExitReport {
    /// Exited on its own, successfully.
    pub fn exited_cleanly(&self) -> bool {
        !self.killed && self.status.is_some_and(|status| status.success())
    }
}

/// The last bytes of a child's stderr.
#[derive(Debug, Clone, Default)]
pub struct StderrRing(Arc<Mutex<VecDeque<u8>>>);

impl StderrRing {
    fn push(&self, bytes: &[u8]) {
        let mut ring = self.0.lock().unwrap_or_else(|e| e.into_inner());
        ring.extend(bytes);
        let excess = ring.len().saturating_sub(STDERR_CAPACITY);
        if excess > 0 {
            ring.drain(..excess);
        }
    }

    /// What has been captured so far, lossily decoded.
    pub fn tail(&self) -> String {
        let ring = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let bytes: Vec<u8> = ring.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// The child's stdin, shared so that it can be closed from outside the connection.
type SharedStdin = Arc<Mutex<Option<ChildStdin>>>;

/// The writer half handed to the protocol connection.
///
/// Writing after the handle was closed fails with `BrokenPipe`, like a pipe whose
/// reader has gone.
pub struct ClosableStdin(SharedStdin);

impl ClosableStdin {
    fn with_stdin<T>(
        &self,
        action: impl FnOnce(Pin<&mut ChildStdin>) -> Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(stdin) => action(Pin::new(stdin)),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }
}

impl AsyncWrite for ClosableStdin {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.with_stdin(|stdin| stdin.poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.with_stdin(|stdin| stdin.poll_flush(cx))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(stdin) => {
                let result = Pin::new(stdin).poll_close(cx);
                if result.is_ready() {
                    guard.take();
                }
                result
            }
            None => Poll::Ready(Ok(())),
        }
    }
}

/// Anything the caller wants kept alive until the process has exited (the
/// per-agent lock), type-erased.
pub type HeldUntilExit = Box<dyn Send + 'static>;

/// A started child process.
///
/// Use [`Self::take_transport`] to talk to it, [`Self::shutdown`] to stop it and
/// wait in the background. Dropping it without calling either stops it the same
/// way.
pub struct RunningChild {
    pid: u32,
    child: Option<Child>,
    stdin: SharedStdin,
    stdout: Option<ChildStdout>,
    stderr: StderrRing,
    grace: Duration,
    held: Option<HeldUntilExit>,
}

impl RunningChild {
    /// Start the process. `grace` is how long it gets to exit after
    /// [`Self::shutdown`] before its tree is killed; `held` is dropped once it is gone.
    pub fn spawn(spec: &ChildSpec, grace: Duration, held: HeldUntilExit) -> io::Result<Self> {
        let mut command = std::process::Command::new(&spec.program);
        command.args(&spec.args);
        command.env_clear();
        command.envs(&spec.env);
        if let Some(dir) = &spec.current_dir {
            command.current_dir(dir);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // Its own process group, so killing it takes its whole tree.
            command.process_group(0);
        }
        let mut command = async_process::Command::from(command);
        #[cfg(windows)]
        {
            use async_process::windows::CommandExt as _;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn()?;
        let pid = child.id();
        let stdin = child.stdin.take().ok_or_else(missing_pipe)?;
        let stdout = child.stdout.take().ok_or_else(missing_pipe)?;
        let stderr_pipe = child.stderr.take().ok_or_else(missing_pipe)?;

        let stderr = StderrRing::default();
        spawn_stderr_reader(stderr_pipe, stderr.clone());

        Ok(Self {
            pid,
            child: Some(child),
            stdin: Arc::new(Mutex::new(Some(stdin))),
            stdout: Some(stdout),
            stderr,
            grace,
            held: Some(held),
        })
    }

    /// The operating-system process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The tail of what the child wrote to stderr so far.
    pub fn stderr_tail(&self) -> String {
        self.stderr.tail()
    }

    /// The protocol transport over the child's stdin and stdout. Only once.
    pub fn take_transport(&mut self) -> Option<ByteStreams<ClosableStdin, ChildStdout>> {
        let stdout = self.stdout.take()?;
        Some(ByteStreams::new(
            ClosableStdin(Arc::clone(&self.stdin)),
            stdout,
        ))
    }

    /// Close the child's stdin: its signal to finish up and exit.
    pub fn close_stdin(&self) {
        close_shared_stdin(&self.stdin);
    }

    /// Kill the whole process tree now, without the grace period.
    pub fn kill_now(&self) {
        kill_process_tree_sync(self.pid);
    }

    /// Signal the child to stop and wait for it in the background: the grace
    /// period, then a kill of its tree. Returns at once; the handle yields how it
    /// ended. Whatever was `held` is released when the process is gone.
    pub fn shutdown(mut self) -> JoinHandle<ExitReport> {
        self.start_shutdown()
            .unwrap_or_else(|| tokio::spawn(async { unreachable_report() }))
    }

    /// Begin the background stop; `None` when it already began.
    fn start_shutdown(&mut self) -> Option<JoinHandle<ExitReport>> {
        let child = self.child.take()?;
        close_shared_stdin(&self.stdin);
        let job = Reap {
            child,
            pid: self.pid,
            grace: self.grace,
            stderr: self.stderr.clone(),
            held: self.held.take(),
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => Some(runtime.spawn(job.run())),
            Err(_) => {
                // No runtime to wait on (the owner is being torn down outside
                // one): there is nobody to wait for the exit, so end it now.
                kill_process_tree_sync(job.pid);
                drop(job);
                None
            }
        }
    }
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        // Never leave a child running unattended (an aborted parent turn lands here).
        let _ = self.start_shutdown();
    }
}

/// The job that waits for a stopping child.
struct Reap {
    child: Child,
    pid: u32,
    grace: Duration,
    stderr: StderrRing,
    held: Option<HeldUntilExit>,
}

impl Reap {
    async fn run(mut self) -> ExitReport {
        let mut killed = false;
        let status = match tokio::time::timeout(self.grace, self.child.status()).await {
            Ok(status) => status.ok(),
            Err(_elapsed) => {
                killed = true;
                log::warn!(
                    "ACP child {} did not exit within {:?}; killing its process tree. stderr: {}",
                    self.pid,
                    self.grace,
                    self.stderr.tail()
                );
                let pid = self.pid;
                let _ = tokio::task::spawn_blocking(move || kill_process_tree_sync(pid)).await;
                tokio::time::timeout(POST_KILL_WAIT, self.child.status())
                    .await
                    .ok()
                    .and_then(Result::ok)
            }
        };
        let report = ExitReport {
            status,
            killed,
            stderr_tail: self.stderr.tail(),
        };
        // Only now may the next run for this agent start.
        drop(self.held.take());
        report
    }
}

fn unreachable_report() -> ExitReport {
    ExitReport {
        status: None,
        killed: false,
        stderr_tail: String::new(),
    }
}

fn missing_pipe() -> io::Error {
    io::Error::other("the child process has no stdio pipe")
}

fn close_shared_stdin(stdin: &SharedStdin) {
    stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// Keep reading the child's stderr so it never blocks on a full pipe, and keep
/// the last bytes for failure reports.
fn spawn_stderr_reader(mut pipe: impl AsyncRead + Unpin + Send + 'static, ring: StderrRing) {
    let read = async move {
        let mut buffer = [0u8; 8 * 1024];
        loop {
            match pipe.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => ring.push(&buffer[..read]),
            }
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn(read);
        }
        Err(_) => {
            std::thread::spawn(move || futures::executor::block_on(read));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A command that reads stdin until EOF and then exits 0.
    fn exit_on_eof() -> ChildSpec {
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from("powershell"),
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-Command"),
                OsString::from("[Console]::In.ReadToEnd() | Out-Null; exit 0"),
            ],
        );
        #[cfg(unix)]
        let (program, args) = (
            PathBuf::from("sh"),
            vec![OsString::from("-c"), OsString::from("cat > /dev/null")],
        );
        ChildSpec {
            program,
            args,
            env: system_env(),
            current_dir: None,
        }
    }

    /// A command that ignores stdin and runs for a long time.
    fn ignores_eof() -> ChildSpec {
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from("ping"),
            vec![
                OsString::from("-n"),
                OsString::from("60"),
                OsString::from("127.0.0.1"),
            ],
        );
        #[cfg(unix)]
        let (program, args) = (PathBuf::from("sleep"), vec![OsString::from("60")]);
        ChildSpec {
            program,
            args,
            env: system_env(),
            current_dir: None,
        }
    }

    /// The little a child needs to find its program: the parent's PATH and the
    /// Windows system root (PowerShell does not start without it).
    fn system_env() -> BTreeMap<String, String> {
        [
            "PATH",
            "Path",
            "SystemRoot",
            "SYSTEMROOT",
            "COMSPEC",
            "PATHEXT",
        ]
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_string(), value))
        })
        .collect()
    }

    /// Sets a flag when dropped, to observe when `held` is released.
    struct Flag(Arc<AtomicBool>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn held_flag() -> (HeldUntilExit, Arc<AtomicBool>) {
        let released = Arc::new(AtomicBool::new(false));
        (Box::new(Flag(Arc::clone(&released))), released)
    }

    #[tokio::test]
    async fn closing_stdin_lets_a_well_behaved_child_exit_on_its_own() {
        let (held, released) = held_flag();
        let child = RunningChild::spawn(&exit_on_eof(), Duration::from_secs(30), held).unwrap();
        assert!(child.pid() > 0);

        let report = child.shutdown().await.unwrap();

        assert!(report.exited_cleanly(), "{report:?}");
        assert!(!report.killed);
        assert!(
            released.load(Ordering::SeqCst),
            "held is released after the exit"
        );
    }

    #[tokio::test]
    async fn a_child_that_ignores_eof_is_killed_after_the_grace_period() {
        let (held, released) = held_flag();
        let child = RunningChild::spawn(&ignores_eof(), Duration::from_millis(500), held).unwrap();
        let pid = child.pid();

        let started = std::time::Instant::now();
        let report = child.shutdown().await.unwrap();

        assert!(report.killed, "{report:?}");
        assert!(!report.exited_cleanly());
        assert!(
            started.elapsed() >= Duration::from_millis(450),
            "waited the grace period"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "did not wait for the 60s sleep"
        );
        assert!(
            released.load(Ordering::SeqCst),
            "held is released after the kill"
        );
        assert!(!process_is_alive(pid), "no process is left behind");
    }

    #[tokio::test]
    async fn dropping_a_running_child_stops_it_in_the_background() {
        let (held, released) = held_flag();
        let child = RunningChild::spawn(&exit_on_eof(), Duration::from_secs(30), held).unwrap();
        let pid = child.pid();

        drop(child);

        let started = std::time::Instant::now();
        while !released.load(Ordering::SeqCst) {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the child was never reaped"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!process_is_alive(pid));
    }

    #[tokio::test]
    async fn dropping_a_child_that_ignores_eof_still_ends_it() {
        let (held, released) = held_flag();
        let child = RunningChild::spawn(&ignores_eof(), Duration::from_millis(300), held).unwrap();
        let pid = child.pid();

        drop(child);

        let started = std::time::Instant::now();
        while !released.load(Ordering::SeqCst) {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the child was never killed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!process_is_alive(pid));
    }

    #[tokio::test]
    async fn the_child_gets_only_the_environment_it_was_given() {
        // PATH and the system root are passed; a variable of this test process is not.
        // SAFETY: setting a variable no other test reads.
        unsafe { std::env::set_var("ACP_TRANSPORT_TEST_SECRET", "leaked") };
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from("cmd"),
            vec![
                OsString::from("/C"),
                OsString::from("echo SECRET=%ACP_TRANSPORT_TEST_SECRET%"),
            ],
        );
        #[cfg(unix)]
        let (program, args) = (
            PathBuf::from("sh"),
            vec![
                OsString::from("-c"),
                OsString::from("echo SECRET=$ACP_TRANSPORT_TEST_SECRET"),
            ],
        );
        let mut spec = ChildSpec {
            program,
            args,
            env: system_env(),
            current_dir: None,
        };
        spec.env
            .insert("ACP_TRANSPORT_TEST_GIVEN".into(), "yes".into());

        let (held, _released) = held_flag();
        let child = RunningChild::spawn(&spec, Duration::from_secs(30), held).unwrap();
        let stderr_handle = child.stderr.clone();
        let mut child = child;
        let mut stdout = child.stdout.take().unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).await.unwrap();
        child.shutdown().await.unwrap();

        assert!(
            output.contains("SECRET=") && !output.contains("leaked"),
            "{output} / {}",
            stderr_handle.tail()
        );
    }

    #[tokio::test]
    async fn the_working_directory_can_be_set() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from("cmd"),
            vec![OsString::from("/C"), OsString::from("cd")],
        );
        #[cfg(unix)]
        let (program, args) = (PathBuf::from("pwd"), Vec::new());
        let spec = ChildSpec {
            program,
            args,
            env: system_env(),
            current_dir: Some(dir.path().to_path_buf()),
        };
        let (held, _released) = held_flag();
        let mut child = RunningChild::spawn(&spec, Duration::from_secs(30), held).unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).await.unwrap();
        child.shutdown().await.unwrap();

        let reported = std::fs::canonicalize(output.trim()).unwrap();
        assert_eq!(reported, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[tokio::test]
    async fn stderr_is_captured_for_failure_reports() {
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from("cmd"),
            vec![
                OsString::from("/C"),
                OsString::from("echo it went wrong 1>&2 & exit 3"),
            ],
        );
        #[cfg(unix)]
        let (program, args) = (
            PathBuf::from("sh"),
            vec![
                OsString::from("-c"),
                OsString::from("echo it went wrong >&2; exit 3"),
            ],
        );
        let spec = ChildSpec {
            program,
            args,
            env: system_env(),
            current_dir: None,
        };
        let (held, _released) = held_flag();
        let child = RunningChild::spawn(&spec, Duration::from_secs(30), held).unwrap();

        let report = child.shutdown().await.unwrap();

        assert!(!report.exited_cleanly());
        assert_eq!(report.status.and_then(|status| status.code()), Some(3));
        assert!(
            report.stderr_tail.contains("it went wrong"),
            "{:?}",
            report.stderr_tail
        );
    }

    #[test]
    fn the_stderr_ring_keeps_only_the_most_recent_bytes() {
        let ring = StderrRing::default();
        ring.push(&vec![b'a'; STDERR_CAPACITY]);
        ring.push(b"the end");
        let tail = ring.tail();
        assert_eq!(tail.len(), STDERR_CAPACITY);
        assert!(tail.ends_with("the end"));
    }

    #[test]
    fn spawning_a_program_that_does_not_exist_is_an_error() {
        let spec = ChildSpec {
            program: PathBuf::from("definitely-not-a-program-xyz"),
            args: Vec::new(),
            env: BTreeMap::new(),
            current_dir: None,
        };
        let (held, _released) = held_flag();
        assert!(RunningChild::spawn(&spec, Duration::from_secs(1), held).is_err());
    }

    /// Whether a process with this id still exists.
    fn process_is_alive(pid: u32) -> bool {
        #[cfg(windows)]
        {
            let output = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output()
                .expect("run tasklist");
            String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
        }
        #[cfg(unix)]
        {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        }
    }
}
