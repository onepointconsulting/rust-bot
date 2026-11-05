//! Clap definitions of every `rust-bot` subcommand and its arguments, plus the
//! default config-path resolution they share.

use crate::security::jwt::{DEFAULT_EXPIRES_IN_MONTHS, UserRole};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Default `--config` path when the flag is omitted.
pub const DEFAULT_CONFIG_PATH: &str = "./.rust-bot/config.json";

/// Environment variable that names the config file when `--config` is omitted.
///
/// It exists for containers, where the config lives on a mounted volume and the
/// command line is fixed by the image.
pub const CONFIG_PATH_ENV_VAR: &str = "RUST_BOT_CONFIG";

/// Per-user config location (`~/.rust-bot/config.json`) written by the one-line installer.
pub(crate) const HOME_CONFIG_RELATIVE_PATH: &str = ".rust-bot/config.json";

/// Pick the config path used when `--config` is omitted.
///
/// A project-local config (`local_config`) always wins. Otherwise the per-user config
/// under `home_dir` is used when it exists, so a globally installed `rust-bot` works from
/// any directory. With neither present the local path is returned, which lets
/// `rust-bot onboard` create it in the current directory.
pub(crate) fn select_default_config_path(
    local_config: &std::path::Path,
    home_dir: Option<&std::path::Path>,
) -> PathBuf {
    let home_config = home_dir.map(|home| home.join(HOME_CONFIG_RELATIVE_PATH));
    match home_config {
        Some(home_config) if !local_config.exists() && home_config.exists() => home_config,
        _ => local_config.to_path_buf(),
    }
}

/// Pick the default config path, letting a non-empty `env_override` (the value of
/// [`CONFIG_PATH_ENV_VAR`]) win over the local and per-user search of
/// [`select_default_config_path`].
pub(crate) fn resolve_default_config_path(
    env_override: Option<std::ffi::OsString>,
    local_config: &std::path::Path,
    home_dir: Option<&std::path::Path>,
) -> PathBuf {
    match env_override.filter(|value| !value.is_empty()) {
        Some(explicit) => PathBuf::from(explicit),
        None => select_default_config_path(local_config, home_dir),
    }
}

/// Default `--config` value for every subcommand; see [`resolve_default_config_path`].
pub fn default_config_path() -> PathBuf {
    resolve_default_config_path(
        std::env::var_os(CONFIG_PATH_ENV_VAR),
        std::path::Path::new(DEFAULT_CONFIG_PATH),
        home::home_dir().as_deref(),
    )
}

/// Arguments of `rust-bot gateway`.
#[derive(Debug, Parser)]
pub struct GatewayArgs {
    /// Workspace directory
    #[arg(short, long)]
    pub workspace: Option<PathBuf>,

    /// Set logging to debug during runtime.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub verbose: bool,

    /// JSON configuration file path
    #[arg(short, long, default_value_os_t = default_config_path())]
    pub config: PathBuf,

    /// Bind address for the combined login + WebSocket gateway server.
    /// Overrides `gateway.host`. Only relevant when a `"websocket"` channel
    /// is declared in the config — otherwise nothing binds this port.
    #[arg(long)]
    pub host: Option<String>,

    /// Port for the combined login + WebSocket gateway server. Overrides
    /// `gateway.port`. Only relevant when a `"websocket"` channel is
    /// declared in the config — otherwise nothing binds this port.
    #[arg(long)]
    pub port: Option<u16>,

    /// Directory of pre-built websockets-chat static assets (`index.html`,
    /// `*.js`, `*.wasm`) to serve instead of the UI compiled into this binary.
    /// Falls back to `gateway.webRoot` in the config file, then to the
    /// embedded bundle. Unlike `api --web-root`, this does **not** default
    /// to `./web` (that folder is the REST UI).
    #[arg(long = "web-root")]
    pub web_root: Option<PathBuf>,
}

/// Top-level `rust-bot` command line.
/// Every `rust-bot` subcommand.
/// Arguments of `rust-bot check-config`.
/// Arguments of `rust-bot subagents`.
/// Arguments of `rust-bot agent`.
/// Arguments of `rust-bot api`.
#[derive(Debug, Parser)]
#[command(
    name = "rust-bot",
    version = env!("CARGO_PKG_VERSION"),
    about = "Personal AI agent with workspace tools, interactive console, API, and gateway",
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run the agent from the command line
    Agent(AgentArgs),

    /// Run the API server
    Api(ApiArgs),

    /// Run the gateway server
    Gateway(GatewayArgs),

    /// Serve rust-bot as an Agent Client Protocol (ACP) agent over stdio
    /// (launched by an ACP client such as Zed)
    Acp(AcpArgs),

    /// Perform interactive channel login (e.g. WhatsApp QR pairing)
    Login(LoginArgs),

    /// Initialize configuration and workspace
    Onboard(OnboardArgs),

    /// Generate an Ed25519 keypair and update api.jwt key paths in the config file
    GenerateJwtKeypair(GenerateJwtKeypairArgs),

    /// Mint an EdDSA JWT using the private key path from the config file and generate the corresponding user.
    GenerateJwtToken(GenerateJwtTokenArgs),

    /// List the ACP child agents stored in the workspace
    Subagents(SubagentsArgs),

    /// Validate the config file (JSON, schema, unset `${VAR}` placeholders) and exit
    CheckConfig(CheckConfigArgs),
}

#[derive(Debug, Parser)]
pub struct CheckConfigArgs {
    /// JSON configuration file path
    #[arg(short, long, default_value_os_t = default_config_path())]
    pub config: PathBuf,
}

#[derive(Debug, Parser)]
pub struct SubagentsArgs {
    /// JSON configuration file path
    #[arg(short, long, default_value_os_t = default_config_path())]
    pub config: PathBuf,

    /// Workspace directory (overrides the config; the config file is then not read)
    #[arg(short, long)]
    pub workspace: Option<PathBuf>,
}

#[derive(Debug, Parser)]
pub struct AgentArgs {
    /// Message to send to the agent
    #[arg(short, long)]
    pub message: Option<String>,

    /// Session ID
    #[arg(short, long, default_value = "cli:direct")]
    pub session: String,

    /// Workspace directory
    #[arg(short, long)]
    pub workspace: Option<PathBuf>,

    /// JSON configuration file path
    #[arg(short, long, default_value_os_t = default_config_path())]
    pub config: PathBuf,

    /// Render assistant output as Markdown
    #[arg(long, default_value_t = true, action = clap::ArgAction::SetTrue)]
    #[arg(
        long = "no-markdown",
        action = clap::ArgAction::SetFalse,
        overrides_with = "markdown"
    )]
    pub markdown: bool,

    /// Show rust-bot runtime logs during chat
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    #[arg(
        long = "no-logs",
        action = clap::ArgAction::SetFalse,
        overrides_with = "logs"
    )]
    pub logs: bool,
}

#[derive(Debug, Parser)]
pub struct ApiArgs {
    /// Port to listen on
    #[arg(short, long, default_value = "8900")]
    pub port: Option<u16>,

    /// Bind address to listen on
    #[arg(long, default_value = "0.0.0.0")]
    pub host: Option<String>,

    /// Timeout for API requests
    #[arg(short, long, default_value = "60")]
    pub timeout: u64,

    #[arg(short, long, default_value = "api:default")]
    pub session: String,

    /// Directory of pre-built web-chat static assets (index.html, *.js, *.wasm)
    /// to serve alongside the API. Falls back to `api.webRoot` in the config
    /// file, then to `./web` if that directory exists.
    #[arg(long = "web-root")]
    pub web_root: Option<PathBuf>,

    /// JSON configuration file path (workspace is taken from `agents.workspace`)
    #[arg(short, long, default_value_os_t = default_config_path())]
    pub config: PathBuf,
}

/// Directory name that holds a rust-bot instance (config, workspace, runtime data).
const INSTANCE_DIR_NAME: &str = ".rust-bot";

/// Where `rust-bot onboard` creates the instance directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OnboardMode {
    /// Install in the current directory: `./.rust-bot/` (config and workspace).
    Local,
    /// Install for the current user: `~/.rust-bot/` (config and workspace).
    User,
}

impl OnboardMode {
    /// Instance directory for this mode: `current_dir/.rust-bot` or `home_dir/.rust-bot`.
    ///
    /// # Errors
    ///
    /// `User` mode fails when the home directory is unknown.
    pub(crate) fn instance_dir(
        self,
        current_dir: &std::path::Path,
        home_dir: Option<&std::path::Path>,
    ) -> Result<PathBuf, String> {
        match self {
            OnboardMode::Local => Ok(current_dir.join(INSTANCE_DIR_NAME)),
            OnboardMode::User => home_dir
                .map(|home| home.join(INSTANCE_DIR_NAME))
                .ok_or_else(|| {
                    "Cannot resolve the home directory for `--mode user`; set HOME.".to_string()
                }),
        }
    }
}

/// Default for `--lock-wait-secs`.
pub const DEFAULT_LOCK_WAIT_SECS: u64 = 30;

/// Arguments of `rust-bot acp`.
#[derive(Debug, Parser)]
pub struct AcpArgs {
    /// JSON configuration file path. Use an absolute path when a client launches rust-bot.
    #[arg(short, long, default_value_os_t = default_config_path())]
    pub config: PathBuf,

    /// Workspace directory (overrides the config)
    #[arg(short, long)]
    pub workspace: Option<PathBuf>,

    /// Write rust-bot runtime logs to stderr (or to `RUST_LOG_FILE`). Never to stdout.
    #[arg(long, default_value_t = false)]
    pub logs: bool,

    /// Run as a child of another rust-bot: `--config` is the parent's config and
    /// this file is a JSON merge patch applied on top of it. Only a short
    /// allowlist of settings may be overridden, and only towards less power (see
    /// `config::overlay`). Repeat it for a child of a child: the overlays apply in
    /// the order given, outermost first, each checked against the one before.
    /// Needs `--workspace`: a child has its own home, it must never share its
    /// parent's memory and sessions.
    #[arg(long, requires = "workspace")]
    pub overlay: Vec<PathBuf>,

    /// How long to wait for another `rust-bot acp` process to release the workspace
    /// before failing `initialize` with an error that names it.
    #[arg(long, default_value_t = DEFAULT_LOCK_WAIT_SECS)]
    pub lock_wait_secs: u64,

    /// A folder the file tools must refuse, e.g. the parent's own home when the
    /// project a child works on contains it. Repeatable. Set by a parent rust-bot.
    #[arg(long = "deny-path")]
    pub deny_path: Vec<PathBuf>,
}

/// Arguments of `rust-bot onboard`.
#[derive(Debug, Parser)]
pub struct OnboardArgs {
    /// Install location: `local` = `./.rust-bot/` in the current directory, `user` =
    /// `~/.rust-bot/`. Config and workspace are both placed there. Without it, an
    /// existing local or per-user config is reused, else `./.rust-bot/` is created.
    #[arg(long, value_enum, conflicts_with = "config")]
    pub mode: Option<OnboardMode>,

    /// Workspace directory (default: `workspace/` next to the config file)
    #[arg(short, long)]
    pub workspace: Option<PathBuf>,

    /// JSON configuration file path (cannot be combined with `--mode`)
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Use interactive setup wizard
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub wizard: bool,
}

/// Arguments of `rust-bot login`.
#[derive(Debug, Parser)]
pub struct LoginArgs {
    /// Channel to log in to (e.g. whatsapp)
    #[arg(long)]
    pub channel: String,

    /// Ignore existing credentials and force re-authentication
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub force: bool,

    /// JSON configuration file path
    #[arg(short, long, default_value_os_t = default_config_path())]
    pub config: PathBuf,
}

/// Arguments of `rust-bot generate-jwt-keypair`.
#[derive(Debug, Parser)]
pub struct GenerateJwtKeypairArgs {
    /// Path to the rust-bot JSON configuration file
    #[arg(short, long, default_value_os_t = default_config_path())]
    pub config: PathBuf,

    /// Directory where private_key.pem and public_key.pem are written
    /// (default: `credentials/` next to the config file)
    #[arg(long)]
    pub credentials_dir: Option<PathBuf>,

    /// Overwrite existing key files
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub force: bool,
}

/// Arguments of `rust-bot generate-jwt-token`.
#[derive(Debug, Parser)]
pub struct GenerateJwtTokenArgs {
    /// Path to the rust-bot JSON configuration file
    #[arg(short, long, default_value_os_t = default_config_path())]
    pub config: PathBuf,

    /// Override issuer (defaults to api.jwt.iss from config)
    #[arg(long)]
    pub iss: Option<String>,

    /// Override audience (defaults to api.jwt.aud from config; empty omits claim)
    #[arg(long)]
    pub aud: Option<String>,

    /// Purpose claim marking what this token was minted for (e.g. "webui"
    /// for a WebSocket-connecting WebUI frontend); omitted when unset
    #[arg(long)]
    pub purpose: Option<String>,

    /// Token lifetime in months (default: 6)
    #[arg(long, default_value_t = DEFAULT_EXPIRES_IN_MONTHS)]
    pub expires_in_months: u32,

    /// The email of the user for whom the token is being generated
    #[arg(long, required = true)]
    pub user_email: String,

    /// Password; stored as an Argon2id hash in the users file
    #[arg(long, required = true)]
    pub password: String,

    /// Role stamped into the token and stored with the user: `admin` may
    /// change settings and restart the bot, `user` may not
    #[arg(long, default_value = "user")]
    pub role: UserRole,

    /// Path to the JSON user registry file (email -> token map)
    #[arg(long, required = true)]
    pub users_file: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates `<root>/.rust-bot/config.json` and returns its path.
    fn write_config_under(root: &std::path::Path) -> PathBuf {
        let config_path = root.join(HOME_CONFIG_RELATIVE_PATH);
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "{}").unwrap();
        config_path
    }

    #[test]
    fn default_config_prefers_local_over_home() {
        let local_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let local_config = write_config_under(local_dir.path());
        write_config_under(home_dir.path());
        assert_eq!(
            select_default_config_path(&local_config, Some(home_dir.path())),
            local_config
        );
    }

    #[test]
    fn default_config_falls_back_to_home_when_local_missing() {
        let local_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let home_config = write_config_under(home_dir.path());
        let missing_local = local_dir.path().join(HOME_CONFIG_RELATIVE_PATH);
        assert_eq!(
            select_default_config_path(&missing_local, Some(home_dir.path())),
            home_config
        );
    }

    #[test]
    fn default_config_keeps_local_when_nothing_exists() {
        let local_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let missing_local = local_dir.path().join(HOME_CONFIG_RELATIVE_PATH);
        assert_eq!(
            select_default_config_path(&missing_local, Some(home_dir.path())),
            missing_local
        );
        assert_eq!(
            select_default_config_path(&missing_local, None),
            missing_local
        );
    }

    #[test]
    fn env_override_wins_over_local_and_home_config() {
        let local_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let local_config = write_config_under(local_dir.path());
        write_config_under(home_dir.path());
        assert_eq!(
            resolve_default_config_path(
                Some("/data/config.json".into()),
                &local_config,
                Some(home_dir.path())
            ),
            PathBuf::from("/data/config.json")
        );
    }

    #[test]
    fn empty_or_missing_env_override_falls_back_to_the_search() {
        let local_dir = tempfile::tempdir().unwrap();
        let local_config = write_config_under(local_dir.path());
        for env_override in [None, Some(std::ffi::OsString::new())] {
            assert_eq!(
                resolve_default_config_path(env_override, &local_config, None),
                local_config
            );
        }
    }

    #[test]
    fn onboard_mode_parses_and_conflicts_with_config() {
        let local = Cli::try_parse_from(["rust-bot", "onboard", "--mode", "local"]).unwrap();
        let Commands::Onboard(args) = local.command else {
            panic!("expected the onboard command");
        };
        assert_eq!(args.mode, Some(OnboardMode::Local));

        let user = Cli::try_parse_from(["rust-bot", "onboard", "--mode", "user"]).unwrap();
        let Commands::Onboard(args) = user.command else {
            panic!("expected the onboard command");
        };
        assert_eq!(args.mode, Some(OnboardMode::User));

        assert!(
            Cli::try_parse_from(["rust-bot", "onboard", "--mode", "local", "-c", "x.json"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["rust-bot", "onboard", "--mode", "bogus"]).is_err());
    }

    #[test]
    fn subagents_command_parses_with_and_without_flags() {
        let bare = Cli::try_parse_from(["rust-bot", "subagents"]).unwrap();
        let Commands::Subagents(args) = bare.command else {
            panic!("expected the subagents command");
        };
        assert_eq!(args.workspace, None);

        let with_workspace =
            Cli::try_parse_from(["rust-bot", "subagents", "--workspace", "some/dir"]).unwrap();
        let Commands::Subagents(args) = with_workspace.command else {
            panic!("expected the subagents command");
        };
        assert_eq!(args.workspace, Some(PathBuf::from("some/dir")));
    }

    #[test]
    fn gateway_args_parses_web_root_flag() {
        let args = GatewayArgs::try_parse_from([
            "rust-bot",
            "--config",
            "config.json",
            "--web-root",
            "./websockets-chat-web",
        ])
        .unwrap();
        assert_eq!(args.web_root, Some(PathBuf::from("./websockets-chat-web")));
    }

    #[test]
    fn gateway_args_web_root_defaults_to_none() {
        let args = GatewayArgs::try_parse_from(["rust-bot", "--config", "config.json"]).unwrap();
        assert_eq!(args.web_root, None);
    }

    #[test]
    fn acp_args_default_to_the_standard_config_path() {
        let args = AcpArgs::try_parse_from(["acp"]).unwrap();
        assert_eq!(args.config, default_config_path());
        assert!(!args.logs);
        assert!(args.workspace.is_none());
        assert_eq!(args.lock_wait_secs, DEFAULT_LOCK_WAIT_SECS);
        assert!(args.deny_path.is_empty());
    }

    #[test]
    fn deny_path_can_be_repeated() {
        let args =
            AcpArgs::try_parse_from(["acp", "--deny-path", "/a", "--deny-path", "/b"]).unwrap();
        assert_eq!(
            args.deny_path,
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }
}
