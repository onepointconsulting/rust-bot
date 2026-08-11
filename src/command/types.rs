use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
pub enum ChatCommand {
    Help,
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
    Goal,
    Cleanup,
    #[serde(rename = "list-sessions", alias = "listsessions")]
    ListSessions,
    ExamplePrompts,
}

impl Default for ChatCommand {
    fn default() -> Self {
        Self::New
    }
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
            "dream" => Ok(ChatCommand::Dream),
            "dream-log" => Ok(ChatCommand::DreamLog),
            "dream-restore" => Ok(ChatCommand::DreamRestore),
            "mcp-list" => Ok(ChatCommand::McpList),
            "mcp-preset" => Ok(ChatCommand::McpPreset),
            "tools" => Ok(ChatCommand::Tools),
            "workspace" => Ok(ChatCommand::Workspace),
            "goal" => Ok(ChatCommand::Goal),
            "cleanup" => Ok(ChatCommand::Cleanup),
            "list-sessions" => Ok(ChatCommand::ListSessions),
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
            ChatCommand::Goal => write!(f, "/goal"),
            ChatCommand::Cleanup => write!(f, "/cleanup"),
            ChatCommand::ListSessions => write!(f, "/list-sessions"),
            ChatCommand::ExamplePrompts => write!(f, "/example-prompts"),
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
            | ChatCommand::Goal => true,
            _ => false,
        }
    }
}
