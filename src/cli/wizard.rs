use std::collections::HashMap;
use std::path::PathBuf;

use anstyle::Style;
use inquire::validator::Validation;
use inquire::{Confirm, CustomType, Password, Select, Text};

use crate::cli::onboard::create_env_file;
use crate::{
    cli::{CliError, commands::OnboardArgs, eprint_error},
    config::{
        channels::EmailConfig,
        loader::{load_config, save_config, set_config_path},
        schema::{
            AgentsConfig, ChannelsConfig, Config, McpServerConfig, McpTransportType,
            ModelPresetConfig, OcrProvider, ProviderRetryMode, RESERVED_MODEL_PRESET_NAME,
            ToolsConfig,
        },
    },
    providers::registry::providers,
    utils::{
        helpers::{ensure_dir, expand_tilde_path, sync_workspace_templates},
        path::display_path,
    },
};

const LLM_PROVIDER: &'static str = "LLM Provider";
const CHAT_CHANNELS: &'static str = "Chat Channels";
const AGENT_SETTINGS: &'static str = "Agent Settings";
const MODEL_PRESETS: &'static str = "Model Presets";
const API: &'static str = "API";
const GATEWAY: &'static str = "Gateway";
const TOOLS: &'static str = "Tools";
const SUBAGENT: &'static str = "Subagent";
const VIEW_CONFIGURATION_SUMMARY: &'static str = "View Configuration Summary";
const SAVE_AND_EXIT: &'static str = "Save and Exit";
const EXIT_WITHOUT_SAVING: &'static str = "Exit Without Saving";

const PROVIDER_OPENROUTER: &'static str = "openrouter";
const PROVIDER_EDENAI: &'static str = "edenai";
const PROVIDER_REQUESTY: &'static str = "requesty";
const PROVIDER_ANTHROPIC: &'static str = "anthropic";

const WIZARD_OPTIONS: [&str; 11] = [
    LLM_PROVIDER,
    CHAT_CHANNELS,
    AGENT_SETTINGS,
    MODEL_PRESETS,
    API,
    GATEWAY,
    TOOLS,
    SUBAGENT,
    VIEW_CONFIGURATION_SUMMARY,
    SAVE_AND_EXIT,
    EXIT_WITHOUT_SAVING,
];

const CREATE_MODEL_PRESET: &'static str = "Create model preset";
const SET_DEFAULT_MODEL_PRESET: &'static str = "Set default model preset";
const MODEL_PRESETS_MENU: [&str; 2] = [CREATE_MODEL_PRESET, SET_DEFAULT_MODEL_PRESET];
const REASONING_EFFORT_CHOICES: [&str; 5] = ["none", "low", "medium", "high", "adaptive"];

const CHANNEL_EMAIL: &'static str = "email";
const CHANNEL_OPTIONS_CHOICE: &'static str = "Channel Options";
const CHANNELS: &'static str = "Channels";
const CHANNELS_MENU: [&str; 2] = [CHANNEL_OPTIONS_CHOICE, CHANNELS];
const AVAILABLE_CHANNELS: [&str; 1] = [CHANNEL_EMAIL];
const TRANSCRIPTION_PROVIDERS_NONE: &'static str = "none";
const TRANSCRIPTION_PROVIDERS: [&str; 3] = [TRANSCRIPTION_PROVIDERS_NONE, "groq", "openai"];

const TOOL_GMAIL: &'static str = "gmail";
const TOOL_WEB: &'static str = "web";
const TOOL_EXEC: &'static str = "exec";
const TOOL_OCR: &'static str = "ocr";
const TOOL_DOCX: &'static str = "docx";
const TOOL_MCP: &'static str = "mcp";
const TOOL_IMAGE_GENERATION: &'static str = "image_generation";
const AVAILABLE_TOOLS: [&str; 7] = [
    TOOL_GMAIL,
    TOOL_WEB,
    TOOL_EXEC,
    TOOL_OCR,
    TOOL_DOCX,
    TOOL_MCP,
    TOOL_IMAGE_GENERATION,
];

const WEB_SEARCH_PROVIDERS: [&str; 5] = ["brave", "tavily", "duckduckgo", "searxng", "jina"];
const EXEC_SANDBOX_OPTIONS: [&str; 2] = ["none", "bwrap"];
const OCR_PROVIDERS: [&str; 1] = ["anthropic"];
const MCP_TRANSPORT_OPTIONS: [&str; 4] = ["auto", "stdio", "sse", "streamableHttp"];

pub fn wizard(args: OnboardArgs) -> Result<(), CliError> {
    let config_path = resolve_onboard_config_path(args.config);
    let mut config =
        apply_workspace_override(load_config(Some(config_path.clone())), args.workspace);
    loop {
        let answer = Select::new(
            "What would you like to configure?",
            WIZARD_OPTIONS.to_vec().clone(),
        )
        .prompt()?;
        match answer {
            LLM_PROVIDER => {
                choose_providers(&mut config)?;
            }
            CHAT_CHANNELS => {
                configure_channels_menu(&mut config)?;
            }
            AGENT_SETTINGS => {
                configure_agent_settings(&mut config)?;
            }
            MODEL_PRESETS => {
                configure_model_presets_menu(&mut config)?;
            }
            API => {
                configure_api(&mut config)?;
            }
            GATEWAY => {
                configure_gateway(&mut config)?;
            }
            TOOLS => {
                configure_tools_main_menu(&mut config)?;
            }
            SUBAGENT => {
                configure_subagent(&mut config)?;
            }
            VIEW_CONFIGURATION_SUMMARY => {
                view_configuration_summary(&config)?;
            }
            SAVE_AND_EXIT => {
                save_config(&config, Some(config_path))?;
                let workspace_path = config.workspace_path();
                ensure_dir(&workspace_path);
                sync_workspace_templates(&workspace_path, false);
                create_env_file();
                break;
            }
            EXIT_WITHOUT_SAVING => {
                break;
            }
            _ => {
                eprint_error("Invalid option");
                return Err(CliError::Inquire(
                    inquire::InquireError::InvalidConfiguration(String::from("Invalid option")),
                ));
            }
        };
    }
    Ok(())
}

//Configure LLM providers.
pub fn choose_providers(config: &mut Config) -> Result<Config, CliError> {
    let provider_names = vec![
        PROVIDER_OPENROUTER,
        PROVIDER_ANTHROPIC,
        PROVIDER_EDENAI,
        PROVIDER_REQUESTY,
    ];
    let answer = Select::new(
        "Select a provider to configure API key and endpoint",
        provider_names.to_vec().clone(),
    )
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
        PROVIDER_EDENAI => {
            config.providers.custom.api_key = api_key;
        }
        PROVIDER_REQUESTY => {
            config.providers.custom.api_key = api_key;
        }
        _ => {
            eprint_error("Invalid provider");
            return Err(CliError::Inquire(
                inquire::InquireError::InvalidConfiguration(String::from("Invalid provider")),
            ));
        }
    }
    return Ok(config.clone());
}

pub fn configure_api_base(config: &mut Config, provider_name: &str) -> Result<Config, CliError> {
    let endpoint = Text::new("Enter endpoint")
        .with_help_message(if provider_name == PROVIDER_OPENROUTER {
            "e.g. https://openrouter.ai/api/v1, https://api.edenai.run/v3"
        } else if provider_name == PROVIDER_EDENAI {
            "e.g. https://api.edenai.run/v3"
        } else if provider_name == PROVIDER_ANTHROPIC {
            "e.g. https://api.anthropic.com/v1"
        } else if provider_name == PROVIDER_REQUESTY {
            "e.g. https://router.requesty.ai/v1"
        } else {
            ""
        })
        .prompt()?;
    match provider_name {
        PROVIDER_OPENROUTER => {
            config.providers.openrouter.api_base = Some(endpoint);
        }
        PROVIDER_ANTHROPIC => {
            config.providers.anthropic.api_base = Some(endpoint);
        }
        PROVIDER_EDENAI => {
            config.providers.custom.api_base = Some(endpoint);
        }
        PROVIDER_REQUESTY => {
            config.providers.custom.api_base = Some(endpoint);
        }
        _ => {}
    }
    return Ok(config.clone());
}

/// Prompt for zero or more custom HTTP headers (`extraHeaders`) for a provider.
pub fn configure_extra_headers(
    config: &mut Config,
    provider_name: &str,
) -> Result<Config, CliError> {
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
        PROVIDER_EDENAI => {
            config.providers.custom.extra_headers = headers;
        }
        _ => {
            eprint_error("Invalid provider");
            return Err(CliError::Inquire(
                inquire::InquireError::InvalidConfiguration(String::from("Invalid provider")),
            ));
        }
    }
    Ok(config.clone())
}

pub fn resolve_onboard_config_path(config_path: PathBuf) -> PathBuf {
    let expanded = PathBuf::from(expand_tilde_path(&config_path.to_string_lossy()).as_ref());
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
        display_path(&config_path),
        dim.render_reset()
    );
    config_path
}

pub fn apply_workspace_override(mut config: Config, workspace: PathBuf) -> Config {
    let expanded = PathBuf::from(expand_tilde_path(&workspace.to_string_lossy()).as_ref());
    config.agents.workspace = expanded.to_string_lossy().into_owned();
    config
}

fn configure_channels_menu(config: &mut Config) -> Result<Config, CliError> {
    let selected = Select::new("Channels menu", CHANNELS_MENU.to_vec()).prompt()?;
    match selected {
        CHANNEL_OPTIONS_CHOICE => {
            configure_channels_options(&mut config.channels)?;
        }
        CHANNELS => {
            configure_chat_channel(config)?;
        }
        _ => return Ok(config.clone()),
    }
    Ok(config.clone())
}

fn configure_channels_options(channels: &mut ChannelsConfig) -> Result<(), CliError> {
    channels.streaming = Confirm::new("Enable streaming?")
        .with_default(channels.streaming)
        .with_help_message("Stream the agent's text output to the channel as it is generated")
        .prompt()?;

    channels.send_progress = Confirm::new("Send progress updates?")
        .with_default(channels.send_progress)
        .with_help_message("Send agent progress messages to the channel")
        .prompt()?;

    channels.send_tool_hints = Confirm::new("Send tool-call hints?")
        .with_default(channels.send_tool_hints)
        .with_help_message("Stream tool-call hints such as read_file(\"…\") to the channel")
        .prompt()?;

    channels.send_max_retries = CustomType::<u8>::new("Send max retries")
        .with_default(channels.send_max_retries)
        .with_help_message("Max delivery attempts including the initial send (0–10)")
        .with_error_message("Please enter a number between 0 and 10")
        .with_validator(|v: &u8| {
            if *v <= 10 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Must be between 0 and 10".into()))
            }
        })
        .prompt()?;

    let current_allow_from = channels.allow_from.join(", ");
    let allow_from = Text::new("Allowed sender IDs")
        .with_default(current_allow_from.as_str())
        .with_help_message("Comma-separated sender IDs, or * to allow anyone")
        .prompt()?;
    channels.allow_from = parse_csv_list(&allow_from);
    if channels.allow_from.is_empty() {
        channels.allow_from = vec!["*".to_string()];
    }

    let transcription_idx = TRANSCRIPTION_PROVIDERS
        .iter()
        .position(|p| {
            *p == channels
                .transcription_provider
                .as_deref()
                .unwrap_or(TRANSCRIPTION_PROVIDERS_NONE)
        })
        .unwrap_or(0);

    let transcription_provider = Select::new(
        "Voice transcription provider",
        TRANSCRIPTION_PROVIDERS.to_vec(),
    )
    .with_starting_cursor(transcription_idx)
    .with_help_message("Backend used for voice message transcription; none disables it")
    .prompt()?;

    channels.transcription_provider = if transcription_provider == TRANSCRIPTION_PROVIDERS_NONE {
        None
    } else {
        Some(transcription_provider.to_string())
    };

    Ok(())
}

fn configure_chat_channel(config: &mut Config) -> Result<Config, CliError> {
    let channel =
        Select::new("Select a channel to configure", AVAILABLE_CHANNELS.to_vec()).prompt()?;
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
    config
        .channels
        .extra
        .insert(CHANNEL_EMAIL.to_string(), value);
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

    config_model(agents)?;

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

    let current_effort = agents.reasoning_effort.as_deref().unwrap_or("none");
    let effort_idx = REASONING_EFFORT_CHOICES
        .iter()
        .position(|e| *e == current_effort)
        .unwrap_or(0);
    let effort = Select::new("Reasoning effort", REASONING_EFFORT_CHOICES.to_vec())
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

pub fn config_model(agents: &mut AgentsConfig) -> Result<(), CliError> {
    let model = agents.model.clone();
    agents.model = Text::new("Model")
        .with_default(model.as_str())
        .with_help_message("e.g. anthropic/claude-opus-5 or openai/gpt-5.6")
        .prompt()?;
    Ok(())
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

fn configure_model_presets_menu(config: &mut Config) -> Result<(), CliError> {
    let selected = Select::new("Model presets menu", MODEL_PRESETS_MENU.to_vec()).prompt()?;
    match selected {
        CREATE_MODEL_PRESET => create_model_presets(config)?,
        SET_DEFAULT_MODEL_PRESET => set_default_model_preset(config)?,
        _ => {}
    }
    Ok(())
}

fn create_model_presets(config: &mut Config) -> Result<(), CliError> {
    if !config.model_presets.is_empty() {
        let names: Vec<&str> = config.model_presets.keys().map(|s| s.as_str()).collect();
        println!("Current model presets: {}", names.join(", "));
    }

    let mut provider_choices = vec!["auto".to_string()];
    provider_choices.extend(providers().into_iter().map(|p| p.name));

    loop {
        let add_more = Confirm::new("Add a model preset?")
            .with_default(config.model_presets.is_empty())
            .with_help_message("y to add a preset, n when finished")
            .prompt()?;
        if !add_more {
            break;
        }

        let name = Text::new("Preset name")
            .with_help_message("Short key used to select this preset, e.g. fast or primary")
            .prompt()?;
        let name = name.trim().to_string();
        if name.is_empty() {
            eprint_error("Preset name cannot be empty");
            continue;
        }
        if name == RESERVED_MODEL_PRESET_NAME {
            eprint_error(format!(
                "'{RESERVED_MODEL_PRESET_NAME}' is reserved and cannot be used as a preset name"
            ));
            continue;
        }
        if config.model_presets.contains_key(&name) {
            let overwrite = Confirm::new(&format!("Preset '{name}' already exists. Overwrite?"))
                .with_default(false)
                .prompt()?;
            if !overwrite {
                continue;
            }
        }

        let mut preset: ModelPresetConfig =
            config.model_presets.get(&name).cloned().unwrap_or_default();

        let current_label = preset.label.clone().unwrap_or_default();
        let label = Text::new("Label")
            .with_default(current_label.as_str())
            .with_help_message("Optional human-readable display label; leave empty to omit")
            .prompt()?;
        preset.label = {
            let trimmed = label.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };

        preset.model = Text::new("Model")
            .with_default(preset.model.as_str())
            .with_help_message("e.g. anthropic/claude-opus-5 or openai/gpt-5.6")
            .with_validator(|v: &str| {
                if v.trim().is_empty() {
                    Ok(Validation::Invalid("Model cannot be empty".into()))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt()?;

        let provider_idx = provider_choices
            .iter()
            .position(|p| p == &preset.provider)
            .unwrap_or(0);
        preset.provider = Select::new("Provider", provider_choices.clone())
            .with_starting_cursor(provider_idx)
            .with_help_message("auto detects from model / API key")
            .prompt()?;

        preset.max_tokens = CustomType::<u32>::new("Max tokens")
            .with_default(preset.max_tokens)
            .with_help_message("Maximum completion tokens per response")
            .with_error_message("Please enter a positive integer")
            .prompt()?;

        preset.context_window_tokens = CustomType::<u64>::new("Context window tokens")
            .with_default(preset.context_window_tokens)
            .with_help_message("Total context window size for the model")
            .with_error_message("Please enter a positive integer")
            .prompt()?;

        preset.temperature = CustomType::<f32>::new("Temperature")
            .with_default(preset.temperature)
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

        let current_effort = preset.reasoning_effort.as_deref().unwrap_or("none");
        let effort_idx = REASONING_EFFORT_CHOICES
            .iter()
            .position(|e| *e == current_effort)
            .unwrap_or(0);
        let effort = Select::new("Reasoning effort", REASONING_EFFORT_CHOICES.to_vec())
            .with_starting_cursor(effort_idx)
            .with_help_message("Provider-specific thinking mode; none disables it")
            .prompt()?;
        preset.reasoning_effort = if effort == "none" {
            None
        } else {
            Some(effort.to_string())
        };

        config.model_presets.insert(name, preset);
    }

    Ok(())
}

fn set_default_model_preset(config: &mut Config) -> Result<(), CliError> {
    let mut choices: Vec<String> = vec![RESERVED_MODEL_PRESET_NAME.to_string()];
    let mut preset_names: Vec<String> = config.model_presets.keys().cloned().collect();
    preset_names.sort();
    choices.extend(preset_names);

    let current = config
        .agents
        .model_preset
        .clone()
        .unwrap_or_else(|| RESERVED_MODEL_PRESET_NAME.to_string());
    let current_idx = choices.iter().position(|c| c == &current).unwrap_or(0);

    let selected = Select::new("Default model preset", choices)
        .with_starting_cursor(current_idx)
        .with_help_message(
            "'default' uses Agent Settings' own model/provider fields; named presets must be created first",
        )
        .prompt()?;

    config.agents.model_preset = if selected == RESERVED_MODEL_PRESET_NAME {
        None
    } else {
        Some(selected)
    };

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

    if !tools.ssrf_whitelist.is_empty() {
        println!(
            "Current SSRF whitelist: {}",
            tools.ssrf_whitelist.join(", ")
        );
    }

    let edit_whitelist = Confirm::new("Configure SSRF whitelist?")
        .with_default(!tools.ssrf_whitelist.is_empty())
        .with_help_message("CIDR ranges allowed past private-network blocking (e.g. Tailscale)")
        .prompt()?;
    if edit_whitelist {
        let mut whitelist = Vec::new();
        loop {
            let add_more = Confirm::new("Add a CIDR range to the SSRF whitelist?")
                .with_default(whitelist.is_empty())
                .with_help_message("y to add a range, n when finished")
                .prompt()?;
            if !add_more {
                break;
            }
            let entry = Text::new("CIDR range")
                .with_help_message("Whitelist entries let the web tools reach otherwise-blocked private/internal networks. e.g. 100.64.0.0/10 or 192.168.0.0/16")
                .prompt()?;
            let entry = entry.trim().to_string();
            if entry.is_empty() {
                eprint_error("CIDR range cannot be empty");
                continue;
            }
            if entry.parse::<ipnet::IpNet>().is_err() {
                eprint_error(format!("'{entry}' is not a valid CIDR range"));
                continue;
            }
            whitelist.push(entry);
        }
        tools.ssrf_whitelist = whitelist;
    }

    Ok(())
}

fn configure_tools(config: &mut Config) -> Result<(), CliError> {
    let tools = &mut config.tools;
    let tool = Select::new("Select a tool to configure", AVAILABLE_TOOLS.to_vec()).prompt()?;
    match tool {
        TOOL_GMAIL => {
            configure_email_tool(tools)?;
        }
        TOOL_WEB => {
            configure_web_tool(tools)?;
        }
        TOOL_EXEC => {
            configure_exec_tool(tools)?;
        }
        TOOL_OCR => {
            configure_ocr_tool(tools)?;
        }
        TOOL_DOCX => {
            configure_docx_tool(tools)?;
        }
        TOOL_MCP => {
            configure_mcp_servers(tools)?;
        }
        TOOL_IMAGE_GENERATION => {
            configure_image_generation_tool(tools)?;
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

    if !gmail.enable {
        // No need to configure anything else
        return Ok(());
    }

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

    if !web.enable {
        // No need to configure anything else
        return Ok(());
    }

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

    web.timeout = CustomType::<u32>::new("Web tools HTTP timeout (seconds)")
        .with_default(web.timeout)
        .with_help_message("Per-request timeout for web_search and web_fetch (≥ 1)")
        .with_error_message("Please enter a positive integer")
        .with_validator(|v: &u32| {
            if *v >= 1 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Must be at least 1".into()))
            }
        })
        .prompt()?;

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

fn configure_exec_tool(tools: &mut ToolsConfig) -> Result<(), CliError> {
    let exec = &mut tools.exec;

    exec.enable = Confirm::new("Enable shell exec tool?")
        .with_default(exec.enable)
        .with_help_message("Allows the agent to run shell commands")
        .prompt()?;

    if !exec.enable {
        return Ok(());
    }

    exec.timeout = CustomType::<u32>::new("Exec timeout (seconds)")
        .with_default(exec.timeout)
        .with_help_message("Command execution timeout (≥ 1)")
        .with_error_message("Please enter a positive integer")
        .with_validator(|v: &u32| {
            if *v >= 1 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Must be at least 1".into()))
            }
        })
        .prompt()?;

    let path_append = exec.path_append.clone();
    exec.path_append = Text::new("PATH append")
        .with_default(path_append.as_str())
        .with_help_message(
            "Extra directories appended to PATH inside the subprocess; leave empty for none",
        )
        .prompt()?;

    let sandbox_idx = if exec.sandbox == "bwrap" { 1 } else { 0 };
    let sandbox = Select::new("Sandbox backend", EXEC_SANDBOX_OPTIONS.to_vec())
        .with_starting_cursor(sandbox_idx)
        .with_help_message("none = no sandbox; bwrap = Bubblewrap (Linux)")
        .prompt()?;
    exec.sandbox = if sandbox == "none" {
        String::new()
    } else {
        sandbox.to_string()
    };

    Ok(())
}

fn configure_ocr_tool(tools: &mut ToolsConfig) -> Result<(), CliError> {
    let ocr = &mut tools.ocr;

    ocr.enable = Confirm::new("Enable OCR tool?")
        .with_default(ocr.enable)
        .with_help_message("Extract text from images via an LLM vision model")
        .prompt()?;

    if !ocr.enable {
        return Ok(());
    }

    let provider_idx = OCR_PROVIDERS
        .iter()
        .position(|p| *p == ocr.provider.as_str())
        .unwrap_or(0);
    let provider = Select::new("OCR provider", OCR_PROVIDERS.to_vec())
        .with_starting_cursor(provider_idx)
        .prompt()?;
    ocr.provider = match provider {
        "anthropic" => OcrProvider::Anthropic,
        _ => OcrProvider::Anthropic,
    };

    let api_key = ocr.api_key.clone();
    ocr.api_key = Text::new("OCR API key")
        .with_default(api_key.as_str())
        .with_help_message("Leave empty to use ANTHROPIC_API_KEY at runtime")
        .prompt()?;

    let model = ocr.model.clone();
    ocr.model = Text::new("OCR model")
        .with_default(model.as_str())
        .with_help_message("Vision-capable model for OCR")
        .prompt()?;

    let base_url = ocr.base_url.clone();
    ocr.base_url = Text::new("OCR API base URL")
        .with_default(base_url.as_str())
        .with_help_message("Provider API base URL")
        .prompt()?;

    Ok(())
}

fn configure_docx_tool(tools: &mut ToolsConfig) -> Result<(), CliError> {
    let docx = &mut tools.docx;

    docx.enable = Confirm::new("Enable DOCX conversion tool?")
        .with_default(docx.enable)
        .with_help_message("Convert DOCX documents (e.g. to PDF)")
        .prompt()?;

    Ok(())
}

fn configure_image_generation_tool(tools: &mut ToolsConfig) -> Result<(), CliError> {
    let image_generation = &mut tools.image_generation;

    image_generation.enable = Confirm::new("Enable image generation tool?")
        .with_default(image_generation.enable)
        .with_help_message("Generate images from a text prompt via an image generation API")
        .prompt()?;

    if !image_generation.enable {
        return Ok(());
    }

    let api_key = image_generation.api_key.clone();
    image_generation.api_key = Text::new("Image generation API key")
        .with_default(api_key.as_str())
        .with_help_message("Leave empty to use OPENROUTER_API_KEY at runtime")
        .prompt()?;

    let model = image_generation.model.clone();
    image_generation.model = Text::new("Image generation model")
        .with_default(model.as_str())
        .with_help_message("Image generation model identifier")
        .prompt()?;

    let base_url = image_generation.base_url.clone();
    image_generation.base_url = Text::new("Image generation API base URL")
        .with_default(base_url.as_str())
        .with_help_message("Provider API base URL")
        .prompt()?;

    let size = image_generation.size.clone();
    image_generation.size = Text::new("Default image size")
        .with_default(size.as_str())
        .with_help_message("Optional: a tier (e.g. '1K', '2K') or explicit pixels (e.g. '1024x1024'); leave empty to omit")
        .prompt()?;

    let quality = image_generation.quality.clone();
    image_generation.quality = Text::new("Default image quality")
        .with_default(quality.as_str())
        .with_help_message("Optional: 'auto', 'low', 'medium', or 'high'; leave empty to omit")
        .prompt()?;

    Ok(())
}

fn configure_mcp_servers(tools: &mut ToolsConfig) -> Result<(), CliError> {
    if !tools.mcp_servers.is_empty() {
        let names: Vec<&str> = tools.mcp_servers.keys().map(|s| s.as_str()).collect();
        println!("Current MCP servers: {}", names.join(", "));
    }

    let replace = Confirm::new("Configure MCP servers?")
        .with_default(tools.mcp_servers.is_empty())
        .with_help_message(
            "Adds named MCP server connections; existing entries are replaced if you continue",
        )
        .prompt()?;
    if !replace {
        return Ok(());
    }

    let mut servers = HashMap::new();
    loop {
        let add_more = Confirm::new("Add an MCP server?")
            .with_default(servers.is_empty())
            .with_help_message("y to add a server, n when finished")
            .prompt()?;
        if !add_more {
            break;
        }

        let name = Text::new("MCP server name")
            .with_help_message("Short key used in config, e.g. ems or filesystem")
            .prompt()?;
        let name = name.trim().to_string();
        if name.is_empty() {
            eprint_error("Server name cannot be empty");
            continue;
        }
        if servers.contains_key(&name) {
            eprint_error(format!("Server '{name}' already added"));
            continue;
        }

        let mut server = McpServerConfig::default();

        let transport = Select::new("Transport type", MCP_TRANSPORT_OPTIONS.to_vec())
            .with_help_message(
                "auto detects from command/url; or force stdio / sse / streamableHttp",
            )
            .prompt()?;
        server.transport_type = match transport {
            "stdio" => Some(McpTransportType::Stdio),
            "sse" => Some(McpTransportType::Sse),
            "streamableHttp" => Some(McpTransportType::StreamableHttp),
            _ => None,
        };

        match server.transport_type {
            Some(McpTransportType::Stdio) | None => {
                configure_mcp_stdio(&mut server)?;
                if server.transport_type.is_none() && server.command.is_empty() {
                    configure_mcp_http(&mut server)?;
                }
            }
            Some(McpTransportType::Sse) | Some(McpTransportType::StreamableHttp) => {
                configure_mcp_http(&mut server)?;
            }
        }

        server.tool_timeout = CustomType::<u32>::new("Tool timeout (seconds)")
            .with_default(server.tool_timeout)
            .with_help_message("Seconds before a tool call is cancelled (≥ 1)")
            .with_error_message("Please enter a positive integer")
            .with_validator(|v: &u32| {
                if *v >= 1 {
                    Ok(Validation::Valid)
                } else {
                    Ok(Validation::Invalid("Must be at least 1".into()))
                }
            })
            .prompt()?;

        let enabled_tools = Text::new("Enabled tools")
            .with_default("*")
            .with_help_message("Comma-separated tool names, or * for all")
            .prompt()?;
        server.enabled_tools = parse_csv_list(&enabled_tools);
        if server.enabled_tools.is_empty() {
            server.enabled_tools = vec!["*".to_string()];
        }

        servers.insert(name, server);
    }

    tools.mcp_servers = servers;
    Ok(())
}

fn configure_mcp_stdio(server: &mut McpServerConfig) -> Result<(), CliError> {
    server.command = Text::new("Command")
        .with_default(server.command.as_str())
        .with_help_message("Executable to run, e.g. npx or uvx — leave empty to skip stdio")
        .prompt()?;
    if server.command.trim().is_empty() {
        server.command.clear();
        return Ok(());
    }

    let args = Text::new("Arguments")
        .with_default(&server.args.join(","))
        .with_help_message(
            "Comma-separated args, e.g. -y,@modelcontextprotocol/server-filesystem,.",
        )
        .prompt()?;
    server.args = parse_csv_list(&args);

    loop {
        let add_env = Confirm::new("Add an environment variable?")
            .with_default(false)
            .prompt()?;
        if !add_env {
            break;
        }
        let key = Text::new("Env var name").prompt()?;
        let key = key.trim().to_string();
        if key.is_empty() {
            eprint_error("Env var name cannot be empty");
            continue;
        }
        let value = Text::new(&format!("Value for '{key}'")).prompt()?;
        server.env.insert(key, value);
    }

    Ok(())
}

fn configure_mcp_http(server: &mut McpServerConfig) -> Result<(), CliError> {
    server.url = Text::new("MCP server URL")
        .with_default(server.url.as_str())
        .with_help_message("e.g. https://example.com/mcp")
        .prompt()?;

    loop {
        let add_header = Confirm::new("Add an HTTP header?")
            .with_default(false)
            .prompt()?;
        if !add_header {
            break;
        }
        let header_name = Text::new("Header name")
            .with_help_message("e.g. Authorization")
            .prompt()?;
        let header_name = header_name.trim().to_string();
        if header_name.is_empty() {
            eprint_error("Header name cannot be empty");
            continue;
        }
        let value = Text::new(&format!("Value for '{header_name}'")).prompt()?;
        server.headers.insert(header_name, value);
    }

    Ok(())
}

fn configure_api(config: &mut Config) -> Result<(), CliError> {
    let api = &mut config.api;

    let current_host = api.host.clone();
    api.host = Text::new("API host")
        .with_default(current_host.as_str())
        .with_help_message("Bind address for the OpenAI-compatible API server (e.g. 127.0.0.1)")
        .prompt()?
        .trim()
        .to_string();

    api.port = CustomType::<u16>::new("API port")
        .with_default(api.port)
        .with_help_message("TCP port the API server listens on (1–65535)")
        .with_error_message("Please enter a port between 1 and 65535")
        .with_validator(|v: &u16| {
            if *v >= 1 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Port must be at least 1".into()))
            }
        })
        .prompt()?;

    api.timeout = CustomType::<f64>::new("API request timeout (seconds)")
        .with_default(api.timeout)
        .with_help_message("Per-request timeout in seconds (default 120.0)")
        .with_error_message("Please enter a number ≥ 0")
        .with_validator(|v: &f64| {
            if *v >= 0.0 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Timeout must be ≥ 0".into()))
            }
        })
        .prompt()?;

    Ok(())
}

fn configure_gateway(config: &mut Config) -> Result<(), CliError> {
    let gateway = &mut config.gateway;

    let current_host = gateway.host.clone();
    gateway.host = Text::new("Gateway host")
        .with_default(current_host.as_str())
        .with_help_message("e.g. 0.0.0.0 for all interfaces, or 127.0.0.1 for local-only")
        .prompt()?
        .trim()
        .to_string();

    gateway.port = CustomType::<u16>::new("Gateway port")
        .with_default(gateway.port)
        .with_help_message("TCP port the gateway listens on (1–65535)")
        .with_error_message("Please enter a port between 1 and 65535")
        .with_validator(|v: &u16| {
            if *v >= 1 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Port must be at least 1".into()))
            }
        })
        .prompt()?;

    let heartbeat = &mut gateway.heartbeat;

    heartbeat.enabled = Confirm::new("Enable heartbeat service?")
        .with_default(heartbeat.enabled)
        .with_help_message("Periodic keep-alive / status checks while the gateway is running")
        .prompt()?;

    heartbeat.interval_s = CustomType::<u64>::new("Heartbeat interval (seconds)")
        .with_default(heartbeat.interval_s)
        .with_help_message("Seconds between heartbeats (default 1800 = 30 minutes)")
        .with_error_message("Please enter an integer ≥ 1")
        .with_validator(|v: &u64| {
            if *v >= 1 {
                Ok(Validation::Valid)
            } else {
                Ok(Validation::Invalid("Must be at least 1".into()))
            }
        })
        .prompt()?;

    heartbeat.keep_recent_messages = CustomType::<u32>::new("Heartbeat keep recent messages")
        .with_default(heartbeat.keep_recent_messages)
        .with_help_message("Number of recent messages retained for heartbeat context (≥ 1)")
        .with_error_message("Please enter an integer ≥ 1")
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

fn view_configuration_summary(config: &Config) -> Result<(), CliError> {
    fn pretty(value: &impl serde::Serialize) -> Result<String, CliError> {
        serde_json::to_string_pretty(value).map_err(|e| {
            CliError::Inquire(inquire::InquireError::InvalidConfiguration(e.to_string()))
        })
    }

    println!("Configuration Summary:");
    println!("LLM Provider:\n{}", pretty(&config.providers)?);
    println!("Chat Channels:\n{}", pretty(&config.channels)?);
    println!("Agent Settings:\n{}", pretty(&config.agents)?);
    println!("Model Presets:\n{}", pretty(&config.model_presets)?);
    println!("API:\n{}", pretty(&config.api)?);
    println!("Gateway:\n{}", pretty(&config.gateway)?);
    println!("Tools:\n{}", pretty(&config.tools)?);
    println!("Subagent:\n{}", pretty(&config.subagent)?);
    Ok(())
}

fn configure_subagent(config: &mut Config) -> Result<(), CliError> {
    let subagent = &mut config.subagent;
    subagent.fail_on_tool_error = Confirm::new("Fail the subagent if a tool call fails?")
        .with_default(subagent.fail_on_tool_error)
        .with_help_message(
            "If enabled, a failed tool call aborts the subagent run instead of continuing",
        )
        .prompt()?;
    Ok(())
}
