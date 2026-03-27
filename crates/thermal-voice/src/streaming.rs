//! WebSocket-based streaming STT client for WhisperLiveKit.
//!
//! Connects to a local WhisperLiveKit server over WebSocket and streams audio
//! in real-time, receiving partial and final transcript events as they arrive.
//!
//! Protocol (WhisperLiveKit JSON messages):
//!   Server → Client: `{"type": "partial", "text": "hello"}`
//!   Server → Client: `{"type": "final", "text": "hello world"}`
//!   Client → Server: binary frames containing 16-bit PCM audio

use anyhow::{Context, Result};
use futures_util::{FutureExt as _, SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, Message},
    MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, info, warn};

/// Default WhisperLiveKit server URL.
pub const DEFAULT_STREAMING_URL: &str = "ws://127.0.0.1:8765";

/// A transcript event received from the streaming STT server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    /// Partial (in-progress) transcript — may change as more audio arrives.
    Partial(String),
    /// Final (confirmed) transcript segment — will not change.
    Final(String),
}

/// Raw JSON message from the WhisperLiveKit server.
#[derive(Debug, Deserialize)]
struct ServerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    text: String,
}

/// WebSocket-based streaming transcriber that connects to a WhisperLiveKit server.
///
/// Usage:
/// 1. `StreamingTranscriber::new(url).await` — connect to the server
/// 2. `send_audio(samples, sample_rate)` — stream audio chunks (called repeatedly)
/// 3. `recv_transcript()` — poll for transcript events (non-blocking)
/// 4. `close()` — send close frame when speech ends
pub struct StreamingTranscriber {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl StreamingTranscriber {
    /// Connect to the WhisperLiveKit server at the given WebSocket URL.
    ///
    /// Returns an error if the connection cannot be established.
    pub async fn new(url: &str) -> Result<Self> {
        info!("connecting to streaming STT server at {url}");
        let (ws, response) = connect_async(url)
            .await
            .with_context(|| format!("failed to connect to streaming STT at {url}"))?;
        info!(
            "connected to streaming STT server (HTTP {})",
            response.status()
        );
        Ok(Self { ws })
    }

    /// Send audio samples to the server as a binary WebSocket message.
    ///
    /// Converts f32 samples (normalized -1.0..1.0) to 16-bit PCM bytes.
    /// The server expects 16kHz mono audio; if `sample_rate` differs from 16000,
    /// the caller should resample before calling this method.
    pub async fn send_audio(&mut self, samples: &[f32], sample_rate: u32) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }

        // Convert f32 samples to 16-bit PCM little-endian bytes
        let mut pcm_bytes = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            let clamped = s.clamp(-1.0, 1.0);
            let sample_i16 = (clamped * 32767.0) as i16;
            pcm_bytes.extend_from_slice(&sample_i16.to_le_bytes());
        }

        debug!(
            "sending {} bytes of PCM audio ({} samples at {sample_rate}Hz)",
            pcm_bytes.len(),
            samples.len()
        );

        self.ws
            .send(Message::Binary(pcm_bytes.into()))
            .await
            .context("failed to send audio to streaming STT server")?;

        Ok(())
    }

    /// Try to receive a transcript event from the server (non-blocking).
    ///
    /// Returns `Ok(Some(event))` if a transcript message was received,
    /// `Ok(None)` if no message is available yet, or an error if the
    /// connection is broken.
    #[allow(dead_code)] // Public API — available for callers that poll manually
    pub async fn recv_transcript(&mut self) -> Result<Option<TranscriptEvent>> {
        // Use try_next for non-blocking semantics via tokio::select or poll
        match futures_util::future::poll_fn(|cx| self.ws.poll_next_unpin(cx)).now_or_never() {
            Some(Some(Ok(msg))) => Self::parse_message(msg),
            Some(Some(Err(e))) => {
                // Connection-level errors
                match &e {
                    tungstenite::Error::ConnectionClosed
                    | tungstenite::Error::AlreadyClosed => {
                        debug!("streaming STT connection closed");
                        Ok(None)
                    }
                    _ => Err(e).context("streaming STT receive error"),
                }
            }
            Some(None) => {
                // Stream ended (server closed)
                debug!("streaming STT stream ended");
                Ok(None)
            }
            None => {
                // No message available right now
                Ok(None)
            }
        }
    }

    /// Receive the next transcript event, waiting until one arrives.
    ///
    /// Returns `Ok(None)` when the connection is closed, or `Ok(Some(event))`
    /// when a transcript message arrives.
    pub async fn recv_transcript_blocking(&mut self) -> Result<Option<TranscriptEvent>> {
        loop {
            match self.ws.next().await {
                Some(Ok(msg)) => {
                    match Self::parse_message(msg)? {
                        Some(event) => return Ok(Some(event)),
                        None => continue, // Skip non-transcript messages (ping, etc.)
                    }
                }
                Some(Err(e)) => {
                    match &e {
                        tungstenite::Error::ConnectionClosed
                        | tungstenite::Error::AlreadyClosed => {
                            return Ok(None);
                        }
                        _ => return Err(e).context("streaming STT receive error"),
                    }
                }
                None => return Ok(None),
            }
        }
    }

    /// Send a close frame to the server, signaling end of audio.
    pub async fn close(&mut self) -> Result<()> {
        info!("closing streaming STT connection");
        self.ws
            .close(None)
            .await
            .context("failed to close streaming STT connection")?;
        Ok(())
    }

    /// Parse a WebSocket message into a TranscriptEvent.
    fn parse_message(msg: Message) -> Result<Option<TranscriptEvent>> {
        match msg {
            Message::Text(text) => {
                let server_msg: ServerMessage = serde_json::from_str(&text)
                    .with_context(|| format!("failed to parse server message: {text}"))?;

                match server_msg.msg_type.as_str() {
                    "partial" => {
                        debug!("partial transcript: {}", server_msg.text);
                        Ok(Some(TranscriptEvent::Partial(server_msg.text)))
                    }
                    "final" => {
                        info!("final transcript: {}", server_msg.text);
                        Ok(Some(TranscriptEvent::Final(server_msg.text)))
                    }
                    other => {
                        debug!("ignoring unknown server message type: {other}");
                        Ok(None)
                    }
                }
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                // Control frames — ignore
                Ok(None)
            }
            Message::Binary(_) => {
                debug!("ignoring unexpected binary message from server");
                Ok(None)
            }
            Message::Close(_) => {
                debug!("server sent close frame");
                Ok(None)
            }
        }
    }
}

/// Collect all remaining final transcripts after sending close.
///
/// After calling `close()`, the server may still send final transcript segments.
/// This drains them and returns the concatenated final text.
pub async fn drain_final_transcripts(transcriber: &mut StreamingTranscriber) -> String {
    let mut finals: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            warn!("timeout waiting for final transcripts from streaming STT");
            break;
        }

        match tokio::time::timeout(remaining, transcriber.recv_transcript_blocking()).await {
            Ok(Ok(Some(TranscriptEvent::Final(text)))) => {
                if !text.is_empty() {
                    finals.push(text);
                }
            }
            Ok(Ok(Some(TranscriptEvent::Partial(_)))) => {
                // Skip partials during drain
                continue;
            }
            Ok(Ok(None)) => {
                // Connection closed
                break;
            }
            Ok(Err(e)) => {
                warn!("error draining transcripts: {e}");
                break;
            }
            Err(_) => {
                // Timeout
                warn!("timeout waiting for final transcripts");
                break;
            }
        }
    }

    finals.join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_partial_message() {
        let msg = Message::Text(r#"{"type": "partial", "text": "hello"}"#.into());
        let result = StreamingTranscriber::parse_message(msg).unwrap();
        assert_eq!(result, Some(TranscriptEvent::Partial("hello".into())));
    }

    #[test]
    fn parse_final_message() {
        let msg = Message::Text(r#"{"type": "final", "text": "hello world"}"#.into());
        let result = StreamingTranscriber::parse_message(msg).unwrap();
        assert_eq!(result, Some(TranscriptEvent::Final("hello world".into())));
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        let msg = Message::Text(r#"{"type": "info", "text": "ready"}"#.into());
        let result = StreamingTranscriber::parse_message(msg).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn parse_ping_returns_none() {
        let msg = Message::Ping(Vec::new().into());
        let result = StreamingTranscriber::parse_message(msg).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn parse_close_returns_none() {
        let msg = Message::Close(None);
        let result = StreamingTranscriber::parse_message(msg).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn parse_binary_returns_none() {
        let msg = Message::Binary(vec![1, 2, 3].into());
        let result = StreamingTranscriber::parse_message(msg).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn parse_empty_text_field() {
        let msg = Message::Text(r#"{"type": "partial", "text": ""}"#.into());
        let result = StreamingTranscriber::parse_message(msg).unwrap();
        assert_eq!(result, Some(TranscriptEvent::Partial("".into())));
    }

    #[test]
    fn parse_missing_text_field_defaults_empty() {
        let msg = Message::Text(r#"{"type": "final"}"#.into());
        let result = StreamingTranscriber::parse_message(msg).unwrap();
        assert_eq!(result, Some(TranscriptEvent::Final("".into())));
    }

    #[test]
    fn transcript_event_equality() {
        assert_eq!(
            TranscriptEvent::Partial("a".into()),
            TranscriptEvent::Partial("a".into())
        );
        assert_ne!(
            TranscriptEvent::Partial("a".into()),
            TranscriptEvent::Final("a".into())
        );
    }
}
