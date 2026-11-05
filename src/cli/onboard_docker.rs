//! `rust-bot onboard --mode docker`: create a self-contained Docker instance of rust-bot.
//!
//! The instance folder (default `./rust-bot-docker`) receives:
//!
//! ```text
//! docker-compose.yml   runs the gateway image, publishes the web UI on 127.0.0.1
//! .env.docker          secrets (provider API keys) for the ${VAR} placeholders in the config
//! .gitignore           "*": the folder holds secrets
//! data/                mounted as /data: config.json, credentials/, users.json, later all state
//! ```
//!
//! The config is built with `/data` as its root (see [`InstanceLayout::container`]), so it is
//! valid inside the container while the files are written on the host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::channels::websocket::CHANNEL_NAME as CHANNEL_WEBSOCKET;
use crate::channels::websocket::types::WebSocketConfig;
use crate::cli::command_specifications::{
    DEFAULT_DOCKER_HOST_PORT, DEFAULT_DOCKER_PROJECT_NAME, DOCKER_DATA_DIR_NAME,
    DOCKER_INSTANCE_DIR_NAME, OnboardDockerOptions,
};
use crate::cli::commands::CliError;
use crate::cli::docker_runtime::{
    ContainerHealth, DockerAvailability, DockerCli, DockerRuntime, same_directory,
};
use crate::cli::instance_layout::InstanceLayout;
use crate::cli::onboard::{
    ExistingConfigChoice, WebUiSetup, build_new_instance_config, extract_provider_secrets,
    print_onboard_ok, print_onboard_warning, prompt_existing_config_choice,
    write_instance_gitignore,
};
use crate::cli::wizard::{WebUiOptions, expand_and_absolutize};
use crate::config::loader::{load_config, save_config};
use crate::config::schema::Config;
use crate::utils::path::display_path;

/// Where the container mounts the instance's `data/` folder.
pub const CONTAINER_DATA_ROOT: &str = "/data";

/// Port the gateway listens on inside the container (the image's `EXPOSE` and healthcheck).
pub const CONTAINER_GATEWAY_PORT: u16 = 18790;

/// Bind address inside the container: with `127.0.0.1` the published port could not reach it.
const CONTAINER_BIND_HOST: &str = "0.0.0.0";

/// Docker Hub repository of the published image; the tag is this binary's version.
pub const DEFAULT_IMAGE_REPOSITORY: &str = "gilf/rust-bot";

/// File names inside the instance folder.
const COMPOSE_FILE_NAME: &str = "docker-compose.yml";
const ENV_FILE_NAME: &str = ".env.docker";
const CONFIG_FILE_NAME: &str = "config.json";

/// Compose file template; see [`render_compose_file`].
const COMPOSE_TEMPLATE: &str = include_str!("../../docker/compose.template.yml");

/// First lines of a newly created `.env.docker`.
const ENV_FILE_HEADER: &str = "\
# Secrets for this rust-bot Docker instance, read by docker compose (env_file).
# data/config.json refers to them as ${VAR} placeholders. Never commit this file.
";

/// Where the container image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// Pull this image reference (default `gilf/rust-bot:<version>`).
    Pull(String),
    /// Build the image from this rust-bot source checkout (it contains a `Dockerfile`).
    Build(PathBuf),
}

/// Everything about a Docker instance that comes from the command line, with defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerInstanceSettings {
    /// Instance folder on the host (absolute).
    pub instance_dir: PathBuf,
    /// Compose project name and container name.
    pub project_name: String,
    /// Host port published on 127.0.0.1.
    pub host_port: u16,
    /// Image to run.
    pub image: ImageSource,
    /// `false` with `--no-start`: write the files only.
    pub start: bool,
}

impl DockerInstanceSettings {
    /// The instance's `data/` folder, mounted as [`CONTAINER_DATA_ROOT`].
    pub fn data_dir(&self) -> PathBuf {
        self.instance_dir.join(DOCKER_DATA_DIR_NAME)
    }

    /// Layout that writes into [`Self::data_dir`] and records `/data/...` paths.
    pub fn layout(&self) -> InstanceLayout {
        InstanceLayout::container(self.data_dir(), CONTAINER_DATA_ROOT)
    }

    /// Host path of the instance's `config.json`.
    pub fn config_path(&self) -> PathBuf {
        self.data_dir().join(CONFIG_FILE_NAME)
    }

    /// Web UI URL on the host.
    pub fn web_ui_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.host_port)
    }
}

/// Apply the defaults to the `--mode docker` flags and check them.
///
/// Relative `--dir` / `--build` paths are taken relative to the current directory.
///
/// # Errors
///
/// An invalid project name, port 0, or a `--build` folder without a `Dockerfile`.
pub fn resolve_docker_settings(
    options: &OnboardDockerOptions,
    current_dir: &Path,
) -> Result<DockerInstanceSettings, String> {
    let instance_dir = match &options.dir {
        Some(dir) => expand_and_absolutize(dir),
        None => current_dir.join(DOCKER_INSTANCE_DIR_NAME),
    };
    let project_name = options
        .name
        .clone()
        .unwrap_or_else(|| DEFAULT_DOCKER_PROJECT_NAME.to_string());
    validate_project_name(&project_name)?;
    let host_port = options.port.unwrap_or(DEFAULT_DOCKER_HOST_PORT);
    if host_port == 0 {
        return Err("--port must be between 1 and 65535".to_string());
    }
    let image = match (&options.build, &options.image) {
        (Some(source_dir), _) => {
            let source_dir = expand_and_absolutize(source_dir);
            if !source_dir.join("Dockerfile").is_file() {
                return Err(format!(
                    "--build {} has no Dockerfile; point it at a rust-bot source checkout",
                    source_dir.display()
                ));
            }
            ImageSource::Build(source_dir)
        }
        (None, Some(image)) => ImageSource::Pull(image.clone()),
        (None, None) => ImageSource::Pull(default_image()),
    };
    Ok(DockerInstanceSettings {
        instance_dir,
        project_name,
        host_port,
        image,
        start: !options.no_start,
    })
}

/// `gilf/rust-bot:<version of this binary>`: the image that reads the config this binary writes.
pub fn default_image() -> String {
    format!("{DEFAULT_IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

/// Docker Compose project names: lowercase letters, digits, `-` and `_`, starting with a
/// letter or digit.
fn validate_project_name(name: &str) -> Result<(), String> {
    let starts_well = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    let all_allowed = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if starts_well && all_allowed {
        Ok(())
    } else {
        Err(format!(
            "--name {name:?} is not a valid compose project name: use lowercase letters, digits, - and _, starting with a letter or digit"
        ))
    }
}

/// Web UI answers fixed for a container: bind `0.0.0.0:18790`, JWT and login always on.
fn container_web_ui_options(replace_existing_keys: bool) -> WebUiOptions {
    WebUiOptions {
        fixed_bind_host: Some(CONTAINER_BIND_HOST.to_string()),
        fixed_port: Some(CONTAINER_GATEWAY_PORT),
        force_jwt: true,
        jwt_default: true,
        force_login: true,
        replace_existing_keys,
    }
}

/// Run `rust-bot onboard --mode docker`.
pub fn run_docker_onboard(options: &OnboardDockerOptions) -> Result<(), CliError> {
    let current_dir = std::env::current_dir()
        .map_err(|e| CliError::Other(format!("Cannot resolve the current directory: {e}")))?;
    let settings = resolve_docker_settings(options, &current_dir).map_err(CliError::Other)?;
    println!(
        "Docker instance folder: {}",
        display_path(&settings.instance_dir)
    );

    let runtime = DockerCli;
    // Before any question: a name or port clash would otherwise surface after all the answers.
    let docker_ready = if settings.start {
        let readiness = preflight(&runtime, &settings).map_err(CliError::Other)?;
        if let Err(reason) = &readiness {
            print_onboard_warning(format!(
                "{reason}. The files are written anyway; start the instance later."
            ));
        }
        readiness.is_ok()
    } else {
        false
    };

    let config = collect_instance_config(&settings)?;
    let written = write_docker_instance(&settings, config, linux_container_user())?;
    for warning in &written.warnings {
        print_onboard_warning(warning);
    }
    print_onboard_ok(format!(
        "Wrote {}",
        display_path(&settings.instance_dir.join(COMPOSE_FILE_NAME))
    ));
    print_onboard_ok(format!(
        "Wrote {} ({})",
        display_path(&settings.instance_dir.join(ENV_FILE_NAME)),
        secrets_summary(written.secret_count)
    ));
    print_onboard_ok(format!("Wrote {}", display_path(&settings.config_path())));

    if !docker_ready {
        print_docker_next_steps(&settings);
        return Ok(());
    }
    println!();
    println!("Starting the container...");
    match start_instance(
        &runtime,
        &settings,
        &HealthWait::default(),
        std::thread::sleep,
    ) {
        StartOutcome::Healthy => {
            print_onboard_ok(format!(
                "rust-bot is running at {} (log in with the user you created)",
                settings.web_ui_url()
            ));
            for line in docker_management_lines(&settings) {
                println!("{line}");
            }
            Ok(())
        }
        StartOutcome::Failed { reason, logs } => {
            if let Some(logs) = logs {
                println!();
                println!("Last lines of the container log:");
                println!("{logs}");
            }
            println!(
                "If the log points at the config, check it with: cd \"{}\" && docker compose run --rm rust-bot check-config",
                display_path(&settings.instance_dir)
            );
            Err(CliError::Other(format!(
                "The container did not start: {reason}"
            )))
        }
    }
}

/// How the `.env.docker` line of the summary describes the secrets written by this run.
fn secrets_summary(new_secret_count: usize) -> String {
    match new_secret_count {
        0 => "existing secrets kept".to_string(),
        1 => "1 secret".to_string(),
        count => format!("{count} secrets"),
    }
}

/// Check, before any question is asked, whether the instance can be started.
///
/// `Ok(Ok(()))`: Docker is ready and nothing is in the way. `Ok(Err(reason))`: Docker cannot be
/// used (not installed, not running, no Compose v2); the files can still be written.
///
/// # Errors
///
/// The container name or the host port is taken by something else; the user must choose
/// another `--name` / `--port` first.
pub fn preflight(
    runtime: &impl DockerRuntime,
    settings: &DockerInstanceSettings,
) -> Result<Result<(), String>, String> {
    match runtime.availability() {
        DockerAvailability::Ready => {}
        DockerAvailability::NoCli => {
            return Ok(Err(
                "Docker is not installed (no `docker` on the PATH)".to_string()
            ));
        }
        DockerAvailability::DaemonDown(detail) => {
            return Ok(Err(format!(
                "Docker is installed but not running; start Docker Desktop or the Docker service ({detail})"
            )));
        }
        DockerAvailability::NoComposeV2 => {
            return Ok(Err(
                "Docker Compose v2 (`docker compose`) is missing; install the compose plugin"
                    .to_string(),
            ));
        }
    }

    let own_container_exists = match runtime.existing_container_project_dir(&settings.project_name)
    {
        None => false,
        Some(Some(project_dir)) if same_directory(&project_dir, &settings.instance_dir) => true,
        Some(Some(project_dir)) => {
            return Err(format!(
                "A container named {:?} already belongs to {}; choose another name with --name",
                settings.project_name,
                project_dir.display()
            ));
        }
        Some(None) => {
            return Err(format!(
                "A container named {:?} already exists (not created by Docker Compose); choose another name with --name",
                settings.project_name
            ));
        }
    };
    // This instance's own running container holds the port; `up -d` recreates it.
    if !own_container_exists && !runtime.host_port_free(settings.host_port) {
        return Err(format!(
            "Port {} on 127.0.0.1 is already in use; choose another with --port",
            settings.host_port
        ));
    }
    Ok(Ok(()))
}

/// How long [`start_instance`] waits for the container to become healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthWait {
    /// Pause between two checks.
    pub interval: Duration,
    /// Number of checks before giving up.
    pub attempts: u32,
}

impl Default for HealthWait {
    /// Every 2 s for up to 120 s: the image's healthcheck has a 20 s start period, and the
    /// entrypoint runs `check-config` first.
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            attempts: 60,
        }
    }
}

/// Number of log lines shown when the container fails to start.
const FAILURE_LOG_LINES: usize = 50;

/// Result of [`start_instance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// The container runs and its healthcheck passes (or it has none).
    Healthy,
    /// It did not come up; `logs` holds the tail of its log when it got as far as starting.
    Failed {
        reason: String,
        logs: Option<String>,
    },
}

/// Pull (or build) the image, `docker compose up -d`, and wait until the container is healthy.
///
/// `sleep` is called between health checks (`std::thread::sleep`; a no-op in tests).
pub fn start_instance(
    runtime: &impl DockerRuntime,
    settings: &DockerInstanceSettings,
    wait: &HealthWait,
    sleep: impl Fn(Duration),
) -> StartOutcome {
    let instance_dir = &settings.instance_dir;
    let fetched = match settings.image {
        ImageSource::Pull(_) => runtime.pull(instance_dir),
        ImageSource::Build(_) => runtime.build(instance_dir),
    };
    if let Err(reason) = fetched {
        return StartOutcome::Failed { reason, logs: None };
    }
    let failed = |reason: String| StartOutcome::Failed {
        reason,
        logs: Some(runtime.logs_tail(instance_dir, FAILURE_LOG_LINES)),
    };
    if let Err(reason) = runtime.up(instance_dir) {
        return failed(reason);
    }
    for attempt in 0..wait.attempts {
        if attempt > 0 {
            sleep(wait.interval);
        }
        match runtime.health(&settings.project_name) {
            ContainerHealth::Healthy | ContainerHealth::RunningWithoutHealthcheck => {
                return StartOutcome::Healthy;
            }
            ContainerHealth::Unhealthy => return failed("its healthcheck fails".to_string()),
            ContainerHealth::Restarting => {
                return failed("it exited and is being restarted".to_string());
            }
            ContainerHealth::NotRunning(status) => {
                return failed(format!("it is not running ({status})"));
            }
            // Just after `up`, inspect can briefly fail; keep waiting.
            ContainerHealth::Starting | ContainerHealth::Unknown(_) => {}
        }
    }
    let waited = wait.interval * wait.attempts.saturating_sub(1);
    failed(format!("it was not healthy after {} s", waited.as_secs()))
}

/// Ask the onboard questions for a new instance, or offer overwrite/refresh for an existing one.
fn collect_instance_config(settings: &DockerInstanceSettings) -> Result<Config, CliError> {
    let config_path = settings.config_path();
    let layout = settings.layout();
    if !config_path.exists() {
        return build_new_instance_config(
            &layout,
            None,
            &WebUiSetup::Always(container_web_ui_options(false)),
        );
    }
    match prompt_existing_config_choice(&config_path) {
        // Overwrite asks every question again, so the keypair is replaced as well.
        ExistingConfigChoice::Overwrite => build_new_instance_config(
            &layout,
            None,
            &WebUiSetup::Always(container_web_ui_options(true)),
        ),
        ExistingConfigChoice::Refresh => Ok(load_config(Some(config_path))),
    }
}

/// What [`write_docker_instance`] did, for the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenInstance {
    /// Number of secrets moved into `.env.docker` by this run.
    pub secret_count: usize,
    /// Settings changed to what the container needs, and paths that point outside `/data`.
    pub warnings: Vec<String>,
}

/// Write the instance folder for an already collected `config`: enforce the container settings,
/// move the API keys into `.env.docker`, and write `data/config.json`, `docker-compose.yml` and
/// `.gitignore`.
///
/// `linux_user` is the `uid:gid` the container runs as (see [`linux_container_user`]).
pub fn write_docker_instance(
    settings: &DockerInstanceSettings,
    mut config: Config,
    linux_user: Option<(u32, u32)>,
) -> Result<WrittenInstance, CliError> {
    let mut warnings = enforce_container_invariants(&mut config).map_err(CliError::Other)?;
    warnings.extend(paths_outside_container_root(&config));

    let secrets = extract_provider_secrets(&mut config);
    let env_path = settings.instance_dir.join(ENV_FILE_NAME);
    let existing_env = match std::fs::read_to_string(&env_path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(CliError::Other(format!(
                "Cannot read {}: {error}",
                env_path.display()
            )));
        }
    };
    let env_content = merge_env_file(existing_env.as_deref(), &secrets).map_err(CliError::Other)?;
    write_file(&env_path, &env_content)?;
    restrict_to_owner(&env_path);

    save_config(&config, Some(settings.config_path()))?;

    let compose = render_compose_file(settings, linux_user);
    write_file(&settings.instance_dir.join(COMPOSE_FILE_NAME), &compose)?;

    write_instance_gitignore(&settings.instance_dir).map_err(|e| {
        CliError::Other(format!(
            "Cannot write .gitignore in {}: {e}",
            settings.instance_dir.display()
        ))
    })?;

    Ok(WrittenInstance {
        secret_count: secrets.len(),
        warnings,
    })
}

/// Create `path`'s parent folders and write `content` to it.
fn write_file(path: &Path, content: &str) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::Other(format!("Cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(path, content)
        .map_err(|e| CliError::Other(format!("Cannot write {}: {e}", path.display())))
}

/// Make a secrets file readable by its owner only (Unix; a no-op elsewhere).
fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            print_onboard_warning(format!(
                "Cannot restrict {} to its owner: {error}",
                path.display()
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Set the values a container depends on: the gateway and the `websocket` channel bind
/// `0.0.0.0:18790`, and JWT and login are on. Returns a note for every value that had to change
/// (a refreshed config may have been edited by hand).
///
/// # Errors
///
/// The config has no `websocket` channel, or it cannot be read; such a config has no web UI.
pub fn enforce_container_invariants(config: &mut Config) -> Result<Vec<String>, String> {
    let mut changes = Vec::new();
    let mut note = |field: &str, from: String, to: String| {
        if from != to {
            changes.push(format!(
                "{field} changed from {from} to {to}, as the container requires"
            ));
        }
    };

    note(
        "gateway.host",
        config.gateway.host.clone(),
        CONTAINER_BIND_HOST.to_string(),
    );
    config.gateway.host = CONTAINER_BIND_HOST.to_string();
    note(
        "gateway.port",
        config.gateway.port.to_string(),
        CONTAINER_GATEWAY_PORT.to_string(),
    );
    config.gateway.port = CONTAINER_GATEWAY_PORT;

    let websocket_value = config.channels.extra.get(CHANNEL_WEBSOCKET).ok_or_else(|| {
        format!(
            "The config has no `channels.{CHANNEL_WEBSOCKET}` (web UI); re-run and choose overwrite"
        )
    })?;
    let mut websocket: WebSocketConfig = serde_json::from_value(websocket_value.clone())
        .map_err(|e| format!("Cannot read `channels.{CHANNEL_WEBSOCKET}`: {e}"))?;
    let field = |name: &str| format!("channels.{CHANNEL_WEBSOCKET}.{name}");
    note(
        &field("host"),
        websocket.host.clone(),
        CONTAINER_BIND_HOST.to_string(),
    );
    websocket.host = CONTAINER_BIND_HOST.to_string();
    note(
        &field("port"),
        websocket.port.to_string(),
        CONTAINER_GATEWAY_PORT.to_string(),
    );
    websocket.port = CONTAINER_GATEWAY_PORT;
    note(
        &field("jwt.enabled"),
        websocket.jwt.enabled.to_string(),
        true.to_string(),
    );
    websocket.jwt.enabled = true;
    note(
        &field("requireAuth"),
        websocket.require_auth.to_string(),
        true.to_string(),
    );
    websocket.require_auth = true;
    let websocket_value = serde_json::to_value(&websocket)
        .map_err(|e| format!("Cannot write `channels.{CHANNEL_WEBSOCKET}`: {e}"))?;
    config
        .channels
        .extra
        .insert(CHANNEL_WEBSOCKET.to_string(), websocket_value);
    Ok(changes)
}

/// Warnings for file paths in `config` that are not under `/data/`: the container cannot see
/// such a file unless it is mounted separately.
pub fn paths_outside_container_root(config: &Config) -> Vec<String> {
    let mut paths = vec![
        ("agents.workspace", config.agents.workspace.clone()),
        (
            "api.jwt.privateKeyPath",
            config.api.jwt.private_key_path.clone(),
        ),
        (
            "api.jwt.publicKeyPath",
            config.api.jwt.public_key_path.clone(),
        ),
        ("api.usersFile", config.api.users_file.clone()),
        (
            "tools.mcpPresetsPath",
            config.tools.mcp_presets_path.clone(),
        ),
    ];
    if let Some(Ok(websocket)) = config
        .channels
        .extra
        .get(CHANNEL_WEBSOCKET)
        .map(|value| serde_json::from_value::<WebSocketConfig>(value.clone()))
    {
        paths.push((
            "channels.websocket.jwt.privateKeyPath",
            websocket.jwt.private_key_path,
        ));
        paths.push((
            "channels.websocket.jwt.publicKeyPath",
            websocket.jwt.public_key_path,
        ));
    }
    let root_prefix = format!("{CONTAINER_DATA_ROOT}/");
    paths
        .into_iter()
        .filter(|(_, path)| !path.is_empty() && !path.starts_with(&root_prefix))
        .map(|(field, path)| {
            format!("{field} is {path}, which is outside {CONTAINER_DATA_ROOT} and not visible in the container")
        })
        .collect()
}

/// Content of `.env.docker` after setting `secrets` in `existing` (or in a new file).
///
/// A `KEY=...` line for a key in `secrets` gets the new value; every other line (comments,
/// variables added by hand) is kept; keys not yet present are appended.
///
/// # Errors
///
/// A secret value spans several lines, which an env file cannot hold.
pub fn merge_env_file(
    existing: Option<&str>,
    secrets: &BTreeMap<String, String>,
) -> Result<String, String> {
    if let Some((name, _)) = secrets
        .iter()
        .find(|(_, value)| value.contains(['\n', '\r']))
    {
        return Err(format!("The value of {name} spans several lines"));
    }
    let mut remaining = secrets.clone();
    let mut lines: Vec<String> = Vec::new();
    for line in existing.unwrap_or(ENV_FILE_HEADER).lines() {
        let key = line.split_once('=').map(|(key, _)| key.trim());
        match key.and_then(|key| remaining.remove_entry(key)) {
            Some((key, value)) => lines.push(format!("{key}={value}")),
            None => lines.push(line.to_string()),
        }
    }
    lines.extend(
        remaining
            .into_iter()
            .map(|(key, value)| format!("{key}={value}")),
    );
    let mut content = lines.join("\n");
    content.push('\n');
    Ok(content)
}

/// `docker-compose.yml` for `settings`, from [`COMPOSE_TEMPLATE`].
///
/// `linux_user` adds `user: "<uid>:<gid>"`, so the container can write the bind-mounted folder.
pub fn render_compose_file(
    settings: &DockerInstanceSettings,
    linux_user: Option<(u32, u32)>,
) -> String {
    let image_source = match &settings.image {
        ImageSource::Pull(image) => format!("    image: {image}"),
        ImageSource::Build(source_dir) => format!(
            "    # Built from a source checkout (`--build`).\n    build: {}",
            yaml_single_quoted(&source_dir.display().to_string())
        ),
    };
    let user_line = match linux_user {
        Some((uid, gid)) => format!(
            "    # Run as your host user so the container can write to ./data.\n    user: \"{uid}:{gid}\"\n"
        ),
        None => String::new(),
    };
    // A Windows checkout (core.autocrlf) may have given the embedded template CRLF endings.
    COMPOSE_TEMPLATE
        .replace("\r\n", "\n")
        .replace("{{PROJECT_NAME}}", &settings.project_name)
        .replace("{{IMAGE_SOURCE}}", &image_source)
        .replace("{{USER_LINE}}", &user_line)
        .replace("{{HOST_PORT}}", &settings.host_port.to_string())
        .replace("{{CONTAINER_PORT}}", &CONTAINER_GATEWAY_PORT.to_string())
}

/// YAML single-quoted scalar: backslashes stay literal (Windows paths), `'` is doubled.
fn yaml_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// `uid:gid` of the current user on Linux, where a bind mount keeps host ownership and the
/// image's own user (uid 1000) might not be allowed to write. `None` elsewhere: Docker Desktop
/// (Windows, macOS) maps file ownership itself.
pub fn linux_container_user() -> Option<(u32, u32)> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getuid/getgid have no preconditions and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Some((uid, gid))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Print how to start the instance by hand.
fn print_docker_next_steps(settings: &DockerInstanceSettings) {
    for line in docker_next_steps_lines(settings) {
        println!("{line}");
    }
}

/// Lines of [`print_docker_next_steps`]: how to start the instance by hand.
fn docker_next_steps_lines(settings: &DockerInstanceSettings) -> Vec<String> {
    let up = match settings.image {
        ImageSource::Pull(_) => "docker compose up -d",
        ImageSource::Build(_) => "docker compose up -d --build",
    };
    let mut lines = vec![
        String::new(),
        "Next steps:".to_string(),
        format!("  cd \"{}\"", display_path(&settings.instance_dir)),
        format!("  {up}"),
        format!(
            "  Open {} and log in with the user you just created",
            settings.web_ui_url()
        ),
    ];
    lines.extend(docker_management_lines(settings));
    lines
}

/// How to follow the logs of, and stop, a running instance.
fn docker_management_lines(settings: &DockerInstanceSettings) -> Vec<String> {
    vec![format!(
        "  In {}: docker compose logs -f (logs), docker compose down (stop)",
        display_path(&settings.instance_dir)
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_in(dir: &Path) -> DockerInstanceSettings {
        DockerInstanceSettings {
            instance_dir: dir.to_path_buf(),
            project_name: "rust-bot".to_string(),
            host_port: 18790,
            image: ImageSource::Pull("gilf/rust-bot:9.9.9".to_string()),
            start: true,
        }
    }

    /// A config as a fresh Docker onboarding builds it, before the questions are answered.
    fn container_config(settings: &DockerInstanceSettings) -> Config {
        let layout = settings.layout();
        let mut config = Config::default();
        config.rebase_default_paths_with(|parts| layout.config_path(parts));
        let mut websocket = WebSocketConfig::default();
        websocket.jwt.private_key_path = layout.config_path(&["credentials", "private_key.pem"]);
        websocket.jwt.public_key_path = layout.config_path(&["credentials", "public_key.pem"]);
        config.channels.extra.insert(
            CHANNEL_WEBSOCKET.to_string(),
            serde_json::to_value(websocket).unwrap(),
        );
        config
    }

    #[test]
    fn settings_default_to_the_published_image_in_rust_bot_docker() {
        let cwd = tempfile::tempdir().unwrap();
        let settings =
            resolve_docker_settings(&OnboardDockerOptions::default(), cwd.path()).unwrap();
        assert_eq!(settings.instance_dir, cwd.path().join("rust-bot-docker"));
        assert_eq!(settings.project_name, "rust-bot");
        assert_eq!(settings.host_port, 18790);
        assert_eq!(
            settings.image,
            ImageSource::Pull(format!("gilf/rust-bot:{}", env!("CARGO_PKG_VERSION")))
        );
        assert!(settings.start);
        assert_eq!(
            settings.config_path(),
            cwd.path()
                .join("rust-bot-docker")
                .join("data")
                .join("config.json")
        );
    }

    #[test]
    fn settings_take_the_flags() {
        let cwd = tempfile::tempdir().unwrap();
        let options = OnboardDockerOptions {
            dir: Some(cwd.path().join("bot")),
            name: Some("bot_2".to_string()),
            port: Some(18800),
            image: Some("rust-bot:local".to_string()),
            build: None,
            no_start: true,
        };
        let settings = resolve_docker_settings(&options, cwd.path()).unwrap();
        assert_eq!(settings.instance_dir, cwd.path().join("bot"));
        assert_eq!(settings.project_name, "bot_2");
        assert_eq!(settings.host_port, 18800);
        assert_eq!(
            settings.image,
            ImageSource::Pull("rust-bot:local".to_string())
        );
        assert!(!settings.start);
    }

    #[test]
    fn build_needs_a_folder_with_a_dockerfile() {
        let cwd = tempfile::tempdir().unwrap();
        let options = OnboardDockerOptions {
            build: Some(cwd.path().to_path_buf()),
            ..OnboardDockerOptions::default()
        };
        let error = resolve_docker_settings(&options, cwd.path()).unwrap_err();
        assert!(error.contains("no Dockerfile"), "{error}");

        std::fs::write(cwd.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let settings = resolve_docker_settings(&options, cwd.path()).unwrap();
        assert_eq!(settings.image, ImageSource::Build(cwd.path().to_path_buf()));
    }

    #[test]
    fn project_names_follow_the_compose_rules() {
        for valid in ["rust-bot", "bot_1", "2bots"] {
            assert_eq!(validate_project_name(valid), Ok(()), "{valid}");
        }
        for invalid in ["", "Rust-Bot", "-bot", "bot one", "bot.one"] {
            assert!(validate_project_name(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn port_zero_is_rejected() {
        let cwd = tempfile::tempdir().unwrap();
        let options = OnboardDockerOptions {
            port: Some(0),
            ..OnboardDockerOptions::default()
        };
        assert!(resolve_docker_settings(&options, cwd.path()).is_err());
    }

    #[test]
    fn compose_file_for_a_pulled_image() {
        let settings = settings_in(Path::new("/srv/bot"));
        let compose = render_compose_file(&settings, None);
        assert!(
            !compose.contains("{{"),
            "unreplaced placeholder:\n{compose}"
        );
        assert!(compose.contains("name: rust-bot\n"));
        assert!(compose.contains("    image: gilf/rust-bot:9.9.9\n"));
        assert!(compose.contains("container_name: rust-bot\n"));
        assert!(compose.contains("- \"127.0.0.1:18790:18790\""));
        assert!(compose.contains("- ./data:/data"));
        assert!(compose.contains("env_file: .env.docker"));
        assert!(compose.contains("RUST_LOG: ${RUST_LOG:-info}"));
        assert!(!compose.contains("user:"));
        assert!(!compose.contains("build:"));
        assert!(!compose.contains('\r'), "LF line endings only");
    }

    #[test]
    fn compose_file_for_a_build_with_a_linux_user_and_another_port() {
        let mut settings = settings_in(Path::new("/srv/bot"));
        settings.image = ImageSource::Build(PathBuf::from(r"C:\dev\it's rust-bot"));
        settings.host_port = 18800;
        let compose = render_compose_file(&settings, Some((1001, 1002)));
        assert!(
            compose.contains(r"    build: 'C:\dev\it''s rust-bot'"),
            "{compose}"
        );
        assert!(!compose.contains("image:"));
        assert!(
            compose.contains("    user: \"1001:1002\"\n    ports:"),
            "{compose}"
        );
        assert!(compose.contains("- \"127.0.0.1:18800:18790\""));
    }

    #[test]
    fn invariants_force_the_container_bind_address_jwt_and_login() {
        let settings = settings_in(Path::new("/srv/bot"));
        let mut config = container_config(&settings);
        // As if edited by hand on the host before a refresh.
        config.gateway.host = "127.0.0.1".to_string();
        let changes = enforce_container_invariants(&mut config).unwrap();
        assert_eq!(config.gateway.host, "0.0.0.0");
        assert_eq!(config.gateway.port, 18790);
        let websocket: WebSocketConfig =
            serde_json::from_value(config.channels.extra[CHANNEL_WEBSOCKET].clone()).unwrap();
        assert_eq!(websocket.host, "0.0.0.0");
        assert_eq!(websocket.port, 18790);
        assert!(websocket.jwt.enabled);
        assert!(websocket.require_auth);
        assert!(
            changes
                .iter()
                .any(|change| change.starts_with("gateway.host")),
            "{changes:?}"
        );

        let again = enforce_container_invariants(&mut config).unwrap();
        assert!(again.is_empty(), "nothing left to change: {again:?}");
    }

    #[test]
    fn invariants_need_a_web_ui_channel() {
        let mut config = Config::default();
        let error = enforce_container_invariants(&mut config).unwrap_err();
        assert!(error.contains("channels.websocket"), "{error}");
    }

    #[test]
    fn paths_outside_data_are_reported() {
        let settings = settings_in(Path::new("/srv/bot"));
        let mut config = container_config(&settings);
        assert!(paths_outside_container_root(&config).is_empty());

        config.api.users_file = "/home/me/users.json".to_string();
        let warnings = paths_outside_container_root(&config);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("api.usersFile"), "{warnings:?}");
    }

    #[test]
    fn env_file_is_created_with_a_header() {
        let secrets = BTreeMap::from([("OPENROUTER_API_KEY".to_string(), "sk-1".to_string())]);
        let content = merge_env_file(None, &secrets).unwrap();
        assert!(content.starts_with("# Secrets"));
        assert!(content.ends_with("OPENROUTER_API_KEY=sk-1\n"), "{content}");
    }

    #[test]
    fn env_file_merge_replaces_known_keys_and_keeps_everything_else() {
        let existing = "# mine\nOPENROUTER_API_KEY=old\nBRAVE_API_KEY=keep\n";
        let secrets = BTreeMap::from([
            ("OPENROUTER_API_KEY".to_string(), "new".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "sk-ant".to_string()),
        ]);
        let content = merge_env_file(Some(existing), &secrets).unwrap();
        assert_eq!(
            content,
            "# mine\nOPENROUTER_API_KEY=new\nBRAVE_API_KEY=keep\nANTHROPIC_API_KEY=sk-ant\n"
        );
    }

    #[test]
    fn env_file_rejects_multi_line_values() {
        let secrets = BTreeMap::from([("KEY".to_string(), "a\nb".to_string())]);
        assert!(merge_env_file(None, &secrets).is_err());
    }

    #[test]
    fn writing_an_instance_puts_secrets_in_the_env_file_and_data_paths_in_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_in(&dir.path().join("rust-bot-docker"));
        let mut config = container_config(&settings);
        config.providers.openrouter.api_key = "sk-or-secret".to_string();

        let written = write_docker_instance(&settings, config, Some((1000, 1000))).unwrap();
        assert_eq!(written.secret_count, 1);

        let saved = std::fs::read_to_string(settings.config_path()).unwrap();
        assert!(
            !saved.contains("sk-or-secret"),
            "the key must not be in the config"
        );
        assert!(saved.contains("${OPENROUTER_API_KEY}"));
        let host_prefix = dir.path().display().to_string().replace('\\', "\\\\");
        assert!(
            !saved.contains(&host_prefix),
            "a host path leaked:\n{saved}"
        );

        let env = std::fs::read_to_string(settings.instance_dir.join(".env.docker")).unwrap();
        assert!(env.contains("OPENROUTER_API_KEY=sk-or-secret\n"));
        let compose =
            std::fs::read_to_string(settings.instance_dir.join("docker-compose.yml")).unwrap();
        assert!(compose.contains("user: \"1000:1000\""));
        assert!(settings.instance_dir.join(".gitignore").is_file());
    }

    #[test]
    fn rewriting_a_refreshed_instance_keeps_the_existing_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_in(&dir.path().join("rust-bot-docker"));
        let mut config = container_config(&settings);
        config.providers.openrouter.api_key = "sk-or-secret".to_string();
        write_docker_instance(&settings, config, None).unwrap();

        // Refresh: the saved config only holds the placeholder now.
        let refreshed = load_config(Some(settings.config_path()));
        let written = write_docker_instance(&settings, refreshed, None).unwrap();
        assert_eq!(written.secret_count, 0);
        let env = std::fs::read_to_string(settings.instance_dir.join(".env.docker")).unwrap();
        assert!(env.contains("OPENROUTER_API_KEY=sk-or-secret\n"), "{env}");
        let saved = std::fs::read_to_string(settings.config_path()).unwrap();
        assert!(saved.contains("${OPENROUTER_API_KEY}"));
    }

    /// Scripted [`DockerRuntime`]: every answer is set up front; the calls made are recorded.
    struct FakeDocker {
        availability: DockerAvailability,
        port_free: bool,
        existing_container: Option<Option<PathBuf>>,
        pull_result: Result<(), String>,
        up_result: Result<(), String>,
        /// Returned by successive `health` calls; the last one repeats.
        health_sequence: Vec<ContainerHealth>,
        calls: std::cell::RefCell<Vec<String>>,
        health_calls: std::cell::Cell<usize>,
    }

    impl FakeDocker {
        /// Docker is ready, nothing is in the way, and the container becomes healthy.
        fn ready() -> Self {
            Self {
                availability: DockerAvailability::Ready,
                port_free: true,
                existing_container: None,
                pull_result: Ok(()),
                up_result: Ok(()),
                health_sequence: vec![ContainerHealth::Starting, ContainerHealth::Healthy],
                calls: std::cell::RefCell::new(Vec::new()),
                health_calls: std::cell::Cell::new(0),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn record(&self, call: &str) {
            self.calls.borrow_mut().push(call.to_string());
        }
    }

    impl DockerRuntime for FakeDocker {
        fn availability(&self) -> DockerAvailability {
            self.availability.clone()
        }
        fn host_port_free(&self, _port: u16) -> bool {
            self.port_free
        }
        fn existing_container_project_dir(&self, _name: &str) -> Option<Option<PathBuf>> {
            self.existing_container.clone()
        }
        fn pull(&self, _dir: &Path) -> Result<(), String> {
            self.record("pull");
            self.pull_result.clone()
        }
        fn build(&self, _dir: &Path) -> Result<(), String> {
            self.record("build");
            Ok(())
        }
        fn up(&self, _dir: &Path) -> Result<(), String> {
            self.record("up");
            self.up_result.clone()
        }
        fn health(&self, _name: &str) -> ContainerHealth {
            let index = self.health_calls.get();
            self.health_calls.set(index + 1);
            let last = self.health_sequence.len() - 1;
            self.health_sequence[index.min(last)].clone()
        }
        fn logs_tail(&self, _dir: &Path, _lines: usize) -> String {
            "[FATAL tini (7)] exec failed".to_string()
        }
    }

    /// Few, instant health checks for the tests.
    const QUICK_WAIT: HealthWait = HealthWait {
        interval: Duration::from_secs(2),
        attempts: 5,
    };

    fn no_sleep(_: Duration) {}

    #[test]
    fn preflight_passes_when_docker_is_ready_and_nothing_is_in_the_way() {
        let settings = settings_in(Path::new("/srv/bot"));
        assert_eq!(preflight(&FakeDocker::ready(), &settings), Ok(Ok(())));
    }

    #[test]
    fn preflight_reports_unusable_docker_without_failing() {
        let settings = settings_in(Path::new("/srv/bot"));
        for (availability, expected) in [
            (DockerAvailability::NoCli, "not installed"),
            (
                DockerAvailability::DaemonDown("pipe not found".to_string()),
                "not running",
            ),
            (DockerAvailability::NoComposeV2, "Compose v2"),
        ] {
            let docker = FakeDocker {
                availability,
                ..FakeDocker::ready()
            };
            let reason = preflight(&docker, &settings).unwrap().unwrap_err();
            assert!(reason.contains(expected), "{reason}");
        }
    }

    #[test]
    fn preflight_rejects_a_name_used_by_another_instance() {
        let settings = settings_in(Path::new("/srv/bot-1"));
        let docker = FakeDocker {
            existing_container: Some(Some(PathBuf::from("/srv/bot-2"))),
            ..FakeDocker::ready()
        };
        let error = preflight(&docker, &settings).unwrap_err();
        assert!(
            error.contains("--name") && error.contains("bot-2"),
            "{error}"
        );

        let not_compose = FakeDocker {
            existing_container: Some(None),
            ..FakeDocker::ready()
        };
        assert!(preflight(&not_compose, &settings).is_err());
    }

    #[test]
    fn preflight_rejects_a_busy_port_unless_this_instance_holds_it() {
        let settings = settings_in(Path::new("/srv/bot"));
        let busy = FakeDocker {
            port_free: false,
            ..FakeDocker::ready()
        };
        let error = preflight(&busy, &settings).unwrap_err();
        assert!(error.contains("--port"), "{error}");

        let busy_by_itself = FakeDocker {
            port_free: false,
            existing_container: Some(Some(PathBuf::from("/srv/bot/"))),
            ..FakeDocker::ready()
        };
        assert_eq!(preflight(&busy_by_itself, &settings), Ok(Ok(())));
    }

    #[test]
    fn start_pulls_starts_and_waits_until_healthy() {
        let settings = settings_in(Path::new("/srv/bot"));
        let docker = FakeDocker::ready();
        let outcome = start_instance(&docker, &settings, &QUICK_WAIT, no_sleep);
        assert_eq!(outcome, StartOutcome::Healthy);
        assert_eq!(docker.calls(), ["pull", "up"]);
        assert_eq!(docker.health_calls.get(), 2);
    }

    #[test]
    fn start_builds_instead_of_pulling_with_build() {
        let mut settings = settings_in(Path::new("/srv/bot"));
        settings.image = ImageSource::Build(PathBuf::from("/src/rust-bot"));
        let docker = FakeDocker::ready();
        start_instance(&docker, &settings, &QUICK_WAIT, no_sleep);
        assert_eq!(docker.calls(), ["build", "up"]);
    }

    #[test]
    fn start_accepts_an_image_without_healthcheck() {
        let settings = settings_in(Path::new("/srv/bot"));
        let docker = FakeDocker {
            health_sequence: vec![ContainerHealth::RunningWithoutHealthcheck],
            ..FakeDocker::ready()
        };
        assert_eq!(
            start_instance(&docker, &settings, &QUICK_WAIT, no_sleep),
            StartOutcome::Healthy
        );
    }

    #[test]
    fn a_failed_pull_stops_before_up_and_has_no_logs() {
        let settings = settings_in(Path::new("/srv/bot"));
        let docker = FakeDocker {
            pull_result: Err("manifest unknown".to_string()),
            ..FakeDocker::ready()
        };
        let outcome = start_instance(&docker, &settings, &QUICK_WAIT, no_sleep);
        assert_eq!(
            outcome,
            StartOutcome::Failed {
                reason: "manifest unknown".to_string(),
                logs: None
            }
        );
        assert_eq!(docker.calls(), ["pull"]);
    }

    #[test]
    fn a_failed_up_reports_the_logs() {
        let settings = settings_in(Path::new("/srv/bot"));
        let docker = FakeDocker {
            up_result: Err("port is already allocated".to_string()),
            ..FakeDocker::ready()
        };
        let StartOutcome::Failed { reason, logs } =
            start_instance(&docker, &settings, &QUICK_WAIT, no_sleep)
        else {
            panic!("expected a failure");
        };
        assert!(reason.contains("already allocated"));
        assert!(logs.is_some());
    }

    #[test]
    fn a_crash_loop_fails_at_once_with_the_logs() {
        let settings = settings_in(Path::new("/srv/bot"));
        let docker = FakeDocker {
            health_sequence: vec![ContainerHealth::Starting, ContainerHealth::Restarting],
            ..FakeDocker::ready()
        };
        let StartOutcome::Failed { reason, logs } =
            start_instance(&docker, &settings, &QUICK_WAIT, no_sleep)
        else {
            panic!("expected a failure");
        };
        assert!(reason.contains("restarted"), "{reason}");
        assert!(logs.unwrap().contains("tini"));
        assert_eq!(docker.health_calls.get(), 2, "no waiting after a crash");
    }

    #[test]
    fn an_unhealthy_or_stopped_container_fails() {
        let settings = settings_in(Path::new("/srv/bot"));
        for health in [
            ContainerHealth::Unhealthy,
            ContainerHealth::NotRunning("exited".to_string()),
        ] {
            let docker = FakeDocker {
                health_sequence: vec![health],
                ..FakeDocker::ready()
            };
            assert!(matches!(
                start_instance(&docker, &settings, &QUICK_WAIT, no_sleep),
                StartOutcome::Failed { .. }
            ));
        }
    }

    #[test]
    fn waiting_gives_up_after_the_last_attempt() {
        let settings = settings_in(Path::new("/srv/bot"));
        let docker = FakeDocker {
            health_sequence: vec![ContainerHealth::Starting],
            ..FakeDocker::ready()
        };
        let slept = std::cell::Cell::new(0);
        let outcome = start_instance(&docker, &settings, &QUICK_WAIT, |_| {
            slept.set(slept.get() + 1)
        });
        let StartOutcome::Failed { reason, .. } = outcome else {
            panic!("expected a timeout");
        };
        assert!(reason.contains("not healthy after 8 s"), "{reason}");
        assert_eq!(docker.health_calls.get(), 5);
        assert_eq!(slept.get(), 4);
    }

    #[test]
    fn secrets_summary_says_when_nothing_new_was_written() {
        assert_eq!(secrets_summary(0), "existing secrets kept");
        assert_eq!(secrets_summary(1), "1 secret");
        assert_eq!(secrets_summary(3), "3 secrets");
    }

    #[test]
    fn next_steps_use_build_when_building() {
        let mut settings = settings_in(Path::new("/srv/bot"));
        assert!(
            docker_next_steps_lines(&settings)
                .iter()
                .any(|line| line.trim() == "docker compose up -d")
        );
        settings.image = ImageSource::Build(PathBuf::from("/src"));
        assert!(
            docker_next_steps_lines(&settings)
                .iter()
                .any(|line| line.trim() == "docker compose up -d --build")
        );
    }
}
