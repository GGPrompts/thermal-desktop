//! Integration tests for the thermal-messages bus daemon socket protocol.
//!
//! These tests spin up a real UnixListener, exercise the JSONL wire protocol,
//! and verify pub/sub, replay, ring buffer overflow, and disconnect behavior.
//!
//! No external services required — runs entirely in-process with temp sockets.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast};

use thermal_core::message::{AgentId, Message, MessageType};

// ---------------------------------------------------------------------------
// Helpers — mini message bus (extracted from daemon internals)
// ---------------------------------------------------------------------------

/// Minimal ring buffer matching the daemon's implementation.
struct RingBuffer {
    buf: std::collections::VecDeque<Message>,
    cap: usize,
}

impl RingBuffer {
    fn new(cap: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn push(&mut self, msg: Message) {
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(msg);
    }

    fn replay_since(&self, since_seq: u64) -> Vec<Message> {
        self.buf.iter().filter(|m| m.seq > since_seq).cloned().collect()
    }

    fn oldest_seq(&self) -> Option<u64> {
        self.buf.front().map(|m| m.seq)
    }

    fn len(&self) -> usize {
        self.buf.len()
    }
}

/// Shared test daemon state.
struct TestBusState {
    ring: Mutex<RingBuffer>,
    seq: AtomicU64,
    broadcast_tx: broadcast::Sender<Arc<Message>>,
}

impl TestBusState {
    fn new(ring_cap: usize) -> Self {
        let (broadcast_tx, _) = broadcast::channel(256);
        Self {
            ring: Mutex::new(RingBuffer::new(ring_cap)),
            seq: AtomicU64::new(0),
            broadcast_tx,
        }
    }

    async fn ingest(&self, mut msg: Message) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        msg.seq = seq;
        msg.ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let arc_msg = Arc::new(msg.clone());

        {
            let mut ring = self.ring.lock().await;
            ring.push(msg);
        }

        let _ = self.broadcast_tx.send(arc_msg);
    }
}

fn make_msg(content: &str) -> Message {
    Message {
        seq: 0,
        ts: 0,
        from: AgentId::new("test", "sender"),
        to: AgentId::new("*", "broadcast"),
        context_id: None,
        project: None,
        content: content.to_string(),
        msg_type: MessageType::AgentMsg,
        metadata: HashMap::new(),
    }
}

fn make_subscribe_msg(since_seq: Option<u64>) -> Message {
    Message {
        seq: 0,
        ts: 0,
        from: AgentId::new("test", "subscriber"),
        to: AgentId::new("daemon", "bus"),
        context_id: None,
        project: None,
        content: String::new(),
        msg_type: MessageType::Subscribe { since_seq },
        metadata: HashMap::new(),
    }
}

/// Spawn a minimal message bus server on a temp socket, returning the socket path
/// and a handle to the state for test inspection.
async fn spawn_test_bus(ring_cap: usize) -> (std::path::PathBuf, Arc<TestBusState>, tokio::task::JoinHandle<()>) {
    let tmp_dir = TempDir::new().unwrap();
    let sock_path = tmp_dir.path().join("test-messages.sock");
    let sock_path_clone = sock_path.clone();

    let state = Arc::new(TestBusState::new(ring_cap));
    let state_clone = Arc::clone(&state);

    let handle = tokio::spawn(async move {
        let listener = UnixListener::bind(&sock_path_clone).unwrap();
        // Keep the TempDir alive for the lifetime of the server.
        let _tmp = tmp_dir;

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let state = Arc::clone(&state_clone);
                    tokio::spawn(async move {
                        handle_test_client(stream, state).await;
                    });
                }
                Err(_) => break,
            }
        }
    });

    // Give the listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    (sock_path, state, handle)
}

/// Minimal client handler replicating the daemon's JSONL protocol.
async fn handle_test_client(stream: UnixStream, state: Arc<TestBusState>) {
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let writer = Arc::new(Mutex::new(writer));

    let first_line = match lines.next_line().await {
        Ok(Some(line)) => line,
        _ => return,
    };

    let first_msg: Message = match serde_json::from_str(&first_line) {
        Ok(m) => m,
        Err(e) => {
            let err = serde_json::json!({"ok": false, "error": format!("invalid JSON: {e}")});
            let mut w = writer.lock().await;
            let _ = w.write_all(err.to_string().as_bytes()).await;
            let _ = w.write_all(b"\n").await;
            return;
        }
    };

    if let MessageType::Subscribe { since_seq } = &first_msg.msg_type {
        let since = since_seq.unwrap_or(0);

        // Replay from ring buffer.
        let replay_msgs = {
            let ring = state.ring.lock().await;

            // Send RingOverflow if requested seq is too old.
            if since > 0 {
                if let Some(oldest) = ring.oldest_seq() {
                    if since < oldest {
                        let overflow = Message {
                            seq: 0,
                            ts: 0,
                            from: AgentId::new("daemon", "bus"),
                            to: AgentId::new("*", "*"),
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

            ring.replay_since(since)
        };

        {
            let mut w = writer.lock().await;
            for msg in &replay_msgs {
                if let Ok(json) = serde_json::to_string(msg) {
                    let _ = w.write_all(json.as_bytes()).await;
                    let _ = w.write_all(b"\n").await;
                }
            }
            let _ = w.flush().await;
        }

        // Stream live messages.
        let mut rx = state.broadcast_tx.subscribe();
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if let Ok(json) = serde_json::to_string(msg.as_ref()) {
                        let mut w = writer.lock().await;
                        if w.write_all(json.as_bytes()).await.is_err() {
                            break;
                        }
                        if w.write_all(b"\n").await.is_err() {
                            break;
                        }
                        let _ = w.flush().await;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    } else {
        // Publisher path.
        state.ingest(first_msg).await;
        {
            let mut w = writer.lock().await;
            let _ = w.write_all(b"{\"ok\":true}\n").await;
            let _ = w.flush().await;
        }

        loop {
            match lines.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    match serde_json::from_str::<Message>(&line) {
                        Ok(msg) => {
                            state.ingest(msg).await;
                            let mut w = writer.lock().await;
                            let _ = w.write_all(b"{\"ok\":true}\n").await;
                            let _ = w.flush().await;
                        }
                        Err(e) => {
                            let err = serde_json::json!({"ok": false, "error": format!("{e}")});
                            let mut w = writer.lock().await;
                            let _ = w.write_all(err.to_string().as_bytes()).await;
                            let _ = w.write_all(b"\n").await;
                        }
                    }
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

/// Test: publish a message and receive an ack.
#[tokio::test]
async fn publish_receives_ack() {
    let (sock_path, _state, _handle) = spawn_test_bus(500).await;

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Send a publish message.
    let msg = make_msg("hello world");
    let json = serde_json::to_string(&msg).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    // Read ack.
    let ack_line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let ack: serde_json::Value = serde_json::from_str(&ack_line).unwrap();
    assert_eq!(ack["ok"], true);
}

/// Test: subscriber receives replayed messages from ring buffer.
#[tokio::test]
async fn subscriber_replay() {
    let (sock_path, state, _handle) = spawn_test_bus(500).await;

    // Pre-populate ring buffer with 3 messages.
    for i in 0..3 {
        state.ingest(make_msg(&format!("historical msg {i}"))).await;
    }

    // Connect a subscriber requesting replay from seq 0 (all messages).
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let sub = make_subscribe_msg(Some(0));
    let json = serde_json::to_string(&sub).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    // Read 3 replayed messages.
    let mut received = Vec::new();
    for _ in 0..3 {
        let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg: Message = serde_json::from_str(&line).unwrap();
        received.push(msg);
    }

    assert_eq!(received.len(), 3);
    assert_eq!(received[0].seq, 1);
    assert_eq!(received[1].seq, 2);
    assert_eq!(received[2].seq, 3);
    assert_eq!(received[0].content, "historical msg 0");
}

/// Test: subscriber with since_seq gets partial replay.
#[tokio::test]
async fn subscriber_partial_replay() {
    let (sock_path, state, _handle) = spawn_test_bus(500).await;

    // Pre-populate 5 messages.
    for i in 0..5 {
        state.ingest(make_msg(&format!("msg {i}"))).await;
    }

    // Subscribe since seq 3 — should only get seq 4 and 5.
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let sub = make_subscribe_msg(Some(3));
    let json = serde_json::to_string(&sub).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let mut received = Vec::new();
    for _ in 0..2 {
        let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg: Message = serde_json::from_str(&line).unwrap();
        received.push(msg);
    }

    assert_eq!(received.len(), 2);
    assert_eq!(received[0].seq, 4);
    assert_eq!(received[1].seq, 5);
}

/// Test: subscriber receives live messages after replay.
#[tokio::test]
async fn subscriber_live_stream() {
    let (sock_path, state, _handle) = spawn_test_bus(500).await;

    // Connect subscriber (no replay needed).
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let sub = make_subscribe_msg(Some(0));
    let json = serde_json::to_string(&sub).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    // Small delay to ensure subscriber is connected.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Ingest a message via the state directly.
    state.ingest(make_msg("live message")).await;

    // Subscriber should receive it.
    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let msg: Message = serde_json::from_str(&line).unwrap();
    assert_eq!(msg.content, "live message");
    assert_eq!(msg.seq, 1);
}

/// Test: publish then subscribe — subscriber gets messages via replay.
#[tokio::test]
async fn publish_then_subscribe_sees_messages() {
    let (sock_path, _state, _handle) = spawn_test_bus(500).await;

    // Publisher sends 3 messages.
    {
        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        for i in 0..3 {
            let msg = make_msg(&format!("published {i}"));
            let json = serde_json::to_string(&msg).unwrap();
            writer.write_all(json.as_bytes()).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
            writer.flush().await.unwrap();

            // Read ack.
            let _ack = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        // Drop publisher connection.
    }

    // Small delay to ensure all ingests complete.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Subscriber connects and replays.
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let sub = make_subscribe_msg(Some(0));
    let json = serde_json::to_string(&sub).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let mut received = Vec::new();
    for _ in 0..3 {
        let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg: Message = serde_json::from_str(&line).unwrap();
        received.push(msg);
    }

    assert_eq!(received.len(), 3);
    assert_eq!(received[0].content, "published 0");
    assert_eq!(received[2].content, "published 2");
}

/// Test: ring buffer overflow — fill 500+ messages, oldest are dropped.
#[tokio::test]
async fn ring_buffer_overflow_drops_oldest() {
    let ring_cap = 10; // Small cap for fast test.
    let (sock_path, state, _handle) = spawn_test_bus(ring_cap).await;

    // Ingest 25 messages (cap is 10).
    for i in 0..25 {
        state.ingest(make_msg(&format!("overflow msg {i}"))).await;
    }

    // Verify ring only holds `ring_cap` messages.
    {
        let ring = state.ring.lock().await;
        assert_eq!(ring.len(), ring_cap);
        // Oldest should be seq 16 (messages 0-14 evicted, 15-24 remain => seq 16-25).
        assert_eq!(ring.oldest_seq(), Some(16));
    }

    // Subscriber requesting since_seq=5 should get RingOverflow notification
    // followed by remaining messages (since 5 < oldest 16).
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let sub = make_subscribe_msg(Some(5));
    let json = serde_json::to_string(&sub).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    // First message should be RingOverflow.
    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let overflow_msg: Message = serde_json::from_str(&line).unwrap();
    match overflow_msg.msg_type {
        MessageType::RingOverflow { oldest_available } => {
            assert_eq!(oldest_available, 16);
        }
        other => panic!("expected RingOverflow, got {:?}", other),
    }

    // Then we should get 10 replayed messages (all in the ring, since since_seq=5 < oldest).
    let mut replayed = Vec::new();
    for _ in 0..10 {
        let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg: Message = serde_json::from_str(&line).unwrap();
        replayed.push(msg);
    }
    assert_eq!(replayed.len(), 10);
    assert_eq!(replayed[0].seq, 16);
    assert_eq!(replayed[9].seq, 25);
}

/// Test: ring buffer at exact capacity — 500 messages with cap 500.
#[tokio::test]
async fn ring_buffer_exact_capacity() {
    let (_sock_path, state, _handle) = spawn_test_bus(500).await;

    // Fill exactly to capacity.
    for i in 0..500 {
        state.ingest(make_msg(&format!("msg {i}"))).await;
    }

    {
        let ring = state.ring.lock().await;
        assert_eq!(ring.len(), 500);
        assert_eq!(ring.oldest_seq(), Some(1));
    }

    // Add one more — oldest should be evicted.
    state.ingest(make_msg("msg 500")).await;

    {
        let ring = state.ring.lock().await;
        assert_eq!(ring.len(), 500);
        assert_eq!(ring.oldest_seq(), Some(2));
    }
}

/// Test: multiple concurrent subscribers receive the same live message.
#[tokio::test]
async fn multiple_subscribers_receive_same_message() {
    let (sock_path, state, _handle) = spawn_test_bus(500).await;

    // Connect two subscribers.
    let mut subscribers = Vec::new();
    for _ in 0..2 {
        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let lines = BufReader::new(reader).lines();

        let sub = make_subscribe_msg(Some(0));
        let json = serde_json::to_string(&sub).unwrap();
        writer.write_all(json.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();

        subscribers.push(lines);
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish a message.
    state.ingest(make_msg("broadcast to all")).await;

    // Both subscribers should receive it.
    for sub_lines in &mut subscribers {
        let line = tokio::time::timeout(Duration::from_secs(2), sub_lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg: Message = serde_json::from_str(&line).unwrap();
        assert_eq!(msg.content, "broadcast to all");
    }
}

/// Test: invalid JSON from publisher gets error response.
#[tokio::test]
async fn invalid_json_returns_error() {
    let (sock_path, _state, _handle) = spawn_test_bus(500).await;

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Send garbage.
    writer.write_all(b"not valid json\n").await.unwrap();
    writer.flush().await.unwrap();

    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["ok"], false);
    assert!(resp["error"].as_str().unwrap().contains("invalid JSON"));
}

/// Test: client disconnect is handled gracefully (no panic or leak).
#[tokio::test]
async fn client_disconnect_graceful() {
    let (sock_path, state, _handle) = spawn_test_bus(500).await;

    // Connect and immediately drop the stream.
    {
        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (_reader, mut writer) = stream.into_split();

        // Send a subscribe, then drop.
        let sub = make_subscribe_msg(Some(0));
        let json = serde_json::to_string(&sub).unwrap();
        writer.write_all(json.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();
    }
    // Stream dropped here.

    // Wait a bit, then verify server is still alive by ingesting a message.
    tokio::time::sleep(Duration::from_millis(100)).await;
    state.ingest(make_msg("after disconnect")).await;

    // Connect a new subscriber to verify server is healthy.
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let sub = make_subscribe_msg(Some(0));
    let json = serde_json::to_string(&sub).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let msg: Message = serde_json::from_str(&line).unwrap();
    assert_eq!(msg.content, "after disconnect");
}

/// Test: publisher sends multiple messages in rapid succession.
#[tokio::test]
async fn rapid_publish_burst() {
    let (sock_path, state, _handle) = spawn_test_bus(500).await;

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let count = 50;
    for i in 0..count {
        let msg = make_msg(&format!("burst {i}"));
        let json = serde_json::to_string(&msg).unwrap();
        writer.write_all(json.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
    }
    writer.flush().await.unwrap();

    // Read all acks.
    for _ in 0..count {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let ack: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ack["ok"], true);
    }

    // Verify all messages are in the ring.
    let ring = state.ring.lock().await;
    assert_eq!(ring.len(), count);
}

/// Test: sequence numbers are monotonically increasing across publishers.
#[tokio::test]
async fn seq_monotonic_across_publishers() {
    let (sock_path, state, _handle) = spawn_test_bus(500).await;

    // Two publishers each send 5 messages.
    for _ in 0..2 {
        let stream = UnixStream::connect(&sock_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        for i in 0..5 {
            let msg = make_msg(&format!("pub msg {i}"));
            let json = serde_json::to_string(&msg).unwrap();
            writer.write_all(json.as_bytes()).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
            writer.flush().await.unwrap();

            let _ack = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    // All 10 messages should have unique, monotonically increasing seq.
    let ring = state.ring.lock().await;
    assert_eq!(ring.len(), 10);
    let mut prev_seq = 0u64;
    for msg in &ring.buf {
        assert!(msg.seq > prev_seq, "seq {} should be > {}", msg.seq, prev_seq);
        prev_seq = msg.seq;
    }
}

/// Test: message metadata is preserved through the bus.
#[tokio::test]
async fn metadata_preserved() {
    let (_sock_path, state, _handle) = spawn_test_bus(500).await;

    let mut msg = make_msg("with metadata");
    msg.metadata.insert("tool".into(), serde_json::Value::String("cargo".into()));
    msg.metadata.insert("exit_code".into(), serde_json::Value::Number(0.into()));

    state.ingest(msg).await;

    let ring = state.ring.lock().await;
    let stored = &ring.buf[0];
    assert_eq!(stored.metadata["tool"], "cargo");
    assert_eq!(stored.metadata["exit_code"], 0);
}
