//! The Docker operations `rust-bot onboard --mode docker` needs, behind [`DockerRuntime`] so the
//! start-up flow can be tested without Docker. [`DockerCli`] is the real implementation: it runs
//! the `docker` command line.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Name of the Docker command-line binary.
const DOCKER_BINARY: &str = "docker";

/// Label Docker Compose puts on a container: the folder of the compose project that created it.
const COMPOSE_WORKING_DIR_LABEL: &str = "com.docker.compose.project.working_dir";

/// Whether Docker can be used, from "installed" to "ready".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerAvailability {
    /// CLI found, daemon reachable, Compose v2 present.
    Ready,
    /// No `docker` on the PATH.
    NoCli,
    /// `docker info` failed: the daemon (Docker Desktop, dockerd) is not running or not reachable.
    DaemonDown(String),
    /// `docker compose version` failed: the Compose v2 plugin is missing.
    NoComposeV2,
}

/// State of a container, as far as starting an instance is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerHealth {
    /// Running; the healthcheck has not passed yet.
    Starting,
    /// Running and the healthcheck passes.
    Healthy,
    /// Running and the healthcheck fails.
    Unhealthy,
    /// Running, and the image has no healthcheck.
    RunningWithoutHealthcheck,
    /// Exited and being restarted by its restart policy: it crashed.
    Restarting,
    /// Not running (`exited`, `dead`, `created`, ...), with Docker's status word.
    NotRunning(String),
    /// No such container, or `docker inspect` failed.
    Unknown(String),
}

/// The Docker operations the start-up flow needs.
pub trait DockerRuntime {
    /// How far Docker is usable.
    fn availability(&self) -> DockerAvailability;
    /// `true` when nothing listens on `127.0.0.1:<port>`.
    fn host_port_free(&self, port: u16) -> bool;
    /// Compose project folder of the existing container called `container_name`, if there is one.
    /// `Some(None)`: the container exists but was not created by Compose.
    fn existing_container_project_dir(&self, container_name: &str) -> Option<Option<PathBuf>>;
    /// `docker compose pull` in `instance_dir`, output shown to the user.
    fn pull(&self, instance_dir: &Path) -> Result<(), String>;
    /// `docker compose build` in `instance_dir`, output shown to the user.
    fn build(&self, instance_dir: &Path) -> Result<(), String>;
    /// `docker compose up -d` in `instance_dir`, output shown to the user.
    fn up(&self, instance_dir: &Path) -> Result<(), String>;
    /// Current state of `container_name`.
    fn health(&self, container_name: &str) -> ContainerHealth;
    /// The last `lines` lines of the instance's logs.
    fn logs_tail(&self, instance_dir: &Path, lines: usize) -> String;
}

/// [`DockerRuntime`] that runs the `docker` command line.
#[derive(Debug, Default, Clone, Copy)]
pub struct DockerCli;

impl DockerCli {
    /// Run `docker <args>` in `dir` and capture its output. `Err` holds stderr (or stdout when
    /// stderr is empty) of a failed run, or why it could not be started.
    fn capture(&self, args: &[&str], dir: Option<&Path>) -> Result<String, String> {
        let mut command = Command::new(DOCKER_BINARY);
        command.args(args).stdin(Stdio::null());
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let output = command
            .output()
            .map_err(|e| format!("Cannot run `docker {}`: {e}", args.join(" ")))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            stdout.trim().to_string()
        } else {
            stderr
        })
    }

    /// Run `docker compose <args>` in `instance_dir` with its output going to the terminal.
    fn compose_visible(&self, args: &[&str], instance_dir: &Path) -> Result<(), String> {
        let status = Command::new(DOCKER_BINARY)
            .arg("compose")
            .args(args)
            .current_dir(instance_dir)
            .stdin(Stdio::null())
            .status()
            .map_err(|e| format!("Cannot run `docker compose {}`: {e}", args.join(" ")))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "`docker compose {}` failed ({status})",
                args.join(" ")
            ))
        }
    }
}

impl DockerRuntime for DockerCli {
    fn availability(&self) -> DockerAvailability {
        if which::which(DOCKER_BINARY).is_err() {
            return DockerAvailability::NoCli;
        }
        if let Err(error) = self.capture(&["info", "--format", "{{.ServerVersion}}"], None) {
            return DockerAvailability::DaemonDown(first_line(&error));
        }
        if self.capture(&["compose", "version"], None).is_err() {
            return DockerAvailability::NoComposeV2;
        }
        DockerAvailability::Ready
    }

    fn host_port_free(&self, port: u16) -> bool {
        TcpListener::bind(("127.0.0.1", port)).is_ok()
    }

    fn existing_container_project_dir(&self, container_name: &str) -> Option<Option<PathBuf>> {
        let format = format!("{{{{index .Config.Labels \"{COMPOSE_WORKING_DIR_LABEL}\"}}}}");
        let output = self
            .capture(
                &["container", "inspect", "--format", &format, container_name],
                None,
            )
            .ok()?;
        Some(parse_project_dir_label(&output))
    }

    fn pull(&self, instance_dir: &Path) -> Result<(), String> {
        self.compose_visible(&["pull"], instance_dir)
    }

    fn build(&self, instance_dir: &Path) -> Result<(), String> {
        self.compose_visible(&["build"], instance_dir)
    }

    fn up(&self, instance_dir: &Path) -> Result<(), String> {
        self.compose_visible(&["up", "-d"], instance_dir)
    }

    fn health(&self, container_name: &str) -> ContainerHealth {
        let format = "{{.State.Status}}|{{if .State.Health}}{{.State.Health.Status}}{{end}}";
        match self.capture(
            &["container", "inspect", "--format", format, container_name],
            None,
        ) {
            Ok(output) => parse_container_state(&output),
            Err(error) => ContainerHealth::Unknown(first_line(&error)),
        }
    }

    fn logs_tail(&self, instance_dir: &Path, lines: usize) -> String {
        let tail = lines.to_string();
        match self.capture(
            &["compose", "logs", "--no-color", "--tail", &tail],
            Some(instance_dir),
        ) {
            Ok(logs) => logs,
            Err(error) => format!("(could not read the logs: {error})"),
        }
    }
}

/// Parse `<status>|<health>` as printed by [`DockerCli::health`]'s inspect format.
pub fn parse_container_state(output: &str) -> ContainerHealth {
    let (status, health) = output.trim().split_once('|').unwrap_or((output.trim(), ""));
    match (status, health) {
        ("running", "healthy") => ContainerHealth::Healthy,
        ("running", "unhealthy") => ContainerHealth::Unhealthy,
        ("running", "") => ContainerHealth::RunningWithoutHealthcheck,
        ("running", _) => ContainerHealth::Starting,
        ("restarting", _) => ContainerHealth::Restarting,
        (other, _) => ContainerHealth::NotRunning(other.to_string()),
    }
}

/// The Compose working-directory label value: empty or `<no value>` means "not from Compose".
fn parse_project_dir_label(output: &str) -> Option<PathBuf> {
    let value = output.trim();
    (!value.is_empty() && value != "<no value>").then(|| PathBuf::from(value))
}

/// First non-empty line of `text`.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `true` when `a` and `b` name the same folder: canonical paths when both exist, otherwise the
/// text without trailing separators (case-insensitive on Windows).
pub fn same_directory(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    let normalize = |path: &Path| {
        let text = path
            .display()
            .to_string()
            .trim_end_matches(['/', '\\'])
            .to_string();
        if cfg!(windows) {
            text.replace('/', "\\").to_lowercase()
        } else {
            text
        }
    };
    normalize(a) == normalize(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_state_is_parsed_from_the_inspect_format() {
        assert_eq!(
            parse_container_state("running|healthy\n"),
            ContainerHealth::Healthy
        );
        assert_eq!(
            parse_container_state("running|starting"),
            ContainerHealth::Starting
        );
        assert_eq!(
            parse_container_state("running|unhealthy"),
            ContainerHealth::Unhealthy
        );
        assert_eq!(
            parse_container_state("running|"),
            ContainerHealth::RunningWithoutHealthcheck
        );
        assert_eq!(
            parse_container_state("restarting|starting"),
            ContainerHealth::Restarting
        );
        assert_eq!(
            parse_container_state("exited|unhealthy"),
            ContainerHealth::NotRunning("exited".to_string())
        );
    }

    #[test]
    fn project_dir_label_is_absent_for_non_compose_containers() {
        assert_eq!(parse_project_dir_label("<no value>\n"), None);
        assert_eq!(parse_project_dir_label("  \n"), None);
        assert_eq!(
            parse_project_dir_label("/srv/bot\n"),
            Some(PathBuf::from("/srv/bot"))
        );
    }

    #[test]
    fn same_directory_ignores_trailing_separators() {
        assert!(same_directory(
            Path::new("/srv/missing-bot/"),
            Path::new("/srv/missing-bot")
        ));
        assert!(!same_directory(
            Path::new("/srv/missing-bot-1"),
            Path::new("/srv/missing-bot-2")
        ));
        let existing = tempfile::tempdir().unwrap();
        assert!(same_directory(existing.path(), existing.path()));
    }

    #[test]
    fn first_line_skips_blank_lines() {
        assert_eq!(
            first_line("\n  error during connect: pipe not found\nmore"),
            "error during connect: pipe not found"
        );
    }
}
