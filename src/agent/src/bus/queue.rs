use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Mutex;
use tokio::sync::mpsc;

use super::events::{InboundMessage, OutboundMessage};

/// Wraps an unbounded receiver and tracks the number of pending messages.
struct TrackedReceiver<T> {
    inner: mpsc::UnboundedReceiver<T>,
    count: Arc<AtomicUsize>,
}

impl<T> TrackedReceiver<T> {
    fn new(inner: mpsc::UnboundedReceiver<T>, count: Arc<AtomicUsize>) -> Self {
        Self { inner, count }
    }

    /// Receive the next message. Decrements the pending count when a message is taken.
    async fn recv(&mut self) -> Option<T> {
        let msg = self.inner.recv().await;
        if msg.is_some() {
            self.count.fetch_sub(1, Ordering::Relaxed);
        }
        msg
    }

    fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        let msg = self.inner.try_recv()?;
        self.count.fetch_sub(1, Ordering::Relaxed);
        Ok(msg)
    }
}

/// Wraps an unbounded sender and increments the pending count on each send.
pub struct TrackedSender<T> {
    inner: mpsc::UnboundedSender<T>,
    count: Arc<AtomicUsize>,
}

impl<T> Clone for TrackedSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            count: Arc::clone(&self.count),
        }
    }
}

impl<T> TrackedSender<T> {
    fn new(inner: mpsc::UnboundedSender<T>, count: Arc<AtomicUsize>) -> Self {
        Self { inner, count }
    }

    /// Send a message. Increments the pending count.
    pub fn send(&self, msg: T) -> Result<(), mpsc::error::SendError<T>> {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.inner.send(msg)
    }
}

/// Async queue, similar to Python's `asyncio.Queue`. Supports send, recv, and len.
///
/// The receiver is held behind a `Mutex` so the queue can be consumed through a
/// shared `&self` (e.g. from an `Arc<MessageBus>`). Sending stays lock-free, so
/// producers are never blocked while a consumer is parked in `recv`.
pub struct AsyncQueue<T> {
    tx: TrackedSender<T>,
    rx: Mutex<TrackedReceiver<T>>,
    count: Arc<AtomicUsize>,
}

impl<T> AsyncQueue<T> {
    fn new(tx: TrackedSender<T>, rx: TrackedReceiver<T>) -> Self {
        let count = Arc::clone(&tx.count);
        Self {
            tx,
            rx: Mutex::new(rx),
            count,
        }
    }

    /// Send a message into the queue (like `queue.put_nowait` / `queue.put`).
    pub fn send(&self, msg: T) -> Result<(), mpsc::error::SendError<T>> {
        self.tx.send(msg)
    }

    /// Put a message into the queue without blocking. Alias for [`Self::send`],
    /// named to match Python's `asyncio.Queue.put_nowait` so call sites ported
    /// from nanobot (e.g. `bus.outbound.put_nowait(...)`) read the same here.
    /// The queue is unbounded, so this never raises `QueueFull` the way the
    /// Python method can; the `Result` can only be `Err` if the receiver has
    /// been dropped.
    pub fn put_nowait(&self, msg: T) -> Result<(), mpsc::error::SendError<T>> {
        self.send(msg)
    }

    /// Receive the next message (like `queue.get()`). Returns `None` when the channel is closed.
    pub async fn recv(&self) -> Option<T> {
        self.rx.lock().await.recv().await
    }

    /// Number of pending messages (like `queue.qsize()`).
    pub fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// True when the queue holds no pending messages.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a sender handle that can be cloned and shared to push messages from elsewhere.
    pub fn sender(&self) -> TrackedSender<T> {
        self.tx.clone()
    }

    pub fn try_recv(&self) -> Result<T, mpsc::error::TryRecvError> {
        // Prefer try_lock so an empty/busy queue doesn't await
        let mut rx = self
            .rx
            .try_lock()
            .map_err(|_| mpsc::error::TryRecvError::Empty)?;
        rx.try_recv()
    }
}

/// What a [`InboundGuard`] decided about a message that is about to be published.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum InboundAdmission {
    /// Enqueue the message normally.
    Accept,
    /// Drop it silently (a duplicate whose first delivery is still being handled).
    Drop,
    /// Drop it and send this reply instead (a duplicate of an answered request).
    Reply(OutboundMessage),
}

/// Hook that sees every inbound message before it enters the queue.
///
/// The inbox ledger implements it to record submissions durably and to drop
/// redeliveries, so a message that is acknowledged to the channel is never lost
/// to a crash between publish and processing.
pub trait InboundGuard: Send + Sync {
    /// Decide what to do with `msg`. May record it durably.
    fn admit(&self, msg: &InboundMessage) -> InboundAdmission;
}

/// Async message bus that decouples chat channels from the agent core.
/// Channels push messages to the inbound queue, and the agent processes
/// them and pushes responses to the outbound queue.
pub struct MessageBus {
    pub inbound: AsyncQueue<InboundMessage>,
    pub outbound: AsyncQueue<OutboundMessage>,
    guard: std::sync::RwLock<Option<Arc<dyn InboundGuard>>>,
}

impl Default for MessageBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageBus {
    /// Create a new message bus with unbounded inbound and outbound queues.
    pub fn new() -> Self {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let inbound_count = Arc::new(AtomicUsize::new(0));
        let outbound_count = Arc::new(AtomicUsize::new(0));
        let inbound = AsyncQueue::new(
            TrackedSender::new(inbound_tx, Arc::clone(&inbound_count)),
            TrackedReceiver::new(inbound_rx, inbound_count),
        );
        let outbound = AsyncQueue::new(
            TrackedSender::new(outbound_tx, Arc::clone(&outbound_count)),
            TrackedReceiver::new(outbound_rx, outbound_count),
        );
        Self {
            inbound,
            outbound,
            guard: std::sync::RwLock::new(None),
        }
    }

    /// Install the guard that screens every message passed to [`Self::publish_inbound`].
    pub fn set_inbound_guard(&self, guard: Arc<dyn InboundGuard>) {
        *self.guard.write().unwrap_or_else(|e| e.into_inner()) = Some(guard);
    }

    /// Number of pending inbound messages.
    pub fn inbound_size(&self) -> usize {
        self.inbound.len()
    }

    /// Number of pending outbound messages.
    pub fn outbound_size(&self) -> usize {
        self.outbound.len()
    }

    /// Publish a message from a channel to the agent.
    ///
    /// With an [`InboundGuard`] installed the message may be dropped (a
    /// duplicate) or answered with a stored reply instead of being enqueued;
    /// both count as success for the caller.
    #[allow(clippy::result_large_err)]
    pub fn publish_inbound(
        &self,
        msg: InboundMessage,
    ) -> Result<(), mpsc::error::SendError<InboundMessage>> {
        let guard = self.guard.read().unwrap_or_else(|e| e.into_inner()).clone();
        match guard.map_or(InboundAdmission::Accept, |guard| guard.admit(&msg)) {
            InboundAdmission::Accept => self.inbound.send(msg),
            InboundAdmission::Drop => Ok(()),
            InboundAdmission::Reply(reply) => {
                if let Err(e) = self.publish_outbound(reply) {
                    log::error!("Failed to publish stored reply for a redelivered message: {e}");
                }
                Ok(())
            }
        }
    }

    /// Enqueue a message that is already recorded (startup re-drive), skipping the guard.
    #[allow(clippy::result_large_err)]
    pub fn republish_inbound(
        &self,
        msg: InboundMessage,
    ) -> Result<(), mpsc::error::SendError<InboundMessage>> {
        self.inbound.send(msg)
    }

    /// Consume the next inbound message (blocks until available).
    pub async fn consume_inbound(&self) -> Option<InboundMessage> {
        self.inbound.recv().await
    }

    /// Publish a response from the agent to channels.
    #[allow(clippy::result_large_err)]
    pub fn publish_outbound(
        &self,
        msg: OutboundMessage,
    ) -> Result<(), mpsc::error::SendError<OutboundMessage>> {
        self.outbound.send(msg)
    }

    /// Consume the next outbound message (blocks until available).
    pub async fn consume_outbound(&self) -> Option<OutboundMessage> {
        self.outbound.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::events::{InboundMessage, OutboundMessage};
    use chrono::Utc;
    use std::collections::HashMap;

    #[test]
    fn publish_inbound_then_consume_then_publish_outbound_then_consume() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let bus = MessageBus::new();

            // Publish "hello, world" to the inbound queue
            let inbound = InboundMessage {
                channel: "test".to_string(),
                sender_id: "user1".to_string(),
                chat_id: "chat1".to_string(),
                content: "hello, world".to_string(),
                timestamp: Utc::now(),
                media: vec![],
                metadata: HashMap::new(),
                session_key_override: None,
                request_id: None,
            };
            bus.publish_inbound(inbound).unwrap();

            // Read it from the inbound queue
            let received_inbound = bus.consume_inbound().await.unwrap();
            assert_eq!(received_inbound.content, "hello, world");

            // Push a message to the outbound queue
            let outbound = OutboundMessage {
                channel: "test".to_string(),
                chat_id: "chat1".to_string(),
                content: "hi back".to_string(),
                reply_to: None,
                media: vec![],
                metadata: HashMap::new(),
                event: None,
            };
            bus.publish_outbound(outbound).unwrap();

            // Read it from the outbound queue
            let received_outbound = bus.consume_outbound().await.unwrap();
            assert_eq!(received_outbound.content, "hi back");
        });
    }

    #[test]
    fn put_nowait_delivers_message_like_send() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let bus = MessageBus::new();

            let outbound = OutboundMessage {
                channel: "test".to_string(),
                chat_id: "chat1".to_string(),
                content: "put_nowait works".to_string(),
                reply_to: None,
                media: vec![],
                metadata: HashMap::new(),
                event: None,
            };
            bus.outbound.put_nowait(outbound).unwrap();
            assert_eq!(bus.outbound_size(), 1);

            let received = bus.consume_outbound().await.unwrap();
            assert_eq!(received.content, "put_nowait works");
        });
    }

    struct ScriptedGuard(std::sync::Mutex<Vec<&'static str>>);

    impl InboundGuard for ScriptedGuard {
        fn admit(&self, msg: &InboundMessage) -> InboundAdmission {
            match self.0.lock().unwrap().remove(0) {
                "accept" => InboundAdmission::Accept,
                "drop" => InboundAdmission::Drop,
                _ => InboundAdmission::Reply(OutboundMessage {
                    channel: msg.channel.clone(),
                    chat_id: msg.chat_id.clone(),
                    content: "stored reply".into(),
                    reply_to: None,
                    media: vec![],
                    metadata: HashMap::new(),
                    event: None,
                }),
            }
        }
    }

    fn inbound(content: &str) -> InboundMessage {
        InboundMessage {
            channel: "c".into(),
            sender_id: "u".into(),
            chat_id: "x".into(),
            content: content.into(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
            session_key_override: None,
            request_id: Some("r".into()),
        }
    }

    #[tokio::test]
    async fn guard_decides_between_enqueue_drop_and_stored_reply() {
        let bus = MessageBus::new();
        bus.set_inbound_guard(Arc::new(ScriptedGuard(std::sync::Mutex::new(vec![
            "accept", "drop", "reply",
        ]))));

        bus.publish_inbound(inbound("first")).unwrap();
        bus.publish_inbound(inbound("dropped")).unwrap();
        bus.publish_inbound(inbound("answered")).unwrap();

        assert_eq!(bus.inbound_size(), 1);
        assert_eq!(bus.consume_inbound().await.unwrap().content, "first");
        assert_eq!(
            bus.consume_outbound().await.unwrap().content,
            "stored reply"
        );
        assert_eq!(bus.outbound_size(), 0);
    }

    #[tokio::test]
    async fn republish_skips_the_guard() {
        let bus = MessageBus::new();
        bus.set_inbound_guard(Arc::new(ScriptedGuard(std::sync::Mutex::new(vec!["drop"]))));
        bus.republish_inbound(inbound("again")).unwrap();
        assert_eq!(bus.consume_inbound().await.unwrap().content, "again");
    }

    #[tokio::test]
    async fn without_a_guard_everything_is_enqueued() {
        let bus = MessageBus::new();
        bus.publish_inbound(inbound("a")).unwrap();
        assert_eq!(bus.inbound_size(), 1);
    }
}
