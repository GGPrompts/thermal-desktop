//! Client connection handler: reads requests from a single Unix socket client,
//! dispatches to the Daemon, and streams responses back.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use crate::protocol::{self, EventScope, Request, Response};
use crate::semantic_state::event_matches_categories;

use super::Daemon;

/// Handle a single client connection.
pub(super) async fn handle_client(daemon: Arc<Daemon>, stream: UnixStream) {
    let (mut reader, mut writer) = stream.into_split();
    let mut attached_session: Option<String> = None;

    // Cancellation token for the broadcast forwarder task. When the client
    // detaches or re-attaches to a different session, we abort the old
    // forwarder so we don't duplicate messages.
    let mut forwarder_handle: Option<tokio::task::JoinHandle<()>> = None;

    // Cancellation handle for the semantic event subscription forwarder.
    let mut event_forwarder_handle: Option<tokio::task::JoinHandle<()>> = None;

    // Spawn a task to forward update broadcasts to this client.
    let (client_tx, mut client_rx) = mpsc::channel::<Response>(64);

    // Writer task: sends responses to the client socket.
    let writer_handle = tokio::spawn(async move {
        while let Some(response) = client_rx.recv().await {
            match protocol::encode_frame(&response) {
                Ok(frame) => {
                    if let Err(e) = writer.write_all(&frame).await {
                        warn!("Failed to write to client: {e}");
                        break;
                    }
                }
                Err(e) => {
                    error!("Failed to encode response: {e}");
                }
            }
        }
    });

    loop {
        // Read the next request from the client.
        let payload = match protocol::read_frame(&mut reader).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                info!("Client disconnected");
                break;
            }
            Err(e) => {
                warn!("Client read error: {e}");
                break;
            }
        };

        let request: Request = match protocol::decode_payload(&payload) {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to decode client request: {e}");
                let _ = client_tx
                    .send(Response::Error {
                        message: format!("Invalid request: {e}"),
                    })
                    .await;
                continue;
            }
        };

        // Handle SubscribeEvents at the connection level — sends initial
        // snapshots for all in-scope sessions, then streams incremental events.
        if let Request::SubscribeEvents { ref scope } = request {
            // Abort any existing event subscription forwarder.
            if let Some(handle) = event_forwarder_handle.take() {
                handle.abort();
            }

            // Subscribe to the broadcast channel *before* taking snapshots
            // so we don't miss events that happen between snapshot and stream.
            let mut event_rx = daemon.event_bus.subscribe();
            let scope_clone = scope.clone();

            // Send initial snapshot syncs for all in-scope sessions.
            let syncs = daemon.event_bus.snapshot_syncs(scope);
            for sync in syncs {
                if client_tx.send(Response::SnapshotSync(sync)).await.is_err() {
                    break;
                }
            }

            // Spawn a forwarder task that streams events matching the scope.
            let client_tx_clone = client_tx.clone();
            let daemon_clone = Arc::clone(&daemon);
            event_forwarder_handle = Some(tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            // Filter by scope.
                            let matches = match &scope_clone {
                                EventScope::All => true,
                                EventScope::Session(id) => event.session_id == *id,
                                EventScope::Categories(cats) => {
                                    event_matches_categories(&event.kind, cats)
                                }
                            };
                            if matches {
                                let batch = crate::protocol::EventBatch {
                                    events: vec![event],
                                };
                                if client_tx_clone
                                    .send(Response::EventStream(batch))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(
                                skipped = n,
                                "Event subscriber lagged; sending resync snapshots"
                            );
                            // Force resync: re-send snapshots for all in-scope sessions.
                            let syncs = daemon_clone.event_bus.snapshot_syncs(&scope_clone);
                            for sync in syncs {
                                if client_tx_clone
                                    .send(Response::SnapshotSync(sync))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }));

            continue;
        }

        // Handle attach specially so the explicit SessionState snapshot is
        // always delivered before streamed broadcasts.
        if let Request::Attach { ref id, .. } = request {
            // If already attached to a session, detach from it first.
            if let Some(ref prev_id) = attached_session {
                // Abort the old forwarder task to stop duplicate messages.
                if let Some(handle) = forwarder_handle.take() {
                    handle.abort();
                }
                // Decrement the old session's attached count.
                let sessions = daemon.sessions.lock();
                if let Some(session_arc) = sessions.get(prev_id) {
                    let session = session_arc.lock();
                    session.attached_count.fetch_sub(1, Ordering::Relaxed);
                }
            }

            let response = daemon.handle_request(&request);
            let attach_ok = !matches!(response, Response::Error { .. });
            if client_tx.send(response).await.is_err() {
                break;
            }

            if attach_ok {
                if let Some(mut rx) = daemon.subscribe(id) {
                    attached_session = Some(id.clone());

                    // Spawn a task to forward broadcasts to the client
                    // channel. If the receiver lags, fall back to a full
                    // snapshot so the client can resynchronize.
                    let client_tx_clone = client_tx.clone();
                    let daemon_clone = Arc::clone(&daemon);
                    let session_id = id.clone();
                    forwarder_handle = Some(tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(response) => {
                                    if client_tx_clone.send(response).await.is_err() {
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(n)) => {
                                    warn!(session = %session_id, skipped = n, "Client lagged; sending full session snapshot");
                                    match daemon_clone.get_session_state(&session_id) {
                                        Some(state) => {
                                            if client_tx_clone.send(state).await.is_err() {
                                                break;
                                            }
                                        }
                                        None => break,
                                    }
                                }
                                Err(broadcast::error::RecvError::Closed) => {
                                    break;
                                }
                            }
                        }
                    }));
                } else {
                    warn!(session = %id, "Attach succeeded but broadcast subscription failed");
                }
            }

            continue;
        }

        // Handle detach — clean up forwarder and attached count.
        if let Request::Detach { ref id } = request {
            if attached_session.as_deref() == Some(id) {
                if let Some(handle) = forwarder_handle.take() {
                    handle.abort();
                }
                attached_session = None;
            }
        }

        let response = daemon.handle_request(&request);
        if client_tx.send(response).await.is_err() {
            break;
        }
    }

    // Clean up: abort forwarders and detach from session if attached.
    if let Some(handle) = forwarder_handle.take() {
        handle.abort();
    }
    if let Some(handle) = event_forwarder_handle.take() {
        handle.abort();
    }
    if let Some(id) = attached_session {
        let sessions = daemon.sessions.lock();
        if let Some(session_arc) = sessions.get(&id) {
            let session = session_arc.lock();
            session.attached_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    drop(client_tx);
    let _ = writer_handle.await;
}
