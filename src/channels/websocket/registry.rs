use axum::extract::ws::Message;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

/// Many-to-many chat_id↔connection subscription registry, mirroring
/// nanobot's `_subs`/`_conn_chats`/`_conn_default`. One connection may be
/// attached to several chat_ids (e.g. several open chats sharing one
/// socket); one chat_id may have several connections attached (e.g. the
/// same conversation open in two tabs). Shared (via `Arc`) between the
/// channel itself and every per-connection task spawned by axum.
#[derive(Default)]
pub struct ConnectionRegistry {
    /// chat_id -> connection_ids subscribed to it (fan-out target).
    subs: HashMap<String, HashSet<String>>,
    /// connection_id -> chat_ids it is subscribed to (O(1) cleanup on disconnect).
    conn_chats: HashMap<String, HashSet<String>>,
    /// connection_id -> its default chat_id, for legacy frames that omit routing.
    conn_default: HashMap<String, String>,
    /// connection_id -> outbound sender, so [`Self::senders_for_chat`] can reach it.
    senders: HashMap<String, mpsc::UnboundedSender<Message>>,
}

impl ConnectionRegistry {
    /// Idempotently subscribe `connection_id` to `chat_id`. Mirrors `_attach`.
    pub fn attach(&mut self, connection_id: &str, chat_id: &str) {
        self.subs
            .entry(chat_id.to_string())
            .or_default()
            .insert(connection_id.to_string());
        self.conn_chats
            .entry(connection_id.to_string())
            .or_default()
            .insert(chat_id.to_string());
    }

    /// Record a newly-opened connection's sender and default chat_id, then attach it.
    pub fn register(
        &mut self,
        connection_id: &str,
        default_chat_id: &str,
        sender: mpsc::UnboundedSender<Message>,
    ) {
        self.senders.insert(connection_id.to_string(), sender);
        self.conn_default
            .insert(connection_id.to_string(), default_chat_id.to_string());
        self.attach(connection_id, default_chat_id);
    }

    /// Remove `connection_id` from every subscription set; safe to call
    /// multiple times. Mirrors `_cleanup_connection`.
    pub fn cleanup_connection(&mut self, connection_id: &str) {
        if let Some(chat_ids) = self.conn_chats.remove(connection_id) {
            for chat_id in chat_ids {
                if let Some(subs) = self.subs.get_mut(&chat_id) {
                    subs.remove(connection_id);
                    if subs.is_empty() {
                        self.subs.remove(&chat_id);
                    }
                }
            }
        }
        self.conn_default.remove(connection_id);
        self.senders.remove(connection_id);
    }

    /// Snapshot the senders currently subscribed to `chat_id`. Mirrors
    /// `list(self._subs.get(chat_id, ()))` in nanobot's `send()`/`send_*` helpers.
    pub fn senders_for_chat(&self, chat_id: &str) -> Vec<(String, mpsc::UnboundedSender<Message>)> {
        let Some(conn_ids) = self.subs.get(chat_id) else {
            return Vec::new();
        };
        conn_ids
            .iter()
            .filter_map(|id| self.senders.get(id).map(|tx| (id.clone(), tx.clone())))
            .collect()
    }

    /// The sender for one specific connection, regardless of what it's
    /// subscribed to. Mirrors nanobot's `_send_event(connection, ...)` —
    /// replying to *the connection that sent an envelope*, as opposed to
    /// [`Self::senders_for_chat`]'s chat_id-keyed fan-out.
    pub fn sender_for(&self, connection_id: &str) -> Option<mpsc::UnboundedSender<Message>> {
        self.senders.get(connection_id).cloned()
    }

    /// Drop all state. Mirrors nanobot's `stop()` clearing `_subs`/`_conn_chats`/etc.
    pub fn clear(&mut self) {
        self.subs.clear();
        self.conn_chats.clear();
        self.conn_default.clear();
        self.senders.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_sender() -> mpsc::UnboundedSender<Message> {
        let (tx, _rx) = mpsc::unbounded_channel::<Message>();
        tx
    }

    #[test]
    fn register_attaches_connection_to_its_default_chat() {
        let mut registry = ConnectionRegistry::default();
        registry.register("conn-1", "chat-1", dummy_sender());

        let recipients = registry.senders_for_chat("chat-1");
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].0, "conn-1");
    }

    #[test]
    fn attach_allows_one_connection_to_subscribe_to_multiple_chats() {
        let mut registry = ConnectionRegistry::default();
        registry.register("conn-1", "chat-1", dummy_sender());
        registry.attach("conn-1", "chat-2");

        assert_eq!(registry.senders_for_chat("chat-1").len(), 1);
        assert_eq!(registry.senders_for_chat("chat-2").len(), 1);
    }

    #[test]
    fn attach_allows_one_chat_to_have_multiple_connections() {
        let mut registry = ConnectionRegistry::default();
        registry.register("conn-1", "chat-1", dummy_sender());
        registry.register("conn-2", "chat-2", dummy_sender());
        registry.attach("conn-2", "chat-1");

        let recipients = registry.senders_for_chat("chat-1");
        let ids: HashSet<_> = recipients.into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            ids,
            HashSet::from(["conn-1".to_string(), "conn-2".to_string()])
        );
    }

    #[test]
    fn cleanup_connection_removes_it_from_every_subscribed_chat() {
        let mut registry = ConnectionRegistry::default();
        registry.register("conn-1", "chat-1", dummy_sender());
        registry.attach("conn-1", "chat-2");

        registry.cleanup_connection("conn-1");

        assert!(registry.senders_for_chat("chat-1").is_empty());
        assert!(registry.senders_for_chat("chat-2").is_empty());
        assert!(!registry.subs.contains_key("chat-1"));
        assert!(!registry.subs.contains_key("chat-2"));
    }

    #[test]
    fn cleanup_connection_is_idempotent() {
        let mut registry = ConnectionRegistry::default();
        registry.register("conn-1", "chat-1", dummy_sender());

        registry.cleanup_connection("conn-1");
        registry.cleanup_connection("conn-1");

        assert!(registry.senders_for_chat("chat-1").is_empty());
    }

    #[test]
    fn cleanup_connection_does_not_affect_other_connections_sharing_a_chat() {
        let mut registry = ConnectionRegistry::default();
        registry.register("conn-1", "chat-1", dummy_sender());
        registry.register("conn-2", "chat-2", dummy_sender());
        registry.attach("conn-2", "chat-1");

        registry.cleanup_connection("conn-1");

        let recipients = registry.senders_for_chat("chat-1");
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].0, "conn-2");
    }

    #[test]
    fn senders_for_chat_returns_empty_for_unknown_chat_id() {
        let registry = ConnectionRegistry::default();
        assert!(registry.senders_for_chat("no-such-chat").is_empty());
    }

    #[test]
    fn sender_for_returns_the_registered_connections_sender() {
        let mut registry = ConnectionRegistry::default();
        registry.register("conn-1", "chat-1", dummy_sender());
        registry.register("conn-2", "chat-2", dummy_sender());

        assert!(registry.sender_for("conn-1").is_some());
        assert!(registry.sender_for("conn-2").is_some());
    }

    #[test]
    fn sender_for_returns_none_for_unknown_connection() {
        let registry = ConnectionRegistry::default();
        assert!(registry.sender_for("no-such-conn").is_none());
    }

    #[test]
    fn sender_for_returns_none_after_cleanup() {
        let mut registry = ConnectionRegistry::default();
        registry.register("conn-1", "chat-1", dummy_sender());

        registry.cleanup_connection("conn-1");

        assert!(registry.sender_for("conn-1").is_none());
    }

    #[test]
    fn clear_removes_all_state() {
        let mut registry = ConnectionRegistry::default();
        registry.register("conn-1", "chat-1", dummy_sender());

        registry.clear();

        assert!(registry.senders_for_chat("chat-1").is_empty());
        assert!(registry.subs.is_empty());
        assert!(registry.conn_chats.is_empty());
        assert!(registry.conn_default.is_empty());
        assert!(registry.senders.is_empty());
    }
}
