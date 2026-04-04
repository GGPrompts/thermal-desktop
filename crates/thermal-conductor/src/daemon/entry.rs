//! Public entry points: `run_daemon_on`, `run_daemon`, and the state file watcher.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use crate::persist;
use crate::protocol;

use super::Daemon;
use super::client_handler::handle_client;

// ── Public entry points ──────────────────────────────────────────────────────

/// Run the session daemon on a given `UnixListener` until the `shutdown` receiver
/// fires.
///
/// This is the core accept loop, factored out so that tests, alternative entry
/// points, and `run_daemon()` can supply their own socket path and shutdown
/// signal. An optional pre-created `Daemon` can be passed in; if `None`, a
/// fresh one is created.
pub async fn run_daemon_on(
    listener: UnixListener,
    mut shutdown: tokio::sync::mpsc::Receiver<()>,
    daemon: Option<Arc<Daemon>>,
) -> Result<Arc<Daemon>> {
    let daemon = daemon.unwrap_or_else(|| Arc::new(Daemon::new()));

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        info!("Client connected");
                        let daemon_clone = Arc::clone(&daemon);
                        tokio::spawn(handle_client(daemon_clone, stream));
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {e}");
                    }
                }
            }
            _ = shutdown.recv() => {
                info!("Shutdown signal received");
                break;
            }
        }
    }

    Ok(daemon)
}

// ── State file watcher ───────────────────────────────────────────────────────

/// Spawn a background task that watches `/tmp/{claude-code,codex,copilot}-state/`
/// via a single `ClaudeStatePoller` (inotify) and relays changes as semantic
/// events through the daemon's event bus.
///
/// This collapses the N separate inotify watchers that consumers (bar, audio,
/// HUD, TUI, monitor) would each create into a single watcher owned by the
/// daemon.  Consumers subscribe to the daemon's event stream instead.
///
/// Sessions imported via this watcher are tagged `backend: "external"` in the
/// semantic state, and `source: "daemon:external"` when converted to
/// `ClaudeSessionState` for subscribers. Daemon-owned PTY sessions have
/// `backend: "daemon"` and `source: "daemon"`.
fn spawn_state_file_watcher(daemon: Arc<Daemon>) {
    use std::collections::HashSet;
    use thermal_core::ClaudeStatePoller;

    tokio::spawn(async move {
        let mut poller = match ClaudeStatePoller::new() {
            Ok(p) => p,
            Err(e) => {
                warn!("State file watcher failed to start: {e}");
                return;
            }
        };

        info!("State file watcher started (single inotify for all consumers)");

        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Track which external session IDs we've seen, so we can detect removals.
        let mut known_external: HashSet<String> = HashSet::new();

        loop {
            interval.tick().await;

            let sessions = poller.poll();
            let mut current_ids: HashSet<String> = HashSet::new();

            for session in &sessions {
                let sid = &session.session_id;

                // Skip sessions the daemon owns via its inference engine.
                if daemon.event_bus.is_daemon_owned(sid) {
                    continue;
                }

                current_ids.insert(sid.clone());

                // Log when a new external session is first imported so the
                // daemon's authority boundary is visible in debug output.
                if !known_external.contains(sid) {
                    info!(
                        session = %sid,
                        agent_type = session.agent_type.as_deref().unwrap_or("unknown"),
                        "Importing external session (file-derived, source: daemon:external)"
                    );
                }
                daemon.event_bus.import_external_session(session);
            }

            // Detect removed external sessions.
            let removed: Vec<String> = known_external.difference(&current_ids).cloned().collect();
            for sid in &removed {
                // Only remove if it's still external (not daemon-owned).
                if !daemon.event_bus.is_daemon_owned(sid) {
                    info!(session = %sid, "Removing external session (state file gone)");
                    daemon.event_bus.remove_external_session(sid);
                }
            }

            known_external = current_ids;
        }
    });
}

/// Run the session daemon.
///
/// This is an async function that runs until interrupted (SIGTERM/SIGINT).
/// It binds a Unix socket and accepts client connections. It also registers
/// a D-Bus interface (`org.thermal.Conductor`) on the session bus so that
/// thermal-bar, thermal-hud, and other components can discover sessions
/// without a direct Unix socket connection.
pub async fn run_daemon() -> Result<()> {
    let socket_path = protocol::socket_path();
    info!(path = %socket_path.display(), "Starting session daemon");

    // Ensure runtime directory exists.
    thermal_core::runtime::ensure_runtime_dir()
        .with_context(|| "Failed to create thermal runtime directory")?;

    // Single-instance guard: flock-based (atomic, no TOCTOU race).
    // The lock is released automatically when _instance_lock is dropped (process exit).
    let _instance_lock = thermal_core::runtime::acquire_instance_lock("conductor");

    // Write pidfile for this instance (still useful for diagnostics / pgrep).
    let pidfile_path = thermal_core::runtime::pidfile_path("conductor");
    thermal_core::runtime::write_pidfile("conductor", &pidfile_path)
        .with_context(|| "Failed to write conductor pidfile")?;

    // Remove stale socket if present (checks whether a listener is alive).
    thermal_core::runtime::cleanup_stale_socket("conductor", &socket_path);

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Failed to bind Unix socket: {}", socket_path.display()))?;

    info!(path = %socket_path.display(), "Daemon listening");

    let daemon = Arc::new(Daemon::new());

    // ── Session recovery from previous daemon instance ──────────────────
    match persist::recover_sessions() {
        Ok(recovered) if !recovered.is_empty() => {
            let alive_count = recovered.iter().filter(|r| r.shell_alive).count();
            let dead_count = recovered.len() - alive_count;
            info!(
                total = recovered.len(),
                alive = alive_count,
                dead = dead_count,
                "Recovered sessions from previous daemon"
            );

            for r in &recovered {
                if r.shell_alive {
                    // The shell is still running, but we've lost the PTY master fd.
                    // Log as orphaned — future versions can re-adopt via fd passing.
                    warn!(
                        id = %r.session.id,
                        name = %r.session.name,
                        pid = r.session.shell_pid,
                        "Orphaned session: shell alive but PTY master lost — \
                         cannot re-attach (run `kill {}` to clean up)",
                        r.session.shell_pid
                    );
                } else {
                    info!(
                        id = %r.session.id,
                        name = %r.session.name,
                        pid = r.session.shell_pid,
                        "Previous session shell has exited — no recovery needed"
                    );
                }
            }
        }
        Ok(_) => {
            // No state file or empty — fresh start.
        }
        Err(e) => {
            warn!("Failed to recover sessions from previous daemon: {e}");
        }
    }

    // Register D-Bus interface on the session bus.
    let dbus_interface = crate::dbus_interface::ConductorInterface::new(Arc::clone(&daemon));
    let _dbus_conn = match zbus::connection::Builder::session()
        .and_then(|b| b.name(crate::dbus_interface::BUS_NAME))
        .and_then(|b| b.serve_at(crate::dbus_interface::OBJECT_PATH, dbus_interface))
    {
        Ok(builder) => match builder.build().await {
            Ok(conn) => {
                info!(
                    name = crate::dbus_interface::BUS_NAME,
                    path = crate::dbus_interface::OBJECT_PATH,
                    "D-Bus interface registered"
                );
                Some(conn)
            }
            Err(e) => {
                warn!("Failed to connect to D-Bus session bus: {e} — running without D-Bus");
                None
            }
        },
        Err(e) => {
            warn!("Failed to build D-Bus connection: {e} — running without D-Bus");
            None
        }
    };

    // Bridge SIGINT (ctrl_c) and SIGTERM into an mpsc channel so we can
    // reuse run_daemon_on(). Both signals trigger a clean shutdown with
    // socket removal — without SIGTERM handling, `kill` or systemd stop
    // would leave conductor.sock on disk.
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm =
                signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received SIGINT — shutting down");
                }
                _ = sigterm.recv() => {
                    info!("Received SIGTERM — shutting down");
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = shutdown_tx.send(()).await;
    });

    // ── Status bar layer-shell surface — top of screen, spawned first ──
    let _bar_handle = crate::bar::spawn(Arc::clone(&daemon.event_bus));

    // ── HUD layer-shell surface — below bar, spawned second ──
    let _hud_handle = crate::hud::spawn(Arc::clone(&daemon.event_bus));

    // ── State file watcher — single inotify for all consumers ──────────
    spawn_state_file_watcher(Arc::clone(&daemon));

    // ── Swarm watcher — auto-spawn terminal windows for subagents ─────
    crate::swarm_watcher::spawn_swarm_watcher(Arc::clone(&daemon.event_bus));

    // ── Transcript watcher — monitor Claude/Codex JSONL files ────────
    let _transcript_handle = crate::transcript_watcher::spawn_transcript_watcher();

    // Delegate to the shared accept loop.
    let daemon = run_daemon_on(listener, shutdown_rx, Some(daemon)).await?;

    // ── Persist session state before shutdown ─────────────────────────────
    let state = daemon.collect_persisted_state();
    if !state.sessions.is_empty() {
        match persist::save_state(&state) {
            Ok(()) => info!(
                sessions = state.sessions.len(),
                "Session state persisted for recovery"
            ),
            Err(e) => error!("Failed to persist session state: {e}"),
        }
    } else {
        // No sessions to save — clean up any stale state file.
        let _ = persist::remove_state_file();
    }

    // Clean up socket and pidfile.
    let _ = std::fs::remove_file(&socket_path);
    thermal_core::runtime::remove_pidfile("conductor", &pidfile_path);
    info!("Daemon shut down");
    Ok(())
}
