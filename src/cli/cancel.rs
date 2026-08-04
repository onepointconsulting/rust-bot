use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal;

/// How often the background poll loop wakes up to check the stop flag.
///
/// Bounds how long the detached blocking thread can outlive a cancelled
/// [`wait_for_escape_cancel`] future (e.g. when `tokio::select!` picks the
/// other branch first) before it notices and exits.
const POLL_INTERVAL: Duration = Duration::from_millis(75);

/// Enables raw mode for the guard's lifetime, restoring cooked mode on drop
/// (cancellation, normal return, and panics all run `Drop`).
///
/// On Windows, `crossterm::terminal::enable_raw_mode` only clears input-handle
/// console modes (line input / echo / processed input); it does not touch the
/// output handle, so concurrent `println!`/`write!` calls elsewhere in the
/// turn are unaffected. On Unix this also disables output post-processing,
/// which is fine here because the only output produced while this guard is
/// live is unrelated CLI turn output — this feature targets the Windows
/// `cmd.exe` "Terminate batch job" problem described in the plan.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Sets the shared stop flag when dropped, so the background poll loop below
/// notices within one [`POLL_INTERVAL`] tick after this future is cancelled.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Resolves when the user presses Esc; otherwise never resolves.
///
/// Intended to be raced with real work via `tokio::select!`:
///
/// ```ignore
/// tokio::select! {
///     response = do_work() => { /* normal path */ }
///     _ = wait_for_escape_cancel() => { /* user cancelled */ }
/// }
/// ```
///
/// When `tokio::select!` picks the other branch, this future is dropped
/// mid-flight; the stop flag and `RawModeGuard` ensure the background thread
/// exits and raw mode is restored shortly after, instead of leaking.
///
/// If stdin/stdout isn't a TTY (e.g. piped input, non-interactive `--message`
/// runs) or raw mode can't be enabled, this never resolves, so it simply
/// loses the race and never falsely cancels the other branch.
pub async fn wait_for_escape_cancel() {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        std::future::pending::<()>().await;
        return;
    }

    let Ok(_raw_guard) = RawModeGuard::enable() else {
        std::future::pending::<()>().await;
        return;
    };

    let stop = Arc::new(AtomicBool::new(false));
    let _stop_guard = StopOnDrop(Arc::clone(&stop));

    let poll_stop = Arc::clone(&stop);
    let handle = tokio::task::spawn_blocking(move || loop {
        if poll_stop.load(Ordering::Relaxed) {
            return false;
        }
        match event::poll(POLL_INTERVAL) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key))
                    if key.code == KeyCode::Esc && key.kind == KeyEventKind::Press =>
                {
                    return true;
                }
                Ok(_) => continue,
                Err(_) => return false,
            },
            Ok(false) => continue,
            Err(_) => return false,
        }
    });

    match handle.await {
        Ok(true) => {}
        _ => std::future::pending::<()>().await,
    }
}
