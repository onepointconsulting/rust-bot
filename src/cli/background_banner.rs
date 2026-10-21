//! CLI side of the "Waiting for N background agents to finish" banner.
//!
//! Two places show it:
//! - while a turn runs, on the "thinking" spinner line, where it disappears as
//!   soon as the last background agent ends ([`keep_spinner_banner_updated`]);
//! - at the prompt, where reedline owns the terminal, as append-only notice lines
//!   printed through reedline's `ExternalPrinter` ([`spawn_prompt_notices`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anstyle::Style;
use futures::lock::Mutex;
use reedline::ExternalPrinter;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::agent::background_activity::{ActivitySnapshot, BackgroundActivity};
use crate::cli::stream::StreamRenderer;

/// The session key the CLI's background agents are counted under. The `spawn`
/// tool keys every CLI task as `cli:direct` (its default context), whatever
/// `--session` the CLI runs with.
pub(crate) const CLI_ACTIVITY_SESSION_KEY: &str = "cli:direct";

/// Shown at the prompt when the last background agent has ended.
const FINISHED_NOTICE: &str = "✓ Background agents finished";

/// Aborts the wrapped task when dropped, so a task tied to one turn cannot
/// outlive it on any return path.
pub(crate) struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The notice to print at the prompt when the running work goes from
/// `previous` to `current`, or `None` when there is nothing new to say.
pub(crate) fn prompt_notice(
    previous: &ActivitySnapshot,
    current: &ActivitySnapshot,
) -> Option<String> {
    if current == previous {
        return None;
    }
    match current.banner() {
        Some(banner) => Some(format!("⏳ {banner}")),
        None => Some(FINISHED_NOTICE.to_string()),
    }
}

/// `text` as a dim line, like the CLI's other progress lines.
fn dim_line(text: &str) -> String {
    let dim = Style::new().dimmed();
    format!("{}{text}{}", dim.render(), dim.render_reset())
}

/// Print the banner for what runs right now, if anything. For the moment just
/// before the prompt appears, when reedline is not yet drawing and a plain
/// `println!` is safe.
pub(crate) fn print_current_banner(activity: &BackgroundActivity) {
    if let Some(banner) = activity.snapshot(CLI_ACTIVITY_SESSION_KEY).banner() {
        println!("{}", dim_line(&format!("⏳ {banner}")));
    }
}

/// Keep the spinner line of `renderer` showing what runs in the background,
/// until the returned guard is dropped.
pub(crate) fn keep_spinner_banner_updated(
    activity: &Arc<BackgroundActivity>,
    renderer: Arc<Mutex<StreamRenderer>>,
) -> AbortOnDrop {
    // Subscribe before reading the current state, so no change slips in between.
    let mut changes = activity.subscribe();
    let activity = Arc::clone(activity);
    AbortOnDrop(tokio::spawn(async move {
        let mut snapshot = activity.snapshot(CLI_ACTIVITY_SESSION_KEY);
        loop {
            renderer
                .lock()
                .await
                .set_background_banner(snapshot.banner());
            snapshot = loop {
                match changes.recv().await {
                    Ok(change) if change.session_key == CLI_ACTIVITY_SESSION_KEY => {
                        break change.snapshot;
                    }
                    Ok(_) => {}
                    // Missed changes: the current state is all that matters.
                    Err(RecvError::Lagged(_)) => {
                        break activity.snapshot(CLI_ACTIVITY_SESSION_KEY);
                    }
                    Err(RecvError::Closed) => return,
                }
            };
        }
    }))
}

/// Print a notice through `printer` whenever the running background work
/// changes while the prompt is up (`prompt_active`), until the returned guard is
/// dropped. While a turn runs the spinner line shows the state instead, so
/// nothing is queued for later.
pub(crate) fn spawn_prompt_notices(
    activity: &Arc<BackgroundActivity>,
    printer: ExternalPrinter<String>,
    prompt_active: Arc<AtomicBool>,
) -> AbortOnDrop {
    let mut changes = activity.subscribe();
    let activity = Arc::clone(activity);
    AbortOnDrop(tokio::spawn(async move {
        let mut previous = activity.snapshot(CLI_ACTIVITY_SESSION_KEY);
        loop {
            let current = match changes.recv().await {
                Ok(change) if change.session_key == CLI_ACTIVITY_SESSION_KEY => change.snapshot,
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) => activity.snapshot(CLI_ACTIVITY_SESSION_KEY),
                Err(RecvError::Closed) => return,
            };
            if prompt_active.load(Ordering::Relaxed)
                && let Some(notice) = prompt_notice(&previous, &current)
            {
                // Never block the listener on a full printer queue; a dropped
                // notice only loses one status line.
                let _ = printer.sender().try_send(dim_line(&notice));
            }
            previous = current;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::background_activity::ActivityKind;

    fn spawn_snapshot(spawn_count: usize) -> ActivitySnapshot {
        ActivitySnapshot {
            spawn_count,
            ..ActivitySnapshot::default()
        }
    }

    #[test]
    fn prompt_notice_announces_the_first_agent() {
        assert_eq!(
            prompt_notice(&spawn_snapshot(0), &spawn_snapshot(1)).as_deref(),
            Some("⏳ Waiting for 1 background agent to finish")
        );
    }

    #[test]
    fn prompt_notice_announces_a_changed_count() {
        assert_eq!(
            prompt_notice(&spawn_snapshot(2), &spawn_snapshot(1)).as_deref(),
            Some("⏳ Waiting for 1 background agent to finish")
        );
    }

    #[test]
    fn prompt_notice_reports_when_everything_has_finished() {
        assert_eq!(
            prompt_notice(&spawn_snapshot(1), &spawn_snapshot(0)).as_deref(),
            Some(FINISHED_NOTICE)
        );
    }

    #[test]
    fn prompt_notice_is_silent_without_a_change() {
        assert_eq!(prompt_notice(&spawn_snapshot(2), &spawn_snapshot(2)), None);
        assert_eq!(prompt_notice(&spawn_snapshot(0), &spawn_snapshot(0)), None);
    }

    /// Wait until `condition` holds, failing the test after a timeout.
    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..200 {
            if condition() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("condition not reached in time");
    }

    #[tokio::test]
    async fn spinner_banner_follows_the_cli_sessions_agents_only() {
        let activity = BackgroundActivity::new();
        let renderer = Arc::new(Mutex::new(StreamRenderer::new(false, false)));
        let _updates = keep_spinner_banner_updated(&activity, Arc::clone(&renderer));

        let _other_session = activity.begin("websocket:abc", ActivityKind::Spawn);
        let guard = activity.begin(CLI_ACTIVITY_SESSION_KEY, ActivityKind::Spawn);
        let banner_of = |renderer: &Arc<Mutex<StreamRenderer>>| {
            renderer
                .try_lock()
                .and_then(|renderer| renderer.background_banner().map(str::to_string))
        };
        wait_until(|| banner_of(&renderer).is_some()).await;
        assert_eq!(
            banner_of(&renderer).as_deref(),
            Some("Waiting for 1 background agent to finish")
        );

        drop(guard);
        wait_until(|| banner_of(&renderer).is_none()).await;
    }

    #[tokio::test]
    async fn spinner_banner_starts_from_what_already_runs() {
        let activity = BackgroundActivity::new();
        let _guard = activity.begin(CLI_ACTIVITY_SESSION_KEY, ActivityKind::Spawn);
        let renderer = Arc::new(Mutex::new(StreamRenderer::new(false, false)));
        let _updates = keep_spinner_banner_updated(&activity, Arc::clone(&renderer));
        wait_until(|| {
            renderer
                .try_lock()
                .is_some_and(|renderer| renderer.background_banner().is_some())
        })
        .await;
    }

    #[tokio::test]
    async fn dropping_the_guard_stops_the_spinner_updates() {
        let activity = BackgroundActivity::new();
        let renderer = Arc::new(Mutex::new(StreamRenderer::new(false, false)));
        let updates = keep_spinner_banner_updated(&activity, Arc::clone(&renderer));
        drop(updates);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _guard = activity.begin(CLI_ACTIVITY_SESSION_KEY, ActivityKind::Spawn);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(renderer.lock().await.background_banner(), None);
    }

    #[tokio::test]
    async fn prompt_notices_are_printed_only_while_the_prompt_is_up() {
        let activity = BackgroundActivity::new();
        let printer = ExternalPrinter::<String>::default();
        let prompt_active = Arc::new(AtomicBool::new(false));
        let listener = spawn_prompt_notices(&activity, printer.clone(), Arc::clone(&prompt_active));

        // A turn is running: the spinner shows this, nothing is queued.
        let first = activity.begin(CLI_ACTIVITY_SESSION_KEY, ActivityKind::Spawn);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(printer.get_line().is_none());

        // The prompt is up: the end of the last agent is announced.
        prompt_active.store(true, Ordering::Relaxed);
        drop(first);
        wait_until(|| {
            printer
                .receiver()
                .try_recv()
                .is_ok_and(|line| line.contains(FINISHED_NOTICE))
        })
        .await;
        drop(listener);
    }
}
