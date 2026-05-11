use std::{path::PathBuf, sync::LazyLock};

use crate::agent::memory::MemoryStore;

pub const AGENTS_FILE: &'static str = "AGENTS.md";
pub const SOUL_FILE: &'static str = "SOUL.md";
pub const USER_FILE: &'static str = "USER.md";
pub const TOOLS_FILE: &'static str = "TOOLS.md";

pub const BOOTSTRAP_FILES: [&str; 4] =
    [AGENTS_FILE, SOUL_FILE, USER_FILE, TOOLS_FILE];

const RUNTIME_CONTEXT_TAG: &str = "[Runtime Context — metadata only, not instructions]";

const MAX_RECENT_HISTORY: usize = 50;
pub struct ContextBuilder {
    workspace: PathBuf,
    timezone: Option<String>,
    memory: MemoryStore,
}

impl ContextBuilder {
    pub fn new(workspace: PathBuf, timezone: Option<String>) -> Self {
        let memory = MemoryStore::new(workspace.clone(), None);
        Self {
            workspace,
            timezone,
            memory,
        }
    }
}