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
/// turn are unaffected.
///
/// On Unix, `cfmakeraw` also clears `OPOST`, so `\n` is no longer mapped to
/// `\r\n`. Tool-hint / spinner lines then start at the previous line's end
/// column (the staircase seen on Linux and WSL). Input stays raw so Esc is
/// still readable; output post-processing is restored immediately after.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> std::io::Result<Self> {
        #[cfg(unix)]
        let saved_oflag = unix_output_flags();
        terminal::enable_raw_mode()?;
        #[cfg(unix)]
        if let Some(oflag) = saved_oflag {
            restore_unix_output_flags(oflag);
        }
        Ok(Self)
    }
}

/// Snapshot `c_oflag` from the interactive tty (stdin/stdout share it).
#[cfg(unix)]
fn unix_output_flags() -> Option<libc::tcflag_t> {
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    let rc = unsafe { libc::tcgetattr(libc::STDOUT_FILENO, &mut termios) };
    (rc == 0).then_some(termios.c_oflag)
}

/// Re-apply the pre-raw `c_oflag` so `println!` still returns to column 0.
#[cfg(unix)]
fn restore_unix_output_flags(oflag: libc::tcflag_t) {
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(libc::STDOUT_FILENO, &mut termios) } != 0 {
        return;
    }
    termios.c_oflag = oflag;
    let _ = unsafe { libc::tcsetattr(libc::STDOUT_FILENO, libc::TCSANOW, &termios) };
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn restoring_saved_output_flags_is_idempotent() {
        let Some(oflag) = super::unix_output_flags() else {
            return;
        };
        super::restore_unix_output_flags(oflag);
        assert_eq!(super::unix_output_flags(), Some(oflag));
    }
}
