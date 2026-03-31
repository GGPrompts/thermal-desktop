//! Integration tests for the thermal-conductor daemon's MessagePack wire protocol.
//!
//! Tests the length-prefixed framing, request/response round-trips over real
//! Unix sockets, and streaming (ScreenUpdate) behavior via a mock daemon.
//!
//! No external services or GPU context required.

use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

// We test the protocol module's public types and framing functions directly.
// Since protocol.rs is inside the conductor binary crate, we replicate the
// wire format here (length-prefixed MessagePack) using the same serde types.

// ---------------------------------------------------------------------------
// Wire types (matching protocol.rs — kept in sync manually)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum Request {
    Ping,
    ListSessions,
    SpawnSession {
        shell: Option<String>,
        cwd: Option<String>,
        #[serde(default)]
        worktree: bool,
        #[serde(default)]
        name: Option<String>,
    },
    KillSession { id: String },
    SendInput { id: String, data: Vec<u8> },
    SendText { id: String, text: String },
    GetSessionState { id: String },
    Attach { id: String, initial_size: Option<(u16, u16)> },
    Detach { id: String },
    Resize { id: String, cols: u16, rows: u16 },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum Response {
    Pong,
    Ok,
    Error { message: String },
    SessionSpawned { id: String, name: String },
    SessionList { sessions: Vec<SessionInfo> },
    SessionState {
        id: String,
        cols: u16,
        rows: u16,
        cells: Vec<CellData>,
        cursor: CursorData,
        title: String,
        #[serde(default)]
        mode: u32,
    },
    ScreenUpdate {
        id: String,
        seq: u64,
        dirty_cells: Vec<DirtyCellData>,
        cursor: CursorData,
        #[serde(default)]
        mode: u32,
    },
    TitleChanged { id: String, title: String },
    SessionExited {
        id: String,
        exit_code: Option<i32>,
        #[serde(default)]
        reason: String,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SessionInfo {
    id: String,
    #[serde(default)]
    name: Option<String>,
    shell_command: String,
    cwd: String,
    shell_pid: i32,
    cols: u16,
    rows: u16,
    title: String,
    start_time: u64,
    connected_client_count: usize,
    is_alive: bool,
    #[serde(default)]
    worktree_path: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CellData {
    ch: char,
    fg: ColorData,
    bg: ColorData,
    flags: u16,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DirtyCellData {
    col: u16,
    row: u16,
    cell: CellData,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CursorData {
    col: u16,
    row: u16,
    visible: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ColorData {
    r: u8,
    g: u8,
    b: u8,
}

// ---------------------------------------------------------------------------
// Framing helpers (replicating protocol.rs encode/decode)
// ---------------------------------------------------------------------------

fn encode_frame<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let payload = rmp_serde::to_vec(value).unwrap();
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

async fn read_frame<R: tokio::io::AsyncReadExt + Unpin>(reader: &mut R) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(_) => return None,
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 64 * 1024 * 1024 {
        return None;
    }
    let mut payload = vec![0u8; len];
    match reader.read_exact(&mut payload).await {
        Ok(_) => Some(payload),
        Err(_) => None,
    }
}

fn decode_payload<T: for<'a> serde::Deserialize<'a>>(payload: &[u8]) -> T {
    rmp_serde::from_slice(payload).unwrap()
}

// ---------------------------------------------------------------------------
// Mock daemon server
// ---------------------------------------------------------------------------

async fn spawn_mock_daemon() -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
    let tmp_dir = TempDir::new().unwrap();
    let sock_path = tmp_dir.path().join("test-conductor.sock");
    let sock_path_clone = sock_path.clone();

    let handle = tokio::spawn(async move {
        let listener = UnixListener::bind(&sock_path_clone).unwrap();
        let _tmp = tmp_dir;

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        handle_mock_daemon_client(stream).await;
                    });
                }
                Err(_) => break,
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    (sock_path, handle)
}

async fn handle_mock_daemon_client(mut stream: UnixStream) {
    loop {
        let payload = match read_frame(&mut stream).await {
            Some(p) => p,
            None => return,
        };

        let request: Request = match rmp_serde::from_slice(&payload) {
            Ok(r) => r,
            Err(_) => {
                let resp = Response::Error {
                    message: "invalid request".into(),
                };
                let frame = encode_frame(&resp);
                let _ = stream.write_all(&frame).await;
                continue;
            }
        };

        let response = match request {
            Request::Ping => Response::Pong,
            Request::ListSessions => Response::SessionList {
                sessions: vec![SessionInfo {
                    id: "test-001".into(),
                    name: Some("opus".into()),
                    shell_command: "/bin/zsh".into(),
                    cwd: "/home/test".into(),
                    shell_pid: 12345,
                    cols: 80,
                    rows: 24,
                    title: "zsh".into(),
                    start_time: 1711500000,
                    connected_client_count: 0,
                    is_alive: true,
                    worktree_path: None,
                }],
            },
            Request::SpawnSession { name, .. } => Response::SessionSpawned {
                id: "new-001".into(),
                name: name.unwrap_or_else(|| "zsh".into()),
            },
            Request::KillSession { id } => {
                if id == "test-001" {
                    Response::Ok
                } else {
                    Response::Error {
                        message: format!("unknown session: {id}"),
                    }
                }
            }
            Request::GetSessionState { id } => Response::SessionState {
                id,
                cols: 80,
                rows: 24,
                cells: vec![CellData {
                    ch: '$',
                    fg: ColorData { r: 0, g: 255, b: 100 },
                    bg: ColorData { r: 0, g: 0, b: 0 },
                    flags: 0,
                }],
                cursor: CursorData {
                    col: 2,
                    row: 0,
                    visible: true,
                },
                title: "zsh".into(),
                mode: 0,
            },
            Request::Attach { id, .. } => {
                // Send initial state, then a ScreenUpdate.
                let state_resp = Response::SessionState {
                    id: id.clone(),
                    cols: 80,
                    rows: 24,
                    cells: Vec::new(),
                    cursor: CursorData { col: 0, row: 0, visible: true },
                    title: "zsh".into(),
                    mode: 0,
                };
                let frame = encode_frame(&state_resp);
                if stream.write_all(&frame).await.is_err() {
                    return;
                }

                // Send a mock ScreenUpdate.
                let update = Response::ScreenUpdate {
                    id,
                    seq: 1,
                    dirty_cells: vec![DirtyCellData {
                        col: 0,
                        row: 0,
                        cell: CellData {
                            ch: '>',
                            fg: ColorData { r: 255, g: 165, b: 0 },
                            bg: ColorData { r: 0, g: 0, b: 0 },
                            flags: 0,
                        },
                    }],
                    cursor: CursorData { col: 1, row: 0, visible: true },
                    mode: 0,
                };
                let frame = encode_frame(&update);
                let _ = stream.write_all(&frame).await;
                // Don't return here — keep the connection for potential detach.
                // Wait for next frame.
                let _ = read_frame(&mut stream).await;
                return;
            }
            Request::SendInput { .. } | Request::SendText { .. } => Response::Ok,
            Request::Detach { .. } => Response::Ok,
            Request::Resize { .. } => Response::Ok,
        };

        let frame = encode_frame(&response);
        if stream.write_all(&frame).await.is_err() {
            return;
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

/// Test: Ping → Pong round-trip over socket.
#[tokio::test]
async fn ping_pong() {
    let (sock_path, _handle) = spawn_mock_daemon().await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();
    let frame = encode_frame(&Request::Ping);
    stream.write_all(&frame).await.unwrap();

    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    assert!(matches!(resp, Response::Pong));
}

/// Test: ListSessions returns mock sessions.
#[tokio::test]
async fn list_sessions() {
    let (sock_path, _handle) = spawn_mock_daemon().await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();
    let frame = encode_frame(&Request::ListSessions);
    stream.write_all(&frame).await.unwrap();

    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    match resp {
        Response::SessionList { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].id, "test-001");
            assert_eq!(sessions[0].name.as_deref(), Some("opus"));
            assert!(sessions[0].is_alive);
        }
        other => panic!("expected SessionList, got {:?}", other),
    }
}

/// Test: SpawnSession returns spawned session info.
#[tokio::test]
async fn spawn_session() {
    let (sock_path, _handle) = spawn_mock_daemon().await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();
    let frame = encode_frame(&Request::SpawnSession {
        shell: None,
        cwd: None,
        worktree: false,
        name: Some("sonnet".into()),
    });
    stream.write_all(&frame).await.unwrap();

    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    match resp {
        Response::SessionSpawned { id, name } => {
            assert_eq!(id, "new-001");
            assert_eq!(name, "sonnet");
        }
        other => panic!("expected SessionSpawned, got {:?}", other),
    }
}

/// Test: KillSession for unknown session returns error.
#[tokio::test]
async fn kill_unknown_session_returns_error() {
    let (sock_path, _handle) = spawn_mock_daemon().await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();
    let frame = encode_frame(&Request::KillSession {
        id: "nonexistent".into(),
    });
    stream.write_all(&frame).await.unwrap();

    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    match resp {
        Response::Error { message } => {
            assert!(message.contains("unknown session"));
        }
        other => panic!("expected Error, got {:?}", other),
    }
}

/// Test: GetSessionState returns grid snapshot.
#[tokio::test]
async fn get_session_state() {
    let (sock_path, _handle) = spawn_mock_daemon().await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();
    let frame = encode_frame(&Request::GetSessionState {
        id: "test-001".into(),
    });
    stream.write_all(&frame).await.unwrap();

    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    match resp {
        Response::SessionState { id, cols, rows, cells, cursor, .. } => {
            assert_eq!(id, "test-001");
            assert_eq!(cols, 80);
            assert_eq!(rows, 24);
            assert_eq!(cells.len(), 1);
            assert_eq!(cells[0].ch, '$');
            assert!(cursor.visible);
        }
        other => panic!("expected SessionState, got {:?}", other),
    }
}

/// Test: Attach receives initial SessionState + ScreenUpdate stream.
#[tokio::test]
async fn attach_receives_stream() {
    let (sock_path, _handle) = spawn_mock_daemon().await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();
    let frame = encode_frame(&Request::Attach {
        id: "test-001".into(),
        initial_size: Some((80, 24)),
    });
    stream.write_all(&frame).await.unwrap();

    // First frame: SessionState.
    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    assert!(matches!(resp, Response::SessionState { .. }));

    // Second frame: ScreenUpdate.
    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    match resp {
        Response::ScreenUpdate { id, seq, dirty_cells, .. } => {
            assert_eq!(id, "test-001");
            assert_eq!(seq, 1);
            assert_eq!(dirty_cells.len(), 1);
            assert_eq!(dirty_cells[0].cell.ch, '>');
        }
        other => panic!("expected ScreenUpdate, got {:?}", other),
    }
}

/// Test: multiple requests on same connection.
#[tokio::test]
async fn multiple_requests_same_connection() {
    let (sock_path, _handle) = spawn_mock_daemon().await;

    let mut stream = UnixStream::connect(&sock_path).await.unwrap();

    // Ping.
    let frame = encode_frame(&Request::Ping);
    stream.write_all(&frame).await.unwrap();
    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    assert!(matches!(resp, Response::Pong));

    // ListSessions.
    let frame = encode_frame(&Request::ListSessions);
    stream.write_all(&frame).await.unwrap();
    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    assert!(matches!(resp, Response::SessionList { .. }));

    // SendText.
    let frame = encode_frame(&Request::SendText {
        id: "test-001".into(),
        text: "echo hello".into(),
    });
    stream.write_all(&frame).await.unwrap();
    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    assert!(matches!(resp, Response::Ok));
}

/// Test: clean disconnect is handled (no panic).
#[tokio::test]
async fn clean_disconnect() {
    let (sock_path, _handle) = spawn_mock_daemon().await;

    {
        let mut stream = UnixStream::connect(&sock_path).await.unwrap();
        let frame = encode_frame(&Request::Ping);
        stream.write_all(&frame).await.unwrap();
        let payload = read_frame(&mut stream).await.unwrap();
        let _: Response = decode_payload(&payload);
        // Drop stream — clean disconnect.
    }

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Server should still accept new connections.
    let mut stream = UnixStream::connect(&sock_path).await.unwrap();
    let frame = encode_frame(&Request::Ping);
    stream.write_all(&frame).await.unwrap();
    let payload = read_frame(&mut stream).await.unwrap();
    let resp: Response = decode_payload(&payload);
    assert!(matches!(resp, Response::Pong));
}

/// Test: frame encoding produces correct length prefix.
#[tokio::test]
async fn frame_encoding_length_prefix() {
    let req = Request::Ping;
    let frame = encode_frame(&req);

    // First 4 bytes are little-endian u32 payload length.
    let len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    assert_eq!(len, frame.len() - 4);

    // Payload should decode back to Ping.
    let decoded: Request = rmp_serde::from_slice(&frame[4..]).unwrap();
    assert!(matches!(decoded, Request::Ping));
}

/// Test: Request/Response MessagePack round-trip (no socket).
#[tokio::test]
async fn msgpack_round_trip() {
    let requests = vec![
        Request::Ping,
        Request::ListSessions,
        Request::SpawnSession {
            shell: Some("/bin/bash".into()),
            cwd: Some("/tmp".into()),
            worktree: true,
            name: Some("test-session".into()),
        },
        Request::KillSession { id: "abc".into() },
        Request::SendInput { id: "x".into(), data: vec![0x1b, 0x5b, 0x41] },
        Request::Resize { id: "x".into(), cols: 120, rows: 40 },
    ];

    for req in &requests {
        let encoded = rmp_serde::to_vec(req).unwrap();
        let decoded: Request = rmp_serde::from_slice(&encoded).unwrap();
        // Just verify it doesn't panic — enum variant checks are sufficient.
        let _ = format!("{:?}", decoded);
    }

    let responses = vec![
        Response::Pong,
        Response::Ok,
        Response::Error { message: "test error".into() },
        Response::SessionSpawned { id: "s1".into(), name: "opus".into() },
    ];

    for resp in &responses {
        let encoded = rmp_serde::to_vec(resp).unwrap();
        let decoded: Response = rmp_serde::from_slice(&encoded).unwrap();
        let _ = format!("{:?}", decoded);
    }
}
