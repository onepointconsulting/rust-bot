//! Onboarding-wizard dialogues for the background memory machinery: Dream
//! (`agents.defaults.dream`), skills (`tools.skills`) and the Consolidator
//! (the compaction settings in `agents.defaults`).

use std::fmt::Display;

use inquire::validator::{CustomTypeValidator, Validation};
use inquire::{Confirm, CustomType, Select, Text};

use crate::cli::CliError;
use crate::config::schema::{
    AgentsConfig, Config, DreamConfig, MIN_SKILL_CHARS, MIN_SKILL_DESCRIPTION_CHARS,
    SkillsToolConfig,
};

/// Dream preset choice meaning "no preset: use the model override or the main model".
pub(crate) const NO_DREAM_PRESET: &str = "none";

/// Smallest accepted gap, in seconds, between two idle-session scans.
const MIN_IDLE_CHECK_INTERVAL_SECONDS: u32 = 1;

/// Trims `input` and returns it, or `None` when nothing but whitespace is left.
pub(crate) fn non_empty_trimmed(input: &str) -> Option<String> {
    let trimmed = input.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Whether `value` reaches `minimum`, as an inquire validation result whose
/// message names the minimum.
pub(crate) fn minimum_validation<T: PartialOrd + Display>(value: &T, minimum: &T) -> Validation {
    if value >= minimum {
        Validation::Valid
    } else {
        Validation::Invalid(format!("Please enter a whole number ≥ {minimum}").into())
    }
}

/// An inquire validator accepting values of at least `minimum`.
fn at_least<T>(minimum: T) -> impl CustomTypeValidator<T>
where
    T: PartialOrd + Display + Clone + 'static,
{
    move |value: &T| Ok(minimum_validation(value, &minimum))
}

/// Prompts for a whole number of at least `minimum`, starting from `current`.
fn prompt_at_least<T>(label: &str, current: T, minimum: T, help: &str) -> Result<T, CliError>
where
    T: Copy + PartialOrd + Display + std::str::FromStr + ToString + Clone + 'static,
{
    Ok(CustomType::<T>::new(label)
        .with_default(current)
        .with_help_message(help)
        .with_error_message("Please enter a whole number")
        .with_validator(at_least(minimum))
        .prompt()?)
}

/// Prompts for the model's context window, the size the Consolidator keeps
/// sessions under. Shared by Agent Settings and the Consolidator dialogue.
pub(crate) fn prompt_context_window_tokens(agents: &mut AgentsConfig) -> Result<(), CliError> {
    agents.context_window_tokens = prompt_at_least(
        "Context window tokens",
        agents.context_window_tokens,
        1,
        "Total context window size for the model; the Consolidator summarises \
         older messages into history when a session gets close to it",
    )?;
    Ok(())
}

/// The Dream preset choices: [`NO_DREAM_PRESET`] first, then the configured
/// preset names in alphabetical order.
pub(crate) fn dream_preset_choices(config: &Config) -> Vec<String> {
    let mut preset_names: Vec<String> = config.model_presets.keys().cloned().collect();
    preset_names.sort();
    std::iter::once(NO_DREAM_PRESET.to_string())
        .chain(preset_names)
        .collect()
}

/// Maps a chosen Dream preset back to the config value: [`NO_DREAM_PRESET`]
/// clears it.
pub(crate) fn dream_preset_from_choice(choice: String) -> Option<String> {
    (choice != NO_DREAM_PRESET).then_some(choice)
}

/// Index of the current Dream preset in `choices`, falling back to
/// [`NO_DREAM_PRESET`] when unset or no longer configured.
pub(crate) fn current_dream_preset_index(choices: &[String], current: Option<&str>) -> usize {
    let current = current.unwrap_or(NO_DREAM_PRESET);
    choices
        .iter()
        .position(|choice| choice == current)
        .unwrap_or(0)
}

/// Dialogue for `agents.defaults.dream`: schedule, model, batch limits and
/// skill learning.
pub(crate) fn configure_dream(config: &mut Config) -> Result<(), CliError> {
    configure_dream_schedule(&mut config.agents.dream)?;
    configure_dream_model(config)?;

    let dream = &mut config.agents.dream;
    dream.max_batch_size = prompt_at_least(
        "Dream max batch size",
        dream.max_batch_size,
        1,
        "Max history entries per Dream run; a larger backlog is spread over several runs",
    )?;
    dream.max_iterations = prompt_at_least(
        "Dream max iterations",
        dream.max_iterations,
        1,
        "Max tool calls per Dream agent phase (memory edits, and skill writes when learning skills)",
    )?;
    dream.learn_skills = Confirm::new("Let Dream learn skills from history?")
        .with_default(dream.learn_skills)
        .with_help_message(
            "Dream turns reusable procedures into workspace skills; with \
             Skills → require approval on they wait for /skills-review",
        )
        .prompt()?;
    Ok(())
}

/// Asks for the Dream interval, offering to drop a legacy cron schedule that
/// would otherwise override it.
fn configure_dream_schedule(dream: &mut DreamConfig) -> Result<(), CliError> {
    if let Some(cron) = dream.cron.clone() {
        let remove_cron = Confirm::new(&format!(
            "Dream uses the legacy cron schedule `{cron}`, which overrides the interval. Remove it?"
        ))
        .with_default(false)
        .prompt()?;
        if remove_cron {
            dream.cron = None;
        }
    }
    dream.interval_h = prompt_at_least(
        "Dream interval (hours)",
        dream.interval_h,
        1,
        "How often Dream runs on its own; /dream runs it at once",
    )?;
    Ok(())
}

/// Asks which model Dream uses: a named preset, or else a raw model override.
fn configure_dream_model(config: &mut Config) -> Result<(), CliError> {
    let choices = dream_preset_choices(config);
    let dream = &mut config.agents.dream;
    let current_index = current_dream_preset_index(&choices, dream.dream_model_preset.as_deref());
    let selected = Select::new("Dream model preset", choices)
        .with_starting_cursor(current_index)
        .with_help_message("'none' uses the model override below, or else the main model")
        .prompt()?;
    dream.dream_model_preset = dream_preset_from_choice(selected);
    if dream.dream_model_preset.is_some() {
        return Ok(());
    }

    let current_override = dream.model_override.clone().unwrap_or_default();
    let model_override = Text::new("Dream model override")
        .with_default(current_override.as_str())
        .with_help_message("Leave empty to use the main agent model")
        .prompt()?;
    dream.model_override = non_empty_trimmed(&model_override);
    Ok(())
}

/// Dialogue for `tools.skills`: learning during turns, approval and size limits.
pub(crate) fn configure_skills(skills: &mut SkillsToolConfig) -> Result<(), CliError> {
    skills.enabled = Confirm::new("Let the agent create and update skills while it works?")
        .with_default(skills.enabled)
        .with_help_message(
            "Registers skill_manage for normal turns; Dream skill learning does not need this",
        )
        .prompt()?;
    skills.require_approval = Confirm::new("Require approval for skill changes?")
        .with_default(skills.require_approval)
        .with_help_message(
            "Turns: asks before every skill_manage call. Dream: stages proposals for /skills-review",
        )
        .prompt()?;
    skills.max_skill_chars = prompt_at_least(
        "Max skill characters",
        skills.max_skill_chars,
        MIN_SKILL_CHARS,
        "Size limit of a SKILL.md and of each supporting file",
    )?;
    skills.max_description_chars = prompt_at_least(
        "Max description characters",
        skills.max_description_chars,
        MIN_SKILL_DESCRIPTION_CHARS,
        "Size limit of a skill's frontmatter description",
    )?;
    Ok(())
}

/// Dialogue for the Consolidator settings in `agents.defaults`: when session
/// history is summarised into `memory/history.jsonl` for Dream.
pub(crate) fn configure_consolidator(agents: &mut AgentsConfig) -> Result<(), CliError> {
    prompt_context_window_tokens(agents)?;
    agents.session_ttl_minutes = CustomType::<u32>::new("Idle compaction after (minutes)")
        .with_default(agents.session_ttl_minutes)
        .with_help_message("Summarise a session left idle this long; 0 turns this off")
        .with_error_message("Please enter a whole number")
        .prompt()?;
    agents.idle_compact_check_interval_seconds = prompt_at_least(
        "Idle check interval (seconds)",
        agents.idle_compact_check_interval_seconds,
        MIN_IDLE_CHECK_INTERVAL_SECONDS,
        "How often to look for idle sessions to summarise",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::ModelPresetConfig;

    #[test]
    fn non_empty_trimmed_drops_blank_input_and_trims_the_rest() {
        assert_eq!(non_empty_trimmed("   "), None);
        assert_eq!(non_empty_trimmed(""), None);
        assert_eq!(non_empty_trimmed("  gpt  "), Some("gpt".to_string()));
    }

    #[test]
    fn minimum_validation_accepts_the_minimum_and_rejects_below_it() {
        assert_eq!(minimum_validation(&1000usize, &1000), Validation::Valid);
        assert_eq!(minimum_validation(&5000usize, &1000), Validation::Valid);
        match minimum_validation(&999usize, &1000) {
            Validation::Invalid(message) => assert!(format!("{message:?}").contains("1000")),
            Validation::Valid => panic!("999 must be rejected"),
        }
    }

    #[test]
    fn dream_preset_choices_list_none_first_then_sorted_presets() {
        let mut config = Config::default();
        for name in ["zeta", "alpha"] {
            config
                .model_presets
                .insert(name.to_string(), ModelPresetConfig::default());
        }
        assert_eq!(dream_preset_choices(&config), ["none", "alpha", "zeta"]);
    }

    #[test]
    fn dream_preset_choice_none_clears_the_preset() {
        assert_eq!(dream_preset_from_choice(NO_DREAM_PRESET.to_string()), None);
        assert_eq!(
            dream_preset_from_choice("cheap".to_string()),
            Some("cheap".to_string())
        );
    }

    #[test]
    fn current_dream_preset_index_falls_back_to_none() {
        let choices = vec!["none".to_string(), "cheap".to_string()];
        assert_eq!(current_dream_preset_index(&choices, Some("cheap")), 1);
        assert_eq!(current_dream_preset_index(&choices, None), 0);
        assert_eq!(current_dream_preset_index(&choices, Some("deleted")), 0);
    }
}
