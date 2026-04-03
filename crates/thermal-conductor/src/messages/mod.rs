//! In-process message bus — replaces the former thermal-messages daemon.
//!
//! Provides a `MessageBus` with:
//!  - `send(msg)` — ingest, assign seq/ts, store in ring buffer, broadcast, route
//!  - `subscribe()` — get a `broadcast::Receiver<Arc<Message>>`
//!  - `replay_since(seq)` — replay from ring buffer
//!
//! The bus is initialized once during conductor startup and shared via `Arc`.

pub mod persist;
pub mod routing;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::{Mutex, broadcast};
use tracing::{info, warn};

use routing::RouteTable;
use thermal_core::message::{Message, MessageType};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default ring buffer capacity.
const DEFAULT_RING_CAP: usize = 500;

/// Broadcast channel capacity for live fan-out.
const BROADCAST_CAP: usize = 128;

// ---------------------------------------------------------------------------
// Ring buffer
// ---------------------------------------------------------------------------

/// Thread-safe ring buffer of messages with configurable capacity.
struct RingBuffer {
    buf: VecDeque<Message>,
    cap: usize,
}

impl RingBuffer {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Push a message, evicting the oldest if at capacity.
    fn push(&mut self, msg: Message) {
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(msg);
    }

    /// Return all messages with seq > `since_seq`.
    fn replay_since(&self, since_seq: u64) -> Vec<Message> {
        self.buf
            .iter()
            .filter(|m| m.seq > since_seq)
            .cloned()
            .collect()
    }

    /// The oldest sequence number still in the buffer, or None if empty.
    #[allow(dead_code)]
    fn oldest_seq(&self) -> Option<u64> {
        self.buf.front().map(|m| m.seq)
    }

    fn len(&self) -> usize {
        self.buf.len()
    }
}

// ---------------------------------------------------------------------------
// MessageBus
// ---------------------------------------------------------------------------

/// In-process message bus with ring buffer, broadcast channel, routing,
/// and optional JSONL persistence.
pub struct MessageBus {
    ring: Mutex<RingBuffer>,
    seq: AtomicU64,
    broadcast_tx: broadcast::Sender<Arc<Message>>,
    route_table: RouteTable,
    /// Optional JSONL persist writer.
    persist_writer: Option<Mutex<persist::PersistWriter>>,
}

impl MessageBus {
    /// Create a new message bus. If `persist` is true, messages are appended
    /// to `~/.local/share/thermal/messages.jsonl` and the log is loaded on
    /// startup to pre-populate the ring buffer.
    pub fn new(do_persist: bool) -> Result<Self> {
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAP);

        // If persistence is enabled, load historical messages and open writer.
        let (initial_msgs, persist_writer) = if do_persist {
            let msgs = persist::load_log(DEFAULT_RING_CAP);
            let writer = persist::PersistWriter::open()?;
            (msgs, Some(Mutex::new(writer)))
        } else {
            (Vec::new(), None)
        };

        // Determine starting seq from loaded messages.
        let start_seq = initial_msgs.last().map_or(0, |m| m.seq);

        // Pre-populate ring buffer with historical messages.
        let mut ring = RingBuffer::new(DEFAULT_RING_CAP);
        for msg in initial_msgs {
            ring.push(msg);
        }

        Ok(Self {
            ring: Mutex::new(ring),
            seq: AtomicU64::new(start_seq),
            broadcast_tx,
            route_table: RouteTable::new(),
            persist_writer,
        })
    }

    /// Subscribe to live message broadcasts.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Message>> {
        self.broadcast_tx.subscribe()
    }

    /// Replay messages from the ring buffer with seq > `since_seq`.
    pub async fn replay_since(&self, since_seq: u64) -> Vec<Message> {
        let ring = self.ring.lock().await;
        ring.replay_since(since_seq)
    }

    /// Ingest a message: assign seq + ts, store in ring, broadcast to subscribers.
    /// Then attempt to route it — if a backend handles it, ingest the response too.
    pub async fn send(self: &Arc<Self>, mut msg: Message) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        msg.seq = seq;
        msg.ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let arc_msg = Arc::new(msg.clone());

        // Store in ring buffer.
        {
            let mut ring = self.ring.lock().await;
            ring.push(msg.clone());
        }

        // Persist to JSONL log if enabled.
        if let Some(ref pw) = self.persist_writer {
            let mut w = pw.lock().await;
            if let Err(e) = w.append(&msg) {
                warn!(error = %e, "failed to persist message");
            }
        }

        // Broadcast to live subscribers (ignore error when no receivers).
        let _ = self.broadcast_tx.send(arc_msg);

        // Attempt to route the message to a backend.
        let is_async = msg
            .metadata
            .get("async")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if is_async {
            if let Some(submitted_msg) = routing::route_message(&msg, &self.route_table).await {
                self.ingest_response(submitted_msg).await;
            }
            let state = Arc::clone(self);
            let msg_clone = msg.clone();
            tokio::spawn(async move {
                state.dispatch_async(msg_clone).await;
            });
        } else if let Some(response) = routing::route_message(&msg, &self.route_table).await {
            self.ingest_response(response).await;
        }
    }

    /// Ingest a routing response message (assign seq/ts, store, broadcast).
    async fn ingest_response(&self, mut msg: Message) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        msg.seq = seq;
        msg.ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let arc_msg = Arc::new(msg.clone());

        {
            let mut ring = self.ring.lock().await;
            ring.push(msg.clone());
        }

        if let Some(ref pw) = self.persist_writer {
            let mut w = pw.lock().await;
            if let Err(e) = w.append(&msg) {
                warn!(error = %e, "failed to persist response message");
            }
        }

        let _ = self.broadcast_tx.send(arc_msg);
    }

    /// Background dispatch for async messages.
    async fn dispatch_async(self: &Arc<Self>, msg: Message) {
        use thermal_core::message::TaskState;

        let task_id = format!("task-{}", msg.seq);
        let agent_type = msg.to.agent_type.clone();

        info!(task_id = %task_id, agent_type = %agent_type, "async dispatch starting");

        let mut sync_msg = msg.clone();
        sync_msg.metadata.remove("async");

        let result = routing::route_message(&sync_msg, &self.route_table).await;

        if let Some(response) = result {
            self.ingest_response(response).await;
        }

        let status_msg = Message {
            seq: 0,
            ts: 0,
            from: msg.to.clone(),
            to: msg.from.clone(),
            context_id: msg.context_id.clone(),
            project: msg.project.clone(),
            content: String::new(),
            msg_type: MessageType::TaskStatus {
                task_id: task_id.clone(),
                state: TaskState::Completed,
            },
            metadata: std::collections::HashMap::new(),
        };
        self.ingest_response(status_msg).await;

        info!(task_id = %task_id, "async dispatch completed");
    }

    /// Flush persistence log (call on shutdown).
    pub async fn flush_persist(&self) {
        if let Some(ref pw) = self.persist_writer {
            let mut w = pw.lock().await;
            if let Err(e) = w.flush() {
                warn!(error = %e, "failed to flush persist log on shutdown");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use thermal_core::message::{AgentId, MessageType};

    fn make_agent_msg(content: &str) -> Message {
        Message {
            seq: 0,
            ts: 0,
            from: AgentId::new("claude", "sess-1"),
            to: AgentId::new("*", "broadcast"),
            context_id: None,
            project: None,
            content: content.to_string(),
            msg_type: MessageType::AgentMsg,
            metadata: HashMap::new(),
        }
    }

    // -- Ring buffer tests --

    #[test]
    fn ring_buffer_push_and_len() {
        let mut ring = RingBuffer::new(5);
        assert_eq!(ring.len(), 0);

        for i in 1..=3 {
            let mut msg = make_agent_msg(&format!("msg {i}"));
            msg.seq = i;
            ring.push(msg);
        }
        assert_eq!(ring.len(), 3);
    }

    #[test]
    fn ring_buffer_evicts_oldest_at_capacity() {
        let mut ring = RingBuffer::new(3);
        for i in 1..=5 {
            let mut msg = make_agent_msg(&format!("msg {i}"));
            msg.seq = i;
            ring.push(msg);
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.oldest_seq(), Some(3));
    }

    #[test]
    fn ring_buffer_replay_since() {
        let mut ring = RingBuffer::new(10);
        for i in 1..=5 {
            let mut msg = make_agent_msg(&format!("msg {i}"));
            msg.seq = i;
            ring.push(msg);
        }

        let replayed = ring.replay_since(3);
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].seq, 4);
        assert_eq!(replayed[1].seq, 5);
    }

    #[test]
    fn ring_buffer_replay_since_zero_returns_all() {
        let mut ring = RingBuffer::new(10);
        for i in 1..=3 {
            let mut msg = make_agent_msg(&format!("msg {i}"));
            msg.seq = i;
            ring.push(msg);
        }

        let replayed = ring.replay_since(0);
        assert_eq!(replayed.len(), 3);
    }

    #[test]
    fn ring_buffer_replay_since_future_seq_returns_empty() {
        let mut ring = RingBuffer::new(10);
        for i in 1..=3 {
            let mut msg = make_agent_msg(&format!("msg {i}"));
            msg.seq = i;
            ring.push(msg);
        }

        let replayed = ring.replay_since(100);
        assert_eq!(replayed.len(), 0);
    }

    #[test]
    fn ring_buffer_oldest_seq_empty() {
        let ring = RingBuffer::new(10);
        assert_eq!(ring.oldest_seq(), None);
    }

    #[test]
    fn ring_buffer_capacity_one() {
        let mut ring = RingBuffer::new(1);
        for i in 1..=3 {
            let mut msg = make_agent_msg(&format!("msg {i}"));
            msg.seq = i;
            ring.push(msg);
        }
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.oldest_seq(), Some(3));
    }

    // -- MessageBus ingest tests --

    #[tokio::test]
    async fn ingest_assigns_seq_and_ts() {
        let bus = Arc::new(MessageBus::new(false).unwrap());
        let msg = make_agent_msg("hello");

        bus.send(msg).await;

        let ring = bus.ring.lock().await;
        assert_eq!(ring.len(), 1);
        let stored = &ring.buf[0];
        assert_eq!(stored.seq, 1);
        assert!(stored.ts > 0);
        assert_eq!(stored.content, "hello");
    }

    #[tokio::test]
    async fn ingest_increments_seq_monotonically() {
        let bus = Arc::new(MessageBus::new(false).unwrap());

        for i in 0..5 {
            let msg = make_agent_msg(&format!("msg {i}"));
            bus.send(msg).await;
        }

        let ring = bus.ring.lock().await;
        for (i, msg) in ring.buf.iter().enumerate() {
            assert_eq!(msg.seq, (i + 1) as u64);
        }
    }

    #[tokio::test]
    async fn ingest_broadcasts_to_subscribers() {
        let bus = Arc::new(MessageBus::new(false).unwrap());
        let mut rx = bus.subscribe();

        let msg = make_agent_msg("broadcast test");
        bus.send(msg).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.content, "broadcast test");
        assert_eq!(received.seq, 1);
    }

    #[tokio::test]
    async fn ingest_respects_ring_cap() {
        // Use a custom ring cap via direct construction
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAP);
        let bus = Arc::new(MessageBus {
            ring: Mutex::new(RingBuffer::new(3)),
            seq: AtomicU64::new(0),
            broadcast_tx,
            route_table: RouteTable::new(),
            persist_writer: None,
        });

        for i in 0..5 {
            let msg = make_agent_msg(&format!("msg {i}"));
            bus.send(msg).await;
        }

        let ring = bus.ring.lock().await;
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.oldest_seq(), Some(3));
    }

    // -- Subscribe message creation test --

    #[test]
    fn subscribe_message_round_trips_through_json() {
        let msg = Message {
            seq: 0,
            ts: 0,
            from: AgentId::new("user", "bob"),
            to: AgentId::new("daemon", "bus"),
            context_id: None,
            project: None,
            content: String::new(),
            msg_type: MessageType::Subscribe {
                since_seq: Some(42),
            },
            metadata: HashMap::new(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded.msg_type,
            MessageType::Subscribe {
                since_seq: Some(42)
            }
        ));
    }
}
