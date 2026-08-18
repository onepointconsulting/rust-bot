use std::io;
use std::path::{Path, PathBuf};

use log::LevelFilter;

/// Crate-wide log target (Rust module path for `rust_bot::*`).
const RUNTIME_LOG_TARGET: &str = "rust_bot";

/// When set, selects the runtime log destination:
/// - a file path: append logs to that file (parent dirs created if needed)
/// - `stdout` or `-`: write logs to standard output
/// - `stderr`: write logs to standard error
///
/// When unset or empty, logs go to standard output (useful for Docker / container logs).
const RUST_LOG_FILE_ENV: &str = "RUST_LOG_FILE";

/// Where runtime logs are written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LogDestination {
    Stdout,
    Stderr,
    File(PathBuf),
}

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

/// Open (or create) a log file, creating parent directories when needed.
fn open_log_file(path: &Path) -> io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

fn parse_log_destination(value: &str) -> LogDestination {
    match value.trim() {
        "" => LogDestination::Stdout,
        "-" | "stdout" | "STDOUT" => LogDestination::Stdout,
        "stderr" | "STDERR" => LogDestination::Stderr,
        path => LogDestination::File(PathBuf::from(path)),
    }
}

fn rust_log_destination() -> LogDestination {
    match std::env::var_os(RUST_LOG_FILE_ENV) {
        Some(path) => parse_log_destination(&path.to_string_lossy()),
        None => LogDestination::Stdout,
    }
}

/// Initialize CLI runtime logging.
///
/// Mirrors Python nanobot's `logger.enable("nanobot")` / `logger.disable("nanobot")`:
/// `--logs` toggles only this crate's logs by default, not third-party crates.
///
/// When `RUST_LOG` is set and logging is enabled, its filters apply (e.g.
/// `RUST_LOG=html5ever=warn` for debugging a dependency). With `--no-logs` and no
/// file destination via `RUST_LOG_FILE`, all logging is suppressed, including
/// third-party crates, even if `RUST_LOG` is set in the environment or `.env`.
///
/// Log destination is controlled by `RUST_LOG_FILE`:
/// - unset / empty / `stdout` / `-`: write to standard output (Docker-friendly default)
/// - `stderr`: write to standard error
/// - any other value: treat as a file path; logging is enabled and output is
///   appended to that file (parent directories are created if missing). This works
///   even without `--logs`, so agent chat stays clean while still writing a log file.
pub fn init_runtime_logging(logs: bool, debug: Option<bool>) {
    let destination = rust_log_destination();
    // A file destination implies logging is wanted even when `--logs` is off (CLI chat).
    let logs = logs || matches!(destination, LogDestination::File(_));
    let has_rust_log = std::env::var_os("RUST_LOG").is_some();
    let mut builder = if logs && has_rust_log {
        env_logger::Builder::from_default_env()
    } else {
        let mut builder = env_logger::Builder::new();
        builder.filter_level(LevelFilter::Off);
        builder
    };

    if logs {
        let has_debug = if let Some(debug) = debug
            && debug
        {
            true
        } else {
            false
        };
        if !has_rust_log || !rust_log_mentions_target(RUNTIME_LOG_TARGET) {
            let level = if has_debug {
                LevelFilter::Debug
            } else {
                LevelFilter::Info
            };
            builder.filter_module(RUNTIME_LOG_TARGET, level);
        } else if has_rust_log && has_debug {
            builder.filter_module(RUNTIME_LOG_TARGET, LevelFilter::Debug);
        }
    } else {
        builder.filter_module(RUNTIME_LOG_TARGET, LevelFilter::Off);
    }

    match &destination {
        LogDestination::Stdout => {
            builder.target(env_logger::Target::Stdout);
        }
        LogDestination::Stderr => {
            builder.target(env_logger::Target::Stderr);
        }
        LogDestination::File(path) => match open_log_file(path) {
            Ok(file) => {
                println!("log file: {}", path.display());
                builder.target(env_logger::Target::Pipe(Box::new(file)));
                builder.write_style(env_logger::WriteStyle::Never);
            }
            Err(err) => {
                eprintln!(
                    "Warning: failed to open {RUST_LOG_FILE_ENV} {}: {err}; falling back to stdout",
                    path.display()
                );
                builder.target(env_logger::Target::Stdout);
            }
        },
    }

    let _ = builder.try_init();
}

pub fn init_logger() {
    env_logger::init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rust_log_mentions_target_detects_crate_and_submodules() {
        let rust_log = "hyper=debug,rust_bot=warn,rust_bot::cli=trace";
        assert!(rust_log_mentions_target_in(rust_log, "rust_bot"));
        assert!(rust_log_mentions_target_in(rust_log, "rust_bot::cli"));
        assert!(rust_log_mentions_target_in(rust_log, "hyper"));
        assert!(!rust_log_mentions_target_in(rust_log, "reqwest"));
    }

    #[test]
    fn parse_log_destination_defaults_and_special_values() {
        assert_eq!(parse_log_destination(""), LogDestination::Stdout);
        assert_eq!(parse_log_destination("  "), LogDestination::Stdout);
        assert_eq!(parse_log_destination("-"), LogDestination::Stdout);
        assert_eq!(parse_log_destination("stdout"), LogDestination::Stdout);
        assert_eq!(parse_log_destination("STDOUT"), LogDestination::Stdout);
        assert_eq!(parse_log_destination("stderr"), LogDestination::Stderr);
        assert_eq!(parse_log_destination("STDERR"), LogDestination::Stderr);
        assert_eq!(
            parse_log_destination("./logs/rust-bot.log"),
            LogDestination::File(PathBuf::from("./logs/rust-bot.log"))
        );
    }

    #[test]
    fn open_log_file_creates_parent_directory() {
        let base = std::env::temp_dir().join(format!("rust-bot-log-test-{}", std::process::id()));
        let log_path = base.join("nested").join("runtime.log");
        let _ = fs::remove_dir_all(&base);

        let mut file = open_log_file(&log_path).expect("open log file");
        use std::io::Write;
        file.write_all(b"test line\n").expect("write log line");
        drop(file);

        let contents = fs::read_to_string(&log_path).expect("read log file");
        assert!(contents.contains("test line"));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn open_log_file_appends_to_existing_file() {
        let log_path =
            std::env::temp_dir().join(format!("rust-bot-log-append-{}.log", std::process::id()));
        let _ = fs::remove_file(&log_path);

        {
            let mut file = open_log_file(&log_path).expect("open log file first time");
            use std::io::Write;
            file.write_all(b"first\n").expect("write first line");
        }
        {
            let mut file = open_log_file(&log_path).expect("open log file second time");
            use std::io::Write;
            file.write_all(b"second\n").expect("write second line");
        }

        let contents = fs::read_to_string(&log_path).expect("read log file");
        assert_eq!(contents, "first\nsecond\n");

        let _ = fs::remove_file(&log_path);
    }
}
