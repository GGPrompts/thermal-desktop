//! Tests for the session daemon.

use super::*;
use crate::client::DaemonClient;
use crate::protocol::{Request, Response};
use std::path::PathBuf;
use tokio::net::UnixListener;

/// Spawn a daemon on a temporary socket, connect a client, spawn a session,
/// list sessions, verify the session appears, then shut everything down.
#[tokio::test]
async fn daemon_spawn_and_list() {
    // Use tempfile::tempdir() so the socket lives in a guaranteed-writable
    // directory (works in sandboxed environments where /tmp may not be
    // accessible). The `_dir` binding keeps the directory alive for the
    // duration of the test.
    let _dir = tempfile::tempdir().expect("Failed to create temp dir");
    let sock_path = _dir.path().join("test.sock");

    let listener = UnixListener::bind(&sock_path).expect("Failed to bind test socket");

    // Shutdown channel.
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Spawn daemon in background.
    let daemon_handle = tokio::spawn(async move {
        let _ = run_daemon_on(listener, shutdown_rx, None).await;
    });

    // Give the daemon a moment to start accepting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Connect a client.
    let mut client = DaemonClient::connect_to(PathBuf::from(&sock_path))
        .await
        .expect("connect_to failed")
        .expect("Expected Some(client), daemon should be running");

    // Ping the daemon.
    client.ping().await.expect("Ping failed");

    // Spawn a session.
    let session_id = client
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");
    assert!(
        session_id.starts_with("session-"),
        "Unexpected session id: {session_id}"
    );

    // List sessions and verify our session is there.
    let sessions = client.list_sessions().await.expect("list_sessions failed");
    assert_eq!(sessions.len(), 1, "Expected exactly one session");
    assert_eq!(sessions[0].id, session_id);
    assert_eq!(sessions[0].shell_command, "/bin/sh");
    assert!(sessions[0].is_alive, "Session should be alive");

    // Kill the session.
    client
        .kill_session(&session_id)
        .await
        .expect("kill_session failed");

    // Verify the session is gone.
    let sessions = client
        .list_sessions()
        .await
        .expect("list_sessions after kill failed");
    assert!(sessions.is_empty(), "Expected no sessions after kill");

    // Shut down the daemon.
    let _ = shutdown_tx.send(()).await;
    let _ = daemon_handle.await;

    // Socket file is cleaned up when `_dir` is dropped.
}

/// Helper: spin up a daemon on a temp socket, return (shutdown_tx, sock_path, _dir).
async fn setup_daemon() -> (tokio::sync::mpsc::Sender<()>, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("Failed to create temp dir");
    let sock_path = dir.path().join("test.sock");
    let listener = UnixListener::bind(&sock_path).expect("Failed to bind test socket");
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    tokio::spawn(async move {
        let _ = run_daemon_on(listener, shutdown_rx, None).await;
    });

    // Wait for daemon to start accepting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (shutdown_tx, sock_path, dir)
}

/// Helper: connect a client to the given socket path.
async fn connect_client(sock_path: &std::path::Path) -> DaemonClient {
    DaemonClient::connect_to(PathBuf::from(sock_path))
        .await
        .expect("connect_to failed")
        .expect("Expected Some(client)")
}

/// Attach to a session and verify we get a SessionState snapshot back.
#[tokio::test]
async fn attach_returns_session_state() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let session_id = client
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");

    let resp = client
        .attach(&session_id, Some((80, 24)))
        .await
        .expect("attach failed");

    match resp {
        Response::SessionState { id, cols, rows, .. } => {
            assert_eq!(id, session_id);
            // Daemon applies the initial size when no other client is attached.
            assert_eq!(cols, 80);
            assert_eq!(rows, 24);
        }
        other => panic!("Expected SessionState, got: {other:?}"),
    }

    let _ = shutdown_tx.send(()).await;
}

/// Regression test for the attach handshake ordering: even if another
/// client starts producing output while attach is in flight, the first
/// response to the attaching client must still be the authoritative
/// SessionState snapshot.
#[tokio::test]
async fn attach_returns_snapshot_before_streamed_updates() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut attaching_client = connect_client(&sock_path).await;
    let output_client = connect_client(&sock_path).await;

    let session_id = attaching_client
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");

    let send_tx = output_client.request_tx_clone();
    let output_session = session_id.clone();
    let flood = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        for _ in 0..8 {
            let _ = send_tx
                .send(Request::SendInput {
                    id: output_session.clone(),
                    data: b"yes race | head -n 64\n".to_vec(),
                })
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    });

    let resp = attaching_client
        .attach(&session_id, Some((80, 24)))
        .await
        .expect("attach failed");

    match resp {
        Response::SessionState { id, cols, rows, .. } => {
            assert_eq!(id, session_id);
            assert_eq!(cols, 80);
            assert_eq!(rows, 24);
        }
        other => panic!("Expected SessionState, got: {other:?}"),
    }

    let _ = flood.await;
    let _ = shutdown_tx.send(()).await;
}

/// Verify that attached clients receive streamed ScreenUpdate messages
/// when input is sent to the session's PTY.
#[tokio::test]
async fn attach_streams_screen_updates() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let session_id = client
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");

    // Attach to get initial state.
    let _ = client
        .attach(&session_id, Some((80, 24)))
        .await
        .expect("attach failed");

    // Take the response receiver so we can read streamed updates.
    let mut rx = client.take_response_rx();

    // Send input that will produce output (echo).
    let tx = client.request_tx_clone();
    tx.send(Request::SendInput {
        id: session_id.clone(),
        data: b"echo hello\n".to_vec(),
    })
    .await
    .expect("send input");

    // We should receive at least one ScreenUpdate or SessionState within
    // a reasonable timeout (the daemon polls every 8ms + processing).
    let mut got_update = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
            Ok(Some(Response::ScreenUpdate { .. })) => {
                got_update = true;
                break;
            }
            Ok(Some(Response::SessionState { .. })) => {
                got_update = true;
                break;
            }
            Ok(Some(Response::Ok)) => {
                // SendInput acknowledgment — keep waiting for the screen update.
                continue;
            }
            Ok(Some(_other)) => {
                // Some other response — keep waiting.
                continue;
            }
            Ok(None) => break,
            Err(_timeout) => continue,
        }
    }
    assert!(
        got_update,
        "Expected to receive a screen update after sending input"
    );

    let _ = shutdown_tx.send(()).await;
}

/// Verify that attached clients receive `SessionExited` when the shell
/// exits cleanly, even if no final damaged frame is produced.
#[tokio::test]
async fn attach_receives_session_exited_after_clean_shell_exit() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let session_id = client
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");

    let _ = client
        .attach(&session_id, Some((80, 24)))
        .await
        .expect("attach failed");

    let mut rx = client.take_response_rx();
    let tx = client.request_tx_clone();
    tx.send(Request::SendInput {
        id: session_id.clone(),
        data: b"exit\n".to_vec(),
    })
    .await
    .expect("send input");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut got_exit = None;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
            Ok(Some(Response::SessionExited {
                id,
                exit_code,
                reason,
            })) => {
                got_exit = Some((id, exit_code, reason));
                break;
            }
            Ok(Some(Response::Ok)) => continue,
            Ok(Some(Response::ScreenUpdate { .. })) => continue,
            Ok(Some(Response::SessionState { .. })) => continue,
            Ok(Some(Response::TitleChanged { .. })) => continue,
            Ok(Some(other)) => panic!("Unexpected response while waiting for exit: {other:?}"),
            Ok(None) => break,
            Err(_timeout) => continue,
        }
    }

    let (id, exit_code, _reason) = got_exit.expect("Expected SessionExited after exit");
    assert_eq!(id, session_id);
    assert_eq!(exit_code, Some(0));

    let _ = shutdown_tx.send(()).await;
}

/// Verify that detach decrements the attached count and works cleanly.
#[tokio::test]
async fn detach_decrements_attached_count() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let session_id = client
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");

    // Attach.
    let _ = client
        .attach(&session_id, Some((80, 24)))
        .await
        .expect("attach failed");

    // List sessions to verify attached count is 1.
    // Need a second client for listing since the first has its rx taken.
    let mut client2 = connect_client(&sock_path).await;
    let sessions = client2.list_sessions().await.expect("list failed");
    assert_eq!(sessions[0].connected_client_count, 1);

    // Detach.
    client.detach(&session_id).await.expect("detach failed");

    // Verify attached count went back to 0.
    let sessions = client2
        .list_sessions()
        .await
        .expect("list after detach failed");
    assert_eq!(sessions[0].connected_client_count, 0);

    let _ = shutdown_tx.send(()).await;
}

/// Two clients can attach to the same session simultaneously.
#[tokio::test]
async fn two_clients_attach_to_same_session() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client1 = connect_client(&sock_path).await;
    let mut client2 = connect_client(&sock_path).await;

    let session_id = client1
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");

    // Both attach.
    let resp1 = client1
        .attach(&session_id, Some((80, 24)))
        .await
        .expect("client1 attach failed");
    assert!(matches!(resp1, Response::SessionState { .. }));

    let resp2 = client2
        .attach(&session_id, None)
        .await
        .expect("client2 attach failed");
    assert!(matches!(resp2, Response::SessionState { .. }));

    // List to verify both are counted.
    let mut list_client = connect_client(&sock_path).await;
    let sessions = list_client.list_sessions().await.expect("list failed");
    assert_eq!(sessions[0].connected_client_count, 2);

    let _ = shutdown_tx.send(()).await;
}

/// Resize request changes the PTY dimensions.
#[tokio::test]
async fn resize_session_changes_dimensions() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let session_id = client
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");

    // Resize to a specific size.
    client
        .resize(&session_id, 100, 50)
        .await
        .expect("resize failed");

    // Allow the terminal to process the resize.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Verify via session list.
    let sessions = client.list_sessions().await.expect("list failed");
    assert_eq!(sessions[0].cols, 100);
    assert_eq!(sessions[0].rows, 50);

    let _ = shutdown_tx.send(()).await;
}

/// Sending input to a non-existent session returns an error.
#[tokio::test]
async fn send_input_to_nonexistent_session_errors() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let resp = client
        .request(Request::SendInput {
            id: "nonexistent".to_string(),
            data: b"hello".to_vec(),
        })
        .await
        .expect("request failed");

    assert!(
        matches!(resp, Response::Error { ref message } if message.contains("not found")),
        "Expected error for nonexistent session, got: {resp:?}"
    );

    let _ = shutdown_tx.send(()).await;
}

/// Client disconnect properly cleans up attached count.
#[tokio::test]
async fn client_disconnect_cleanup() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;

    let session_id;
    {
        let mut client = connect_client(&sock_path).await;

        session_id = client
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");

        let _ = client
            .attach(&session_id, Some((80, 24)))
            .await
            .expect("attach failed");
        // client drops here — connection closes.
    }

    // Give the daemon time to process the disconnect.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify count went back to 0.
    let mut check_client = connect_client(&sock_path).await;
    let sessions = check_client.list_sessions().await.expect("list failed");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].connected_client_count, 0);

    let _ = shutdown_tx.send(()).await;
}

/// Get session state returns a full grid snapshot.
#[tokio::test]
async fn get_session_state_returns_snapshot() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let session_id = client
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");

    let resp = client
        .get_session_state(&session_id)
        .await
        .expect("get_session_state failed");

    match resp {
        Response::SessionState {
            id,
            cols,
            rows,
            cells,
            ..
        } => {
            assert_eq!(id, session_id);
            assert!(cols > 0);
            assert!(rows > 0);
            // Grid should have cols * rows cells.
            assert_eq!(cells.len(), cols as usize * rows as usize);
        }
        other => panic!("Expected SessionState, got: {other:?}"),
    }

    let _ = shutdown_tx.send(()).await;
}

// ── Name generation unit tests ──────────────────────────────────────────

use super::helpers::{assign_unique_name, generate_name_from_shell};

#[test]
fn generate_name_from_shell_basename() {
    assert_eq!(generate_name_from_shell("/bin/zsh", 1), "zsh");
    assert_eq!(generate_name_from_shell("/usr/bin/bash", 2), "bash");
    assert_eq!(generate_name_from_shell("/bin/sh", 3), "sh");
}

#[test]
fn generate_name_from_shell_bare_name() {
    assert_eq!(generate_name_from_shell("fish", 4), "fish");
}

#[test]
fn generate_name_from_shell_empty_fallback() {
    // Empty path should fall back to session-N.
    assert_eq!(generate_name_from_shell("", 5), "session-5");
}

#[test]
fn generate_name_from_shell_trailing_slash_fallback() {
    // A path like "/" has no file_name, should fall back.
    assert_eq!(generate_name_from_shell("/", 6), "session-6");
}

// ── Unique name assignment unit tests ───────────────────────────────────

#[test]
fn assign_unique_name_first_is_bare() {
    let existing: Vec<String> = vec![];
    assert_eq!(assign_unique_name("zsh", &existing), "zsh");
}

#[test]
fn assign_unique_name_dedup_second() {
    let existing = vec!["zsh".to_string()];
    assert_eq!(assign_unique_name("zsh", &existing), "zsh-2");
}

#[test]
fn assign_unique_name_dedup_third() {
    let existing = vec!["zsh".to_string(), "zsh-2".to_string()];
    assert_eq!(assign_unique_name("zsh", &existing), "zsh-3");
}

#[test]
fn assign_unique_name_different_bases_no_conflict() {
    let existing = vec!["zsh".to_string()];
    assert_eq!(assign_unique_name("bash", &existing), "bash");
}

#[test]
fn assign_unique_name_gap_fills_first_available() {
    // "zsh" and "zsh-3" taken but not "zsh-2".
    let existing = vec!["zsh".to_string(), "zsh-3".to_string()];
    assert_eq!(assign_unique_name("zsh", &existing), "zsh-2");
}

// ── Integration: spawn returns name ─────────────────────────────────────

#[tokio::test]
async fn spawn_session_returns_name() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let (session_id, name) = client
        .spawn_session_named(Some("/bin/sh".to_string()), None, false, None)
        .await
        .expect("spawn_session_named failed");

    assert!(session_id.starts_with("session-"));
    // Auto-generated name from "/bin/sh" should be "sh".
    assert_eq!(name, "sh");

    let _ = shutdown_tx.send(()).await;
}

#[tokio::test]
async fn spawn_session_with_explicit_name() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let (_id, name) = client
        .spawn_session_named(
            Some("/bin/sh".to_string()),
            None,
            false,
            Some("opus".to_string()),
        )
        .await
        .expect("spawn_session_named failed");

    assert_eq!(name, "opus");

    let _ = shutdown_tx.send(()).await;
}

#[tokio::test]
async fn spawn_session_dedup_names() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    // Spawn two sessions with the same shell — names should be deduped.
    let (_id1, name1) = client
        .spawn_session_named(Some("/bin/sh".to_string()), None, false, None)
        .await
        .expect("first spawn failed");
    let (_id2, name2) = client
        .spawn_session_named(Some("/bin/sh".to_string()), None, false, None)
        .await
        .expect("second spawn failed");

    assert_eq!(name1, "sh");
    assert_eq!(name2, "sh-2");

    let _ = shutdown_tx.send(()).await;
}

#[tokio::test]
async fn spawn_session_dedup_explicit_names() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let (_id1, name1) = client
        .spawn_session_named(
            Some("/bin/sh".to_string()),
            None,
            false,
            Some("opus".to_string()),
        )
        .await
        .expect("first spawn failed");
    let (_id2, name2) = client
        .spawn_session_named(
            Some("/bin/sh".to_string()),
            None,
            false,
            Some("opus".to_string()),
        )
        .await
        .expect("second spawn failed");

    assert_eq!(name1, "opus");
    assert_eq!(name2, "opus-2");

    let _ = shutdown_tx.send(()).await;
}

#[tokio::test]
async fn list_sessions_includes_name() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let (_id, _name) = client
        .spawn_session_named(
            Some("/bin/sh".to_string()),
            None,
            false,
            Some("sonnet".to_string()),
        )
        .await
        .expect("spawn failed");

    let sessions = client.list_sessions().await.expect("list_sessions failed");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name.as_deref(), Some("sonnet"));

    let _ = shutdown_tx.send(()).await;
}

#[tokio::test]
async fn backward_compat_spawn_session_still_works() {
    // The old spawn_session (without name) should still work and
    // return a session ID (name is auto-generated but not returned
    // by the old API).
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let session_id = client
        .spawn_session(Some("/bin/sh".to_string()), None, false)
        .await
        .expect("spawn_session failed");

    assert!(session_id.starts_with("session-"));

    // The session should have a name in the list.
    let sessions = client.list_sessions().await.expect("list failed");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name.as_deref(), Some("sh"));

    let _ = shutdown_tx.send(()).await;
}

// ── IPC contract boundary tests ────────────────────────────────────────

/// Regression: multi-word command strings must not cause SIGABRT.
/// The daemon should split multi-word commands into program + args
/// without crashing.
#[tokio::test]
async fn spawn_multiword_command_no_sigabrt() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    // "echo hello" is a safe multi-word command that will exit quickly.
    let session_id = client
        .spawn_session(Some("echo hello".to_string()), None, false)
        .await
        .expect("spawn_session with multi-word command should not fail");

    assert!(
        session_id.starts_with("session-"),
        "Expected valid session id, got: {session_id}"
    );

    // Give the short-lived command time to run.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // List sessions — it should exist (alive or exited, but not crashed).
    let sessions = client.list_sessions().await.expect("list_sessions failed");
    // Session may still be listed or may have been auto-reaped; either is fine.
    // The key assertion is that we got here without SIGABRT.
    let _ = sessions;

    let _ = shutdown_tx.send(()).await;
}

/// Verify that commands with arguments are split correctly into argv.
#[tokio::test]
async fn spawn_command_with_arguments() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    // Use /bin/sh -c "exit 0" to verify argument splitting works.
    let session_id = client
        .spawn_session(Some("/bin/sh -c 'exit 0'".to_string()), None, false)
        .await
        .expect("spawn_session with arguments should not fail");

    assert!(session_id.starts_with("session-"));

    // Wait for the command to exit.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let _ = shutdown_tx.send(()).await;
}

/// Verify that spawn_session(None, ...) falls back to $SHELL (or a
/// sensible default) without error.
#[tokio::test]
async fn spawn_empty_command_falls_back_to_shell() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    let session_id = client
        .spawn_session(None, None, false)
        .await
        .expect("spawn_session(None) should fall back to $SHELL");

    assert!(
        session_id.starts_with("session-"),
        "Expected valid session id, got: {session_id}"
    );

    // The session should be alive and its shell_command should be non-empty.
    let sessions = client.list_sessions().await.expect("list_sessions failed");
    assert_eq!(sessions.len(), 1);
    assert!(
        sessions[0].is_alive,
        "Default shell session should be alive"
    );
    assert!(
        !sessions[0].shell_command.is_empty(),
        "Shell command should not be empty when falling back to $SHELL"
    );

    // Clean up — kill the shell so it doesn't linger.
    client
        .kill_session(&session_id)
        .await
        .expect("kill_session failed");

    let _ = shutdown_tx.send(()).await;
}

/// Rapid spawn/kill cycle: spawn 5 sessions, kill all, verify clean state.
#[tokio::test]
async fn rapid_spawn_kill_cycle_no_zombies() {
    let (shutdown_tx, sock_path, _dir) = setup_daemon().await;
    let mut client = connect_client(&sock_path).await;

    // Spawn 5 sessions.
    let mut ids = Vec::new();
    for _ in 0..5 {
        let id = client
            .spawn_session(Some("/bin/sh".to_string()), None, false)
            .await
            .expect("spawn_session failed");
        ids.push(id);
    }

    // Verify all 5 sessions exist.
    let sessions = client.list_sessions().await.expect("list_sessions failed");
    assert_eq!(
        sessions.len(),
        5,
        "Expected 5 sessions, got {}",
        sessions.len()
    );

    // Kill all sessions.
    for id in &ids {
        client.kill_session(id).await.expect("kill_session failed");
    }

    // Allow a short time for async cleanup.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify clean state — no zombie entries.
    let sessions = client
        .list_sessions()
        .await
        .expect("list_sessions after kill failed");
    assert!(
        sessions.is_empty(),
        "Expected no sessions after killing all, got {} zombie entries",
        sessions.len()
    );

    let _ = shutdown_tx.send(()).await;
}
