use std::collections::HashMap;
use std::path::PathBuf;

use anstyle::Style;
use inquire::validator::Validation;
use inquire::{Confirm, CustomType, Password, Select, Text};

use crate::{
    cli::{CliError, commands::OnboardArgs, eprint_error},
    config::{
        channels::EmailConfig,
        loader::{get_config_path, load_config, save_config, set_config_path},
        schema::{AgentsConfig, Config, ProviderRetryMode, ToolsConfig},
    },
    providers::registry::providers,
    utils::helpers::expand_tilde_path,
};

const LLM_PROVIDER: &'static str = "LLM Provider";
const CHAT_CHANNELS: &'static str = "Chat Channels";
const AGENT_SETTINGS: &'static str = "Agent Settings";
const GATEWAY: &'static str = "Gateway";
const TOOLS: &'static str = "Tools";
const VIEW_CONFIGURATION_SUMMARY: &'static str = "View Configuration Summary";
const SAVE_AND_EXIT: &'static str = "Save and Exit";
const EXIT_WITHOUT_SAVING: &'static str = "Exit Without Saving";

const PROVIDER_OPENROUTER: &'static str = "openrouter";
const PROVIDER_ANTHROPIC: &'static str = "anthropic";

const WIZARD_OPTIONS: [&str; 8] = [
    LLM_PROVIDER,
    CHAT_CHANNELS,
    AGENT_SETTINGS,
    GATEWAY,
    TOOLS,
    VIEW_CONFIGURATION_SUMMARY,
    SAVE_AND_EXIT,
    EXIT_WITHOUT_SAVING
];

const CHANNEL_EMAIL: &'static str = "email";
const AVAILABLE_CHANNELS: [&str; 1] = [CHANNEL_EMAIL];

const TOOL_GMAIL: &'static str = "gmail";
const TOOL_WEB: &'static str = "web";
const AVAILABLE_TOOLS: [&str; 2] = [TOOL_GMAIL, TOOL_WEB];

const WEB_SEARCH_PROVIDERS: [&str; 5] = ["brave", "tavily", "duckduckgo", "searxng", "jina"];

pub fn wizard(args: OnboardArgs) -> Result<(), CliError> {
    let config_path = resolve_onboard_config_path(args.config);
    let mut config = apply_workspace_override(load_config(Some(config_path.clone())), args.workspace.as_ref());
    loop {
        let answer = Select::new(
            "What would you like to configure?", WIZARD_OPTIONS.to_vec().clone()
        ).prompt()?;
        match answer {
            LLM_PROVIDER => {
                choose_providers(&mut config)?;
            }
            CHAT_CHANNELS => {
                configure_chat_channel(&mut config)?;
            }
            AGENT_SETTINGS => {
                configure_agent_settings(&mut config)?;
            }
            TOOLS => {
                configure_tools_main_menu(&mut config)?;
            }
            SAVE_AND_EXIT => {
                save_config(&config, Some(config_path))?;
                break;
            }
            EXIT_WITHOUT_SAVING => {
                break;
            }
            _ => {
                eprint_error("Invalid option");
                return Err(CliError::Inquire(inquire::InquireError::InvalidConfiguration(String::from("Invalid option"))));
            }
        };
    }
    Ok(())
}

//Configure LLM providers.
pub fn choose_providers(config: &mut Config) -> Result<Config, CliError> {
    let provider_names = vec![PROVIDER_OPENROUTER, PROVIDER_ANTHROPIC];
    let answer = Select::new(
        "Select a provider to configure API key and endpoint", provider_names.to_vec().clone())
        .prompt_skippable()?;
    match answer {
        Some(provider) => {
            configure_provider(config, &provider)?;
            configure_api_base(config, &provider)?;
            configure_extra_headers(config, &provider)?;
        }
        None => return Ok(config.clone()), // caller re-shows the main menu
    }
    return Ok(config.clone());
}

pub fn configure_provider(config: &mut Config, provider_name: &str) -> Result<Config, CliError> {
    let api_key = Text::new("Enter API key").prompt()?;
    match provider_name {
        PROVIDER_OPENROUTER => {
            config.providers.openrouter.api_key = api_key;
        }
        PROVIDER_ANTHROPIC => {
            config.providers.anthropic.api_key = api_key;
        }
        _ => {
            eprint_error("Invalid provider");
            return Err(CliError::Inquire(inquire::InquireError::InvalidConfiguration(String::from("Invalid provider"))));
        }
    }
    return Ok(config.clone());
}

pub fn configure_api_base(config: &mut Config, provider_name: &str) -> Result<Config, CliError> {
    let endpoint = Text::new("Enter endpoint").prompt()?;
    match provider_name {
        PROVIDER_OPENROUTER => {
            config.providers.openrouter.api_base = Some(endpoint);
        }
        PROVIDER_ANTHROPIC => {
            config.providers.anthropic.api_base = Some(endpoint);
        }
        _ => {}
    }
    return Ok(config.clone());
}

/// Prompt for zero or more custom HTTP headers (`extraHeaders`) for a provider.
pub fn configure_extra_headers(config: &mut Config, provider_name: &str) -> Result<Config, CliError> {
    let mut headers = HashMap::new();
    loop {
        let add_more = Confirm::new("Add an extra HTTP header?")
            .with_default(false)
            .with_help_message("y to add a header, n when finished")
            .prompt()?;
        if !add_more {
            break;
        }
        let name = Text::new("Header name")
            .with_help_message("e.g. APP-Code")
            .prompt()?;
        let name = name.trim().to_string();
        if name.is_empty() {
            eprint_error("Header name cannot be empty");
            continue;
        }
        let value = Text::new(&format!("Value for '{name}'")).prompt()?;
        headers.insert(name, value);
    }

    if headers.is_empty() {
        return Ok(config.clone());
    }

    let headers = Some(headers);
    match provider_name {
        PROVIDER_OPENROUTER => {
            config.providers.openrouter.extra_headers = headers;
        }
        PROVIDER_ANTHROPIC => {
            config.providers.anthropic.extra_headers = headers;
        }
        _ => {
            eprint_error("Invalid provider");
            return Err(CliError::Inquire(inquire::InquireError::InvalidConfiguration(
                String::from("Invalid provider"),
            )));
        }
    }
    Ok(config.clone())
}

pub fn resolve_onboard_config_path(config: Option<PathBuf>) -> PathBuf {
    if let Some(config) = config {
        let expanded = PathBuf::from(expand_tilde_path(&config.to_string_lossy()).as_ref());
        let config_path = if expanded.is_absolute() {
            expanded
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(expanded)
        };
        set_config_path(config_path.clone());
        let dim = Style::new().dimmed();
        println!(
            "{}Using config: {}{}",
            dim.render(),
            config_path.display(),
            dim.render_reset()
        );
        config_path
    } else {
        get_config_path()
    }
}

pub fn apply_workspace_override(mut config: Config, workspace: Option<&PathBuf>) -> Config {
    if let Some(workspace) = workspace {
        config.agents.workspace = workspace.to_string_lossy().into_owned();
    }
    config
}

fn configure_chat_channel(config: &mut Config) -> Result<Config, CliError> {
    let channel = Select::new(
        "Select a channel to configure",
        AVAILABLE_CHANNELS.to_vec(),
    )
    .prompt()?;
    match channel {
        CHANNEL_EMAIL => {
            configure_mail_channel(config)?;
        }
        _ => return Ok(config.clone()),
    }
    Ok(config.clone())
}

fn configure_mail_channel(config: &mut Config) -> Result<Config, CliError> {
    let mut email = EmailConfig::default();

    email.imap_host = Text::new("Enter IMAP host")
        .with_help_message("e.g. imap.gmail.com")
        .prompt()?;
    email.imap_port = CustomType::<u16>::new("Enter IMAP port")
        .with_default(993)
        .with_error_message("Please enter a valid port (0-65535)")
        .prompt()?;
    email.imap_username = Text::new("Enter IMAP username").prompt()?;
    email.imap_password = Password::new("Enter IMAP password")
        .without_confirmation()
        .prompt()?;
    email.imap_mailbox = Text::new("Enter IMAP mailbox")
        .with_default("INBOX")
        .prompt()?;
    email.imap_use_ssl = Confirm::new("Use SSL for IMAP?")
        .with_default(true)
        .prompt()?;

    email.smtp_host = Text::new("Enter SMTP host")
        .with_help_message("e.g. smtp.gmail.com")
        .prompt()?;
    email.smtp_port = CustomType::<u16>::new("Enter SMTP port")
        .with_default(587)
        .with_error_message("Please enter a valid port (0-65535)")
        .prompt()?;
    email.smtp_username = Text::new("Enter SMTP username").prompt()?;
    email.smtp_password = Password::new("Enter SMTP password")
        .without_confirmation()
        .prompt()?;
    email.smtp_use_tls = Confirm::new("Use STARTTLS for SMTP?")
        .with_default(true)
        .prompt()?;
    email.smtp_use_ssl = Confirm::new("Use implicit SSL for SMTP?")
        .with_default(false)
        .prompt()?;
    email.from_address = Text::new("Enter from address")
        .with_help_message("Sender address for outbound mail")
        .prompt()?;

    let allow_from = Text::new("Enter allowed from addresses")
        .with_help_message("Comma-separated list, or * for anyone")
        .with_default("*")
        .prompt()?;
    email.allow_from = parse_csv_list(&allow_from);

    email.enabled = Confirm::new("Enable email channel?")
        .with_default(true)
        .prompt()?;
    email.consent_granted = if email.enabled {
        Confirm::new("Grant consent to read and process mailbox email?")
            .with_default(true)
            .prompt()?
    } else {
        false
    };

    let value = serde_json::to_value(&email).map_err(|e| {
        CliError::Inquire(inquire::InquireError::InvalidConfiguration(e.to_string()))
    })?;
    config.channels.extra.insert(CHANNEL_EMAIL.to_string(), value);
    Ok(config.clone())
}

fn parse_csv_list(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn configure_agent_settings(config: &mut Config) -> Result<Config, CliError> {
    let agents = &mut config.agents;

    let workspace = agents.workspace.clone();
    agents.workspace = Text::new("Workspace path")
        .with_default(workspace.as_str())
        .with_help_message("Directory for agent files, memory, and credentials")
        .prompt()?;

    let model = agents.model.clone();
    agents.model = Text::new("Model")
        .with_default(model.as_str())
        .with_help_message("e.g. anthropic/claude-opus-4-6 or openai/gpt-5.2")
        .prompt()?;

    let mut provider_choices = vec!["auto".to_string()];
    provider_choices.extend(providers().into_iter().map(|p| p.name));
    let provider_idx = provider_choices
        .iter()
        .position(|p| p == &agents.provider)
        .unwrap_or(0);
    agents.provider = Select::new("Provider", provider_choices)
        .with_starting_cursor(provider_idx)
        .with_help_message("auto detects from model / API key")
        .prompt()?;

    agents.max_tokens = CustomType::<u32>::new("Max tokens")
        .with_default(agents.max_tokens)
        .with_help_message("Maximum completion tokens per response")
        .with_error_message("Please enter a positive integer")
        .prompt()?;

    agents.context_window_tokens = CustomType::<u64>::new("Context window tokens")
        .with_default(agents.context_window_tokens)
        .with_help_message("Total context window size for the model")
        .with_error_message("Please enter a positive integer")
        .prompt()?;

    agents.temperature = CustomType::<f32>::new("Temperature")
        .with_default(agents.temperature)
        .with_help_message("0.0 = deterministic, 1.0 = more creative")
        .with_error_message("Please enter a number between 0.0 and 1.0")
        .with_validator(|v: &f32| {
            if (0.0..=1.0).contains(v) {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid(
                    "Temperature must be between 0.0 and 1.0".into(),
                ))
            }
        })
        .prompt()?;

    agents.max_tool_iterations = CustomType::<u32>::new("Max tool iterations")
        .with_default(agents.max_tool_iterations)
        .with_help_message("Maximum tool-call rounds per turn")
        .with_error_message("Please enter a positive integer")
        .prompt()?;

    agents.max_tool_result_chars = CustomType::<u32>::new("Max tool result chars")
        .with_default(agents.max_tool_result_chars)
        .with_help_message("Truncate tool outputs longer than this")
        .with_error_message("Please enter a positive integer")
        .prompt()?;

    let retry_choices = vec![
        ProviderRetryMode::Standard.as_str(),
        ProviderRetryMode::Persistent.as_str(),
    ];
    let retry_idx = match agents.provider_retry_mode {
        ProviderRetryMode::Standard => 0,
        ProviderRetryMode::Persistent => 1,
    };
    let retry = Select::new("Provider retry mode", retry_choices)
        .with_starting_cursor(retry_idx)
        .prompt()?;
    agents.provider_retry_mode = match retry {
        "persistent" => ProviderRetryMode::Persistent,
        _ => ProviderRetryMode::Standard,
    };

    let effort_choices = vec!["none", "low", "medium", "high", "adaptive"];
    let current_effort = agents
        .reasoning_effort
        .as_deref()
        .unwrap_or("none");
    let effort_idx = effort_choices
        .iter()
        .position(|e| *e == current_effort)
        .unwrap_or(0);
    let effort = Select::new("Reasoning effort", effort_choices)
        .with_starting_cursor(effort_idx)
        .with_help_message("Provider-specific thinking mode; none disables it")
        .prompt()?;
    agents.reasoning_effort = if effort == "none" {
        None
    } else {
        Some(effort.to_string())
    };

    let timezone = agents.timezone.clone();
    agents.timezone = Text::new("Timezone")
        .with_default(timezone.as_str())
        .with_help_message("IANA timezone, e.g. UTC or Europe/London")
        .prompt()?;

    configure_dream_settings(agents)?;

    Ok(config.clone())
}

fn configure_dream_settings(agents: &mut AgentsConfig) -> Result<(), CliError> {
    let configure_dream = Confirm::new("Configure Dream memory consolidation?")
        .with_default(true)
        .prompt()?;
    if !configure_dream {
        return Ok(());
    }

    let dream = &mut agents.dream;

    dream.interval_h = CustomType::<u32>::new("Dream interval (hours)")
        .with_default(dream.interval_h)
        .with_help_message("How often Dream consolidates memory (≥ 1)")
        .with_error_message("Please enter an integer ≥ 1")
        .prompt()?;

    let current_override = dream.model_override.clone().unwrap_or_default();
    let model_override = Text::new("Dream model override")
        .with_default(current_override.as_str())
        .with_help_message("Leave empty to use the main agent model")
        .prompt()?;
    dream.model_override = {
        let trimmed = model_override.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };

    dream.max_batch_size = CustomType::<u32>::new("Dream max batch size")
        .with_default(dream.max_batch_size)
        .with_help_message("Max history entries per consolidation run")
        .with_error_message("Please enter a positive integer")
        .prompt()?;

    dream.max_iterations = CustomType::<u32>::new("Dream max iterations")
        .with_default(dream.max_iterations)
        .with_help_message("Max tool calls allowed during Dream Phase 2")
        .with_error_message("Please enter a positive integer")
        .prompt()?;

    Ok(())
}

fn configure_tools_main_menu(config: &mut Config) -> Result<(), CliError> {
    const TOOLS_OPTION: &str = "Configure tools";
    const TOOL_OPTIONS_OPTION: &str = "General tool options";
    let selected = Select::new(
        "Configure tools or general tool options",
        vec![TOOLS_OPTION, TOOL_OPTIONS_OPTION],
    )
    .prompt()?;
    match selected {
        TOOLS_OPTION => configure_tools(config)?,
        TOOL_OPTIONS_OPTION => configure_tool_options(config)?,
        _ => {}
    }
    Ok(())
}

fn configure_tool_options(config: &mut Config) -> Result<(), CliError> {
    let tools = &mut config.tools;
    tools.restrict_to_workspace = Confirm::new("Restrict all tools to workspace folder?")
        .with_default(tools.restrict_to_workspace)
        .with_help_message("Only allow tools to access files in the workspace directory")
        .prompt()?;
    Ok(())
}

fn configure_tools(config: &mut Config) -> Result<(), CliError> {
    let tools = &mut config.tools;
    let tool = Select::new("Select a tool to configure", AVAILABLE_TOOLS.to_vec())
        .prompt()?;
    match tool {
        TOOL_GMAIL => {
            configure_email_tool(tools)?;
        }
        TOOL_WEB => {
            configure_web_tool(tools)?;
        }
        _ => return Ok(()),
    }
    Ok(())
}

fn configure_email_tool(tools: &mut ToolsConfig) -> Result<(), CliError> {
    let gmail = &mut tools.gmail;

    gmail.enable = Confirm::new("Enable Gmail tool?")
        .with_default(gmail.enable)
        .with_help_message("Requires OAuth client_secret.json and token cache")
        .prompt()?;

    let client_secret_path = gmail.client_secret_path.clone();
    gmail.client_secret_path = Text::new("Gmail OAuth client secret path")
        .with_default(client_secret_path.as_str())
        .with_help_message("Path to client_secret.json from Google Cloud Console")
        .prompt()?;

    let token_cache_path = gmail.token_cache_path.clone();
    gmail.token_cache_path = Text::new("Gmail OAuth token cache path")
        .with_default(token_cache_path.as_str())
        .with_help_message("Where refresh/access tokens are stored")
        .prompt()?;

    gmail.max_results = CustomType::<u32>::new("Gmail max results")
        .with_default(gmail.max_results)
        .with_help_message("Maximum messages returned per query (≥ 1)")
        .with_error_message("Please enter a positive integer")
        .with_validator(|v: &u32| {
            if *v >= 1 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Must be at least 1".into()))
            }
        })
        .prompt()?;

    Ok(())
}

fn configure_web_tool(tools: &mut ToolsConfig) -> Result<(), CliError> {
    let web = &mut tools.web;

    web.enable = Confirm::new("Enable web tools?")
        .with_default(web.enable)
        .with_help_message("Web search and related HTTP tools")
        .prompt()?;

    let current_proxy = web.proxy.clone().unwrap_or_default();
    let proxy = Text::new("HTTP/SOCKS5 proxy URL")
        .with_default(current_proxy.as_str())
        .with_help_message("e.g. http://127.0.0.1:7890 — leave empty for none")
        .prompt()?;
    web.proxy = {
        let trimmed = proxy.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };

    let provider_idx = WEB_SEARCH_PROVIDERS
        .iter()
        .position(|p| *p == web.search.provider.as_str())
        .unwrap_or(0);
    web.search.provider = Select::new("Web search provider", WEB_SEARCH_PROVIDERS.to_vec())
        .with_starting_cursor(provider_idx)
        .with_help_message("brave, tavily, duckduckgo, searxng, or jina")
        .prompt()?
        .to_string();

    if web.search.provider == "duckduckgo" {
        web.search.api_key.clear();
    } else {
        let api_key = web.search.api_key.clone();
        web.search.api_key = Text::new("Search provider API key")
            .with_default(api_key.as_str())
            .with_help_message("Required for brave/tavily/jina; leave empty if unused")
            .prompt()?;
    }

    let base_url = web.search.base_url.clone();
    web.search.base_url = Text::new("Search provider base URL")
        .with_default(base_url.as_str())
        .with_help_message("Used by self-hosted backends like SearXNG; leave empty for defaults")
        .prompt()?;

    web.search.max_results = CustomType::<u32>::new("Web search max results")
        .with_default(web.search.max_results)
        .with_help_message("Maximum results returned per query (≥ 1)")
        .with_error_message("Please enter a positive integer")
        .with_validator(|v: &u32| {
            if *v >= 1 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Must be at least 1".into()))
            }
        })
        .prompt()?;

    web.search.timeout = CustomType::<u32>::new("Web search timeout (seconds)")
        .with_default(web.search.timeout)
        .with_help_message("Wall-clock timeout per search (≥ 1)")
        .with_error_message("Please enter a positive integer")
        .with_validator(|v: &u32| {
            if *v >= 1 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Must be at least 1".into()))
            }
        })
        .prompt()?;

    Ok(())
}