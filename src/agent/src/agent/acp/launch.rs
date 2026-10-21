//! The command line and environment of a child rust-bot.
//!
//! A child is `rust-bot acp --config <parent config> --overlay <its overlay>
//! --workspace <its home> --lock-wait-secs <n> [--deny-path <folder>]...`, run as
//! an argv array (never through a shell) with an environment derived from its
//! config (plan decisions 19 and 20): the files are named by absolute paths, so
//! the child reads exactly the files the parent means whatever its working
//! directory, and it gets only the variables its config needs.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::acp::transport::ChildSpec;
use crate::config::child_env::{MissingEnvVars, child_environment};
use crate::config::schema::{AcpConfig, Config};

/// Name of the built-in launch preset: this executable.
pub const RUSTBOT_PRESET: &str = "rustbot";
/// The program name that means "this executable" in a preset's command.
const SELF_PROGRAM: &str = "self";

/// Why a child could not be launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// A file or folder the child needs does not exist.
    MissingPath { what: &'static str, path: PathBuf },
    /// The config references environment variables the parent does not have.
    MissingEnv(MissingEnvVars),
    /// The preset's command is unusable.
    BadPreset(String),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::MissingPath { what, path } => {
                write!(f, "{what} not found: {}", path.display())
            }
            LaunchError::MissingEnv(missing) => write!(f, "{missing}"),
            LaunchError::BadPreset(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Program and leading arguments of the `rustbot` preset.
///
/// Without a configured `rustbot` preset it is this executable with `acp`. A
/// configured one may name another program (or `self`) and other leading
/// arguments. `executable_override` replaces `self` (used by tests and by
/// embedders that know better than `current_exe`).
pub fn rustbot_command(
    acp: &AcpConfig,
    executable_override: Option<&Path>,
) -> Result<(PathBuf, Vec<String>), LaunchError> {
    let this_executable = || -> Result<PathBuf, LaunchError> {
        match executable_override {
            Some(path) => Ok(path.to_path_buf()),
            None => std::env::current_exe()
                .map_err(|e| LaunchError::BadPreset(format!("cannot find this executable: {e}"))),
        }
    };
    match acp.launch_presets.get(RUSTBOT_PRESET) {
        None => Ok((this_executable()?, vec!["acp".to_string()])),
        Some(preset) => {
            let (program, rest) = preset.command.split_first().ok_or_else(|| {
                LaunchError::BadPreset(format!(
                    "launch preset '{RUSTBOT_PRESET}' has an empty command"
                ))
            })?;
            let program = if program == SELF_PROGRAM {
                this_executable()?
            } else {
                PathBuf::from(program)
            };
            Ok((program, rest.to_vec()))
        }
    }
}

/// Everything that goes into a child's command line and environment.
pub struct RustBotLaunch<'a> {
    pub program: PathBuf,
    /// Arguments before the rust-bot flags (`acp`).
    pub leading_args: Vec<String>,
    pub parent_config: &'a Path,
    /// The overlays the child runs on, outermost first: those of the children
    /// above this process, then the child's own. Every level's restrictions
    /// reach the child through this chain.
    pub overlays: &'a [PathBuf],
    pub home: &'a Path,
    pub denied_roots: &'a [PathBuf],
    /// How long the child may wait for its workspace lock.
    pub lock_wait: Duration,
    /// The child's depth in the chain (its parent's plus one).
    pub depth: u32,
    /// The child's effective config **before** `${VAR}` expansion.
    pub child_config: &'a Config,
    /// The parent's environment, a snapshot.
    pub parent_env: &'a HashMap<String, String>,
}

/// An existing path made absolute (no symlink resolution, no `\\?\` prefix).
fn absolute_existing(
    path: &Path,
    what: &'static str,
    must_be_dir: bool,
) -> Result<PathBuf, LaunchError> {
    let absolute = std::path::absolute(path).map_err(|_| LaunchError::MissingPath {
        what,
        path: path.to_path_buf(),
    })?;
    let exists = if must_be_dir {
        absolute.is_dir()
    } else {
        absolute.is_file()
    };
    if exists {
        Ok(absolute)
    } else {
        Err(LaunchError::MissingPath {
            what,
            path: absolute,
        })
    }
}

/// Build the process description of a child rust-bot.
pub fn build_rustbot_spec(launch: &RustBotLaunch<'_>) -> Result<ChildSpec, LaunchError> {
    let config = absolute_existing(launch.parent_config, "parent config file", false)?;
    let overlays = launch
        .overlays
        .iter()
        .map(|overlay| absolute_existing(overlay, "overlay file", false))
        .collect::<Result<Vec<_>, _>>()?;
    let home = absolute_existing(launch.home, "agent home folder", true)?;

    let env: BTreeMap<String, String> =
        child_environment(launch.child_config, launch.depth, launch.parent_env)
            .map_err(LaunchError::MissingEnv)?
            .into_iter()
            .collect();

    let mut args: Vec<OsString> = launch.leading_args.iter().map(OsString::from).collect();
    args.extend([OsString::from("--config"), config.into_os_string()]);
    for overlay in overlays {
        args.extend([OsString::from("--overlay"), overlay.into_os_string()]);
    }
    args.extend([
        OsString::from("--workspace"),
        home.into_os_string(),
        OsString::from("--lock-wait-secs"),
        OsString::from(launch.lock_wait.as_secs().to_string()),
    ]);
    for denied in launch.denied_roots {
        args.push(OsString::from("--deny-path"));
        args.push(denied.clone().into_os_string());
    }

    Ok(ChildSpec {
        program: launch.program.clone(),
        args,
        env,
        // The parent's own process working directory (decision 20).
        current_dir: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::child_env::DEPTH_ENV_VAR;
    use crate::config::schema::AcpLaunchPreset;
    use serde_json::json;

    fn files() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        let overlay = dir.path().join("overlay.json");
        let home = dir.path().join("home");
        std::fs::write(&config, "{}").unwrap();
        std::fs::write(&overlay, "{}").unwrap();
        std::fs::create_dir(&home).unwrap();
        (dir, config, overlay, home)
    }

    fn launch<'a>(
        config: &'a Path,
        overlay: &'a PathBuf,
        home: &'a Path,
        child_config: &'a Config,
        env: &'a HashMap<String, String>,
        denied: &'a [PathBuf],
    ) -> RustBotLaunch<'a> {
        RustBotLaunch {
            program: PathBuf::from("rust-bot"),
            leading_args: vec!["acp".to_string()],
            parent_config: config,
            overlays: std::slice::from_ref(overlay),
            home,
            denied_roots: denied,
            lock_wait: Duration::from_secs(240),
            depth: 1,
            child_config,
            parent_env: env,
        }
    }

    fn args_of(spec: &ChildSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_command_line_names_absolute_files_and_the_lock_wait() {
        let (_dir, config, overlay, home) = files();
        let child_config = Config::default();
        let env = HashMap::new();
        let denied = vec![PathBuf::from("/parent/home")];

        let spec = build_rustbot_spec(&launch(
            &config,
            &overlay,
            &home,
            &child_config,
            &env,
            &denied,
        ))
        .unwrap();
        let args = args_of(&spec);

        assert_eq!(args[0], "acp");
        let value_after = |flag: &str| {
            let index = args.iter().position(|arg| arg == flag).unwrap();
            PathBuf::from(&args[index + 1])
        };
        for (flag, expected) in [
            ("--config", &config),
            ("--overlay", &overlay),
            ("--workspace", &home),
        ] {
            let value = value_after(flag);
            assert!(value.is_absolute(), "{flag} {value:?}");
            assert_eq!(&value, expected);
        }
        assert_eq!(
            args[args.iter().position(|a| a == "--lock-wait-secs").unwrap() + 1],
            "240"
        );
        assert_eq!(value_after("--deny-path"), PathBuf::from("/parent/home"));
        assert!(
            spec.current_dir.is_none(),
            "the child inherits the parent's working directory"
        );
    }

    #[test]
    fn a_chain_of_overlays_is_passed_in_order_outermost_first() {
        let (dir, config, overlay, home) = files();
        let outer = dir.path().join("outer-overlay.json");
        std::fs::write(&outer, "{}").unwrap();
        let child_config = Config::default();
        let env = HashMap::new();
        let chain = vec![outer.clone(), overlay.clone()];

        let spec = build_rustbot_spec(&RustBotLaunch {
            overlays: &chain,
            ..launch(&config, &overlay, &home, &child_config, &env, &[])
        })
        .unwrap();
        let args = args_of(&spec);

        let overlay_values: Vec<PathBuf> = args
            .iter()
            .enumerate()
            .filter(|(_, arg)| *arg == "--overlay")
            .map(|(index, _)| PathBuf::from(&args[index + 1]))
            .collect();
        assert_eq!(overlay_values, vec![outer, overlay]);
    }

    #[test]
    fn a_relative_path_is_made_absolute_against_the_working_directory() {
        // `cargo test` runs in the package root, where Cargo.toml exists.
        let (_dir, _config, overlay, home) = files();
        let child_config = Config::default();
        let env = HashMap::new();
        let relative_config = PathBuf::from("Cargo.toml");

        let spec = build_rustbot_spec(&launch(
            &relative_config,
            &overlay,
            &home,
            &child_config,
            &env,
            &[],
        ))
        .unwrap();
        let args = args_of(&spec);
        let config_arg =
            PathBuf::from(&args[args.iter().position(|a| a == "--config").unwrap() + 1]);

        assert!(config_arg.is_absolute());
        assert!(config_arg.ends_with("Cargo.toml"));
    }

    #[test]
    fn missing_files_are_named_and_nothing_is_built() {
        let (dir, config, overlay, home) = files();
        let child_config = Config::default();
        let env = HashMap::new();
        let gone = dir.path().join("gone.json");

        for (bad_config, bad_overlay, bad_home, what) in [
            (&gone, &overlay, &home, "parent config file"),
            (&config, &gone, &home, "overlay file"),
            (&config, &overlay, &gone, "agent home folder"),
        ] {
            let error = build_rustbot_spec(&launch(
                bad_config,
                bad_overlay,
                bad_home,
                &child_config,
                &env,
                &[],
            ))
            .unwrap_err();
            assert!(
                matches!(&error, LaunchError::MissingPath { what: w, .. } if *w == what),
                "{error}"
            );
            assert!(error.to_string().contains("gone.json"), "{error}");
        }
    }

    #[test]
    fn the_environment_is_derived_and_carries_the_depth() {
        let (_dir, config, overlay, home) = files();
        let mut child_config = Config::default();
        child_config.tools.mcp_servers = serde_json::from_value(json!({
            "docs": {"url": "https://docs.example/mcp", "headers": {"Authorization": "Bearer ${ACP_LAUNCH_TEST_TOKEN}"}}
        }))
        .unwrap();
        let env: HashMap<String, String> = [
            ("ACP_LAUNCH_TEST_TOKEN", "t0ken"),
            ("UNRELATED_SECRET", "must-not-leak"),
            ("PATH", "/bin"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let spec = build_rustbot_spec(&launch(&config, &overlay, &home, &child_config, &env, &[]))
            .unwrap();

        assert_eq!(
            spec.env.get("ACP_LAUNCH_TEST_TOKEN").map(String::as_str),
            Some("t0ken")
        );
        assert_eq!(spec.env.get(DEPTH_ENV_VAR).map(String::as_str), Some("1"));
        assert!(!spec.env.contains_key("UNRELATED_SECRET"));
    }

    #[test]
    fn an_unset_referenced_variable_fails_in_the_parent_naming_it() {
        let (_dir, config, overlay, home) = files();
        let mut child_config = Config::default();
        child_config.tools.mcp_servers = serde_json::from_value(json!({
            "docs": {"url": "https://docs.example/mcp", "headers": {"Authorization": "Bearer ${ACP_LAUNCH_TEST_ABSENT}"}}
        }))
        .unwrap();
        let env = HashMap::new();

        let error = build_rustbot_spec(&launch(&config, &overlay, &home, &child_config, &env, &[]))
            .unwrap_err();

        assert!(matches!(error, LaunchError::MissingEnv(_)));
        assert!(
            error.to_string().contains("ACP_LAUNCH_TEST_ABSENT"),
            "{error}"
        );
    }

    // ── the preset ──────────────────────────────────────────────────────────

    #[test]
    fn without_a_configured_preset_the_child_is_this_executable_running_acp() {
        let acp = AcpConfig::default();
        let (program, args) = rustbot_command(&acp, Some(Path::new("/opt/rust-bot"))).unwrap();
        assert_eq!(program, PathBuf::from("/opt/rust-bot"));
        assert_eq!(args, vec!["acp"]);
        // And with no override it is the running executable.
        let (program, _) = rustbot_command(&acp, None).unwrap();
        assert_eq!(program, std::env::current_exe().unwrap());
    }

    #[test]
    fn a_configured_preset_names_its_own_program_and_arguments() {
        let mut acp = AcpConfig::default();
        acp.launch_presets.insert(
            RUSTBOT_PRESET.to_string(),
            AcpLaunchPreset {
                command: vec![
                    "D:/tools/rust-bot.exe".to_string(),
                    "acp".to_string(),
                    "--logs".to_string(),
                ],
            },
        );
        let (program, args) = rustbot_command(&acp, None).unwrap();
        assert_eq!(program, PathBuf::from("D:/tools/rust-bot.exe"));
        assert_eq!(args, vec!["acp", "--logs"]);
    }

    #[test]
    fn self_in_a_preset_means_this_executable() {
        let mut acp = AcpConfig::default();
        acp.launch_presets.insert(
            RUSTBOT_PRESET.to_string(),
            AcpLaunchPreset {
                command: vec!["self".to_string(), "acp".to_string()],
            },
        );
        let (program, args) = rustbot_command(&acp, Some(Path::new("/opt/rust-bot"))).unwrap();
        assert_eq!(program, PathBuf::from("/opt/rust-bot"));
        assert_eq!(args, vec!["acp"]);
    }

    #[test]
    fn an_empty_preset_command_is_an_error() {
        let mut acp = AcpConfig::default();
        acp.launch_presets.insert(
            RUSTBOT_PRESET.to_string(),
            AcpLaunchPreset {
                command: Vec::new(),
            },
        );
        assert!(matches!(
            rustbot_command(&acp, None),
            Err(LaunchError::BadPreset(_))
        ));
    }
}
