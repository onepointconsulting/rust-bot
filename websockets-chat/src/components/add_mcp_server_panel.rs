//! The words of the "Add MCP Server" button and panel of the MCP Servers page. The button
//! and panel themselves are the shared [`super::add_item_panel`] components.

/// Label of the button that opens the panel.
pub const ADD_BUTTON_LABEL: &str = "+ Add MCP Server";

/// Heading of the panel.
pub const ADD_PANEL_TITLE: &str = "Add MCP Server";

/// Key the page's `busy` signal holds while an add request is in flight. It is not a valid
/// server name, so it can never be mistaken for a row.
pub const ADD_BUSY_KEY: &str = "+add-mcp-server";

/// Hint under the name input.
pub const NAME_HINT: &str =
    "Letters, digits, _ - . and spaces. Must not be used by another server.";

/// The message shown once the server `name` was written to the configuration file.
pub fn success_message(name: &str) -> String {
    format!("MCP server '{name}' added. Restart to start it.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_success_message_names_the_server_and_says_a_restart_is_needed() {
        let message = success_message("cms");
        assert!(message.contains("'cms'") && message.contains("Restart"));
    }

    #[test]
    fn the_busy_key_cannot_be_a_server_name() {
        // Server names never contain '+', so the key cannot collide with a row.
        assert!(ADD_BUSY_KEY.contains('+'));
    }
}
