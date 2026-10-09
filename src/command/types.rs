use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const DREAM_JOB_NAME: &str = "dream";
pub const EVICT_STALE_SESSIONS_JOB_NAME: &str = "evict_stale_sessions";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommandLifecycle {
    SideChannel,
    FinalizeActiveTurn,
    StopActiveTurn,
    AgentTurn,
    AgentTurnWithArgs,
}

impl std::fmt::Display for CommandLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandLifecycle::SideChannel => write!(f, "side_channel"),
            CommandLifecycle::FinalizeActiveTurn => write!(f, "finalize_active_turn"),
            CommandLifecycle::StopActiveTurn => write!(f, "stop_active_turn"),
            CommandLifecycle::AgentTurn => write!(f, "agent_turn"),
            CommandLifecycle::AgentTurnWithArgs => write!(f, "agent_turn_with_args"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ChatCommand {
    Help,
    #[default]
    New,
    Stop,
    Restart,
    Status,
    ModelPreset,
    Model,
    ModelPresets,
    Dream,
    DreamLog,
    DreamRestore,
    McpList,
    McpPreset,
    Tools,
    Workspace,
    Mode,
    Goal,
    Cleanup,
    #[serde(rename = "list-sessions", alias = "listsessions")]
    ListSessions,
    ExamplePrompts,
    #[serde(alias = "list-subagents")]
    Subagents,
}


impl std::str::FromStr for ChatCommand {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "help" => Ok(ChatCommand::Help),
            "new" => Ok(ChatCommand::New),
            "stop" => Ok(ChatCommand::Stop),
            "restart" => Ok(ChatCommand::Restart),
            "status" => Ok(ChatCommand::Status),
            "model" => Ok(ChatCommand::Model),
            "model-preset" => Ok(ChatCommand::ModelPreset),
            "model-presets" => Ok(ChatCommand::ModelPresets),
            DREAM_JOB_NAME => Ok(ChatCommand::Dream),
            "dream-log" => Ok(ChatCommand::DreamLog),
            "dream-restore" => Ok(ChatCommand::DreamRestore),
            "mcp-list" => Ok(ChatCommand::McpList),
            "mcp-preset" => Ok(ChatCommand::McpPreset),
            "tools" => Ok(ChatCommand::Tools),
            "workspace" => Ok(ChatCommand::Workspace),
            "mode" => Ok(ChatCommand::Mode),
            "goal" => Ok(ChatCommand::Goal),
            "cleanup" => Ok(ChatCommand::Cleanup),
            "list-sessions" => Ok(ChatCommand::ListSessions),
            "subagents" | "list-subagents" => Ok(ChatCommand::Subagents),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for ChatCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatCommand::Help => write!(f, "/help"),
            ChatCommand::New => write!(f, "/new"),
            ChatCommand::Stop => write!(f, "/stop"),
            ChatCommand::Restart => write!(f, "/restart"),
            ChatCommand::Status => write!(f, "/status"),
            ChatCommand::Model => write!(f, "/model"),
            ChatCommand::ModelPreset => write!(f, "/model-preset"),
            ChatCommand::Dream => write!(f, "/dream"),
            ChatCommand::DreamLog => write!(f, "/dream-log"),
            ChatCommand::DreamRestore => write!(f, "/dream-restore"),
            ChatCommand::McpList => write!(f, "/mcp-list"),
            ChatCommand::McpPreset => write!(f, "/mcp-preset"),
            ChatCommand::Tools => write!(f, "/tools"),
            ChatCommand::Workspace => write!(f, "/workspace"),
            ChatCommand::Mode => write!(f, "/mode"),
            ChatCommand::Goal => write!(f, "/goal"),
            ChatCommand::Cleanup => write!(f, "/cleanup"),
            ChatCommand::ListSessions => write!(f, "/list-sessions"),
            ChatCommand::ExamplePrompts => write!(f, "/example-prompts"),
            ChatCommand::Subagents => write!(f, "/subagents"),
            ChatCommand::ModelPresets => write!(f, "/model-presets"),
        }
    }
}

impl ChatCommand {
    pub const fn lifecycle(self) -> Option<CommandLifecycle> {
        match self {
            ChatCommand::New => Some(CommandLifecycle::FinalizeActiveTurn),
            ChatCommand::Stop => Some(CommandLifecycle::StopActiveTurn),
            ChatCommand::Goal => Some(CommandLifecycle::AgentTurnWithArgs),
            _ => None,
        }
    }

    pub const fn accepts_args(self) -> bool {
        match self {
            ChatCommand::Model
            | ChatCommand::ModelPreset
            | ChatCommand::DreamLog
            | ChatCommand::DreamRestore
            | ChatCommand::McpPreset
            | ChatCommand::Workspace
            | ChatCommand::Mode
            | ChatCommand::Goal
            | ChatCommand::Subagents => true,
            _ => false,
        }
    }
}

/// Which sections `/subagents` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentsFilter {
    /// Spawn tasks only.
    Spawn,
    /// ACP child agents only.
    Acp,
    /// Both sections (the default).
    All,
}

impl SubagentsFilter {
    /// The usage line shown for an unknown argument.
    pub const USAGE: &'static str = "Usage: /subagents [spawn|acp|all]";

    /// Parse the text after `/subagents`: empty means [`SubagentsFilter::All`];
    /// otherwise one token, trimmed and case-insensitive. Anything else is an
    /// error carrying [`Self::USAGE`].
    pub fn parse(args: &str) -> Result<Self, &'static str> {
        match args.trim().to_lowercase().as_str() {
            "" | "all" => Ok(SubagentsFilter::All),
            "spawn" => Ok(SubagentsFilter::Spawn),
            "acp" => Ok(SubagentsFilter::Acp),
            _ => Err(Self::USAGE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn subagents_parses_with_its_alias_and_displays_canonically() {
        assert_eq!(ChatCommand::from_str("subagents"), Ok(ChatCommand::Subagents));
        assert_eq!(
            ChatCommand::from_str("list-subagents"),
            Ok(ChatCommand::Subagents)
        );
        assert_eq!(ChatCommand::Subagents.to_string(), "/subagents");
    }

    #[test]
    fn subagents_accepts_args_and_has_no_lifecycle() {
        assert!(ChatCommand::Subagents.accepts_args());
        assert_eq!(ChatCommand::Subagents.lifecycle(), None);
    }

    #[test]
    fn subagents_serde_name_and_alias() {
        assert_eq!(
            serde_json::to_string(&ChatCommand::Subagents).unwrap(),
            "\"subagents\""
        );
        assert_eq!(
            serde_json::from_str::<ChatCommand>("\"list-subagents\"").unwrap(),
            ChatCommand::Subagents
        );
    }

    #[test]
    fn filter_defaults_to_all_and_ignores_case_and_whitespace() {
        assert_eq!(SubagentsFilter::parse(""), Ok(SubagentsFilter::All));
        assert_eq!(SubagentsFilter::parse("   "), Ok(SubagentsFilter::All));
        assert_eq!(SubagentsFilter::parse("ALL"), Ok(SubagentsFilter::All));
        assert_eq!(SubagentsFilter::parse(" Spawn "), Ok(SubagentsFilter::Spawn));
        assert_eq!(SubagentsFilter::parse("acp"), Ok(SubagentsFilter::Acp));
    }

    #[test]
    fn filter_rejects_unknown_tokens_with_usage() {
        assert_eq!(SubagentsFilter::parse("garbage"), Err(SubagentsFilter::USAGE));
        assert_eq!(SubagentsFilter::parse("acp spawn"), Err(SubagentsFilter::USAGE));
    }
}
