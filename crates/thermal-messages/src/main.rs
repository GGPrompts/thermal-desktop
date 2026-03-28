//! thermal-messages: message bus daemon for inter-agent communication.
//!
//! All agent communication flows through this daemon. TUI subscribes for
//! display, dispatcher sends routed messages, `td` CLI sends one-shot commands.
//!
//! Wire format: JSONL (newline-delimited JSON) over Unix sockets.
//! Socket path: `/run/user/<uid>/thermal/messages.sock`

mod persist;
mod routing;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast};
use tracing::{error, info, warn};

use thermal_core::message::{Message, MessageType};
use routing::RouteTable;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "thermal-messages", about = "Message bus daemon for inter-agent communication")]
struct Cli {
    /// Enable JSONL append-log persistence (~/.local/share/thermal/messages.jsonl).
    /// On startup, the log is loaded to populate the ring buffer.
    #[arg(long)]
    persist: bool,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default ring buffer capacity.
const DEFAULT_RING_CAP: usize = 500;

/// Broadcast channel capacity for live fan-out.
const BROADCAST_CAP: usize = 128;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn socket_dir() -> PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    PathBuf::from(format!("/run/user/{uid}/thermal"))
}

fn socket_path() -> PathBuf {
    socket_dir().join("messages.sock")
}

fn pidfile_path() -> PathBuf {
    socket_dir().join("messages.pid")
}

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
    fn oldest_seq(&self) -> Option<u64> {
        self.buf.front().map(|m| m.seq)
    }

    fn len(&self) -> usize {
        self.buf.len()
    }
}

// ---------------------------------------------------------------------------
// Shared daemon state
// ---------------------------------------------------------------------------

struct DaemonState {
    ring: Mutex<RingBuffer>,
    seq: AtomicU64,
    broadcast_tx: broadcast::Sender<Arc<Message>>,
    route_table: RouteTable,
    /// Optional JSONL persist writer (enabled via `--persist`).
    persist_writer: Option<Mutex<persist::PersistWriter>>,
}

impl DaemonState {
    fn new(ring_cap: usize, persist: bool) -> Result<Self> {
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAP);

        // If persistence is enabled, load historical messages and open writer.
        let (initial_msgs, persist_writer) = if persist {
            let msgs = persist::load_log(ring_cap);
            let writer = persist::PersistWriter::open()?;
            (msgs, Some(Mutex::new(writer)))
        } else {
            (Vec::new(), None)
        };

        // Determine starting seq from loaded messages.
        let start_seq = initial_msgs.last().map_or(0, |m| m.seq);

        // Pre-populate ring buffer with historical messages.
        let mut ring = RingBuffer::new(ring_cap);
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

    /// Ingest a message: assign seq + ts, store in ring, broadcast to subscribers.
    /// Then attempt to route it — if a backend handles it, ingest the response too.
    async fn ingest(self: &Arc<Self>, mut msg: Message) {
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
        // Check for async mode — if set, spawn routing in background.
        let is_async = msg
            .metadata
            .get("async")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if is_async {
            // For async messages, route_message returns TaskStatus::Submitted
            // immediately. We ingest that, then spawn the actual dispatch.
            if let Some(submitted_msg) = routing::route_message(&msg, &self.route_table).await {
                self.ingest_response(submitted_msg).await;
            }
            // Spawn background dispatch for the actual work.
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

        // Persist to JSONL log if enabled.
        if let Some(ref pw) = self.persist_writer {
            let mut w = pw.lock().await;
            if let Err(e) = w.append(&msg) {
                warn!(error = %e, "failed to persist response message");
            }
        }

        let _ = self.broadcast_tx.send(arc_msg);
    }

    /// Background dispatch for async messages. Runs the backend, then ingest
    /// the result (or failure) as a new message.
    async fn dispatch_async(self: &Arc<Self>, msg: Message) {
        use thermal_core::message::TaskState;

        let task_id = format!("task-{}", msg.seq);
        let agent_type = msg.to.agent_type.clone();

        info!(task_id = %task_id, agent_type = %agent_type, "async dispatch starting");

        // Strip "async" from metadata so the backend dispatch doesn't see it again
        let mut sync_msg = msg.clone();
        sync_msg.metadata.remove("async");

        // Dispatch synchronously now (route_message won't see "async" flag)
        let result = routing::route_message(&sync_msg, &self.route_table).await;

        // Ingest the result. If the backend returned a response, use it.
        // Otherwise, ingest a Completed TaskStatus.
        if let Some(response) = result {
            self.ingest_response(response).await;
        }

        // Also send a TaskStatus::Completed (or Failed) notification.
        let status_msg = Message {
            seq: 0,
            ts: 0,
            from: msg.to.clone(),
            to: msg.from.clone(),
            context_id: msg.context_id.clone(),
            project: msg.project.clone(),
            content: String::new(),
            msg_type: thermal_core::message::MessageType::TaskStatus {
                task_id: task_id.clone(),
                state: TaskState::Completed,
            },
            metadata: std::collections::HashMap::new(),
        };
        self.ingest_response(status_msg).await;

        info!(task_id = %task_id, "async dispatch completed");
    }
}

// ---------------------------------------------------------------------------
// Per-client handler
// ---------------------------------------------------------------------------

async fn handle_client(stream: UnixStream, state: Arc<DaemonState>) {
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let writer = Arc::new(Mutex::new(writer));

    // Read the first line to determine if this is a Subscribe or a publish client.
    let first_line = match lines.next_line().await {
        Ok(Some(line)) => line,
        Ok(None) => return,
        Err(e) => {
            warn!("error reading from client: {e}");
            return;
        }
    };

    // Try to parse as a Message.
    let first_msg: Message = match serde_json::from_str(&first_line) {
        Ok(m) => m,
        Err(e) => {
            warn!("invalid JSON from client: {e}");
            let _ = send_error(&writer, &format!("invalid JSON: {e}")).await;
            return;
        }
    };

    // Check if it's a Subscribe message.
    if let MessageType::Subscribe { since_seq } = &first_msg.msg_type {
        handle_subscriber(since_seq.unwrap_or(0), lines, writer, state).await;
    } else {
        // It's a publish message — ingest it and continue reading.
        state.ingest(first_msg).await;
        handle_publisher(lines, writer, state).await;
    }
}

/// Handle a subscriber: replay from ring buffer, then stream live messages.
async fn handle_subscriber(
    since_seq: u64,
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    state: Arc<DaemonState>,
) {
    info!(since_seq, "new subscriber connected");

    // Replay from ring buffer.
    let replay_msgs = {
        let ring = state.ring.lock().await;

        // Check if the requested seq is too old (ring overflow).
        if since_seq > 0 {
            if let Some(oldest) = ring.oldest_seq() {
                if since_seq < oldest {
                    // Notify client of overflow.
                    let overflow = Message {
                        seq: 0,
                        ts: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                        from: thermal_core::AgentId::new("daemon", "bus"),
                        to: thermal_core::AgentId::new("*", "*"),
                        context_id: None,
                        project: None,
                        content: String::new(),
                        msg_type: MessageType::RingOverflow {
                            oldest_available: oldest,
                        },
                        metadata: Default::default(),
                    };
                    if let Ok(json) = serde_json::to_string(&overflow) {
                        let mut w = writer.lock().await;
                        let _ = w.write_all(json.as_bytes()).await;
                        let _ = w.write_all(b"\n").await;
                    }
                }
            }
        }

        ring.replay_since(since_seq)
    };

    // Send replayed messages.
    {
        let mut w = writer.lock().await;
        for msg in &replay_msgs {
            if let Ok(json) = serde_json::to_string(msg) {
                if w.write_all(json.as_bytes()).await.is_err() {
                    return;
                }
                if w.write_all(b"\n").await.is_err() {
                    return;
                }
            }
        }
        if w.flush().await.is_err() {
            return;
        }
    }

    info!(
        replayed = replay_msgs.len(),
        "replay complete, streaming live"
    );

    // Subscribe to broadcast channel for live messages.
    let mut rx = state.broadcast_tx.subscribe();

    // Spawn a task that reads from the client (for disconnect detection).
    let writer_clone = Arc::clone(&writer);
    let (disconnect_tx, mut disconnect_rx) = tokio::sync::oneshot::channel::<()>();

    let read_task = tokio::spawn(async move {
        loop {
            match lines.next_line().await {
                Ok(Some(_)) => {
                    // Subscribers don't normally send data after Subscribe,
                    // but we keep the connection alive.
                }
                _ => break,
            }
        }
        let _ = disconnect_tx.send(());
    });

    // Stream live messages to the subscriber.
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(msg) => {
                        if let Ok(json) = serde_json::to_string(msg.as_ref()) {
                            let mut w = writer_clone.lock().await;
                            if w.write_all(json.as_bytes()).await.is_err() {
                                break;
                            }
                            if w.write_all(b"\n").await.is_err() {
                                break;
                            }
                            if w.flush().await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "subscriber lagged, dropped messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            _ = &mut disconnect_rx => {
                break;
            }
        }
    }

    let _ = read_task.await;
    info!("subscriber disconnected");
}

/// Handle a publisher: keep reading JSONL messages and ingesting them.
async fn handle_publisher(
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    state: Arc<DaemonState>,
) {
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Message>(&line) {
                    Ok(msg) => {
                        state.ingest(msg).await;
                        // Ack back to the publisher.
                        let ack = r#"{"ok":true}"#;
                        let mut w = writer.lock().await;
                        let _ = w.write_all(ack.as_bytes()).await;
                        let _ = w.write_all(b"\n").await;
                        let _ = w.flush().await;
                    }
                    Err(e) => {
                        warn!("invalid message JSON: {e}");
                        let _ = send_error(&writer, &format!("invalid JSON: {e}")).await;
                    }
                }
            }
            Ok(None) => break, // Client disconnected.
            Err(e) => {
                warn!("error reading from publisher: {e}");
                break;
            }
        }
    }
    info!("publisher disconnected");
}

async fn send_error(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    msg: &str,
) -> std::io::Result<()> {
    let err = serde_json::json!({"ok": false, "error": msg});
    let mut w = writer.lock().await;
    w.write_all(err.to_string().as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

// ---------------------------------------------------------------------------
// Pidfile guard
// ---------------------------------------------------------------------------

fn write_pidfile(path: &Path) -> Result<()> {
    let pid = std::process::id();
    std::fs::write(path, pid.to_string()).with_context(|| {
        format!("writing pidfile to {}", path.display())
    })?;
    info!(pid, path = %path.display(), "wrote pidfile");
    Ok(())
}

fn remove_pidfile(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_file(path);
        info!(path = %path.display(), "removed pidfile");
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "thermal_messages=info".parse().unwrap()),
        )
        .init();

    info!(
        "thermal-messages v{} starting (persist={})",
        env!("CARGO_PKG_VERSION"),
        cli.persist,
    );

    // Ensure socket directory exists.
    let sock_dir = socket_dir();
    tokio::fs::create_dir_all(&sock_dir)
        .await
        .with_context(|| format!("creating socket dir {}", sock_dir.display()))?;

    // Remove stale socket.
    let sock_path = socket_path();
    if sock_path.exists() {
        tokio::fs::remove_file(&sock_path)
            .await
            .context("removing stale socket")?;
    }

    // Pidfile guard.
    let pid_path = pidfile_path();
    write_pidfile(&pid_path)?;

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding Unix socket at {}", sock_path.display()))?;
    info!(path = %sock_path.display(), "listening");

    let state = Arc::new(DaemonState::new(DEFAULT_RING_CAP, cli.persist)?);

    // Graceful shutdown on Ctrl-C.
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            handle_client(stream, state).await;
                        });
                    }
                    Err(e) => {
                        error!("accept error: {e}");
                    }
                }
            }
            _ = &mut shutdown => {
                info!("shutting down");
                break;
            }
        }
    }

    // Flush persist log before exit.
    if let Some(ref pw) = state.persist_writer {
        let mut w = pw.lock().await;
        if let Err(e) = w.flush() {
            warn!(error = %e, "failed to flush persist log on shutdown");
        }
    }

    // Cleanup.
    remove_pidfile(&pid_path);
    let _ = tokio::fs::remove_file(&sock_path).await;
    info!("goodbye");

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use thermal_core::message::{AgentId, MessageType};
    use std::collections::HashMap;

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

    // ── Ring buffer tests ────────────────────────────────────────────────────

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
        // Should only have seq 3, 4, 5.
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

        // Replay since seq 3 should return seq 4 and 5.
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

    // ── Message ingest tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn ingest_assigns_seq_and_ts() {
        let state = Arc::new(DaemonState::new(10, false).unwrap());
        let msg = make_agent_msg("hello");

        state.ingest(msg).await;

        let ring = state.ring.lock().await;
        assert_eq!(ring.len(), 1);
        let stored = &ring.buf[0];
        assert_eq!(stored.seq, 1);
        assert!(stored.ts > 0);
        assert_eq!(stored.content, "hello");
    }

    #[tokio::test]
    async fn ingest_increments_seq_monotonically() {
        let state = Arc::new(DaemonState::new(10, false).unwrap());

        for i in 0..5 {
            let msg = make_agent_msg(&format!("msg {i}"));
            state.ingest(msg).await;
        }

        let ring = state.ring.lock().await;
        for (i, msg) in ring.buf.iter().enumerate() {
            assert_eq!(msg.seq, (i + 1) as u64);
        }
    }

    #[tokio::test]
    async fn ingest_broadcasts_to_subscribers() {
        let state = Arc::new(DaemonState::new(10, false).unwrap());
        let mut rx = state.broadcast_tx.subscribe();

        let msg = make_agent_msg("broadcast test");
        state.ingest(msg).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.content, "broadcast test");
        assert_eq!(received.seq, 1);
    }

    #[tokio::test]
    async fn ingest_respects_ring_cap() {
        let state = Arc::new(DaemonState::new(3, false).unwrap());

        for i in 0..5 {
            let msg = make_agent_msg(&format!("msg {i}"));
            state.ingest(msg).await;
        }

        let ring = state.ring.lock().await;
        assert_eq!(ring.len(), 3);
        // Oldest should be seq 3 (first two evicted).
        assert_eq!(ring.oldest_seq(), Some(3));
    }

    // ── Subscribe message creation test ──────────────────────────────────────

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
            MessageType::Subscribe { since_seq: Some(42) }
        ));
    }
}
