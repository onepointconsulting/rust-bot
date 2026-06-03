//! Task-local cron execution flag (Python `ContextVar` for `cron_in_context`).

use std::cell::RefCell;
use std::future::Future;

tokio::task_local! {
    static CRON_CONTEXT_STACK: RefCell<Vec<bool>>;
}

/// Token returned by [`set_cron_context`](crate::agent::tools::cron::CronTool::set_cron_context);
/// pass to [`reset_cron_context`](crate::agent::tools::cron::CronTool::reset_cron_context).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CronContextToken;

/// Run a future with an initialized cron context stack (call at agent / `on_job` entry).
pub async fn with_cron_context_stack<F, Fut>(f: F) -> Fut::Output
where
    F: FnOnce() -> Fut,
    Fut: Future,
{
    CRON_CONTEXT_STACK
        .scope(RefCell::new(Vec::new()), f())
        .await
}

/// Whether the current task is inside a cron job callback (`ContextVar.get()`, default `false`).
pub fn in_cron_context() -> bool {
    CRON_CONTEXT_STACK
        .try_with(|stack| *stack.borrow().last().unwrap_or(&false))
        .unwrap_or(false)
}

/// Mark cron context active for this task (Python `ContextVar.set`).
pub fn set_cron_context(active: bool) -> CronContextToken {
    CRON_CONTEXT_STACK.with(|stack| {
        stack.borrow_mut().push(active);
    });
    CronContextToken
}

/// Restore previous cron context (Python `ContextVar.reset`).
pub fn reset_cron_context(_token: CronContextToken) {
    let _ = CRON_CONTEXT_STACK.try_with(|stack| {
        stack.borrow_mut().pop();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_is_false() {
        with_cron_context_stack(|| async {
            assert!(!in_cron_context());
        })
        .await;
    }

    #[tokio::test]
    async fn set_and_reset_restores_previous() {
        with_cron_context_stack(|| async {
            let t1 = set_cron_context(true);
            assert!(in_cron_context());

            let t2 = set_cron_context(false);
            assert!(!in_cron_context());

            reset_cron_context(t2);
            assert!(in_cron_context());

            reset_cron_context(t1);
            assert!(!in_cron_context());
        })
        .await;
    }
}
