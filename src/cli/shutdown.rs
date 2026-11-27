//! Process shutdown signal handling shared by the gateway and the REST API.

use std::time::Duration;

/// Convert the configured `gateway.shutdownGraceSecs` into a [`Duration`].
pub fn grace_duration(grace_secs: u64) -> Duration {
    Duration::from_secs(grace_secs)
}

/// Resolve once the first signal in `signals` fires, returning its name.
///
/// Pure helper over named futures so the "first one wins" logic is testable
/// without raising real OS signals. Never resolves for an empty list.
pub async fn first_signal<F>(signals: Vec<(&'static str, F)>) -> &'static str
where
    F: std::future::Future<Output = ()> + Unpin,
{
    if signals.is_empty() {
        return std::future::pending().await;
    }
    let (name, _, _) = futures::future::select_all(signals.into_iter().map(|(name, fut)| {
        Box::pin(async move {
            fut.await;
            name
        })
    }))
    .await;
    name
}

/// Wait for a termination request and return the signal's name.
///
/// Unix: SIGINT or SIGTERM (docker stop, systemd, k8s).
/// Windows: Ctrl-C, Ctrl-Break, console close or system shutdown.
/// If a handler cannot be installed, falls back to Ctrl-C only.
pub async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(mut sigint), Ok(mut sigterm)) => {
                return first_signal(vec![
                    (
                        "SIGINT",
                        Box::pin(async move {
                            sigint.recv().await;
                        })
                            as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
                    ),
                    (
                        "SIGTERM",
                        Box::pin(async move {
                            sigterm.recv().await;
                        }),
                    ),
                ])
                .await;
            }
            (Err(err), _) | (_, Err(err)) => {
                log::warn!("Could not install signal handlers ({err}); falling back to Ctrl-C");
            }
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        match (
            windows::ctrl_c(),
            windows::ctrl_break(),
            windows::ctrl_close(),
            windows::ctrl_shutdown(),
        ) {
            (Ok(mut c), Ok(mut b), Ok(mut close), Ok(mut shutdown)) => {
                return first_signal(vec![
                    (
                        "Ctrl-C",
                        Box::pin(async move {
                            c.recv().await;
                        })
                            as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
                    ),
                    (
                        "Ctrl-Break",
                        Box::pin(async move {
                            b.recv().await;
                        }),
                    ),
                    (
                        "console close",
                        Box::pin(async move {
                            close.recv().await;
                        }),
                    ),
                    (
                        "system shutdown",
                        Box::pin(async move {
                            shutdown.recv().await;
                        }),
                    ),
                ])
                .await;
            }
            _ => log::warn!("Could not install signal handlers; falling back to Ctrl-C"),
        }
    }
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl-C"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;

    type BoxedSignal = Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

    #[tokio::test]
    async fn first_signal_returns_the_one_that_fires() {
        let never: BoxedSignal = Box::pin(std::future::pending());
        let ready: BoxedSignal = Box::pin(async {});
        let name = first_signal(vec![("never", never), ("ready", ready)]).await;
        assert_eq!(name, "ready");
    }

    #[tokio::test]
    async fn first_signal_with_no_signals_never_resolves() {
        let result = tokio::time::timeout(
            Duration::from_millis(50),
            first_signal::<BoxedSignal>(vec![]),
        )
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn grace_duration_converts_seconds() {
        assert_eq!(grace_duration(0), Duration::ZERO);
        assert_eq!(grace_duration(8), Duration::from_secs(8));
    }
}
