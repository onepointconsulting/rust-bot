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

/// Async message bus that decouples chat channels from the agent core.
/// Channels push messages to the inbound queue, and the agent processes
/// them and pushes responses to the outbound queue.
pub struct MessageBus {
    pub inbound: AsyncQueue<InboundMessage>,
    pub outbound: AsyncQueue<OutboundMessage>,
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
        Self { inbound, outbound }
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
    pub fn publish_inbound(
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
}
