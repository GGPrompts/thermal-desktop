//! Unix socket client for connecting to the thermal-conductor session daemon.
//!
//! Used by `window.rs` in client mode: when a daemon is running, the window
//! connects via this client instead of owning its own PTY directly.
//!
//! Features:
//! - Automatic reconnect on socket disconnect (configurable attempts).
//! - Timeout on request/response round-trips (default 5 seconds).
//! - Ping/pong health check.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::protocol::{self, Request, Response, SessionInfo};

/// Default request timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum reconnect attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Delay between reconnect attempts.
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

/// A client connection to the session daemon.
#[allow(dead_code)]
pub struct DaemonClient {
    /// Sender for outgoing requests.
    request_tx: mpsc::Sender<Request>,
    /// Receiver for incoming responses/updates.
    response_rx: mpsc::Receiver<Response>,
    /// The socket path used for this connection (for reconnect).
    socket_path: PathBuf,
    /// Request timeout duration.
    timeout: Duration,
}

#[allow(dead_code)]
impl DaemonClient {
    /// Try to connect to the daemon socket at the default path.
    ///
    /// Returns `Ok(Some(client))` if the daemon is running and connection succeeded.
    /// Returns `Ok(None)` if the socket does not exist (daemon not running).
    /// Returns `Err` on connection errors.
    pub async fn connect() -> Result<Option<Self>> {
        let socket_path = protocol::socket_path();
        Self::connect_to(socket_path).await
    }

    /// Try to connect to the daemon at a specific socket path.
    ///
    /// Returns `Ok(Some(client))` if connection succeeded.
    /// Returns `Ok(None)` if the socket does not exist.
    /// Returns `Err` on connection errors.
    pub async fn connect_to(socket_path: PathBuf) -> Result<Option<Self>> {
        if !socket_path.exists() {
            return Ok(None);
        }

        let stream = match UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(e) => {
                // Connection refused means daemon crashed but socket remains.
                if e.kind() == std::io::ErrorKind::ConnectionRefused {
                    warn!(
                        "Stale daemon socket at {}; daemon not running",
                        socket_path.display()
                    );
                    return Ok(None);
                }
                return Err(e).context("Failed to connect to daemon socket");
            }
        };

        info!(path = %socket_path.display(), "Connected to session daemon");

        let (request_tx, response_rx) = Self::spawn_io_tasks(stream);

        Ok(Some(Self {
            request_tx,
            response_rx,
            socket_path,
            timeout: DEFAULT_TIMEOUT,
        }))
    }

    /// Spawn reader and writer tasks for a connected stream.
    /// Returns the (request_sender, response_receiver) pair.
    fn spawn_io_tasks(stream: UnixStream) -> (mpsc::Sender<Request>, mpsc::Receiver<Response>) {
        let (reader, mut writer) = stream.into_split();

        // Channel for outgoing requests.
        let (request_tx, mut request_rx) = mpsc::channel::<Request>(32);

        // Channel for incoming responses.
        let (response_tx, response_rx) = mpsc::channel::<Response>(64);

        // Spawn writer task: sends requests to the daemon.
        tokio::spawn(async move {
            while let Some(request) = request_rx.recv().await {
                match protocol::encode_frame(&request) {
                    Ok(frame) => {
                        if let Err(e) = writer.write_all(&frame).await {
                            warn!("Failed to write to daemon: {e}");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to encode request: {e}");
                    }
                }
            }
        });

        // Spawn reader task: reads responses from the daemon.
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                match protocol::read_frame(&mut reader).await {
                    Ok(Some(payload)) => match protocol::decode_payload::<Response>(&payload) {
                        Ok(response) => {
                            if response_tx.send(response).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!("Failed to decode daemon response: {e}");
                        }
                    },
                    Ok(None) => {
                        info!("Daemon connection closed");
                        break;
                    }
                    Err(e) => {
                        warn!("Daemon read error: {e}");
                        break;
                    }
                }
            }
        });

        (request_tx, response_rx)
    }

    /// Set the request timeout duration.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Attempt to reconnect to the daemon socket.
    ///
    /// Tries up to `MAX_RECONNECT_ATTEMPTS` times with exponential backoff.
    /// Returns `Ok(true)` if reconnected, `Ok(false)` if the socket doesn't
    /// exist (daemon not running), or `Err` on persistent failure.
    pub async fn reconnect(&mut self) -> Result<bool> {
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            info!(attempt, "Attempting to reconnect to daemon");

            if !self.socket_path.exists() {
                warn!("Daemon socket does not exist; daemon not running");
                return Ok(false);
            }

            match UnixStream::connect(&self.socket_path).await {
                Ok(stream) => {
                    info!(
                        path = %self.socket_path.display(),
                        "Reconnected to session daemon"
                    );
                    let (request_tx, response_rx) = Self::spawn_io_tasks(stream);
                    self.request_tx = request_tx;
                    self.response_rx = response_rx;
                    return Ok(true);
                }
                Err(e) => {
                    warn!(
                        attempt,
                        error = %e,
                        "Reconnect attempt failed"
                    );
                    if attempt < MAX_RECONNECT_ATTEMPTS {
                        let delay = RECONNECT_DELAY * attempt;
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        anyhow::bail!("Failed to reconnect to daemon after {MAX_RECONNECT_ATTEMPTS} attempts")
    }

    /// Check whether the daemon connection is healthy via a Ping/Pong exchange.
    ///
    /// Returns `true` if the daemon responded within the timeout, `false` otherwise.
    pub async fn is_healthy(&mut self) -> bool {
        matches!(self.request_with_timeout(Request::Ping).await, Ok(Response::Pong))
    }

    /// Send a request to the daemon.
    pub async fn send(&self, request: Request) -> Result<()> {
        self.request_tx
            .send(request)
            .await
            .map_err(|_| anyhow::anyhow!("Daemon connection lost"))
    }

    /// Receive the next response from the daemon.
    ///
    /// Returns `None` if the connection has been closed.
    pub async fn recv(&mut self) -> Option<Response> {
        self.response_rx.recv().await
    }

    /// Try to receive a response without blocking.
    pub fn try_recv(&mut self) -> Option<Response> {
        self.response_rx.try_recv().ok()
    }

    /// Send a request and wait for a single response, with the configured timeout.
    pub async fn request(&mut self, request: Request) -> Result<Response> {
        self.request_with_timeout(request).await
    }

    /// Send a request and wait for a response with timeout.
    async fn request_with_timeout(&mut self, request: Request) -> Result<Response> {
        self.send(request).await?;
        match tokio::time::timeout(self.timeout, self.response_rx.recv()).await {
            Ok(Some(response)) => Ok(response),
            Ok(None) => anyhow::bail!("Daemon connection lost while waiting for response"),
            Err(_) => anyhow::bail!("Request timed out after {:?}", self.timeout),
        }
    }

    // ── High-level API methods ──────────────────────────────────────────────

    /// Spawn a session on the daemon.
    ///
    /// If `worktree` is true, the daemon will create a git worktree from the
    /// cwd's repo so this session gets its own working directory.
    pub async fn spawn_session(
        &mut self,
        shell: Option<String>,
        cwd: Option<String>,
        worktree: bool,
    ) -> Result<String> {
        let response = self
            .request(Request::SpawnSession {
                shell,
                cwd,
                worktree,
            })
            .await?;
        match response {
            Response::SessionSpawned { id } => Ok(id),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            other => anyhow::bail!("Unexpected response: {other:?}"),
        }
    }

    /// Kill a session.
    pub async fn kill_session(&mut self, id: &str) -> Result<()> {
        let response = self
            .request(Request::KillSession { id: id.to_string() })
            .await?;
        match response {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            other => anyhow::bail!("Unexpected response: {other:?}"),
        }
    }

    /// List all sessions.
    pub async fn list_sessions(&mut self) -> Result<Vec<SessionInfo>> {
        let response = self.request(Request::ListSessions).await?;
        match response {
            Response::SessionList { sessions } => Ok(sessions),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            other => anyhow::bail!("Unexpected response: {other:?}"),
        }
    }

    /// Get a full grid snapshot for a session.
    pub async fn get_session_state(&mut self, id: &str) -> Result<Response> {
        let response = self
            .request(Request::GetSessionState { id: id.to_string() })
            .await?;
        match response {
            state @ Response::SessionState { .. } => Ok(state),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            other => anyhow::bail!("Unexpected response: {other:?}"),
        }
    }

    /// Attach to a session and begin receiving updates.
    pub async fn attach(&mut self, id: &str, initial_size: Option<(u16, u16)>) -> Result<Response> {
        self.request(Request::Attach {
            id: id.to_string(),
            initial_size,
        })
        .await
    }

    /// Detach from a session.
    pub async fn detach(&mut self, id: &str) -> Result<()> {
        let response = self.request(Request::Detach { id: id.to_string() }).await?;
        match response {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            _ => Ok(()),
        }
    }

    /// Send input bytes to a session.
    pub async fn send_input(&mut self, id: &str, data: Vec<u8>) -> Result<()> {
        let response = self
            .request(Request::SendInput {
                id: id.to_string(),
                data,
            })
            .await?;
        match response {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            _ => Ok(()),
        }
    }

    /// Resize a session's PTY.
    pub async fn resize(&mut self, id: &str, cols: u16, rows: u16) -> Result<()> {
        let response = self
            .request(Request::Resize {
                id: id.to_string(),
                cols,
                rows,
            })
            .await?;
        match response {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            _ => Ok(()),
        }
    }

    /// Clone the underlying request sender.
    ///
    /// Used by `window.rs` for fire-and-forget sends (input, resize) from
    /// the synchronous event loop without needing `&mut self`.
    pub fn request_tx_clone(&self) -> mpsc::Sender<Request> {
        self.request_tx.clone()
    }

    /// Send a ping and wait for pong.
    pub async fn ping(&mut self) -> Result<()> {
        let response = self.request(Request::Ping).await?;
        match response {
            Response::Pong => Ok(()),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            other => anyhow::bail!("Unexpected response to ping: {other:?}"),
        }
    }
}
