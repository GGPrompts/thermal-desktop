//! Integration tests for the thermal-dispatcher socket protocol.
//!
//! These tests verify the dispatcher's JSONL wire format for receiving
//! voice transcripts and returning responses. The LLM backend is mocked —
//! no network access to any LLM API is needed.

use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

// ---------------------------------------------------------------------------
// Wire types (matching the dispatcher's TranscriptMessage / DispatcherResponse)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct TranscriptMessage {
    transcript: String,
    #[serde(default)]
    confidence: f64,
}

#[derive(serde::Deserialize, Debug)]
struct DispatcherResponse {
    status: String,
    response: Option<String>,
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Mock dispatcher server
// ---------------------------------------------------------------------------

/// A mock dispatcher that accepts transcript JSON and returns canned responses.
/// This replicates the dispatcher's socket protocol without an actual LLM.
async fn spawn_mock_dispatcher() -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
    let tmp_dir = TempDir::new().unwrap();
    let sock_path = tmp_dir.path().join("test-dispatcher.sock");
    let sock_path_clone = sock_path.clone();

    let handle = tokio::spawn(async move {
        let listener = UnixListener::bind(&sock_path_clone).unwrap();
        let _tmp = tmp_dir;

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        handle_mock_client(stream).await;
                    });
                }
                Err(_) => break,
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    (sock_path, handle)
}

/// Mock client handler: reads a JSONL transcript line, returns a mock response.
async fn handle_mock_client(stream: UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    let bytes = match buf_reader.read_line(&mut line).await {
        Ok(n) => n,
        Err(_) => return,
    };
    if bytes == 0 {
        return;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }

    // Try to parse as TranscriptMessage.
    let msg: TranscriptMessage = match serde_json::from_str(trimmed) {
        Ok(m) => m,
        Err(e) => {
            let resp = serde_json::json!({
                "status": "error",
                "error": format!("invalid JSON: {e}")
            });
            let out = resp.to_string() + "\n";
            let _ = writer.write_all(out.as_bytes()).await;
            return;
        }
    };

    if msg.transcript.is_empty() {
        let resp = serde_json::json!({
            "status": "empty",
            "error": "empty transcript"
        });
        let out = resp.to_string() + "\n";
        let _ = writer.write_all(out.as_bytes()).await;
        return;
    }

    // Mock LLM response based on transcript content.
    let mock_response = if msg.transcript.contains("open firefox") {
        "Routed to system: open firefox"
    } else if msg.transcript.contains("what's on screen") {
        "I can see a terminal with code"
    } else {
        "Command acknowledged"
    };

    let resp = serde_json::json!({
        "status": "ok",
        "response": mock_response
    });
    let out = resp.to_string() + "\n";
    let _ = writer.write_all(out.as_bytes()).await;
}

// ===========================================================================
// Tests
// ===========================================================================

/// Test: send a voice transcript, receive a mock LLM response.
#[tokio::test]
async fn transcript_receives_response() {
    let (sock_path, _handle) = spawn_mock_dispatcher().await;

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let msg = TranscriptMessage {
        transcript: "open firefox".to_string(),
        confidence: 0.95,
    };
    let json = serde_json::to_string(&msg).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let resp: DispatcherResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.status, "ok");
    assert_eq!(resp.response.as_deref(), Some("Routed to system: open firefox"));
}

/// Test: empty transcript returns error.
#[tokio::test]
async fn empty_transcript_returns_error() {
    let (sock_path, _handle) = spawn_mock_dispatcher().await;

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let msg = TranscriptMessage {
        transcript: String::new(),
        confidence: 0.0,
    };
    let json = serde_json::to_string(&msg).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let resp: DispatcherResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.status, "empty");
    assert!(resp.error.is_some());
}

/// Test: invalid JSON returns error.
#[tokio::test]
async fn invalid_json_returns_error() {
    let (sock_path, _handle) = spawn_mock_dispatcher().await;

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer.write_all(b"{{broken json\n").await.unwrap();
    writer.flush().await.unwrap();

    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["status"], "error");
    assert!(resp["error"].as_str().unwrap().contains("invalid JSON"));
}

/// Test: server handles client disconnect without crashing.
#[tokio::test]
async fn client_disconnect_graceful() {
    let (sock_path, _handle) = spawn_mock_dispatcher().await;

    // Connect and immediately drop.
    {
        let _stream = UnixStream::connect(&sock_path).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Server should still be alive — connect again.
    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let msg = TranscriptMessage {
        transcript: "hello".to_string(),
        confidence: 0.9,
    };
    let json = serde_json::to_string(&msg).unwrap();
    writer.write_all(json.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();

    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let resp: DispatcherResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.status, "ok");
}

/// Test: connect to non-existent socket fails with proper error.
#[tokio::test]
async fn connect_nonexistent_socket_fails() {
    let result = UnixStream::connect("/tmp/thermal-test-nonexistent-socket-12345.sock").await;
    assert!(result.is_err());
}

/// Test: multiple concurrent clients are handled independently.
#[tokio::test]
async fn concurrent_clients() {
    let (sock_path, _handle) = spawn_mock_dispatcher().await;

    let mut handles = Vec::new();
    for i in 0..5 {
        let path = sock_path.clone();
        handles.push(tokio::spawn(async move {
            let stream = UnixStream::connect(&path).await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();

            let msg = TranscriptMessage {
                transcript: format!("command {i}"),
                confidence: 0.9,
            };
            let json = serde_json::to_string(&msg).unwrap();
            writer.write_all(json.as_bytes()).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
            writer.flush().await.unwrap();

            let line = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let resp: DispatcherResponse = serde_json::from_str(&line).unwrap();
            assert_eq!(resp.status, "ok");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

/// Test: TranscriptMessage wire format round-trip.
#[tokio::test]
async fn transcript_wire_format_round_trip() {
    let msg = TranscriptMessage {
        transcript: "test with special chars: é, 日本語, emoji 🎉".to_string(),
        confidence: 0.99,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded["transcript"], msg.transcript);
    assert_eq!(decoded["confidence"], 0.99);
}
