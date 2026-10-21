//! Task-local progress channel for tools.
//!
//! A tool's `execute` takes only its parameters and returns one string at the
//! end, so a tool that blocks for minutes (`acp_run_agent` waiting for a child)
//! could show the operator nothing. The agent loop binds the turn's progress
//! callback here around the whole run; a tool calls [`report_progress`] and the
//! line reaches whatever shows progress (the CLI and the web UI both render
//! [`ProgressKind::ToolHint`]).
//!
//! Same shape as `workspace_context` and `cron_context`. With nothing bound
//! (a tool run outside a turn, or a turn without a progress callback) every call
//! is a no-op, so existing tools are unaffected and others can adopt it later.

use std::future::Future;

use crate::agent::agent_loop::ProgressCallback;
use crate::bus::outbound_events::ProgressKind;

tokio::task_local! {
    static TOOL_PROGRESS: Option<ProgressCallback>;
}

/// Run `future` with `callback` as the turn's tool-progress channel.
pub async fn with_tool_progress<F: Future>(
    callback: Option<ProgressCallback>,
    future: F,
) -> F::Output {
    TOOL_PROGRESS.scope(callback, future).await
}

/// The callback bound for the current turn, if any.
///
/// For tools that report from another task: take the callback here, while still
/// inside the turn, and move it into the task.
pub fn current_tool_progress() -> Option<ProgressCallback> {
    TOOL_PROGRESS.try_with(Clone::clone).ok().flatten()
}

/// Show `text` to the operator as a tool-progress line; a no-op outside a turn.
pub async fn report_progress(text: impl Into<String>) {
    if let Some(callback) = current_tool_progress() {
        callback(text.into(), ProgressKind::ToolHint).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type SeenProgress = Arc<Mutex<Vec<(String, ProgressKind)>>>;

    fn recording_callback() -> (ProgressCallback, SeenProgress) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let callback: ProgressCallback = Arc::new(move |text, kind| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                sink.lock().unwrap().push((text, kind));
            })
        });
        (callback, seen)
    }

    #[tokio::test]
    async fn a_reported_line_reaches_the_bound_callback_as_a_tool_hint() {
        let (callback, seen) = recording_callback();

        with_tool_progress(Some(callback), async {
            report_progress("code-review › read src/main.rs").await;
            report_progress("code-review › still working (30s)").await;
        })
        .await;

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, "code-review › read src/main.rs");
        assert_eq!(seen[0].1, ProgressKind::ToolHint);
    }

    #[tokio::test]
    async fn reporting_with_nothing_bound_does_nothing() {
        // Outside any scope ...
        report_progress("lost").await;
        assert!(current_tool_progress().is_none());
        // ... and inside a scope that carries no callback.
        with_tool_progress(None, async {
            report_progress("also lost").await;
            assert!(current_tool_progress().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn the_callback_can_be_taken_into_another_task() {
        let (callback, seen) = recording_callback();

        with_tool_progress(Some(callback), async {
            let taken = current_tool_progress().expect("a callback is bound");
            tokio::spawn(async move {
                taken("from a spawned task".to_string(), ProgressKind::ToolHint).await;
            })
            .await
            .unwrap();
        })
        .await;

        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn parallel_futures_in_one_task_all_see_the_callback() {
        let (callback, seen) = recording_callback();

        with_tool_progress(Some(callback), async {
            futures::future::join_all((0..3).map(|n| async move {
                report_progress(format!("tool {n}")).await;
            }))
            .await;
        })
        .await;

        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn an_inner_scope_replaces_the_callback_and_restores_it() {
        let (outer, outer_seen) = recording_callback();
        let (inner, inner_seen) = recording_callback();

        with_tool_progress(Some(outer), async {
            with_tool_progress(Some(inner), async {
                report_progress("inner").await;
            })
            .await;
            report_progress("outer").await;
        })
        .await;

        assert_eq!(inner_seen.lock().unwrap()[0].0, "inner");
        assert_eq!(outer_seen.lock().unwrap()[0].0, "outer");
    }
}
