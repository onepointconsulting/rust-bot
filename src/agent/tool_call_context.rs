//! Identity of the tool call that is currently executing.
//!
//! `Tool::execute` receives only the call's arguments. A tool that must act
//! idempotently under crash recovery (`spawn`) needs to know *which* call it
//! is serving, so the runner scopes `(turn_id, call_id)` around `execute` in a
//! task-local, the same way the ambient workspace scope is carried.

use std::future::Future;

tokio::task_local! {
    static CURRENT_TOOL_CALL: ToolCallIdentity;
}

/// Journal identity of a tool call: the turn it belongs to and the provider's call id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallIdentity {
    pub turn_id: String,
    pub call_id: String,
}

/// Run `future` with `identity` as the current tool call.
pub async fn with_tool_call<F: Future>(identity: ToolCallIdentity, future: F) -> F::Output {
    CURRENT_TOOL_CALL.scope(identity, future).await
}

/// The tool call being executed on this task, if the runner scoped one.
pub fn current_tool_call() -> Option<ToolCallIdentity> {
    CURRENT_TOOL_CALL.try_with(Clone::clone).ok()
}

/// Stable 8-hex-digit id derived from a tool call's identity.
///
/// FNV-1a, so the value is the same in every process and Rust version (a
/// replay after a crash must produce the id the first execution used).
pub fn deterministic_task_id(identity: &ToolCallIdentity) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for byte in identity
        .turn_id
        .bytes()
        .chain(std::iter::once(0))
        .chain(identity.call_id.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:08x}", hash >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(turn: &str, call: &str) -> ToolCallIdentity {
        ToolCallIdentity {
            turn_id: turn.into(),
            call_id: call.into(),
        }
    }

    #[tokio::test]
    async fn the_identity_is_visible_inside_the_scope_only() {
        assert_eq!(current_tool_call(), None);
        let seen = with_tool_call(identity("t", "c"), async { current_tool_call() }).await;
        assert_eq!(seen, Some(identity("t", "c")));
        assert_eq!(current_tool_call(), None);
    }

    #[test]
    fn the_task_id_is_stable_and_depends_on_turn_and_call() {
        let id = deterministic_task_id(&identity("turn-1", "call-1"));
        assert_eq!(id, deterministic_task_id(&identity("turn-1", "call-1")));
        assert_eq!(id.len(), 8);
        assert_ne!(id, deterministic_task_id(&identity("turn-2", "call-1")));
        assert_ne!(id, deterministic_task_id(&identity("turn-1", "call-2")));
        // The separator keeps ("ab","c") apart from ("a","bc").
        assert_ne!(
            deterministic_task_id(&identity("ab", "c")),
            deterministic_task_id(&identity("a", "bc"))
        );
    }

    #[test]
    fn the_task_id_has_a_pinned_value() {
        // Guards against an accidental change of the algorithm between releases.
        assert_eq!(deterministic_task_id(&identity("t", "c")), "566dd719");
    }
}
