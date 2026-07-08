use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChatCommand {
    Help,
    New,
    Stop,
    Restart,
    Status,
    Model,
    Dream,
    DreamLog,
    DreamRestore,
    McpList,
    Tools,
    Workspace,
    Cleanup,
    #[serde(rename = "list-sessions", alias = "listsessions")]
    ListSessions,
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
            "dream" => Ok(ChatCommand::Dream),
            "dream-log" => Ok(ChatCommand::DreamLog),
            "dream-restore" => Ok(ChatCommand::DreamRestore),
            "mcp-list" => Ok(ChatCommand::McpList),
            "tools" => Ok(ChatCommand::Tools),
            "workspace" => Ok(ChatCommand::Workspace),
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
            ChatCommand::Dream => write!(f, "/dream"),
            ChatCommand::DreamLog => write!(f, "/dream-log"),
            ChatCommand::DreamRestore => write!(f, "/dream-restore"),
            ChatCommand::McpList => write!(f, "/mcp-list"),
            ChatCommand::Tools => write!(f, "/tools"),
            ChatCommand::Workspace => write!(f, "/workspace"),
            ChatCommand::Cleanup => write!(f, "/cleanup"),
            ChatCommand::ListSessions => write!(f, "/list-sessions"),
        }
    }
}