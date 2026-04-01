//! SCTK + wgpu window for thermal-conductor.
//!
//! Creates an xdg_toplevel window with a wgpu render pipeline that renders
//! a live terminal via glyphon. The grid renderer reads the term's
//! renderable content each frame and renders it via GPU.
//!
//! Supports two session modes:
//! - **Client mode**: connects to the session daemon via Unix socket. Input
//!   and resize are forwarded to the daemon; screen updates arrive as
//!   `ScreenUpdate` messages which are applied to the local Term.
//! - **Standalone mode**: spawns a PTY directly and owns it in-process.
//!   This is the legacy fallback when no daemon is running.
//!
//! Supports mouse-based text selection (click-drag) and primary selection
//! (middle-click paste) via the Wayland pointer protocol.

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, Modifiers},
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
};

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::term::TermMode;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr::NonNull;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::time::Instant;
use thermal_core::claude_state::{ClaudeSessionState, ClaudeStatePoller};

use crate::agent_graph::{AgentGraph, GRAPH_OVERLAY_HEIGHT};
use crate::agent_timeline::{AgentTimeline, TIMELINE_BAR_HEIGHT};
use crate::client::DaemonClient;
use crate::context_environment::{TerminalContext, detect_context};
use crate::font_config::FontConfig;
use crate::grid_renderer::{
    ContextHeatmapPipeline, EnvironmentEffectPipeline, GridRenderer,
};
use crate::inject::{self, InjectWatcher};
use crate::input;
use crate::protocol::Response;
use crate::terminal::Terminal;

const DEFAULT_WIDTH: u32 = 1200;
const DEFAULT_HEIGHT: u32 = 800;


/// Launch the SCTK + wgpu window with a live terminal.
pub fn run() -> anyhow::Result<()> {
    tracing::info!("thermal-conductor window starting");

    // ── Wayland connection ────────────────────────────────────────────────────
    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland display");
    let (globals, mut event_queue) = registry_queue_init(&conn).expect("Failed to init registry");
    let qh = event_queue.handle();

    // ── Bind globals ──────────────────────────────────────────────────────────
    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor is not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg_wm_base is not available");

    // ── Create xdg toplevel window ────────────────────────────────────────────
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    // Initial title — will be updated once session mode is determined.
    window.set_title("thermal");
    window.set_app_id("thermal-conductor");
    window.set_min_size(Some((400, 300)));

    // Initial commit — compositor will respond with a configure event
    window.commit();

    // ── wgpu setup ────────────────────────────────────────────────────────────
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        ..Default::default()
    });

    let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        NonNull::new(conn.backend().display_ptr() as *mut _).expect("Wayland display ptr is null"),
    ));
    let raw_window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
        NonNull::new(window.wl_surface().id().as_ptr().cast::<std::ffi::c_void>())
            .expect("wl_surface ptr is null"),
    ));

    let wgpu_surface = unsafe {
        instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle,
                raw_window_handle,
            })
            .expect("Failed to create wgpu surface")
    };

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: Some(&wgpu_surface),
        power_preference: wgpu::PowerPreference::None,
        force_fallback_adapter: false,
    }))
    .expect("Failed to find a suitable wgpu adapter");

    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
            .expect("Failed to create wgpu device");

    let caps = wgpu_surface.get_capabilities(&adapter);
    // Use non-sRGB format so color values pass through without gamma conversion.
    // This matches how traditional terminals work — sRGB values are written directly.
    let surface_format = caps
        .formats
        .iter()
        .copied()
        .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
        .or_else(|| {
            caps.formats
                .iter()
                .copied()
                .find(|f| *f == wgpu::TextureFormat::Bgra8UnormSrgb)
        })
        .unwrap_or(caps.formats[0]);
    let surface_config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    wgpu_surface.configure(&device, &surface_config);

    // ── Font configuration ─────────────────────────────────────────────────────
    let font_config = FontConfig::from_env();

    // ── Grid renderer ─────────────────────────────────────────────────────────
    let grid_renderer = GridRenderer::new(
        &device,
        &queue,
        surface_format,
        DEFAULT_WIDTH,
        DEFAULT_HEIGHT,
        font_config,
    );

    // ── Context heatmap pipeline ─────────────────────────────────────────────
    let context_heatmap = ContextHeatmapPipeline::new(&device, surface_format);

    // ── Environment effect pipeline ──────────────────────────────────────────
    let environment_effect = EnvironmentEffectPipeline::new(&device, surface_format);
    let terminal_context = detect_context();
    tracing::info!(?terminal_context, "Detected terminal environment context");

    // ── Terminal + session (daemon client or standalone PTY) ──────────────────
    // Calculate initial grid size from the renderer's cell metrics.
    let (init_cols, init_rows) = grid_renderer.grid_size(DEFAULT_WIDTH, DEFAULT_HEIGHT);
    let mut terminal = Terminal::with_size(init_cols, init_rows);

    // Start a tokio runtime for the async PTY reader / daemon client.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .expect("Failed to create tokio runtime");

    // Shared dirty flag: set to true whenever new terminal content is
    // available (from either the PTY byte processor or daemon screen updates).
    let pty_dirty = Arc::new(AtomicBool::new(false));
    // Wakeup pipe: written to after content updates so poll() wakes immediately.
    let (wakeup_read, wakeup_write) = nix::unistd::pipe().expect("Failed to create wakeup pipe");
    // Set read end to non-blocking so we can drain it without blocking.
    {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let flags = fcntl(wakeup_read.as_raw_fd(), FcntlArg::F_GETFL).unwrap_or(0);
        let _ = fcntl(
            wakeup_read.as_raw_fd(),
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        );
    }
    let wakeup_read_fd = wakeup_read.as_raw_fd();

    // Enter the tokio runtime context for spawning async tasks.
    let _guard = tokio_rt.enter();

    // Shared flag for the daemon reader task to signal session exit.
    let daemon_exit_requested = Arc::new(AtomicBool::new(false));
    // Daemon-fed client updates mutate the local Term directly, so force a
    // full render on the next frame instead of trusting alacritty damage.
    let force_full_redraw = Arc::new(AtomicBool::new(false));
    // Client mode also needs the daemon's terminal mode bits for mouse,
    // bracketed paste, focus reporting, and kitty keyboard handling.
    let synced_term_mode = Arc::new(AtomicU32::new(TermMode::default().bits()));

    // Shared slot for the daemon reader task to deliver title updates.
    // The render loop drains this each iteration and calls window.set_title().
    let pending_title: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Try to connect to the session daemon. If it is running, use client
    // mode; otherwise fall back to standalone mode with a local PTY.
    let (session_mode, term_event_rx, pty_child_pid) = tokio_rt.block_on(async {
        match DaemonClient::connect().await {
            Ok(Some(mut client)) => {
                // Verify the daemon is actually alive (stale sockets can
                // linger after a crash or restart).
                if !client.is_healthy().await {
                    tracing::warn!("Daemon socket exists but is not responding — standalone mode");
                    let _ = std::fs::remove_file(crate::protocol::socket_path());
                    return setup_standalone_session(
                        &mut terminal,
                        init_cols,
                        init_rows,
                        Arc::clone(&pty_dirty),
                        wakeup_write,
                    );
                }

                tracing::info!("Session daemon available — entering client mode");

                // List existing sessions.
                let sessions = match client.list_sessions().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("Failed to list sessions: {e} — falling back to standalone");
                        return setup_standalone_session(
                            &mut terminal,
                            init_cols,
                            init_rows,
                            Arc::clone(&pty_dirty),
                            wakeup_write,
                        );
                    }
                };

                // Always spawn a fresh session. Reusing orphaned sessions from
                // previous windows leads to stale shells with wrong terminal
                // size and leftover state. Orphaned sessions (alive but 0
                // connected clients) are cleaned up below.
                for orphan in sessions.iter().filter(|s| s.is_alive && s.connected_client_count == 0) {
                    tracing::info!(id = %orphan.id, "Killing orphaned daemon session");
                    let _ = client.send(crate::protocol::Request::KillSession {
                        id: orphan.id.clone(),
                    }).await;
                }
                let session_id = {
                    let shell =
                        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                    match client.spawn_session(Some(shell), None, false).await {
                        Ok(id) => {
                            tracing::info!(id = %id, "Spawned new session on daemon");
                            id
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to spawn session on daemon: {e} — falling back to standalone"
                            );
                            return setup_standalone_session(
                                &mut terminal,
                                init_cols,
                                init_rows,
                                Arc::clone(&pty_dirty),
                                wakeup_write,
                            );
                        }
                    }
                };

                // Attach to the session with our initial grid size.
                let attach_response = match client
                    .attach(&session_id, Some((init_cols as u16, init_rows as u16)))
                    .await
                {
                    Ok(resp) => resp,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to attach to session: {e} — falling back to standalone"
                        );
                        return setup_standalone_session(
                            &mut terminal,
                            init_cols,
                            init_rows,
                            Arc::clone(&pty_dirty),
                            wakeup_write,
                        );
                    }
                };

                // If the daemon sent initial session state, apply it to the
                // local alacritty Term so the first frame renders correctly.
                if let Response::SessionState {
                    cols,
                    rows,
                    mode,
                    cells: ref _cells,
                    ..
                } = attach_response
                {
                    tracing::info!(
                        cols,
                        rows,
                        "Received initial session state from daemon"
                    );
                    synced_term_mode.store(mode, Ordering::Release);
                    apply_session_state_to_term(&terminal, &attach_response);
                }

                // Take the terminal event receiver.
                let term_event_rx =
                    terminal.take_event_rx().expect("event_rx already taken");

                // Take the response receiver from the client so the daemon
                // reader task can consume streamed ScreenUpdate messages.
                // The client retains the request sender for input/resize.
                let response_rx = client.take_response_rx();

                // Dup the write end of the wakeup pipe for the daemon reader
                // task. The original OwnedFd will drop when this async block
                // ends (in the standalone path it's moved to spawn_byte_processor
                // instead). The dup'd OwnedFd is moved into the spawned task.
                let task_wakeup_fd = {
                    let raw = nix::unistd::dup(wakeup_write.as_raw_fd())
                        .expect("Failed to dup wakeup write fd");
                    unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }
                };

                // Spawn a background task that reads daemon responses and
                // feeds screen updates into the local Term + dirty flag.
                spawn_daemon_reader_task(
                    &terminal,
                    response_rx,
                    Arc::clone(&pty_dirty),
                    Arc::clone(&force_full_redraw),
                    Arc::clone(&synced_term_mode),
                    Arc::clone(&daemon_exit_requested),
                    Arc::clone(&pending_title),
                    task_wakeup_fd,
                );

                let mode = SessionMode::Client {
                    client,
                    session_id,
                };

                // No PTY child PID in client mode — the daemon owns it.
                (mode, term_event_rx, 0i32)
            }
            Ok(None) => {
                tracing::info!("No session daemon running — standalone mode");
                setup_standalone_session(
                    &mut terminal,
                    init_cols,
                    init_rows,
                    Arc::clone(&pty_dirty),
                    wakeup_write,
                )
            }
            Err(e) => {
                tracing::warn!("Daemon connection error: {e} — standalone mode");
                setup_standalone_session(
                    &mut terminal,
                    init_cols,
                    init_rows,
                    Arc::clone(&pty_dirty),
                    wakeup_write,
                )
            }
        }
    });

    tracing::info!(cols = init_cols, rows = init_rows, "Terminal initialized");

    // Set initial window title based on session mode.
    {
        let initial_title = match &session_mode {
            SessionMode::Client { session_id, .. } => {
                format!(
                    "thermal \u{2014} session {}",
                    &session_id[..session_id.len().min(8)]
                )
            }
            SessionMode::Standalone { .. } => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
                let shell_name = std::path::Path::new(&shell)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("sh");
                format!("thermal \u{2014} {shell_name}")
            }
        };
        window.set_title(&initial_title);
    }

    // ── Agent state source ───────────────────────────────────────────────────
    // In client mode (daemon available), prefer semantic subscriptions.
    // Sessions via daemon have source: "daemon" or "daemon:external".
    // In standalone mode, fall back to file-watching via ClaudeStatePoller
    // (compatibility path for unmanaged sessions, source not tagged).
    let is_client_mode = matches!(session_mode, SessionMode::Client { .. });
    let daemon_sub_rx = if is_client_mode {
        crate::daemon_subscriber::try_spawn_subscriber()
    } else {
        None
    };
    let claude_poller = if daemon_sub_rx.is_some() {
        tracing::info!("Using daemon semantic subscription for agent state (source: daemon)");
        None
    } else {
        match ClaudeStatePoller::new() {
            Ok(poller) => {
                tracing::info!("Using ClaudeStatePoller fallback for agent state (source: file-derived)");
                Some(poller)
            }
            Err(e) => {
                tracing::warn!("Failed to create Claude state poller: {e} — HUD disabled");
                None
            }
        }
    };

    // ── Cross-pane inject watcher ───────────────────────────────────────────
    let inject_session_id = inject::generate_session_id();
    let inject_watcher = match InjectWatcher::new(inject_session_id.clone()) {
        Ok(w) => {
            tracing::info!(session_id = %inject_session_id, "Inject watcher initialized");
            Some(w)
        }
        Err(e) => {
            tracing::warn!("Failed to create inject watcher: {e} — cross-pane inject disabled");
            None
        }
    };

    // ── Build state ───────────────────────────────────────────────────────────
    let mut state = ConductorWindow {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        window,
        wgpu: WgpuState {
            device,
            queue,
            surface: wgpu_surface,
            config: surface_config,
        },
        grid_renderer,
        context_heatmap,
        environment_effect,
        terminal_context,
        terminal,
        session_mode,
        _tokio_rt: tokio_rt,
        configured: false,
        dirty: true,
        pty_dirty,
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        exit: false,
        daemon_exit_requested,
        force_full_redraw,
        synced_term_mode,
        pending_title,
        keyboard: None,
        seat: None,
        modifiers: Modifiers {
            ctrl: false,
            alt: false,
            shift: false,
            caps_lock: false,
            logo: false,
            num_lock: false,
        },
        pointer: None,
        mouse_left_held: false,
        repeat_key: None,
        repeat_next: None,
        repeat_delay: std::time::Duration::from_millis(400),
        repeat_rate: std::time::Duration::from_millis(33),
        render_deadline: None,
        term_event_rx,
        claude_poller,
        claude_session: None,
        daemon_sub_rx,
        pty_child_pid,
        inject_session_id,
        inject_watcher,
        context_warning_active: false,
        context_critical_active: false,
        agent_timeline: AgentTimeline::new(),
        agent_graph: AgentGraph::new(),
        bell_mode: BellMode::from_env(),
        bell_flash_until: None,
    };

    // ── Event loop ────────────────────────────────────────────────────────────
    // Non-blocking dispatch with short poll timeout for low-latency input
    // and PTY output. Key repeat is driven by our own timer since we don't
    // use calloop (which SCTK's built-in repeat requires).
    loop {
        // Flush outgoing Wayland requests.
        if let Err(e) = conn.flush() {
            tracing::warn!("Wayland flush failed: {e}");
        }

        // Determine poll timeout based on whether key repeat is active.
        // When idle, use 16ms (~60fps). When repeat is pending, wake at
        // the exact repeat time. This avoids busy-spinning while idle.
        let poll_ms: u16 = if let Some(next) = state.repeat_next {
            let until = next.saturating_duration_since(std::time::Instant::now());
            (until.as_millis().min(16) as u16).max(1)
        } else {
            16
        };

        // Try to prepare a read guard. If None, there are already pending events.
        if let Some(guard) = conn.prepare_read() {
            use std::os::fd::AsRawFd;
            let wl_fd = guard.connection_fd().as_raw_fd();
            // Poll BOTH the Wayland fd AND the wakeup pipe. This way we
            // wake instantly when either Wayland events or PTY data arrive,
            // instead of waiting for the timeout.
            let mut pollfds = [
                nix::poll::PollFd::new(
                    unsafe { std::os::fd::BorrowedFd::borrow_raw(wl_fd) },
                    nix::poll::PollFlags::POLLIN,
                ),
                nix::poll::PollFd::new(
                    unsafe { std::os::fd::BorrowedFd::borrow_raw(wakeup_read_fd) },
                    nix::poll::PollFlags::POLLIN,
                ),
            ];
            let _ = nix::poll::poll(&mut pollfds, nix::poll::PollTimeout::from(poll_ms));
            let _ = guard.read();
            // Drain the wakeup pipe (non-blocking).
            let mut drain_buf = [0u8; 64];
            use std::io::Read;
            let mut wakeup_file = unsafe { std::fs::File::from_raw_fd(wakeup_read_fd) };
            let _ = wakeup_file.read(&mut drain_buf);
            // Don't let File close the fd — we need it for the next iteration.
            std::mem::forget(wakeup_file);
        }

        // Dispatch all pending Wayland events.
        event_queue
            .dispatch_pending(&mut state)
            .expect("Wayland event dispatch failed");

        // ── Key repeat ──────────────────────────────────────────────────
        if let (Some(key), Some(next)) = (&state.repeat_key, state.repeat_next)
            && std::time::Instant::now() >= next
        {
            let key_clone = key.clone();
            // Use kitty Repeat event type when kitty keyboard protocol is active.
            if let Some(bytes) = state.encode_key_event(&key_clone, input::KeyEventType::Repeat) {
                state.write_session(&bytes);
            }
            state.repeat_next = Some(std::time::Instant::now() + state.repeat_rate);
            // Don't set dirty — PTY echo will set pty_dirty.
        }

        // Drain terminal events — relay PtyWrite responses back to the PTY.
        // This is critical: alacritty_terminal generates responses to DA1, DA2,
        // mode queries, etc. via Event::PtyWrite. Without this, TUI apps like
        // bubbletea timeout waiting for responses (~2-4 seconds).
        while let Ok(event) = state.term_event_rx.try_recv() {
            match event {
                TermEvent::PtyWrite(text) => {
                    state.write_session(text.as_bytes());
                }
                TermEvent::Title(title) => {
                    state.window.set_title(&title);
                }
                TermEvent::Bell => {
                    if state.bell_mode == BellMode::Visual {
                        state.bell_flash_until = Some(Instant::now() + BELL_FLASH_DURATION);
                        state.dirty = true;
                    }
                }
                _ => {}
            }
        }

        // Drain pending title from daemon reader task (client mode).
        if let Ok(mut guard) = state.pending_title.try_lock() {
            if let Some(title) = guard.take() {
                state.window.set_title(&title);
            }
        }

        // ── Poll Claude state ──────────────────────────────────────────
        // Prefer daemon subscription (client mode) over file-watching.
        let all_sessions = if let Some(ref rx) = state.daemon_sub_rx {
            let sessions = rx.borrow().clone();
            // In client mode, match by session_id if available, else by pid.
            if let SessionMode::Client { ref session_id, .. } = state.session_mode {
                state.claude_session = sessions
                    .iter()
                    .find(|s| s.session_id == *session_id)
                    .cloned()
                    .or_else(|| find_matching_session(&sessions, state.pty_child_pid));
            } else {
                state.claude_session = find_matching_session(&sessions, state.pty_child_pid);
            }
            sessions
        } else if let Some(ref mut poller) = state.claude_poller {
            let sessions = poller.poll();
            state.claude_session = find_matching_session(&sessions, state.pty_child_pid);
            sessions
        } else {
            Vec::new()
        };

        // ── Track tool changes for the agent timeline ────────────────────
        if let Some(ref session) = state.claude_session {
            state
                .agent_timeline
                .record_tool_change(session.current_tool.as_deref());
        } else if state.agent_timeline.visible {
            state.agent_timeline.record_idle();
        }

        // ── Update agent communication graph ─────────────────────────────
        if state.agent_graph.visible {
            state
                .agent_graph
                .set_layout_size(state.width as f32, GRAPH_OVERLAY_HEIGHT as f32);
            state.agent_graph.update_from_sessions(&all_sessions);
            state.agent_graph.tick_layout();
        }

        // ── Update context saturation warnings ──────────────────────────
        state.update_context_warnings();

        // ── Poll cross-pane inject watcher ────────────────────────────────
        // Non-blocking: picks up inject files from other windows.
        state.poll_inject_watcher();

        // Check whether the byte processor has produced new PTY output.
        if state.pty_dirty.swap(false, Ordering::AcqRel) {
            state.dirty = true;
            // Set a coalescing deadline: wait up to 8ms for more PTY data
            // to arrive before rendering. This avoids rendering dozens of
            // intermediate frames during TUI startup floods.
            if state.render_deadline.is_none() {
                state.render_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(8));
            }
        }

        // Keep redrawing when the timeline is visible (pulse animation).
        if state.agent_timeline.visible && !state.agent_timeline.entries.is_empty() {
            state.dirty = true;
        }

        // Keep redrawing when the agent graph is visible (layout animation + arc fading).
        if state.agent_graph.visible && !state.agent_graph.nodes.is_empty() {
            state.dirty = true;
        }

        // Keep redrawing while bell flash is active; clear once expired.
        if let Some(until) = state.bell_flash_until {
            if Instant::now() < until {
                state.dirty = true;
            } else {
                state.bell_flash_until = None;
                state.dirty = true; // one final redraw to clear the overlay
            }
        }

        if state.configured && state.dirty {
            // If we have a coalescing deadline and it hasn't expired yet,
            // skip this frame to accumulate more PTY output.
            if let Some(deadline) = state.render_deadline
                && std::time::Instant::now() < deadline
            {
                continue;
            }
            state.render_frame();
            state.dirty = false;
            state.render_deadline = None;
        }

        // Exit if the shell process died (e.g. user typed `exit`).
        if state.session_has_exited() {
            tracing::info!("Shell exited, closing window");
            break;
        }

        if state.exit {
            tracing::info!("thermal-conductor window exiting");
            break;
        }
    }

    // ── Cleanup: kill daemon session on window close ─────────────────────
    // In client mode, the window spawned (or attached to) a daemon session.
    // If we don't kill it, the shell keeps running in the background and the
    // next `thc window` will reattach to the stale session instead of getting
    // a fresh one.
    if let SessionMode::Client { client, session_id } = &state.session_mode {
        tracing::info!(session = %session_id, "Killing daemon session on window close");
        let client_tx = client.request_tx_clone();
        let id = session_id.clone();
        let _ = state._tokio_rt.block_on(async {
            let _ = client_tx
                .send(crate::protocol::Request::KillSession { id })
                .await;
        });
    }

    Ok(())
}


// ── wgpu state ────────────────────────────────────────────────────────────────

pub(super) struct WgpuState {
    pub(super) device: wgpu::Device,
    pub(super) queue: wgpu::Queue,
    pub(super) surface: wgpu::Surface<'static>,
    pub(super) config: wgpu::SurfaceConfiguration,
}

// ── Main window struct ────────────────────────────────────────────────────────

pub(super) struct ConductorWindow {
    pub(super) registry_state: RegistryState,
    pub(super) seat_state: SeatState,
    pub(super) output_state: OutputState,
    #[allow(dead_code)]
    pub(super) window: Window,
    pub(super) wgpu: WgpuState,
    pub(super) grid_renderer: GridRenderer,
    /// Context heatmap vignette — subtle edge glow driven by context_percent.
    pub(super) context_heatmap: ContextHeatmapPipeline,
    /// Environment effect — border glow indicating Docker/worktree/SSH context.
    pub(super) environment_effect: EnvironmentEffectPipeline,
    /// Detected terminal execution environment (Docker, worktree, SSH, or main).
    pub(super) terminal_context: TerminalContext,
    pub(super) terminal: Terminal,
    /// Session mode: either daemon client or standalone PTY.
    pub(super) session_mode: SessionMode,
    pub(super) _tokio_rt: tokio::runtime::Runtime,
    pub(super) configured: bool,
    /// Whether the window needs to be redrawn this iteration.
    pub(super) dirty: bool,
    /// Set to `true` by the PTY byte processor (or daemon reader) when new
    /// terminal output has been processed; cleared each time the render loop
    /// checks it.
    pub(super) pty_dirty: Arc<AtomicBool>,
    /// Force a full grid snapshot on the next render.
    pub(super) force_full_redraw: Arc<AtomicBool>,
    /// Mirrored terminal mode bits from the daemon in client mode.
    pub(super) synced_term_mode: Arc<AtomicU32>,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) exit: bool,
    /// Set to `true` by the daemon reader task when a `SessionExited` message
    /// arrives. Checked by `session_has_exited()` in client mode.
    pub(super) daemon_exit_requested: Arc<AtomicBool>,
    /// Pending title update from the daemon reader task. Drained each event
    /// loop iteration and applied to the Wayland surface via `set_title()`.
    pub(super) pending_title: Arc<Mutex<Option<String>>>,
    pub(super) keyboard: Option<wl_keyboard::WlKeyboard>,
    pub(super) seat: Option<wl_seat::WlSeat>,
    pub(super) modifiers: Modifiers,
    // Mouse / pointer state
    pub(super) pointer: Option<wl_pointer::WlPointer>,
    /// Whether the left mouse button is currently held (for drag selection).
    pub(super) mouse_left_held: bool,
    // Key repeat state
    /// The last key event that should repeat, or None if no repeat is active.
    pub(super) repeat_key: Option<KeyEvent>,
    /// When key repeat should next fire.
    pub(super) repeat_next: Option<std::time::Instant>,
    /// Delay before first repeat (typically ~400ms).
    pub(super) repeat_delay: std::time::Duration,
    /// Interval between repeats (typically ~33ms for 30 chars/sec).
    pub(super) repeat_rate: std::time::Duration,
    /// When set, defer rendering until this deadline to coalesce PTY output
    /// (e.g. during TUI startup floods). Cleared after each render.
    pub(super) render_deadline: Option<std::time::Instant>,
    /// Terminal event receiver — relays PtyWrite responses back to the PTY.
    pub(super) term_event_rx: tokio::sync::mpsc::UnboundedReceiver<TermEvent>,
    /// Claude state poller — watches /tmp/claude-code-state/ for session files.
    /// Used in standalone mode; `None` when daemon subscription is active.
    pub(super) claude_poller: Option<ClaudeStatePoller>,
    /// Cached matching Claude session for the HUD overlay.
    pub(super) claude_session: Option<ClaudeSessionState>,
    /// Daemon semantic subscription — receives session states from the daemon.
    /// Active in client mode; takes priority over `claude_poller`.
    pub(super) daemon_sub_rx: Option<tokio::sync::watch::Receiver<Vec<ClaudeSessionState>>>,
    /// PID of the PTY child process, used to read cwd via /proc/<pid>/cwd.
    /// Zero in client mode (daemon owns the process).
    pub(super) pty_child_pid: i32,
    // Cross-pane prompt injection
    /// Unique session ID for this window instance (used to ignore own inject files).
    pub(super) inject_session_id: String,
    /// File watcher on `/tmp/thermal-inject/` for receiving injections from other windows.
    pub(super) inject_watcher: Option<InjectWatcher>,
    // Context saturation warning state
    /// Whether the 85% context warning overlay is currently displayed.
    pub(super) context_warning_active: bool,
    /// Whether the 95% context critical overlay is currently displayed.
    pub(super) context_critical_active: bool,
    /// Agent tool-usage timeline bar (toggled with Ctrl+Shift+T).
    pub(super) agent_timeline: AgentTimeline,
    /// Agent communication graph overlay (toggled with F3).
    pub(super) agent_graph: AgentGraph,
    // Bell (visual flash) state
    /// How to handle BEL characters from the terminal.
    pub(super) bell_mode: BellMode,
    /// When set, a translucent flash overlay is rendered until this instant.
    pub(super) bell_flash_until: Option<Instant>,
}


// ── Compositor handler ────────────────────────────────────────────────────────

impl CompositorHandler for ConductorWindow {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

// ── Output handler ────────────────────────────────────────────────────────────

impl OutputHandler for ConductorWindow {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

// ── XDG window handler ───────────────────────────────────────────────────────

impl WindowHandler for ConductorWindow {
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {
        tracing::info!("Window close requested");
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let (new_w, new_h) = configure.new_size;
        let w = new_w.map(|v| v.get()).unwrap_or(self.width);
        let h = new_h.map(|v| v.get()).unwrap_or(self.height);

        if w != self.width || h != self.height || !self.configured {
            self.width = w;
            self.height = h;
            self.wgpu.config.width = w;
            self.wgpu.config.height = h;
            self.wgpu
                .surface
                .configure(&self.wgpu.device, &self.wgpu.config);

            // Resize the grid renderer viewport.
            self.grid_renderer
                .resize(&self.wgpu.device, &self.wgpu.queue, w, h);

            // Recalculate terminal grid dimensions and resize.
            // Account for the timeline bar and graph overlay when visible.
            let mut effective_h = h;
            if self.agent_timeline.visible {
                effective_h = effective_h.saturating_sub(TIMELINE_BAR_HEIGHT);
            }
            if self.agent_graph.visible {
                effective_h = effective_h.saturating_sub(GRAPH_OVERLAY_HEIGHT);
            }
            let (cols, rows) = self.grid_renderer.grid_size(w, effective_h);
            self.terminal.resize(
                cols,
                rows,
                self.grid_renderer.cell_width as u16,
                self.grid_renderer.cell_height as u16,
            );
            self.resize_session(cols as u16, rows as u16);

            tracing::debug!("Window configured: {}x{} (grid: {}x{})", w, h, cols, rows);

            // On the first configure, clear any scrollback created by the
            // initial resize (terminal was created at DEFAULT_WIDTH x DEFAULT_HEIGHT
            // but the compositor may force a different size). Mirrors kitty's
            // approach of deferring the authoritative size until after configure.
            if !self.configured {
                self.terminal.clear_history();
            }
        }

        self.configured = true;
        self.dirty = true;
    }
}

// ── Seat handler ──────────────────────────────────────────────────────────────

impl SeatHandler for ConductorWindow {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("Failed to create keyboard");
            self.keyboard = Some(keyboard);
            self.seat = Some(seat.clone());
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = Some(
                self.seat_state
                    .get_pointer(qh, &seat)
                    .expect("Failed to create pointer"),
            );
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(kb) = self.keyboard.take()
        {
            kb.release();
        }
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

// ── Delegate macros ───────────────────────────────────────────────────────────

delegate_compositor!(ConductorWindow);
delegate_output!(ConductorWindow);
delegate_seat!(ConductorWindow);
delegate_keyboard!(ConductorWindow);
delegate_pointer!(ConductorWindow);
delegate_xdg_shell!(ConductorWindow);
delegate_xdg_window!(ConductorWindow);
delegate_registry!(ConductorWindow);

impl ProvidesRegistryState for ConductorWindow {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

mod claude_session;
mod clipboard;
mod daemon_reader;
mod input_handlers;
mod render;
mod session_mode;
mod url_detection;

use claude_session::find_matching_session;
use daemon_reader::{apply_session_state_to_term, spawn_daemon_reader_task};
use session_mode::{BellMode, BELL_FLASH_DURATION, SessionMode, setup_standalone_session};
