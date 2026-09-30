//! ACP (Agent Client Protocol) support.
//!
//! Milestone 1: rust-bot as a standalone ACP **agent** (`rust-bot acp`) that an
//! ACP client such as Zed launches over stdio.
//!
//! * [`agent_mode`]: the request handlers (`initialize`, `session/new`,
//!   `session/prompt`, `session/cancel`).
//! * [`session_hook`]: structured tool events and fail-closed permissions.
//! * [`mapping`]: pure tool-call to ACP-event rules.
//! * [`registry`]: per-session project folder and running turn.
//! * [`link`]: the hook's view of the client connection.

pub mod agent_mode;
pub mod link;
pub mod mapping;
pub mod registry;
pub mod session_hook;

/// Channel name of every turn that comes from an ACP client.
pub const ACP_CHANNEL: &str = "acp";

/// Tools that cannot work when rust-bot runs headless behind an ACP client.
///
/// * `spawn`: background subagents have no approval hook, run in the wrong
///   folder, cannot be cancelled and lose their result (plan decision 17).
/// * `question`: there is nobody at a terminal to answer it.
/// * `message`: publishes to a channel bus that nothing reads in this mode, so
///   its output would be lost while the model believes it was delivered; the
///   reply reaches the client through `session/update` instead.
pub const HEADLESS_DISABLED_TOOLS: [&str; 3] = ["spawn", "question", "message"];

#[cfg(test)]
mod tests {
    use crate::agent::agent_loop::AgentLoop;
    use std::sync::Arc;

    fn assert_send<T: Send>(_: T) {}

    /// The ACP handlers spawn the agent turn onto the connection's task set, which
    /// requires a `Send` future. This fails to compile if `process_direct` ever
    /// stops being `Send`, instead of failing obscurely inside the handlers.
    #[allow(dead_code)]
    fn process_direct_future_is_send(agent_loop: Arc<AgentLoop>) {
        assert_send(agent_loop.process_direct("hi", None, None, None, None, None, None, None));
    }
}
