//! Wire protocol types for the thermal-conductor session daemon.
//!
//! All messages between frontends and the daemon are framed as:
//!
//! ```text
//! ┌──────────────────────────────────────┐
//! │  u32 length (little-endian)          │  4 bytes — length of payload
//! │  payload bytes (MessagePack)         │  `length` bytes
//! └──────────────────────────────────────┘
//! ```
//!
//! Payloads are serialized with `rmp-serde` (MessagePack).

use serde::{Deserialize, Serialize};

// ── Socket path ──────────────────────────────────────────────────────────────

/// Return the daemon socket path: `/run/user/<uid>/thermal/conductor.sock`
pub fn socket_path() -> std::path::PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    std::path::PathBuf::from(format!("/run/user/{uid}/thermal/conductor.sock"))
}

// ── Client → Daemon ──────────────────────────────────────────────────────────

/// A request sent from a frontend client to the session daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Spawn a new PTY session.
    SpawnSession {
        /// Shell binary path. `None` means use `$SHELL`.
        shell: Option<String>,
        /// Initial working directory. `None` means use `$HOME`.
        cwd: Option<String>,
        /// If true, create a git worktree for the session so multiple agents
        /// can work on the same repo without file-edit conflicts.
        #[serde(default)]
        worktree: bool,
    },

    /// Kill a session (sends SIGHUP to PTY child).
    KillSession { id: String },

    /// List all active sessions.
    ListSessions,

    /// Send input bytes to a session's PTY.
    SendInput { id: String, data: Vec<u8> },

    /// Request a full grid snapshot for a session.
    GetSessionState { id: String },

    /// Attach to a session and begin receiving screen updates.
    Attach {
        id: String,
        /// Initial window size (cols, rows). Applied if no other frontend is attached.
        initial_size: Option<(u16, u16)>,
    },

    /// Detach from a session.
    Detach { id: String },

    /// Notify the daemon of a window resize.
    Resize { id: String, cols: u16, rows: u16 },

    /// Connection health check — daemon responds with `Pong`.
    Ping,
}

// ── Daemon → Client ──────────────────────────────────────────────────────────

/// A response sent from the session daemon to a frontend client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// A new session was spawned successfully.
    SessionSpawned { id: String },

    /// List of all active sessions.
    SessionList { sessions: Vec<SessionInfo> },

    /// Full grid snapshot for a session (response to Attach or GetSessionState).
    SessionState {
        id: String,
        cols: u16,
        rows: u16,
        /// Row-major flat list of cells, length = cols * rows.
        cells: Vec<CellData>,
        cursor: CursorData,
        /// Current window title.
        title: String,
    },

    /// Incremental screen update (streamed to attached clients).
    ScreenUpdate {
        id: String,
        /// Monotonically increasing sequence number.
        seq: u64,
        dirty_cells: Vec<DirtyCellData>,
        cursor: CursorData,
    },

    /// Terminal title changed.
    TitleChanged { id: String, title: String },

    /// Session's child process exited.
    SessionExited { id: String, exit_code: Option<i32> },

    /// Generic success acknowledgment.
    Ok,

    /// Error response.
    Error { message: String },

    /// Health check response.
    Pong,
}

// ── Supporting types ─────────────────────────────────────────────────────────

/// Summary info for a session, used in `SessionList`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    /// Shell command that was spawned (e.g. "/bin/zsh").
    pub shell_command: String,
    /// Working directory the session was started in.
    pub cwd: String,
    pub shell_pid: i32,
    pub cols: u16,
    pub rows: u16,
    /// Terminal title (from OSC sequences).
    pub title: String,
    /// Seconds since Unix epoch when the session was created.
    pub start_time: u64,
    /// Number of connected frontend clients (attached for streaming).
    pub connected_client_count: usize,
    /// Whether the child process is still alive.
    pub is_alive: bool,
    /// If the session was spawned in a git worktree, the worktree path.
    #[serde(default)]
    pub worktree_path: Option<String>,
}

/// A single terminal cell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellData {
    pub ch: char,
    pub fg: ColorData,
    pub bg: ColorData,
    pub flags: u16,
}

/// A changed cell in an incremental update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirtyCellData {
    pub col: u16,
    pub row: u16,
    pub cell: CellData,
}

/// Terminal cursor state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorData {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
}

/// 24-bit RGB color.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorData {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

// ── Frame encoding / decoding ────────────────────────────────────────────────

/// Encode a serializable value into a length-prefixed MessagePack frame.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let payload = rmp_serde::to_vec(value)?;
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Read a length-prefixed MessagePack frame from an async reader.
///
/// Returns `None` if the connection is cleanly closed (zero-length read).
pub async fn read_frame<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;

    // Sanity check: reject frames larger than 64 MB.
    if len > 64 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes"),
        ));
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

/// Decode a MessagePack payload into a typed value.
pub fn decode_payload<T: for<'a> Deserialize<'a>>(
    payload: &[u8],
) -> Result<T, rmp_serde::decode::Error> {
    rmp_serde::from_slice(payload)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Round-trip a Request through MessagePack encode → decode.
    fn rt_request(req: &Request) -> Request {
        let payload = rmp_serde::to_vec(req).expect("encode should succeed");
        rmp_serde::from_slice(&payload).expect("decode should succeed")
    }

    /// Round-trip a Response through MessagePack encode → decode.
    fn rt_response(resp: &Response) -> Response {
        let payload = rmp_serde::to_vec(resp).expect("encode should succeed");
        rmp_serde::from_slice(&payload).expect("decode should succeed")
    }

    // ── encode_frame / decode_payload ─────────────────────────────────────────

    #[test]
    fn encode_frame_has_correct_length_prefix() {
        let req = Request::Ping;
        let frame = encode_frame(&req).expect("encode_frame should succeed");
        // First 4 bytes are the little-endian payload length.
        let payload_len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(payload_len, frame.len() - 4);
    }

    #[test]
    fn encode_frame_payload_round_trips() {
        let req = Request::KillSession {
            id: "sess-42".into(),
        };
        let frame = encode_frame(&req).expect("encode_frame should succeed");
        let payload = &frame[4..];
        let decoded: Request = decode_payload(payload).expect("decode_payload should succeed");
        assert!(matches!(decoded, Request::KillSession { id } if id == "sess-42"));
    }

    #[test]
    fn decode_payload_error_on_garbage() {
        let bad: &[u8] = &[0xFF, 0xFE, 0x00, 0x01];
        let result: Result<Request, _> = decode_payload(bad);
        assert!(result.is_err(), "garbage bytes should fail to decode");
    }

    #[test]
    fn encode_frame_empty_response() {
        let resp = Response::Ok;
        let frame = encode_frame(&resp).expect("should encode Ok");
        assert!(
            frame.len() >= 4,
            "frame must contain at least the length prefix"
        );
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len + 4, frame.len());
    }

    // ── Request round-trips ───────────────────────────────────────────────────

    #[test]
    fn request_ping_round_trip() {
        let decoded = rt_request(&Request::Ping);
        assert!(matches!(decoded, Request::Ping));
    }

    #[test]
    fn request_list_sessions_round_trip() {
        let decoded = rt_request(&Request::ListSessions);
        assert!(matches!(decoded, Request::ListSessions));
    }

    #[test]
    fn request_spawn_session_with_fields_round_trip() {
        let req = Request::SpawnSession {
            shell: Some("/bin/zsh".into()),
            cwd: Some("/home/builder".into()),
            worktree: false,
        };
        let decoded = rt_request(&req);
        match decoded {
            Request::SpawnSession {
                shell,
                cwd,
                worktree,
            } => {
                assert_eq!(shell.as_deref(), Some("/bin/zsh"));
                assert_eq!(cwd.as_deref(), Some("/home/builder"));
                assert!(!worktree);
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn request_spawn_session_none_fields_round_trip() {
        let req = Request::SpawnSession {
            shell: None,
            cwd: None,
            worktree: false,
        };
        let decoded = rt_request(&req);
        match decoded {
            Request::SpawnSession {
                shell,
                cwd,
                worktree,
            } => {
                assert!(shell.is_none());
                assert!(cwd.is_none());
                assert!(!worktree);
            }
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn request_kill_session_round_trip() {
        let req = Request::KillSession {
            id: "abc-123".into(),
        };
        let decoded = rt_request(&req);
        assert!(matches!(decoded, Request::KillSession { id } if id == "abc-123"));
    }

    #[test]
    fn request_send_input_round_trip() {
        let data = vec![0x1b, 0x5b, 0x41]; // ESC [ A
        let req = Request::SendInput {
            id: "s1".into(),
            data: data.clone(),
        };
        let decoded = rt_request(&req);
        match decoded {
            Request::SendInput { id, data: d } => {
                assert_eq!(id, "s1");
                assert_eq!(d, data);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn request_send_input_empty_data_round_trip() {
        let req = Request::SendInput {
            id: "s2".into(),
            data: vec![],
        };
        let decoded = rt_request(&req);
        match decoded {
            Request::SendInput { id, data } => {
                assert_eq!(id, "s2");
                assert!(data.is_empty());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn request_get_session_state_round_trip() {
        let req = Request::GetSessionState {
            id: "my-session".into(),
        };
        let decoded = rt_request(&req);
        assert!(matches!(decoded, Request::GetSessionState { id } if id == "my-session"));
    }

    #[test]
    fn request_attach_with_size_round_trip() {
        let req = Request::Attach {
            id: "attach-me".into(),
            initial_size: Some((80, 24)),
        };
        let decoded = rt_request(&req);
        match decoded {
            Request::Attach { id, initial_size } => {
                assert_eq!(id, "attach-me");
                assert_eq!(initial_size, Some((80, 24)));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn request_attach_no_size_round_trip() {
        let req = Request::Attach {
            id: "x".into(),
            initial_size: None,
        };
        let decoded = rt_request(&req);
        match decoded {
            Request::Attach { id, initial_size } => {
                assert_eq!(id, "x");
                assert!(initial_size.is_none());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn request_detach_round_trip() {
        let req = Request::Detach { id: "d1".into() };
        let decoded = rt_request(&req);
        assert!(matches!(decoded, Request::Detach { id } if id == "d1"));
    }

    #[test]
    fn request_resize_round_trip() {
        let req = Request::Resize {
            id: "r1".into(),
            cols: 120,
            rows: 40,
        };
        let decoded = rt_request(&req);
        match decoded {
            Request::Resize { id, cols, rows } => {
                assert_eq!(id, "r1");
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── Response round-trips ──────────────────────────────────────────────────

    #[test]
    fn response_ok_round_trip() {
        let decoded = rt_response(&Response::Ok);
        assert!(matches!(decoded, Response::Ok));
    }

    #[test]
    fn response_pong_round_trip() {
        let decoded = rt_response(&Response::Pong);
        assert!(matches!(decoded, Response::Pong));
    }

    #[test]
    fn response_error_round_trip() {
        let resp = Response::Error {
            message: "something broke".into(),
        };
        let decoded = rt_response(&resp);
        assert!(matches!(decoded, Response::Error { message } if message == "something broke"));
    }

    #[test]
    fn response_session_spawned_round_trip() {
        let resp = Response::SessionSpawned {
            id: "new-sess".into(),
        };
        let decoded = rt_response(&resp);
        assert!(matches!(decoded, Response::SessionSpawned { id } if id == "new-sess"));
    }

    #[test]
    fn response_title_changed_round_trip() {
        let resp = Response::TitleChanged {
            id: "t1".into(),
            title: "My Terminal".into(),
        };
        let decoded = rt_response(&resp);
        match decoded {
            Response::TitleChanged { id, title } => {
                assert_eq!(id, "t1");
                assert_eq!(title, "My Terminal");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn response_session_exited_with_code_round_trip() {
        let resp = Response::SessionExited {
            id: "ex1".into(),
            exit_code: Some(0),
        };
        let decoded = rt_response(&resp);
        match decoded {
            Response::SessionExited { id, exit_code } => {
                assert_eq!(id, "ex1");
                assert_eq!(exit_code, Some(0));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn response_session_exited_no_code_round_trip() {
        let resp = Response::SessionExited {
            id: "ex2".into(),
            exit_code: None,
        };
        let decoded = rt_response(&resp);
        match decoded {
            Response::SessionExited { id, exit_code } => {
                assert_eq!(id, "ex2");
                assert!(exit_code.is_none());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn response_session_list_empty_round_trip() {
        let resp = Response::SessionList { sessions: vec![] };
        let decoded = rt_response(&resp);
        match decoded {
            Response::SessionList { sessions } => assert!(sessions.is_empty()),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn response_session_list_with_entry_round_trip() {
        let info = SessionInfo {
            id: "info-1".into(),
            shell_command: "/bin/bash".into(),
            cwd: "/tmp".into(),
            shell_pid: 1234,
            cols: 80,
            rows: 24,
            title: "bash".into(),
            start_time: 1_700_000_000,
            connected_client_count: 2,
            is_alive: true,
            worktree_path: None,
        };
        let resp = Response::SessionList {
            sessions: vec![info],
        };
        let decoded = rt_response(&resp);
        match decoded {
            Response::SessionList { sessions } => {
                assert_eq!(sessions.len(), 1);
                let si = &sessions[0];
                assert_eq!(si.id, "info-1");
                assert_eq!(si.shell_command, "/bin/bash");
                assert_eq!(si.cwd, "/tmp");
                assert_eq!(si.shell_pid, 1234);
                assert_eq!(si.cols, 80);
                assert_eq!(si.rows, 24);
                assert_eq!(si.title, "bash");
                assert_eq!(si.start_time, 1_700_000_000);
                assert_eq!(si.connected_client_count, 2);
                assert!(si.is_alive);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn response_session_state_round_trip() {
        let cursor = CursorData {
            col: 5,
            row: 3,
            visible: true,
        };
        let cell = CellData {
            ch: 'A',
            fg: ColorData {
                r: 255,
                g: 128,
                b: 0,
            },
            bg: ColorData { r: 0, g: 0, b: 0 },
            flags: 0,
        };
        let resp = Response::SessionState {
            id: "ss1".into(),
            cols: 80,
            rows: 24,
            cells: vec![cell],
            cursor: cursor.clone(),
            title: "term".into(),
        };
        let decoded = rt_response(&resp);
        match decoded {
            Response::SessionState {
                id,
                cols,
                rows,
                cells,
                cursor: c,
                title,
            } => {
                assert_eq!(id, "ss1");
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
                assert_eq!(cells.len(), 1);
                assert_eq!(cells[0].ch, 'A');
                assert_eq!(cells[0].fg.r, 255);
                assert_eq!(cells[0].fg.g, 128);
                assert_eq!(cells[0].fg.b, 0);
                assert_eq!(c.col, 5);
                assert_eq!(c.row, 3);
                assert!(c.visible);
                assert_eq!(title, "term");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn response_screen_update_round_trip() {
        let dirty = DirtyCellData {
            col: 10,
            row: 2,
            cell: CellData {
                ch: 'z',
                fg: ColorData { r: 0, g: 255, b: 0 },
                bg: ColorData { r: 0, g: 0, b: 0 },
                flags: 1,
            },
        };
        let resp = Response::ScreenUpdate {
            id: "su1".into(),
            seq: 42,
            dirty_cells: vec![dirty],
            cursor: CursorData {
                col: 11,
                row: 2,
                visible: false,
            },
        };
        let decoded = rt_response(&resp);
        match decoded {
            Response::ScreenUpdate {
                id,
                seq,
                dirty_cells,
                cursor,
            } => {
                assert_eq!(id, "su1");
                assert_eq!(seq, 42);
                assert_eq!(dirty_cells.len(), 1);
                assert_eq!(dirty_cells[0].col, 10);
                assert_eq!(dirty_cells[0].row, 2);
                assert_eq!(dirty_cells[0].cell.ch, 'z');
                assert_eq!(dirty_cells[0].cell.flags, 1);
                assert!(!cursor.visible);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── Supporting types ──────────────────────────────────────────────────────

    #[test]
    fn color_data_boundary_values_round_trip() {
        let color = ColorData {
            r: 0,
            g: 128,
            b: 255,
        };
        let bytes = rmp_serde::to_vec(&color).unwrap();
        let decoded: ColorData = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded.r, 0);
        assert_eq!(decoded.g, 128);
        assert_eq!(decoded.b, 255);
    }

    #[test]
    fn cursor_data_invisible_round_trip() {
        let cursor = CursorData {
            col: 0,
            row: 0,
            visible: false,
        };
        let bytes = rmp_serde::to_vec(&cursor).unwrap();
        let decoded: CursorData = rmp_serde::from_slice(&bytes).unwrap();
        assert!(!decoded.visible);
        assert_eq!(decoded.col, 0);
        assert_eq!(decoded.row, 0);
    }

    #[test]
    fn cell_data_unicode_char_round_trip() {
        let cell = CellData {
            ch: '🔥',
            fg: ColorData {
                r: 255,
                g: 80,
                b: 0,
            },
            bg: ColorData {
                r: 20,
                g: 20,
                b: 20,
            },
            flags: 0b0000_0011,
        };
        let bytes = rmp_serde::to_vec(&cell).unwrap();
        let decoded: CellData = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded.ch, '🔥');
        assert_eq!(decoded.flags, 0b0000_0011);
    }

    #[test]
    fn session_info_not_alive_round_trip() {
        let info = SessionInfo {
            id: "dead-sess".into(),
            shell_command: "/bin/sh".into(),
            cwd: "/".into(),
            shell_pid: 0,
            cols: 80,
            rows: 24,
            title: String::new(),
            start_time: 0,
            connected_client_count: 0,
            is_alive: false,
            worktree_path: None,
        };
        let bytes = rmp_serde::to_vec(&info).unwrap();
        let decoded: SessionInfo = rmp_serde::from_slice(&bytes).unwrap();
        assert!(!decoded.is_alive);
        assert_eq!(decoded.connected_client_count, 0);
    }
}
