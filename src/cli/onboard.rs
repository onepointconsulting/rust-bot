use inquire::Confirm;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anstyle::{AnsiColor, Color, Style};

use crate::channels::websocket::CHANNEL_NAME;
use crate::cli::command_specifications::{OnboardArgs, OnboardMode, validate_onboard_args};
use crate::cli::commands::{CliError, resolve_onboard_targets_from_env};
use crate::cli::instance_layout::InstanceLayout;
use crate::cli::onboard_docker::run_docker_onboard;
use crate::cli::wizard::{
    WebUiOptions, apply_workspace_override, choose_providers, config_model,
    configure_websocket_channel, resolve_onboard_config_path, wizard,
};
use crate::config::loader::{load_config, save_config};
use crate::config::schema::{Config, ModelPresetConfig};
use crate::utils::helpers::{ensure_dir, sync_workspace_templates};
use crate::utils::logo::LOGO;
use crate::utils::path::{display_path, normalize_path_separators};

pub fn run_onboard(args: OnboardArgs) -> Result<(), CliError> {
    validate_onboard_args(&args).map_err(CliError::Other)?;
    if args.mode == Some(OnboardMode::Docker) {
        return run_docker_onboard(&args.docker);
    }
    if args.wizard {
        return wizard(args);
    }
    let targets = resolve_onboard_targets_from_env(&args)?;
    let config_path = resolve_onboard_config_path(targets.config_path);
    let layout = InstanceLayout::for_config_file(&config_path);
    let workspace_override = targets.workspace;

    let config = if config_path.exists() {
        match prompt_existing_config_choice(&config_path) {
            ExistingConfigChoice::Overwrite => {
                let config = new_default_config(&layout, Some(workspace_override));
                save_config(&config, Some(config_path.clone()))?;
                print_onboard_ok(format!(
                    "Config reset to defaults at {}",
                    config_path.display()
                ));
                config
            }
            ExistingConfigChoice::Refresh => {
                // Refresh: keep existing values (including workspace) and only add new fields.
                let config = load_config(Some(config_path.clone()));
                save_config(&config, Some(config_path.clone()))?;
                print_onboard_ok(format!(
                    "Config refreshed at {} (existing values preserved)",
                    config_path.display()
                ));
                config
            }
        }
    } else {
        let config = build_new_instance_config(
            &layout,
            Some(workspace_override),
            &WebUiSetup::Offer(WebUiOptions::interactive(true)),
        )?;
        save_config(&config, Some(config_path.clone()))?;
        print_onboard_ok(format!("Created config at {}", config_path.display()));
        config
    };

    // Prefer the configured workspace path (including any --workspace override).
    let workspace_path = config.workspace_path();
    if !workspace_path.exists() {
        ensure_dir(&workspace_path);
        print_onboard_ok(format!("Created workspace at {}", workspace_path.display()));
    } else {
        ensure_dir(&workspace_path);
    }

    sync_workspace_templates(&workspace_path, false);

    if args.mode == Some(OnboardMode::Local)
        && let Some(instance_dir) = config_path.parent()
    {
        match write_instance_gitignore(instance_dir) {
            Ok(true) => print_onboard_ok(format!(
                "Created {} (keeps secrets out of version control)",
                instance_dir.join(INSTANCE_GITIGNORE_FILE).display()
            )),
            Ok(false) => {}
            Err(err) => print_onboard_err(format!("Failed to write .gitignore: {err}")),
        }
    }

    create_env_file();

    let users_file = websocket_jwt_enabled(&config).then_some(config.api.users_file.as_str());
    print_next_steps(&config_path, users_file);
    Ok(())
}

/// Build the default config for a new or reset instance.
///
/// Every built-in default path (workspace, credentials, users file, MCP presets, Gmail tokens)
/// is moved into the instance described by `layout`, so instances stay isolated. A `workspace`
/// override replaces the workspace path; without one it stays `workspace/` in the instance.
fn new_default_config(layout: &InstanceLayout, workspace: Option<PathBuf>) -> Config {
    let mut config = Config::default();
    config.rebase_default_paths_with(|relative_parts| layout.config_path(relative_parts));
    match workspace {
        Some(workspace) => apply_workspace_override(config, workspace),
        None => config,
    }
}

/// How the web UI (the gateway's `websocket` channel) is set up for a new instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebUiSetup {
    /// Ask "Configure the gateway web UI?" first; configure it with these options on yes.
    Offer(WebUiOptions),
    /// Always configure it with these options (a Docker instance always has the web UI).
    Always(WebUiOptions),
}

/// Ask the onboard questions for a new instance and return its config (not yet saved).
///
/// Asks for the provider and model, adds the default model preset, then sets up the web UI as
/// `web_ui` says. Keys and the users file are written into `layout`'s host folder right away.
pub fn build_new_instance_config(
    layout: &InstanceLayout,
    workspace: Option<PathBuf>,
    web_ui: &WebUiSetup,
) -> Result<Config, CliError> {
    let mut config = new_default_config(layout, workspace);
    choose_providers(&mut config, false)?;
    config_model(&mut config.agents, &config.providers)?;
    create_default_model_presets(&mut config);
    match web_ui {
        WebUiSetup::Offer(options) => configure_web_app(&mut config, layout, options)?,
        WebUiSetup::Always(options) => {
            configure_websocket_channel(&mut config, layout, options)?;
        }
    }
    Ok(config)
}

/// What to do with a config file that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingConfigChoice {
    /// Replace it with defaults; existing values are lost.
    Overwrite,
    /// Keep existing values and only add fields introduced since it was written.
    Refresh,
}

/// Tell the user that `config_path` exists and ask whether to overwrite or refresh it.
///
/// Anything but `y`/`yes` (including a read error) means [`ExistingConfigChoice::Refresh`].
pub fn prompt_existing_config_choice(config_path: &Path) -> ExistingConfigChoice {
    let yellow = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
    let bold = Style::new().bold();
    println!(
        "{}Config already exists at {}{}",
        yellow.render(),
        config_path.display(),
        yellow.render_reset()
    );
    println!(
        "  {}y{} = overwrite with defaults (existing values will be lost)",
        bold.render(),
        bold.render_reset()
    );
    println!(
        "  {}N{} = refresh config, keeping existing values and adding new fields",
        bold.render(),
        bold.render_reset()
    );
    if confirm_overwrite() {
        ExistingConfigChoice::Overwrite
    } else {
        ExistingConfigChoice::Refresh
    }
}

/// Environment variable that holds the API key of provider slot `slot_name` (a camelCase config
/// key such as `azureOpenai`) in a Docker instance: `AZURE_OPENAI_API_KEY`.
pub fn provider_api_key_env_var(slot_name: &str) -> String {
    let mut screaming_snake = String::new();
    for character in slot_name.chars() {
        if character.is_ascii_uppercase() && !screaming_snake.is_empty() {
            screaming_snake.push('_');
        }
        screaming_snake.push(character.to_ascii_uppercase());
    }
    format!("{screaming_snake}_API_KEY")
}

/// Move every provider API key out of `config` and return them by environment variable name.
///
/// Each non-empty key is replaced by a `${<SLOT>_API_KEY}` placeholder, which rust-bot expands
/// from the environment at startup (a Docker instance passes the values in `.env.docker`).
/// Empty keys and keys that already are `${...}` placeholders are left as they are.
pub fn extract_provider_secrets(config: &mut Config) -> BTreeMap<String, String> {
    let mut secrets = BTreeMap::new();
    for (slot_name, provider) in config.providers.named_slots_mut() {
        if provider.api_key.is_empty() || is_env_placeholder(&provider.api_key) {
            continue;
        }
        let variable = provider_api_key_env_var(slot_name);
        let api_key = std::mem::replace(&mut provider.api_key, format!("${{{variable}}}"));
        secrets.insert(variable, api_key);
    }
    secrets
}

/// `true` for a value that is exactly one `${VAR}` placeholder.
fn is_env_placeholder(value: &str) -> bool {
    value.starts_with("${") && value.ends_with('}')
}

/// File name of the ignore file written into a local instance directory.
const INSTANCE_GITIGNORE_FILE: &str = ".gitignore";

/// Write `<instance_dir>/.gitignore` ignoring everything in the directory.
///
/// A local instance keeps API keys and JWT keys inside the project, so the directory must
/// never be committed by accident. An existing file is left untouched.
///
/// Returns `true` when the file was created, `false` when it already existed.
pub(crate) fn write_instance_gitignore(instance_dir: &Path) -> io::Result<bool> {
    let gitignore_path = instance_dir.join(INSTANCE_GITIGNORE_FILE);
    if gitignore_path.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(instance_dir)?;
    std::fs::write(&gitignore_path, "*\n")?;
    Ok(true)
}

/// File name of the binary inside a release archive.
const LOCAL_BIN_FILE: &str = if cfg!(windows) {
    "rust-bot.exe"
} else {
    "rust-bot"
};

/// Binary invocation used in onboarding "next steps" examples.
///
/// When the binary sits in the current directory (an unpacked archive) it is run as
/// `./rust-bot` (Windows: `.\rust-bot.exe`). Otherwise it was installed globally by the
/// one-line installer and is invoked by name from `PATH`.
fn cli_bin() -> &'static str {
    cli_bin_name(Path::new(LOCAL_BIN_FILE).is_file())
}

/// Pure variant of [`cli_bin`]: `local_binary_present` tells whether the binary is in the cwd.
fn cli_bin_name(local_binary_present: bool) -> &'static str {
    match (local_binary_present, cfg!(windows)) {
        (true, true) => r".\rust-bot.exe",
        (true, false) => "./rust-bot",
        (false, _) => "rust-bot",
    }
}

fn print_next_steps(config_path: &Path, users_file: Option<&str>) {
    for line in next_steps_lines(config_path, users_file) {
        println!("{line}");
    }
}

fn next_steps_lines(config_path: &Path, users_file: Option<&str>) -> Vec<String> {
    let bin = cli_bin();
    let config = display_path(config_path);
    let mut lines = vec![
        String::new(),
        format!("{LOGO} is ready!"),
        String::new(),
        "Next steps:".to_string(),
        format!("  1. Change the config file to your needs at {config}"),
        format!("  2. a) One message chat: {bin} agent -c \"{config}\" -m \"Hello!\""),
        format!("  2. b) Interactive chat: {bin} agent -c \"{config}\" "),
        format!("  2. c) API: {bin} api -c \"{config}\""),
        format!("  2. d) Gateway: {bin} gateway -c \"{config}\""),
    ];
    if let Some(users_file) = users_file {
        let users_file = normalize_path_separators(users_file);
        lines.push(format!(
            "  3. Mint a web UI user: {bin} generate-jwt-token -c \"{config}\" --user-email you@example.com --users-file \"{users_file}\" --purpose webui --password <password>",
        ));
    }
    lines
}

pub fn create_env_file() {
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            print_onboard_err(format!("Failed to resolve current directory: {err}"));
            return;
        }
    };

    let logs_dir = cwd.join("logs");
    ensure_dir(&logs_dir);

    let env_file_path = cwd.join(".env");
    if env_file_path.exists() {
        print_onboard_ok(format!(
            "Env file already exists at {}",
            env_file_path.display()
        ));
        return;
    }

    let content = "RUST_LOG=info\nRUST_LOG_FILE=./logs/rust-bot.log\n";
    match std::fs::write(&env_file_path, content) {
        Ok(()) => print_onboard_ok(format!("Env file created at {}", env_file_path.display())),
        Err(err) => print_onboard_err(format!(
            "Failed to create env file at {}: {err}",
            env_file_path.display()
        )),
    }
}

fn confirm_overwrite() -> bool {
    print!("Overwrite? [y/N]: ");
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Print a green check mark followed by `message`.
pub(crate) fn print_onboard_ok(message: impl fmt::Display) {
    let green = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
    println!("{}✓{} {message}", green.render(), green.render_reset());
}

/// Print a yellow exclamation mark followed by `message`.
pub(crate) fn print_onboard_warning(message: impl fmt::Display) {
    let yellow = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));
    println!("{}!{} {message}", yellow.render(), yellow.render_reset());
}

fn print_onboard_err(message: impl fmt::Display) {
    let red = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));
    println!("{}✗{} {message}", red.render(), red.render_reset());
}

/// Name of the model preset created during onboarding: named after the provider whose API key
/// was entered (Together wins over OpenAI), or `"primary"` when neither key is set.
fn default_preset_name(config: &Config) -> &'static str {
    if !config.providers.together.api_key.is_empty() {
        "together-model"
    } else if !config.providers.openai.api_key.is_empty() {
        "openai-model"
    } else {
        "primary"
    }
}

fn create_default_model_presets(config: &mut Config) {
    if config.agents.model.is_empty() || config.agents.provider.is_empty() {
        return;
    }
    let preset_name = default_preset_name(config);
    config.model_presets.insert(
        preset_name.to_string(),
        ModelPresetConfig {
            label: None,
            model: config.agents.model.clone(),
            provider: config.agents.provider.clone(),
            max_tokens: config.agents.max_tokens,
            context_window_tokens: config.agents.context_window_tokens,
            temperature: config.agents.temperature,
            reasoning_effort: config.agents.reasoning_effort.clone(),
        },
    );
    config.agents.model_preset = Some(preset_name.to_string());
}

fn websocket_jwt_enabled(config: &Config) -> bool {
    config
        .channels
        .extra
        .get(CHANNEL_NAME)
        .and_then(|value| value.get("jwt"))
        .and_then(|value| value.get("enabled"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Ask whether to set up the gateway web UI and, on yes, configure it with `options`.
fn configure_web_app(
    config: &mut Config,
    layout: &InstanceLayout,
    options: &WebUiOptions,
) -> Result<(), CliError> {
    let configure_web_app = Confirm::new("Configure the gateway web UI?")
        .with_default(true)
        .with_help_message(
            "Adds a WebSocket channel for rust-bot gateway and can generate a JWT keypair for login",
        )
        .prompt()?;
    if configure_web_app {
        // JWT defaults to on: the web UI's /v1/login will not mint tokens without it.
        // `configure_websocket_channel` separately prompts whether login is *required*
        // (`WebSocketConfig::require_auth`) — a bot can enable JWT for optional sign-in
        // while still allowing guest use.
        configure_websocket_channel(config, layout, options)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_default_config_keeps_every_default_path_next_to_the_config() {
        let instance_dir = std::env::temp_dir()
            .join("rust-bot-onboard-instance")
            .join(".rust-bot");
        let config = new_default_config(
            &InstanceLayout::for_config_file(&instance_dir.join("config.json")),
            Some(instance_dir.join("workspace")),
        );
        let in_instance = |path: &str| Path::new(path).starts_with(&instance_dir);
        assert!(in_instance(&config.agents.workspace));
        assert!(in_instance(&config.api.users_file));
        assert!(in_instance(&config.api.jwt.private_key_path));
        assert!(in_instance(&config.tools.mcp_presets_path));
        assert!(in_instance(&config.tools.gmail.token_cache_path));
    }

    #[test]
    fn new_default_config_for_a_container_records_only_container_paths() {
        let host_dir = tempfile::tempdir().unwrap();
        let layout = InstanceLayout::container(host_dir.path().join("data"), "/data");
        let config = new_default_config(&layout, None);
        assert_eq!(config.agents.workspace, "/data/workspace");
        assert_eq!(config.api.users_file, "/data/users.json");
        assert_eq!(
            config.api.jwt.private_key_path,
            "/data/credentials/private_key.pem"
        );
        assert_eq!(config.tools.mcp_presets_path, "/data/mcp_presets.json");
        let serialized = serde_json::to_string(&config).unwrap();
        let host_prefix = host_dir.path().display().to_string().replace('\\', "\\\\");
        assert!(
            !serialized.contains(&host_prefix),
            "a host path leaked into the container config"
        );
    }

    #[test]
    fn provider_api_key_env_var_is_screaming_snake_case() {
        assert_eq!(provider_api_key_env_var("openrouter"), "OPENROUTER_API_KEY");
        assert_eq!(
            provider_api_key_env_var("azureOpenai"),
            "AZURE_OPENAI_API_KEY"
        );
    }

    #[test]
    fn extract_provider_secrets_moves_keys_into_placeholders() {
        let mut config = Config::default();
        config.providers.openrouter.api_key = "sk-or-secret".to_string();
        config.providers.azure_openai.api_key = "az-secret".to_string();

        let secrets = extract_provider_secrets(&mut config);

        assert_eq!(
            secrets.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("sk-or-secret")
        );
        assert_eq!(
            secrets.get("AZURE_OPENAI_API_KEY").map(String::as_str),
            Some("az-secret")
        );
        assert_eq!(secrets.len(), 2, "empty keys are not exported: {secrets:?}");
        assert_eq!(config.providers.openrouter.api_key, "${OPENROUTER_API_KEY}");
        assert_eq!(
            config.providers.azure_openai.api_key,
            "${AZURE_OPENAI_API_KEY}"
        );
        assert_eq!(config.providers.anthropic.api_key, "");
    }

    #[test]
    fn extract_provider_secrets_leaves_existing_placeholders_alone() {
        let mut config = Config::default();
        config.providers.openrouter.api_key = "${OPENROUTER_API_KEY}".to_string();

        let secrets = extract_provider_secrets(&mut config);

        assert!(secrets.is_empty());
        assert_eq!(config.providers.openrouter.api_key, "${OPENROUTER_API_KEY}");
    }

    #[test]
    fn instance_gitignore_is_created_once_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let instance_dir = dir.path().join(".rust-bot");
        assert!(write_instance_gitignore(&instance_dir).unwrap());
        let gitignore = instance_dir.join(INSTANCE_GITIGNORE_FILE);
        assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "*\n");

        std::fs::write(&gitignore, "custom\n").unwrap();
        assert!(!write_instance_gitignore(&instance_dir).unwrap());
        assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "custom\n");
    }

    #[test]
    fn cli_bin_uses_platform_invocation() {
        #[cfg(windows)]
        assert_eq!(cli_bin_name(true), r".\rust-bot.exe");
        #[cfg(not(windows))]
        assert_eq!(cli_bin_name(true), "./rust-bot");
    }

    #[test]
    fn cli_bin_uses_bare_name_when_installed_globally() {
        assert_eq!(cli_bin_name(false), "rust-bot");
    }

    #[test]
    fn next_steps_commands_use_cli_bin() {
        let config = PathBuf::from(".rust-bot").join("config.json");
        let bin = cli_bin();
        let lines = next_steps_lines(&config, None);
        let commands: Vec<_> = lines.iter().filter(|line| line.contains("  2.")).collect();
        assert_eq!(commands.len(), 4);
        for line in &commands {
            assert!(
                line.contains(&format!("{bin} ")),
                "expected {bin:?} in next-steps line: {line}"
            );
        }
        assert!(commands[0].starts_with(&format!("  2. a) One message chat: {bin} agent")));
    }

    #[test]
    fn next_steps_jwt_line_uses_cli_bin_and_native_separators() {
        let config = PathBuf::from("config.json");
        let lines = next_steps_lines(&config, Some("./.rust-bot/users.json"));
        let jwt_line = lines
            .iter()
            .find(|line| line.contains("generate-jwt-token"))
            .expect("JWT next-steps line");
        assert!(jwt_line.contains(&format!("{} generate-jwt-token", cli_bin())));
        #[cfg(not(windows))]
        assert!(jwt_line.contains("--users-file \"./.rust-bot/users.json\""));
        #[cfg(windows)]
        assert!(jwt_line.contains(r#"--users-file ".\.rust-bot\users.json""#));
    }

    #[test]
    fn default_preset_name_follows_the_configured_provider_key() {
        let mut config = Config::default();
        assert_eq!(default_preset_name(&config), "primary");

        config.providers.openai.api_key = "sk-openai".to_string();
        assert_eq!(default_preset_name(&config), "openai-model");

        config.providers.together.api_key = "tg-key".to_string();
        assert_eq!(default_preset_name(&config), "together-model");
    }
}
