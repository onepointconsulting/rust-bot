use inquire::Confirm;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anstyle::{AnsiColor, Color, Style};

use crate::cli::commands::{CliError, OnboardArgs};
use crate::cli::wizard::{
    apply_workspace_override, choose_providers, config_model, configure_websocket_channel,
    resolve_onboard_config_path, wizard,
};
use crate::config::loader::{load_config, save_config};
use crate::config::schema::{Config, ModelPresetConfig};
use crate::utils::helpers::{ensure_dir, sync_workspace_templates};
use crate::utils::logo::LOGO;
use crate::utils::path::{display_path, normalize_path_separators};

pub fn run_onboard(args: OnboardArgs) -> Result<(), CliError> {
    if args.wizard {
        return wizard(args);
    }
    let config_path = resolve_onboard_config_path(args.config);

    let config = if config_path.exists() {
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
            let config = apply_workspace_override(Config::default(), args.workspace);
            save_config(&config, Some(config_path.clone()))?;
            print_onboard_ok(format!(
                "Config reset to defaults at {}",
                config_path.display()
            ));
            config
        } else {
            // Refresh: keep existing values (including workspace) and only add new fields.
            let config = load_config(Some(config_path.clone()));
            save_config(&config, Some(config_path.clone()))?;
            print_onboard_ok(format!(
                "Config refreshed at {} (existing values preserved)",
                config_path.display()
            ));
            config
        }
    } else {
        let mut config = apply_workspace_override(Config::default(), args.workspace);
        choose_providers(&mut config)?;
        config_model(&mut config.agents)?;
        create_default_model_presets(&mut config);
        configure_web_app(&mut config, config_path.clone())?;
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

    create_env_file();

    let users_file = websocket_jwt_enabled(&config).then_some(config.api.users_file.as_str());
    print_next_steps(&config_path, users_file);
    Ok(())
}

/// Binary name used in onboarding "next steps" examples.
///
/// Linux/macOS archives are run from the extract directory (`./rust-bot`).
/// Windows archives use `.\rust-bot.exe`.
fn cli_bin() -> &'static str {
    if cfg!(windows) {
        r".\rust-bot.exe"
    } else {
        "./rust-bot"
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

fn print_onboard_ok(message: impl fmt::Display) {
    let green = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green)));
    println!("{}✓{} {message}", green.render(), green.render_reset());
}

fn print_onboard_err(message: impl fmt::Display) {
    let red = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red)));
    println!("{}✗{} {message}", red.render(), red.render_reset());
}

fn create_default_model_presets(config: &mut Config) {
    if config.agents.model.is_empty() || config.agents.provider.is_empty() {
        return;
    }
    config.model_presets.insert(
        "primary".to_string(),
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
    config.agents.model_preset = Some("primary".to_string());
}

fn websocket_jwt_enabled(config: &Config) -> bool {
    config
        .channels
        .extra
        .get("websocket")
        .and_then(|value| value.get("jwt"))
        .and_then(|value| value.get("enabled"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn configure_web_app(config: &mut Config, config_path: PathBuf) -> Result<(), CliError> {
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
        configure_websocket_channel(config, config_path, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_bin_uses_platform_invocation() {
        #[cfg(windows)]
        assert_eq!(cli_bin(), r".\rust-bot.exe");
        #[cfg(not(windows))]
        assert_eq!(cli_bin(), "./rust-bot");
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
        #[cfg(not(windows))]
        assert!(commands[0].starts_with("  2. a) One message chat: ./rust-bot agent"));
        #[cfg(windows)]
        assert!(commands[0].starts_with(r"  2. a) One message chat: .\rust-bot.exe agent"));
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
}
