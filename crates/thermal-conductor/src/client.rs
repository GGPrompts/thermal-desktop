//! Unix socket client for connecting to the thermal-conductor session daemon.
//!
//! Used by `window.rs` in client mode: when a daemon is running, the window
//! connects via this client instead of owning its own PTY directly.

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::protocol::{self, Request, Response};

/// A client connection to the session daemon.
#[allow(dead_code)]
pub struct DaemonClient {
    /// Sender for outgoing requests.
    request_tx: mpsc::Sender<Request>,
    /// Receiver for incoming responses/updates.
    response_rx: mpsc::Receiver<Response>,
}

#[allow(dead_code)]
impl DaemonClient {
    /// Try to connect to the daemon socket.
    ///
    /// Returns `Ok(Some(client))` if the daemon is running and connection succeeded.
    /// Returns `Ok(None)` if the socket does not exist (daemon not running).
    /// Returns `Err` on connection errors.
    pub async fn connect() -> Result<Option<Self>> {
        let socket_path = protocol::socket_path();

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
                    Ok(Some(payload)) => {
                        match protocol::decode_payload::<Response>(&payload) {
                            Ok(response) => {
                                if response_tx.send(response).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("Failed to decode daemon response: {e}");
                            }
                        }
                    }
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

        Ok(Some(Self {
            request_tx,
            response_rx,
        }))
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

    /// Send a request and wait for a single response.
    pub async fn request(&mut self, request: Request) -> Result<Response> {
        self.send(request).await?;
        self.recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("Daemon connection lost while waiting for response"))
    }

    /// Spawn a session on the daemon.
    pub async fn spawn_session(
        &mut self,
        shell: Option<String>,
        cwd: Option<String>,
    ) -> Result<String> {
        let response = self.request(Request::SpawnSession { shell, cwd }).await?;
        match response {
            Response::SessionSpawned { id } => Ok(id),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            other => anyhow::bail!("Unexpected response: {other:?}"),
        }
    }

    /// List all sessions.
    pub async fn list_sessions(&mut self) -> Result<Vec<protocol::SessionInfo>> {
        let response = self.request(Request::ListSessions).await?;
        match response {
            Response::SessionList { sessions } => Ok(sessions),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            other => anyhow::bail!("Unexpected response: {other:?}"),
        }
    }

    /// Attach to a session and begin receiving updates.
    pub async fn attach(
        &mut self,
        id: &str,
        initial_size: Option<(u16, u16)>,
    ) -> Result<Response> {
        self.request(Request::Attach {
            id: id.to_string(),
            initial_size,
        })
        .await
    }

    /// Detach from a session.
    pub async fn detach(&mut self, id: &str) -> Result<()> {
        let response = self
            .request(Request::Detach {
                id: id.to_string(),
            })
            .await?;
        match response {
            Response::Ok => Ok(()),
            Response::Error { message } => anyhow::bail!("Daemon error: {message}"),
            _ => Ok(()),
        }
    }

    /// Send input bytes to a session.
    pub async fn send_input(&self, id: &str, data: Vec<u8>) -> Result<()> {
        self.send(Request::SendInput {
            id: id.to_string(),
            data,
        })
        .await
    }

    /// Notify the daemon of a resize.
    pub async fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        self.send(Request::Resize {
            id: id.to_string(),
            cols,
            rows,
        })
        .await
    }
}
