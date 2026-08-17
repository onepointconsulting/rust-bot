//! Owner-keyed registry of in-flight WebSocket turns per chat. Port of
//! `nanobot/session/webui_turns.py`'s turn-tracking surface (the
//! `_WEBSOCKET_ACTIVE_TURNS` map and its accessors), scoped to just that —
//! title generation, `WebuiTurnRoutePolicy`, and `WebuiTurnCoordinator` are
//! not part of this port (see the plan for why: they each depend on pieces
//! that don't exist in rust-bot yet — history visibility filtering, a
//! `FallbackModelObserver`, and above all a `RuntimeEventBus` to drive
//! registration automatically from real turn lifecycle events, none of
//! which exist in this codebase today). The `LLMRuntime` concept nanobot's
//! title generation depends on does have a counterpart here — see
//! `ModelRuntime`/`ModelRuntimeResolver` in `agent/model_runtime.rs`.
//!
//! Nothing calls [`WebsocketTurnRegistry`]'s methods yet — wiring it into
//! `WsShared`/`EnvelopeDispatchContext` is a separate follow-up, same as
//! `is_valid_chat_id`/`WorkspaceRequestHandler` before their callers existed.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

/// Wall-clock seconds since the Unix epoch — matches nanobot's `time.time()`
/// exactly. Deliberately not `std::time::Instant`: `started_at` is sent
/// as-is to the browser (`send_goal_status`'s `started_at` field) so it can
/// render "running since ..."; `Instant` is monotonic-only and has no epoch,
/// so it can't produce a value like that.
fn now_wall_clock_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// One owner's record of an in-flight WebSocket turn for a chat. Mirrors
/// nanobot's `_WebsocketTurn` (`session/webui_turns.py:64-68`).
#[derive(Clone)]
struct OwnedTurn {
    owner: String,
    started_at: f64,
    turn_id: Option<String>,
    transcript_persistence_failed: bool,
}

/// All in-flight turn owners for one chat, oldest-first / latest-last —
/// mirrors the insertion order nanobot's `_sync_websocket_turn_projection`
/// relies on via `next(reversed(turns))`. A `Vec`, not a map: per-chat owner
/// counts are always tiny (usually 1, rarely 2-3), so no ordered-map
/// dependency is warranted.
#[derive(Default, Clone)]
struct ChatTurns(Vec<OwnedTurn>);

impl ChatTurns {
    fn latest(&self) -> Option<&OwnedTurn> {
        self.0.last()
    }

    fn find(&self, owner: &str) -> Option<&OwnedTurn> {
        self.0.iter().find(|t| t.owner == owner)
    }

    fn remove(&mut self, owner: &str) {
        self.0.retain(|t| t.owner != owner);
    }

    /// Move-to-latest-or-insert: drop any existing entry for this owner, then
    /// push. Mirrors the pop+reinsert dance in nanobot's
    /// `publish_turn_run_status` ("Re-registration makes this owner the
    /// latest projection").
    fn upsert_latest(&mut self, turn: OwnedTurn) {
        self.remove(&turn.owner);
        self.0.push(turn);
    }
}

/// Per-chat registry of in-flight WebSocket turns, keyed by owner. Mirrors
/// nanobot's `_WEBSOCKET_ACTIVE_TURNS` (`session/webui_turns.py:73`) — an
/// instance, not a process-global static, matching this codebase's existing
/// convention (e.g. `channels::websocket::registry::ConnectionRegistry`)
/// rather than nanobot's module-level dicts. Entirely in-process,
/// non-persistent state, dropped on restart, exactly like the Python source.
#[derive(Default, Clone)]
pub struct WebsocketTurnRegistry {
    turns: HashMap<String, ChatTurns>,
}

impl WebsocketTurnRegistry {
    /// Unconditionally (re-)register `owner` as active for `chat_id`, moving
    /// it to the latest position. Mirrors the inline registration inside
    /// nanobot's `publish_turn_run_status` (`webui_turns.py:363-373`) —
    /// always admits, regardless of how many other owners are already
    /// active for this chat.
    pub fn start_turn(&mut self, chat_id: &str, owner: &str, turn_id: Option<&str>) {
        self.turns.entry(chat_id.to_string()).or_default().upsert_latest(OwnedTurn {
            owner: owner.to_string(),
            started_at: now_wall_clock_secs(),
            turn_id: turn_id.map(str::to_string),
            transcript_persistence_failed: false,
        });
    }

    /// Admit a new turn only if `chat_id` has no active owner yet; mints and
    /// registers a fresh owner id (`Uuid::new_v4().simple()`, matching
    /// nanobot's `uuid4().hex`) and returns it, or `None` if the chat is
    /// already running. Mirrors `register_queued_websocket_turn_if_idle`
    /// (`webui_turns.py:252-265`).
    pub fn register_queued_turn_if_idle(
        &mut self,
        chat_id: &str,
        turn_id: Option<&str>,
    ) -> Option<String> {
        if self.websocket_turn_wall_started_at(chat_id).is_some() {
            return None;
        }
        let owner = Uuid::new_v4().simple().to_string();
        self.start_turn(chat_id, &owner, turn_id);
        Some(owner)
    }

    /// The wall-clock moment the *latest* owner's turn began, or `None` if
    /// idle. Mirrors `websocket_turn_wall_started_at`; callers check
    /// `.is_some()` for a `chat_running` boolean — exactly nanobot's own
    /// `websocket_turn_wall_started_at(cid) is not None` idiom, so no
    /// separate boolean wrapper is added here.
    pub fn websocket_turn_wall_started_at(&self, chat_id: &str) -> Option<f64> {
        self.turns.get(chat_id).and_then(ChatTurns::latest).map(|t| t.started_at)
    }

    /// The latest owner's WebUI-supplied turn identity, if it has one.
    /// Mirrors `websocket_turn_id` — returns `None` even while a turn is
    /// active if that owner's `turn_id` happens to be `None` (matches
    /// nanobot's own projection-clearing behavior in that case).
    pub fn websocket_turn_id(&self, chat_id: &str) -> Option<String> {
        self.turns.get(chat_id).and_then(ChatTurns::latest).and_then(|t| t.turn_id.clone())
    }

    /// Whether `owner` is still registered for `chat_id` with exactly this
    /// `turn_id`. Mirrors `websocket_turn_owner_is_registered`.
    pub fn owner_is_registered(&self, chat_id: &str, owner: &str, turn_id: Option<&str>) -> bool {
        self.turns
            .get(chat_id)
            .and_then(|c| c.find(owner))
            .is_some_and(|t| t.turn_id.as_deref() == turn_id)
    }

    /// Whether one active owner (`owner`, or the latest if `None`) has an
    /// incomplete canonical transcript. Mirrors
    /// `websocket_turn_transcript_persistence_failed`.
    pub fn transcript_persistence_failed(&self, chat_id: &str, owner: Option<&str>) -> bool {
        let Some(chat) = self.turns.get(chat_id) else { return false };
        let turn = match owner {
            Some(o) => chat.find(o),
            None => chat.latest(),
        };
        turn.is_some_and(|t| t.transcript_persistence_failed)
    }

    /// Keep `owner`'s turn active past normal completion because a canonical
    /// display event failed to persist. Mirrors
    /// `mark_websocket_turn_transcript_persistence_failed`.
    pub fn mark_transcript_persistence_failed(&mut self, chat_id: &str, owner: Option<&str>) -> bool {
        let Some(owner) = owner.filter(|o| !o.is_empty()) else { return false };
        let Some(chat) = self.turns.get_mut(chat_id) else { return false };
        let Some(turn) = chat.0.iter_mut().find(|t| t.owner == owner) else { return false };
        turn.transcript_persistence_failed = true;
        true
    }

    /// Clear one lifecycle owner without disturbing concurrent turns for the
    /// same chat; returns whether anything was cleared. Mirrors
    /// `clear_websocket_turn_if_current` — minus its legacy-projection
    /// fallback branch, which has no analog here (there is no pre-owner-map
    /// representation to stay compatible with in this port).
    pub fn clear_turn_if_current(
        &mut self,
        chat_id: &str,
        owner: Option<&str>,
        preserve_persistence_failure: bool,
    ) -> bool {
        let Some(owner) = owner.filter(|o| !o.is_empty()) else { return false };
        let should_remove_chat = {
            let Some(chat) = self.turns.get_mut(chat_id) else { return false };
            let Some(turn) = chat.find(owner) else { return false };
            if preserve_persistence_failure && turn.transcript_persistence_failed {
                return false;
            }
            chat.remove(owner);
            chat.0.is_empty()
        };
        if should_remove_chat {
            self.turns.remove(chat_id);
        }
        true
    }

    /// Drop every in-flight owner for `chat_id`. Used when a `TurnEnd` arrives
    /// without `_websocket_turn_owner` (e.g. `/stop`, whose inbound metadata
    /// is the stop command, not the original turn).
    pub fn clear_chat(&mut self, chat_id: &str) -> bool {
        self.turns.remove(chat_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_queued_turn_if_idle_admits_when_chat_has_no_active_owner() {
        let mut registry = WebsocketTurnRegistry::default();
        let owner = registry.register_queued_turn_if_idle("chat-1", Some("turn-1"));
        assert!(owner.is_some());
        assert!(registry.websocket_turn_wall_started_at("chat-1").is_some());
    }

    #[test]
    fn register_queued_turn_if_idle_refuses_when_chat_already_running() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.register_queued_turn_if_idle("chat-1", None);
        assert!(registry.register_queued_turn_if_idle("chat-1", None).is_none());
    }

    #[test]
    fn start_turn_supports_multiple_concurrent_owners_for_one_chat() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", None);
        registry.start_turn("chat-1", "owner-b", None);

        assert!(registry.websocket_turn_wall_started_at("chat-1").is_some());
        registry.clear_turn_if_current("chat-1", Some("owner-a"), false);
        assert!(
            registry.websocket_turn_wall_started_at("chat-1").is_some(),
            "owner-b should keep the chat running after owner-a clears"
        );
        registry.clear_turn_if_current("chat-1", Some("owner-b"), false);
        assert!(registry.websocket_turn_wall_started_at("chat-1").is_none());
    }

    #[test]
    fn websocket_turn_wall_started_at_reflects_the_latest_registered_owner() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", None);
        let first_started_at = registry.websocket_turn_wall_started_at("chat-1").unwrap();
        registry.start_turn("chat-1", "owner-b", None);
        let latest_started_at = registry.websocket_turn_wall_started_at("chat-1").unwrap();
        assert!(latest_started_at >= first_started_at);
    }

    #[test]
    fn websocket_turn_id_is_none_when_latest_owner_has_no_turn_id() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", Some("turn-1"));
        assert_eq!(registry.websocket_turn_id("chat-1"), Some("turn-1".to_string()));

        registry.start_turn("chat-1", "owner-b", None);
        assert_eq!(
            registry.websocket_turn_id("chat-1"),
            None,
            "latest owner has no turn_id, so the projection reads as None even though a turn is active"
        );
    }

    #[test]
    fn re_registering_an_owner_moves_it_to_latest() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", Some("turn-1"));
        registry.start_turn("chat-1", "owner-b", Some("turn-2"));
        assert_eq!(registry.websocket_turn_id("chat-1"), Some("turn-2".to_string()));

        registry.start_turn("chat-1", "owner-a", Some("turn-3"));
        assert_eq!(
            registry.websocket_turn_id("chat-1"),
            Some("turn-3".to_string()),
            "re-registering owner-a should move it back to the latest position"
        );
    }

    #[test]
    fn owner_is_registered_requires_exact_turn_id_match() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", Some("turn-1"));
        assert!(registry.owner_is_registered("chat-1", "owner-a", Some("turn-1")));
        assert!(!registry.owner_is_registered("chat-1", "owner-a", Some("turn-2")));
        assert!(!registry.owner_is_registered("chat-1", "owner-a", None));
        assert!(!registry.owner_is_registered("chat-1", "owner-missing", Some("turn-1")));
    }

    #[test]
    fn mark_transcript_persistence_failed_requires_a_present_non_empty_owner() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", None);

        assert!(!registry.mark_transcript_persistence_failed("chat-1", None));
        assert!(!registry.mark_transcript_persistence_failed("chat-1", Some("")));
        assert!(!registry.mark_transcript_persistence_failed("chat-1", Some("owner-missing")));
        assert!(registry.mark_transcript_persistence_failed("chat-1", Some("owner-a")));
        assert!(registry.transcript_persistence_failed("chat-1", Some("owner-a")));
    }

    #[test]
    fn clear_turn_if_current_respects_preserve_persistence_failure() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", None);
        registry.mark_transcript_persistence_failed("chat-1", Some("owner-a"));

        assert!(!registry.clear_turn_if_current("chat-1", Some("owner-a"), true));
        assert!(registry.websocket_turn_wall_started_at("chat-1").is_some());

        assert!(registry.clear_turn_if_current("chat-1", Some("owner-a"), false));
        assert!(registry.websocket_turn_wall_started_at("chat-1").is_none());
    }

    #[test]
    fn clear_turn_if_current_does_not_disturb_other_owners_of_the_same_chat() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", Some("turn-1"));
        registry.start_turn("chat-1", "owner-b", Some("turn-2"));

        assert!(registry.clear_turn_if_current("chat-1", Some("owner-a"), false));
        assert!(registry.owner_is_registered("chat-1", "owner-b", Some("turn-2")));
        assert!(!registry.owner_is_registered("chat-1", "owner-a", Some("turn-1")));
    }

    #[test]
    fn clear_turn_if_current_is_idempotent_and_noop_for_unknown_owner() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", None);

        assert!(registry.clear_turn_if_current("chat-1", Some("owner-a"), false));
        assert!(!registry.clear_turn_if_current("chat-1", Some("owner-a"), false));
        assert!(!registry.clear_turn_if_current("chat-1", Some("owner-missing"), false));
        assert!(!registry.clear_turn_if_current("unknown-chat", Some("owner-a"), false));
        assert!(!registry.clear_turn_if_current("chat-1", None, false));
    }

    #[test]
    fn clear_chat_removes_all_owners_for_that_chat_only() {
        let mut registry = WebsocketTurnRegistry::default();
        registry.start_turn("chat-1", "owner-a", None);
        registry.start_turn("chat-1", "owner-b", None);
        registry.start_turn("chat-2", "owner-c", None);

        assert!(registry.clear_chat("chat-1"));
        assert!(registry.websocket_turn_wall_started_at("chat-1").is_none());
        assert!(registry.websocket_turn_wall_started_at("chat-2").is_some());
        assert!(!registry.clear_chat("chat-1"));
    }
}
