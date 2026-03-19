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
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
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
    SessionExited {
        id: String,
        exit_code: Option<i32>,
    },

    /// Generic success acknowledgment.
    Ok,

    /// Error response.
    Error { message: String },
}

// ── Supporting types ─────────────────────────────────────────────────────────

/// Summary info for a session, used in `SessionList`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub shell_pid: i32,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    /// Whether the child process has exited.
    pub exited: bool,
    /// Number of attached frontend clients.
    pub attached_clients: usize,
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
