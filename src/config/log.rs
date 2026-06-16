use log::LevelFilter;

/// Crate-wide log target (Rust module path for `rust_bot::*`).
const RUNTIME_LOG_TARGET: &str = "rust_bot";

fn rust_log_mentions_target_in(rust_log: &str, target: &str) -> bool {
    rust_log.split(',').any(|part| {
        let name = part.split('=').next().unwrap_or(part).trim();
        name == target || name.starts_with(&format!("{target}::"))
    })
}

fn rust_log_mentions_target(target: &str) -> bool {
    std::env::var("RUST_LOG")
        .map(|rust_log| rust_log_mentions_target_in(&rust_log, target))
        .unwrap_or(false)
}

/// Initialize CLI runtime logging.
///
/// Mirrors Python nanobot's `logger.enable("nanobot")` / `logger.disable("nanobot")`:
/// `--logs` toggles only this crate's logs by default, not third-party crates.
///
/// When `RUST_LOG` is set and `--logs` is on, its filters apply (e.g. `RUST_LOG=html5ever=warn`
/// for debugging a dependency). With `--no-logs`, all logging is suppressed, including
/// third-party crates, even if `RUST_LOG` is set in the environment or `.env`.
pub fn init_runtime_logging(logs: bool) {
    let has_rust_log = std::env::var_os("RUST_LOG").is_some();
    let mut builder = if logs && has_rust_log {
        env_logger::Builder::from_default_env()
    } else {
        let mut builder = env_logger::Builder::new();
        builder.filter_level(LevelFilter::Off);
        builder
    };

    if logs {
        if !has_rust_log || !rust_log_mentions_target(RUNTIME_LOG_TARGET) {
            builder.filter_module(RUNTIME_LOG_TARGET, LevelFilter::Info);
        }
    } else {
        builder.filter_module(RUNTIME_LOG_TARGET, LevelFilter::Off);
    }

    let _ = builder.try_init();
}

pub fn init_logger() {
    env_logger::init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_log_mentions_target_detects_crate_and_submodules() {
        let rust_log = "hyper=debug,rust_bot=warn,rust_bot::cli=trace";
        assert!(rust_log_mentions_target_in(rust_log, "rust_bot"));
        assert!(rust_log_mentions_target_in(rust_log, "rust_bot::cli"));
        assert!(rust_log_mentions_target_in(rust_log, "hyper"));
        assert!(!rust_log_mentions_target_in(rust_log, "reqwest"));
    }
}
