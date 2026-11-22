//! Warnings about tools that were left out at start-up (for example Gmail
//! without credentials). They are shown in the CLI, the gateway banner and the
//! websockets-chat UI instead of stopping the process.

use serde::{Deserialize, Serialize};

/// A tool section that was enabled in the config but could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStartupWarning {
    /// Config section under `tools` (for example `gmail`).
    pub section: String,
    /// User-facing explanation, including how to fix it.
    pub message: String,
}

impl ToolStartupWarning {
    /// A warning for the tool config `section` with the user-facing `message`.
    pub fn new(section: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            section: section.into(),
            message: message.into(),
        }
    }
}
