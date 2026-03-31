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
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, PointerEvent, PointerEventKind, PointerHandler},
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
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{TermDamage, TermMode};
use std::collections::HashSet;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr::NonNull;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use thermal_core::claude_state::{ClaudeSessionState, ClaudeStatePoller};

// ── Bell configuration ──────────────────────────────────────────────────────

/// How to handle BEL (0x07) from the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BellMode {
    /// Brief translucent screen flash.
    Visual,
    /// Bell is silently ignored.
    None,
}

impl BellMode {
    /// Read from `THERMAL_BELL` env var. Defaults to `Visual`.
    fn from_env() -> Self {
        match std::env::var("THERMAL_BELL").as_deref() {
            Ok("none") => BellMode::None,
            _ => BellMode::Visual,
        }
    }
}

/// Duration of the visual bell flash overlay.
const BELL_FLASH_DURATION: Duration = Duration::from_millis(200);

use crate::agent_graph::{AgentGraph, GRAPH_OVERLAY_HEIGHT};
use crate::agent_timeline::{AgentTimeline, TIMELINE_BAR_HEIGHT};
use crate::client::DaemonClient;
use crate::context_environment::{TerminalContext, detect_context};
use crate::font_config::FontConfig;
use crate::grid_renderer::{self, ContextHeatmapPipeline, EnvironmentEffectPipeline, GridRenderer, RenderCell};
use crate::inject::{self, InjectWatcher};
use crate::input;
use crate::protocol::Response;
use crate::pty::PtySession;
use crate::terminal::Terminal;

const DEFAULT_WIDTH: u32 = 1200;
const DEFAULT_HEIGHT: u32 = 800;

// ── Session mode ──────────────────────────────────────────────────────────────

/// How this window is connected to a terminal session.
///
/// In **client mode** the session daemon owns the PTY; we receive screen
/// updates over a Unix socket and forward input/resize there.
///
/// In **standalone mode** we own the PTY directly (legacy, no daemon).
enum SessionMode {
    /// Connected to the session daemon.
    Client {
        /// Daemon client for sending requests (input, resize, detach).
        client: DaemonClient,
        /// The session ID we are attached to.
        session_id: String,
    },
    /// Direct PTY ownership (no daemon running).
    Standalone { pty: PtySession },
}

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
        .or_else(|| caps.formats.iter().copied().find(|f| *f == wgpu::TextureFormat::Bgra8UnormSrgb))
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

                // Pick an existing live session or spawn a new one.
                let session_id = if let Some(session) = sessions.iter().find(|s| s.is_alive) {
                    tracing::info!(id = %session.id, "Attaching to existing session");
                    session.id.clone()
                } else {
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
                    cells: ref _cells,
                    ..
                } = attach_response
                {
                    tracing::info!(
                        cols,
                        rows,
                        "Received initial session state from daemon"
                    );
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
                format!("thermal \u{2014} session {}", &session_id[..session_id.len().min(8)])
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

    // ── Claude state poller ──────────────────────────────────────────────────
    let claude_poller = match ClaudeStatePoller::new() {
        Ok(poller) => {
            tracing::info!("Claude state poller initialized");
            Some(poller)
        }
        Err(e) => {
            tracing::warn!("Failed to create Claude state poller: {e} — HUD disabled");
            None
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
                        state.bell_flash_until =
                            Some(Instant::now() + BELL_FLASH_DURATION);
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
        // Non-blocking: drains file-watch events and re-reads changed files.
        let all_sessions = if let Some(ref mut poller) = state.claude_poller {
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
            state.agent_graph.set_layout_size(
                state.width as f32,
                GRAPH_OVERLAY_HEIGHT as f32,
            );
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

    Ok(())
}

// ── Standalone session setup ──────────────────────────────────────────────────

/// Set up a standalone PTY session (no daemon). This is the legacy code path.
///
/// Spawns a PTY, connects its output to the terminal byte processor, and
/// returns the session mode, event receiver, and child PID.
fn setup_standalone_session(
    terminal: &mut Terminal,
    init_cols: usize,
    init_rows: usize,
    pty_dirty: Arc<AtomicBool>,
    wakeup_write: std::os::fd::OwnedFd,
) -> (
    SessionMode,
    tokio::sync::mpsc::UnboundedReceiver<TermEvent>,
    i32,
) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    // Spawn the PTY at the correct initial size so the shell doesn't need a
    // SIGWINCH resize cycle (which creates spurious scrollback on launch).
    let mut pty = PtySession::spawn_sized(&shell, None, init_cols as u16, init_rows as u16)
        .expect("Failed to spawn PTY");

    // Connect PTY output to the terminal byte processor.
    let pty_output_rx = pty.take_output();
    terminal.spawn_byte_processor(pty_output_rx, pty_dirty, wakeup_write);

    // Take the terminal event receiver.
    let term_event_rx = terminal.take_event_rx().expect("event_rx already taken");

    let child_pid = pty.child_pid().as_raw();

    let mode = SessionMode::Standalone { pty };
    (mode, term_event_rx, child_pid)
}

// ── Daemon reader task ────────────────────────────────────────────────────────

/// Spawn a tokio task that reads daemon responses and applies screen updates
/// to the local terminal. Signals the wakeup pipe so the render loop wakes.
///
/// In client mode the daemon streams `ScreenUpdate` and `SessionExited`
/// messages after an `Attach`. This task processes them in the background,
/// writing dirty cells into the Term and setting the pty_dirty flag.
fn spawn_daemon_reader_task(
    terminal: &Terminal,
    mut response_rx: tokio::sync::mpsc::Receiver<Response>,
    pty_dirty: Arc<AtomicBool>,
    exit_requested: Arc<AtomicBool>,
    pending_title: Arc<Mutex<Option<String>>>,
    wakeup_write: std::os::fd::OwnedFd,
) {
    let term_handle = terminal.term_handle();
    let wakeup_write_fd = wakeup_write.as_raw_fd();

    tokio::spawn(async move {
        // Keep the OwnedFd alive for the lifetime of the task.
        let _wakeup_owner = wakeup_write;

        tracing::info!("Daemon reader task started — streaming screen updates");

        while let Some(response) = response_rx.recv().await {
            match response {
                Response::ScreenUpdate {
                    dirty_cells,
                    cursor,
                    ..
                } => {
                    // Apply dirty cells incrementally to the local term.
                    let mut term = term_handle.lock();
                    let (screen_lines, screen_cols) = {
                        use alacritty_terminal::grid::Dimensions;
                        (term.screen_lines(), term.columns())
                    };

                    for dc in &dirty_cells {
                        let row = dc.row as usize;
                        let col = dc.col as usize;
                        if row >= screen_lines || col >= screen_cols {
                            continue;
                        }
                        let point = Point::new(
                            alacritty_terminal::index::Line(row as i32),
                            Column(col),
                        );
                        let grid_cell = &mut term.grid_mut()[point];
                        grid_cell.c = dc.cell.ch;
                        grid_cell.flags = Flags::from_bits_truncate(dc.cell.flags);
                        grid_cell.fg = alacritty_terminal::vte::ansi::Color::Spec(
                            alacritty_terminal::vte::ansi::Rgb {
                                r: dc.cell.fg.r,
                                g: dc.cell.fg.g,
                                b: dc.cell.fg.b,
                            },
                        );
                        grid_cell.bg = alacritty_terminal::vte::ansi::Color::Spec(
                            alacritty_terminal::vte::ansi::Rgb {
                                r: dc.cell.bg.r,
                                g: dc.cell.bg.g,
                                b: dc.cell.bg.b,
                            },
                        );
                    }

                    // Update cursor position.
                    if cursor.visible {
                        term.grid_mut().cursor.point = Point::new(
                            alacritty_terminal::index::Line(cursor.row as i32),
                            Column(cursor.col as usize),
                        );
                    }

                    drop(term);

                    // Signal render loop: new content available.
                    pty_dirty.store(true, Ordering::Release);
                    wake_render_loop(wakeup_write_fd);

                    tracing::trace!(
                        dirty = dirty_cells.len(),
                        "Applied incremental screen update from daemon"
                    );
                }

                Response::SessionState {
                    cols,
                    rows,
                    ref cells,
                    ref cursor,
                    ..
                } => {
                    // Full redraw — the daemon sends this when damage is too
                    // large for an incremental update.
                    let mut term = term_handle.lock();

                    // Resize if needed.
                    let current_cols = {
                        use alacritty_terminal::grid::Dimensions;
                        term.columns()
                    };
                    let current_rows = {
                        use alacritty_terminal::grid::Dimensions;
                        term.screen_lines()
                    };
                    if current_cols != cols as usize || current_rows != rows as usize {
                        use crate::terminal::ConductorTerminalSize;
                        let size = ConductorTerminalSize::new(cols as usize, rows as usize);
                        term.resize(size);
                    }

                    // Apply all cells.
                    let safe_cols = {
                        use alacritty_terminal::grid::Dimensions;
                        term.columns()
                    };
                    for (i, cell_data) in cells.iter().enumerate() {
                        let row = i / (cols as usize);
                        let col = i % (cols as usize);
                        if row < rows as usize && col < safe_cols {
                            let point = Point::new(
                                alacritty_terminal::index::Line(row as i32),
                                Column(col),
                            );
                            let grid_cell = &mut term.grid_mut()[point];
                            grid_cell.c = cell_data.ch;
                            grid_cell.flags = Flags::from_bits_truncate(cell_data.flags);
                            grid_cell.fg = alacritty_terminal::vte::ansi::Color::Spec(
                                alacritty_terminal::vte::ansi::Rgb {
                                    r: cell_data.fg.r,
                                    g: cell_data.fg.g,
                                    b: cell_data.fg.b,
                                },
                            );
                            grid_cell.bg = alacritty_terminal::vte::ansi::Color::Spec(
                                alacritty_terminal::vte::ansi::Rgb {
                                    r: cell_data.bg.r,
                                    g: cell_data.bg.g,
                                    b: cell_data.bg.b,
                                },
                            );
                        }
                    }

                    // Position the cursor.
                    if cursor.visible {
                        term.grid_mut().cursor.point = Point::new(
                            alacritty_terminal::index::Line(cursor.row as i32),
                            Column(cursor.col as usize),
                        );
                    }

                    drop(term);

                    pty_dirty.store(true, Ordering::Release);
                    wake_render_loop(wakeup_write_fd);

                    tracing::debug!(
                        cols,
                        rows,
                        cells = cells.len(),
                        "Applied full session state from daemon (streamed)"
                    );
                }

                Response::SessionExited {
                    id,
                    exit_code,
                    reason,
                } => {
                    tracing::info!(
                        session_id = %id,
                        ?exit_code,
                        %reason,
                        "Daemon reports session exited"
                    );
                    exit_requested.store(true, Ordering::Release);
                    // Wake the render loop so it checks exit promptly.
                    wake_render_loop(wakeup_write_fd);
                    break;
                }

                Response::TitleChanged { title, .. } => {
                    tracing::debug!(title = %title, "Daemon title changed — forwarding to window");
                    if let Ok(mut guard) = pending_title.lock() {
                        *guard = Some(title);
                    }
                    // Wake the render loop so it picks up the title promptly.
                    wake_render_loop(wakeup_write_fd);
                }

                // Ignore request-response messages (Ok, Pong, Error, etc.)
                // that may arrive before the stream settles.
                other => {
                    tracing::trace!(?other, "Daemon reader: ignoring non-stream response");
                }
            }
        }

        // If we exit the loop without having received a SessionExited
        // message, the daemon connection was lost (crash, socket closed,
        // etc.). Signal the window to exit so it doesn't hang with a
        // frozen terminal.
        if !exit_requested.load(Ordering::Acquire) {
            tracing::warn!(
                "Daemon reader: connection lost without SessionExited — \
                 signaling exit to avoid frozen window"
            );
            exit_requested.store(true, Ordering::Release);
            wake_render_loop(wakeup_write_fd);
        }

        tracing::info!("Daemon reader task exiting");
    });
}

/// Write a single byte to the wakeup pipe to unblock the poll() in the
/// render loop. Errors are silently ignored (pipe full is harmless).
fn wake_render_loop(wakeup_write_fd: i32) {
    let _ = nix::unistd::write(unsafe { std::os::fd::BorrowedFd::borrow_raw(wakeup_write_fd) }, &[1u8]);
}

// ── Apply daemon session state to local term ──────────────────────────────────

/// Apply a `SessionState` response from the daemon to the local alacritty Term.
///
/// This paints the initial grid contents received on attach so the first
/// frame renders the correct terminal state.
fn apply_session_state_to_term(terminal: &Terminal, response: &Response) {
    if let Response::SessionState {
        cols,
        rows,
        cells,
        cursor,
        ..
    } = response
    {
        let term_handle = terminal.term_handle();
        let mut term = term_handle.lock();

        // Resize the term to match the daemon's grid if needed.
        let current_cols = term.columns();
        let current_rows = term.screen_lines();
        if current_cols != *cols as usize || current_rows != *rows as usize {
            use crate::terminal::ConductorTerminalSize;
            let size = ConductorTerminalSize::new(*cols as usize, *rows as usize);
            term.resize(size);
        }

        // Apply cells to the grid.
        let safe_cols = term.columns();
        for (i, cell_data) in cells.iter().enumerate() {
            let row = i / (*cols as usize);
            let col = i % (*cols as usize);
            if row < *rows as usize && col < safe_cols {
                let point = Point::new(alacritty_terminal::index::Line(row as i32), Column(col));
                let grid_cell = &mut term.grid_mut()[point];
                grid_cell.c = cell_data.ch;
                grid_cell.flags = Flags::from_bits_truncate(cell_data.flags);
                // Map protocol colors to alacritty Color.
                grid_cell.fg = alacritty_terminal::vte::ansi::Color::Spec(
                    alacritty_terminal::vte::ansi::Rgb {
                        r: cell_data.fg.r,
                        g: cell_data.fg.g,
                        b: cell_data.fg.b,
                    },
                );
                grid_cell.bg = alacritty_terminal::vte::ansi::Color::Spec(
                    alacritty_terminal::vte::ansi::Rgb {
                        r: cell_data.bg.r,
                        g: cell_data.bg.g,
                        b: cell_data.bg.b,
                    },
                );
            }
        }

        // Position the cursor.
        if cursor.visible {
            term.grid_mut().cursor.point = Point::new(
                alacritty_terminal::index::Line(cursor.row as i32),
                Column(cursor.col as usize),
            );
        }

        tracing::debug!(
            cols,
            rows,
            cells = cells.len(),
            "Applied daemon session state to local term"
        );
    }
}

// ── wgpu state ────────────────────────────────────────────────────────────────

struct WgpuState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

// ── Main window struct ────────────────────────────────────────────────────────

struct ConductorWindow {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    #[allow(dead_code)]
    window: Window,
    wgpu: WgpuState,
    grid_renderer: GridRenderer,
    /// Context heatmap vignette — subtle edge glow driven by context_percent.
    context_heatmap: ContextHeatmapPipeline,
    /// Environment effect — border glow indicating Docker/worktree/SSH context.
    environment_effect: EnvironmentEffectPipeline,
    /// Detected terminal execution environment (Docker, worktree, SSH, or main).
    terminal_context: TerminalContext,
    terminal: Terminal,
    /// Session mode: either daemon client or standalone PTY.
    session_mode: SessionMode,
    _tokio_rt: tokio::runtime::Runtime,
    configured: bool,
    /// Whether the window needs to be redrawn this iteration.
    dirty: bool,
    /// Set to `true` by the PTY byte processor (or daemon reader) when new
    /// terminal output has been processed; cleared each time the render loop
    /// checks it.
    pty_dirty: Arc<AtomicBool>,
    width: u32,
    height: u32,
    exit: bool,
    /// Set to `true` by the daemon reader task when a `SessionExited` message
    /// arrives. Checked by `session_has_exited()` in client mode.
    daemon_exit_requested: Arc<AtomicBool>,
    /// Pending title update from the daemon reader task. Drained each event
    /// loop iteration and applied to the Wayland surface via `set_title()`.
    pending_title: Arc<Mutex<Option<String>>>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    seat: Option<wl_seat::WlSeat>,
    modifiers: Modifiers,
    // Mouse / pointer state
    pointer: Option<wl_pointer::WlPointer>,
    /// Whether the left mouse button is currently held (for drag selection).
    mouse_left_held: bool,
    // Key repeat state
    /// The last key event that should repeat, or None if no repeat is active.
    repeat_key: Option<KeyEvent>,
    /// When key repeat should next fire.
    repeat_next: Option<std::time::Instant>,
    /// Delay before first repeat (typically ~400ms).
    repeat_delay: std::time::Duration,
    /// Interval between repeats (typically ~33ms for 30 chars/sec).
    repeat_rate: std::time::Duration,
    /// When set, defer rendering until this deadline to coalesce PTY output
    /// (e.g. during TUI startup floods). Cleared after each render.
    render_deadline: Option<std::time::Instant>,
    /// Terminal event receiver — relays PtyWrite responses back to the PTY.
    term_event_rx: tokio::sync::mpsc::UnboundedReceiver<TermEvent>,
    /// Claude state poller — watches /tmp/claude-code-state/ for session files.
    claude_poller: Option<ClaudeStatePoller>,
    /// Cached matching Claude session for the HUD overlay.
    claude_session: Option<ClaudeSessionState>,
    /// PID of the PTY child process, used to read cwd via /proc/<pid>/cwd.
    /// Zero in client mode (daemon owns the process).
    pty_child_pid: i32,
    // Cross-pane prompt injection
    /// Unique session ID for this window instance (used to ignore own inject files).
    inject_session_id: String,
    /// File watcher on `/tmp/thermal-inject/` for receiving injections from other windows.
    inject_watcher: Option<InjectWatcher>,
    // Context saturation warning state
    /// Whether the 85% context warning overlay is currently displayed.
    context_warning_active: bool,
    /// Whether the 95% context critical overlay is currently displayed.
    context_critical_active: bool,
    /// Agent tool-usage timeline bar (toggled with Ctrl+Shift+T).
    agent_timeline: AgentTimeline,
    /// Agent communication graph overlay (toggled with F3).
    agent_graph: AgentGraph,
    // Bell (visual flash) state
    /// How to handle BEL characters from the terminal.
    bell_mode: BellMode,
    /// When set, a translucent flash overlay is rendered until this instant.
    bell_flash_until: Option<Instant>,
}

impl ConductorWindow {
    // ── Kitty keyboard protocol helpers ──────────────────────────────────

    /// Query the alacritty_terminal mode flags and return the active kitty
    /// keyboard protocol flags.  Returns `KittyFlags::NONE` when kitty
    /// keyboard mode is not active.
    fn kitty_flags(&self) -> input::KittyFlags {
        let th = self.terminal.term_handle();
        let t = th.lock();
        let mode = t.mode();
        let mut flags: u8 = 0;
        if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) { flags |= 1; }
        if mode.contains(TermMode::REPORT_EVENT_TYPES)     { flags |= 2; }
        if mode.contains(TermMode::REPORT_ALTERNATE_KEYS)  { flags |= 4; }
        if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) { flags |= 8; }
        if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT)  { flags |= 16; }
        input::KittyFlags(flags)
    }

    /// Encode a key event, choosing kitty or legacy encoding based on the
    /// terminal's active mode.
    fn encode_key_event(&self, event: &KeyEvent, event_type: input::KeyEventType) -> Option<Vec<u8>> {
        let flags = self.kitty_flags();
        if flags.contains(input::KittyFlags::DISAMBIGUATE) {
            input::encode_key_kitty(event, &self.modifiers, flags, event_type)
        } else {
            input::encode_key(event, &self.modifiers)
        }
    }

    // ── Session mode dispatch helpers ─────────────────────────────────────

    /// Write bytes to the active session (PTY or daemon).
    fn write_session(&self, bytes: &[u8]) {
        match &self.session_mode {
            SessionMode::Standalone { pty } => {
                if let Err(e) = pty.write(bytes) {
                    tracing::warn!("Failed to write to PTY: {e}");
                }
            }
            SessionMode::Client { client, session_id } => {
                let data = bytes.to_vec();
                let id = session_id.clone();
                // Fire-and-forget async send — input is latency-sensitive so
                // we don't block the event loop waiting for a response.
                let client_tx = client.request_tx_clone();
                tokio::spawn(async move {
                    if let Err(e) = client_tx
                        .send(crate::protocol::Request::SendInput { id, data })
                        .await
                    {
                        tracing::warn!("Failed to send input to daemon: {e}");
                    }
                });
            }
        }
    }

    /// Resize the active session (PTY or daemon).
    fn resize_session(&self, cols: u16, rows: u16) {
        match &self.session_mode {
            SessionMode::Standalone { pty } => {
                let _ = pty.resize(cols, rows);
            }
            SessionMode::Client { client, session_id } => {
                let id = session_id.clone();
                let client_tx = client.request_tx_clone();
                tokio::spawn(async move {
                    if let Err(e) = client_tx
                        .send(crate::protocol::Request::Resize { id, cols, rows })
                        .await
                    {
                        tracing::warn!("Failed to send resize to daemon: {e}");
                    }
                });
            }
        }
    }

    /// Check whether the session has exited.
    fn session_has_exited(&self) -> bool {
        match &self.session_mode {
            SessionMode::Standalone { pty } => pty.has_exited(),
            // In client mode, the daemon reader task sets this flag when
            // it receives a `SessionExited` message.
            SessionMode::Client { .. } => self.daemon_exit_requested.load(Ordering::Acquire),
        }
    }

    /// Render a frame: clear to BG, then render the terminal grid.
    // TODO: [code-review] extract render_terminal_grid, render_overlays, render_hud sub-methods
    fn render_frame(&mut self) {
        let output = match self.wgpu.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Outdated) => {
                self.wgpu
                    .surface
                    .configure(&self.wgpu.device, &self.wgpu.config);
                return;
            }
            Err(e) => {
                tracing::warn!("Failed to acquire surface texture: {}", e);
                return;
            }
        };

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.wgpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("conductor frame"),
                });

        // ── Clear pass ───────────────────────────────────────────────────
        // Palette BG (#0a0010) in linear space for the sRGB surface.
        // Must match TERM_BG in grid_renderer.rs (after sRGB→linear conversion).
        let bg: [f32; 4] = grid_renderer::clear_color();
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("conductor clear pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Pass drops here — just a clear
        }

        // ── Context heatmap vignette (renders BEFORE grid so text is on top) ──
        if let Some(ref session) = self.claude_session
            && let Some(ctx_pct) = session.context_percent
        {
            // Normalize from 0-100 to 0.0-1.0.
            let normalized = (ctx_pct as f32 / 100.0).clamp(0.0, 1.0);
            self.context_heatmap.render(
                normalized,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
        }

        // ── Environment effect (renders BEFORE grid so text is on top) ──
        self.environment_effect.render(
            self.terminal_context.as_uniform(),
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // ── Render terminal grid ─────────────────────────────────────────
        // Lock the terminal and read renderable content.
        let term_handle = self.terminal.term_handle();
        let mut term = term_handle.lock();

        // Query damage BEFORE reading content — damage() requires &mut self.
        let damaged_rows: Option<HashSet<usize>> = match term.damage() {
            TermDamage::Full => None, // None means "full redraw"
            TermDamage::Partial(iter) => {
                let set: HashSet<usize> = iter
                    .filter(|bounds| bounds.is_damaged())
                    .map(|bounds| bounds.line)
                    .collect();
                if set.is_empty() {
                    // Nothing damaged — reuse entire cache, skip cell collection.
                    let screen_lines = term.screen_lines();
                    let content = term.renderable_content();
                    let display_offset = content.display_offset;
                    let cursor = content.cursor;
                    let selection_range = content.selection;
                    term.reset_damage();
                    drop(term);

                    self.grid_renderer.render_cached(
                        &cursor,
                        screen_lines,
                        selection_range.as_ref(),
                        display_offset,
                        &self.wgpu.device,
                        &self.wgpu.queue,
                        &mut encoder,
                        &view,
                        self.width,
                        self.height,
                    );

                    // ── Kitty graphics inline images ─────────────────────────
                    {
                        let store = self.terminal.image_store();
                        let mut store_guard = store.lock();
                        self.grid_renderer.render_images(
                            &store_guard,
                            &self.wgpu.device,
                            &self.wgpu.queue,
                            &mut encoder,
                            &view,
                            self.width,
                            self.height,
                        );
                        self.grid_renderer
                            .periodic_image_cleanup(&mut store_guard, screen_lines);
                    }

                    // ── Command block overlays ──────────────────────────────
                    {
                        let tracker = self.terminal.command_tracker();
                        let blocks = tracker.lock().blocks.clone();
                        self.grid_renderer.render_command_blocks(
                            &blocks,
                            display_offset,
                            screen_lines,
                            &self.wgpu.device,
                            &self.wgpu.queue,
                            &mut encoder,
                            &view,
                            self.width,
                            self.height,
                        );
                    }

                    // ── Scroll indicator overlay ─────────────────────────────
                    self.grid_renderer.render_scroll_indicator(
                        display_offset,
                        &self.wgpu.device,
                        &self.wgpu.queue,
                        &mut encoder,
                        &view,
                        self.width,
                        self.height,
                    );

                    // Claude HUD overlay disabled — redundant with Claude's
                    // built-in statusline and thermal-monitor dashboard.

                    // ── Context saturation warning overlay ─────────────────
                    if self.context_warning_active {
                        let ctx_pct = self
                            .claude_session
                            .as_ref()
                            .and_then(|s| s.context_percent)
                            .unwrap_or(0.0) as f32;
                        self.grid_renderer.render_context_warning(
                            ctx_pct,
                            &self.wgpu.device,
                            &self.wgpu.queue,
                            &mut encoder,
                            &view,
                            self.width,
                            self.height,
                        );
                    }

                    // ── Agent timeline overlay ─────────────────────────────
                    self.grid_renderer.render_agent_timeline(
                        &self.agent_timeline,
                        &self.wgpu.device,
                        &self.wgpu.queue,
                        &mut encoder,
                        &view,
                        self.width,
                        self.height,
                    );

                    // ── Agent graph overlay ────────────────────────────────
                    self.grid_renderer.render_agent_graph(
                        &self.agent_graph,
                        &self.wgpu.device,
                        &self.wgpu.queue,
                        &mut encoder,
                        &view,
                        self.width,
                        self.height,
                    );

                    // ── Bell flash overlay ─────────────────────────────────
                    if self.bell_flash_until.is_some() {
                        self.grid_renderer.render_bell_flash(
                            &self.wgpu.device,
                            &self.wgpu.queue,
                            &mut encoder,
                            &view,
                            self.width,
                            self.height,
                        );
                    }

                    self.wgpu.queue.submit(std::iter::once(encoder.finish()));
                    output.present();
                    return;
                }
                Some(set)
            }
        };

        let content = term.renderable_content();

        let screen_lines = term.screen_lines();
        let display_offset = content.display_offset;
        let cursor = content.cursor;
        let selection_range = content.selection;

        // Collect cells into RenderCell snapshots while holding the lock.
        // When we have partial damage, only collect cells from damaged rows.
        let cells: Vec<RenderCell> = content
            .display_iter
            .filter_map(|indexed| {
                let point = indexed.point;
                let cell = indexed.cell;

                // Convert grid line to viewport row index.
                let viewport_line = point.line.0 + display_offset as i32;
                let row = usize::try_from(viewport_line).ok()?;
                if row >= screen_lines {
                    return None;
                }

                // Skip rows that aren't damaged (partial damage only).
                if let Some(ref damaged) = damaged_rows
                    && !damaged.contains(&row)
                {
                    return None;
                }

                // Skip wide char spacers.
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    return None;
                }

                let hyperlink = cell.hyperlink().map(|h| h.uri().to_owned());

                Some(RenderCell {
                    row,
                    col: point.column.0,
                    c: cell.c,
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                    hyperlink,
                })
            })
            .collect();

        // Reset damage while we still hold the lock.
        term.reset_damage();

        // Release the term lock before the (potentially slow) GPU work.
        drop(term);

        // ── Regex URL detection ──────────────────────────────────────────
        // For cells that don't already have an OSC 8 hyperlink, detect
        // URLs via regex and annotate them as clickable.
        let mut cells = cells;
        detect_urls_in_cells(&mut cells, screen_lines);

        self.grid_renderer.render(
            &cells,
            &cursor,
            screen_lines,
            selection_range.as_ref(),
            display_offset,
            damaged_rows.as_ref(),
            &self.wgpu.device,
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // ── Kitty graphics inline images ──────────────────────────────────
        {
            let store = self.terminal.image_store();
            let mut store_guard = store.lock();
            self.grid_renderer.render_images(
                &store_guard,
                &self.wgpu.device,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
            self.grid_renderer
                .periodic_image_cleanup(&mut store_guard, screen_lines);
        }

        // ── Command block overlays ──────────────────────────────────────
        {
            let tracker = self.terminal.command_tracker();
            let blocks = tracker.lock().blocks.clone();
            self.grid_renderer.render_command_blocks(
                &blocks,
                display_offset,
                screen_lines,
                &self.wgpu.device,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
        }

        // ── Scroll indicator overlay ─────────────────────────────────────
        self.grid_renderer.render_scroll_indicator(
            display_offset,
            &self.wgpu.device,
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // Claude HUD overlay disabled — redundant with Claude's
        // built-in statusline and thermal-monitor dashboard.

        // ── Context saturation warning overlay ──────────────────────────
        if self.context_warning_active {
            let ctx_pct = self
                .claude_session
                .as_ref()
                .and_then(|s| s.context_percent)
                .unwrap_or(0.0) as f32;
            self.grid_renderer.render_context_warning(
                ctx_pct,
                &self.wgpu.device,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
        }

        // ── Agent timeline overlay ──────────────────────────────────────
        self.grid_renderer.render_agent_timeline(
            &self.agent_timeline,
            &self.wgpu.device,
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // ── Agent graph overlay ─────────────────────────────────────────
        self.grid_renderer.render_agent_graph(
            &self.agent_graph,
            &self.wgpu.device,
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // ── Bell flash overlay ──────────────────────────────────────────
        if self.bell_flash_until.is_some() {
            self.grid_renderer.render_bell_flash(
                &self.wgpu.device,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
        }

        self.wgpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }

    /// Copy the current terminal selection to the Wayland clipboard via `wl-copy`.
    fn clipboard_copy(&self) {
        let term_handle = self.terminal.term_handle();
        let term = term_handle.lock();
        let text = term.selection_to_string();
        drop(term);

        if let Some(text) = text {
            if text.is_empty() {
                tracing::debug!("Clipboard copy: selection is empty");
                return;
            }
            // Shell out to wl-copy for clipboard access.
            match std::process::Command::new("wl-copy")
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(ref mut stdin) = child.stdin {
                        use std::io::Write;
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    let _ = child.wait();
                    tracing::debug!(len = text.len(), "Clipboard copy: text sent to wl-copy");
                }
                Err(e) => {
                    tracing::warn!("Failed to run wl-copy: {} (is wl-clipboard installed?)", e);
                }
            }
        } else {
            tracing::debug!("Clipboard copy: no selection");
        }
    }

    // ── Mouse selection helpers ──────────────────────────────────────────────

    /// Convert pixel coordinates to terminal grid position (col, line, side).
    ///
    /// The `side` indicates whether the click was on the left or right half
    /// of the cell, which alacritty_terminal uses for precise selection edges.
    fn pixel_to_grid(&self, px: f64, py: f64) -> (Column, Line, Side) {
        let padding_x = self.grid_renderer.padding_x();
        let padding_y = self.grid_renderer.padding_y();
        let cell_w = self.grid_renderer.cell_width as f64;
        let cell_h = self.grid_renderer.cell_height as f64;

        let x = (px - padding_x as f64).max(0.0);
        let y = (py - padding_y as f64).max(0.0);

        let col = (x / cell_w) as usize;
        let row = (y / cell_h) as i32;

        // Determine which side of the cell the click is on.
        let cell_x_offset = x - (col as f64 * cell_w);
        let side = if cell_x_offset < cell_w / 2.0 {
            Side::Left
        } else {
            Side::Right
        };

        // Clamp to grid bounds.
        let term_handle = self.terminal.term_handle();
        let term = term_handle.lock();
        let max_col = term.columns().saturating_sub(1);
        let max_row = term.screen_lines() as i32 - 1;
        drop(term);

        let col = col.min(max_col);
        let row = row.min(max_row);

        (Column(col), Line(row), side)
    }

    /// Start a new text selection at the given grid position.
    fn selection_start(&mut self, col: Column, line: Line, side: Side) {
        let point = Point::new(line, col);
        let selection = Selection::new(SelectionType::Simple, point, side);
        let term_handle = self.terminal.term_handle();
        let mut term = term_handle.lock();
        term.selection = Some(selection);
        tracing::debug!(?point, ?side, "Selection started");
    }

    /// Update the end point of an in-progress selection.
    fn selection_update(&mut self, col: Column, line: Line, side: Side) {
        let point = Point::new(line, col);
        let term_handle = self.terminal.term_handle();
        let mut term = term_handle.lock();
        if let Some(ref mut sel) = term.selection {
            sel.update(point, side);
        }
    }

    /// Finalize the selection: extract text and set primary selection via wl-copy.
    fn selection_finalize(&self) {
        let term_handle = self.terminal.term_handle();
        let term = term_handle.lock();
        let text = term.selection_to_string();
        drop(term);

        if let Some(ref text) = text {
            if text.is_empty() {
                return;
            }
            // Set primary selection via wl-copy --primary.
            match std::process::Command::new("wl-copy")
                .arg("--primary")
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(ref mut stdin) = child.stdin {
                        use std::io::Write;
                        let _ = stdin.write_all(text.as_bytes());
                    }
                    let _ = child.wait();
                    tracing::debug!(
                        len = text.len(),
                        "Primary selection set via wl-copy --primary"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to run wl-copy --primary: {} (is wl-clipboard installed?)",
                        e
                    );
                }
            }
        }
    }

    /// Clear any active selection.
    #[allow(dead_code)]
    fn selection_clear(&self) {
        let term_handle = self.terminal.term_handle();
        let mut term = term_handle.lock();
        term.selection = None;
    }

    // ── Hyperlink click handling ────────────────────────────────────────────

    /// Look up the hyperlink URL at the given grid position.
    /// Returns `Some(url)` if the cell has an associated hyperlink.
    fn hyperlink_at(&self, row: usize, col: usize) -> Option<String> {
        self.grid_renderer.hyperlink_map.get(&(row, col)).cloned()
    }

    /// Open a URL via `xdg-open` (spawned detached, non-blocking).
    /// Only http:// and https:// URIs are allowed — other schemes (file://,
    /// javascript:, data:, etc.) are rejected to prevent local file access
    /// and code execution via crafted terminal hyperlinks.
    fn open_url(&self, url: &str) {
        // Validate URI scheme — only allow http(s)
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            tracing::warn!(url, "Rejected hyperlink with disallowed URI scheme");
            return;
        }

        tracing::info!(url, "Opening hyperlink via xdg-open");
        match std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => {
                tracing::debug!(url, "xdg-open spawned successfully");
            }
            Err(e) => {
                tracing::warn!(url, error = %e, "Failed to spawn xdg-open");
            }
        }
    }

    /// Paste from the primary selection (middle-click) into the session.
    fn primary_paste(&self) {
        let output = match std::process::Command::new("wl-paste")
            .arg("--primary")
            .arg("--no-newline")
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    "Failed to run wl-paste --primary: {} (is wl-clipboard installed?)",
                    e
                );
                return;
            }
        };

        if !output.status.success() {
            tracing::debug!(
                "wl-paste --primary returned non-zero (primary selection may be empty)"
            );
            return;
        }

        let text = &output.stdout;
        if text.is_empty() {
            return;
        }

        // Check if the terminal has bracketed paste mode enabled.
        let bracketed = {
            let term_handle = self.terminal.term_handle();
            let term = term_handle.lock();
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };

        if bracketed {
            let mut payload = Vec::with_capacity(text.len() + 12);
            payload.extend_from_slice(b"\x1b[200~");
            payload.extend_from_slice(text);
            payload.extend_from_slice(b"\x1b[201~");
            self.write_session(&payload);
        } else {
            self.write_session(text);
        }

        tracing::debug!(
            len = text.len(),
            bracketed,
            "Primary paste: sent to session"
        );
    }

    /// Paste from the Wayland clipboard into the session, with bracketed paste
    /// support when the terminal has DECSET 2004 enabled.
    fn clipboard_paste(&self) {
        // Read clipboard contents via wl-paste.
        let output = match std::process::Command::new("wl-paste")
            .arg("--no-newline")
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("Failed to run wl-paste: {} (is wl-clipboard installed?)", e);
                return;
            }
        };

        if !output.status.success() {
            tracing::debug!("wl-paste returned non-zero (clipboard may be empty)");
            return;
        }

        let text = &output.stdout;
        if text.is_empty() {
            return;
        }

        // Check if the terminal has bracketed paste mode enabled (DECSET 2004).
        let bracketed = {
            let term_handle = self.terminal.term_handle();
            let term = term_handle.lock();
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };

        if bracketed {
            // Wrap paste in bracketed paste escape sequences:
            //   \x1b[200~ ... \x1b[201~
            let mut payload = Vec::with_capacity(text.len() + 12);
            payload.extend_from_slice(b"\x1b[200~");
            payload.extend_from_slice(text);
            payload.extend_from_slice(b"\x1b[201~");
            self.write_session(&payload);
        } else {
            self.write_session(text);
        }

        tracing::debug!(
            len = text.len(),
            bracketed,
            "Clipboard paste: sent to session"
        );
    }

    // ── Cross-pane prompt injection ──────────────────────────────────────────

    /// Inject the current terminal selection into other thermal-conductor windows.
    ///
    /// If a daemon is running, sends via the daemon client to all other sessions.
    /// Otherwise, writes to `/tmp/thermal-inject/` for file-based pickup.
    fn inject_selection(&self) {
        let term_handle = self.terminal.term_handle();
        let term = term_handle.lock();
        let text = term.selection_to_string();
        drop(term);

        let Some(text) = text else {
            tracing::debug!("Inject: no selection");
            return;
        };
        if text.is_empty() {
            tracing::debug!("Inject: selection is empty");
            return;
        }

        // NOTE: A future enhancement could use the daemon client to send
        // input directly to other sessions via `send_input`. For now we use
        // the file-based approach which works universally — both with and
        // without the daemon running.

        // File-based approach: write to /tmp/thermal-inject/.
        match inject::write_inject_file(&self.inject_session_id, &text) {
            Ok(path) => {
                tracing::info!(
                    path = %path.display(),
                    text_len = text.len(),
                    "Injected selection to other windows"
                );
                inject::notify_injection("sent", text.len());
            }
            Err(e) => {
                tracing::warn!("Failed to write inject file: {e}");
            }
        }
    }

    /// Poll the inject watcher for incoming injections from other windows.
    ///
    /// Any received text is pasted into this window's PTY session, respecting
    /// bracketed paste mode.
    fn poll_inject_watcher(&self) {
        let watcher = match &self.inject_watcher {
            Some(w) => w,
            None => return,
        };

        let payloads = watcher.poll();
        for text in payloads {
            if text.is_empty() {
                continue;
            }

            // Check if the terminal has bracketed paste mode enabled.
            let bracketed = {
                let term_handle = self.terminal.term_handle();
                let term = term_handle.lock();
                term.mode().contains(TermMode::BRACKETED_PASTE)
            };

            if bracketed {
                let mut payload = Vec::with_capacity(text.len() + 12);
                payload.extend_from_slice(b"\x1b[200~");
                payload.extend_from_slice(text.as_bytes());
                payload.extend_from_slice(b"\x1b[201~");
                self.write_session(&payload);
            } else {
                self.write_session(text.as_bytes());
            }

            tracing::info!(
                text_len = text.len(),
                bracketed,
                "Injected text from another window into session"
            );
            inject::notify_injection("received", text.len());
        }
    }

    // ── Context saturation monitoring ──────────────────────────────────────

    /// Update context warning state based on the current Claude session.
    ///
    /// Sets `context_warning_active` when context_percent >= 85% and
    /// `context_critical_active` when >= 95%. Resets flags when context
    /// drops below thresholds (e.g. after a new session starts).
    fn update_context_warnings(&mut self) {
        let context_pct = self
            .claude_session
            .as_ref()
            .and_then(|s| s.context_percent)
            .unwrap_or(0.0);

        let was_warning = self.context_warning_active;
        let was_critical = self.context_critical_active;

        self.context_warning_active = context_pct >= 85.0;
        self.context_critical_active = context_pct >= 95.0;

        // Log transitions for observability.
        if self.context_warning_active && !was_warning {
            tracing::warn!(
                context_percent = context_pct,
                "Context window approaching limit (>= 85%)"
            );
        }
        if self.context_critical_active && !was_critical {
            tracing::warn!(
                context_percent = context_pct,
                "Context window saturated (>= 95%) — Ctrl+Shift+N to spawn continuation"
            );
        }
        if !self.context_warning_active && was_warning {
            tracing::info!("Context warning cleared (dropped below 85%)");
        }

        // Mark dirty if state changed so the overlay is rendered/cleared.
        if self.context_warning_active != was_warning
            || self.context_critical_active != was_critical
        {
            self.dirty = true;
        }
    }

    /// Spawn a continuation session in a new window.
    ///
    /// In client mode: asks the daemon to spawn a new session.
    /// In standalone mode: spawns a new `thermal-conductor window` process.
    fn spawn_continuation(&self) {
        tracing::info!("Spawning continuation session (Ctrl+Shift+N)");

        match &self.session_mode {
            SessionMode::Client { client, .. } => {
                let client_tx = client.request_tx_clone();
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                tokio::spawn(async move {
                    // Spawn a new session on the daemon.
                    if let Err(e) = client_tx
                        .send(crate::protocol::Request::SpawnSession {
                            shell: Some(shell),
                            cwd: None,
                            worktree: false,
                            name: None,
                        })
                        .await
                    {
                        tracing::warn!("Failed to spawn continuation session on daemon: {e}");
                        return;
                    }
                    tracing::info!("Continuation session spawn request sent to daemon");

                    // Launch a new window process to attach to the new session.
                    match std::process::Command::new(
                        std::env::current_exe()
                            .unwrap_or_else(|_| std::path::PathBuf::from("thermal-conductor")),
                    )
                    .arg("window")
                    .spawn()
                    {
                        Ok(_) => {
                            tracing::info!(
                                "Launched new thermal-conductor window for continuation"
                            );
                        }
                        Err(e) => {
                            tracing::warn!("Failed to launch continuation window: {e}");
                        }
                    }
                });
            }
            SessionMode::Standalone { .. } => {
                // Spawn a new thermal-conductor window process directly.
                match std::process::Command::new(
                    std::env::current_exe()
                        .unwrap_or_else(|_| std::path::PathBuf::from("thermal-conductor")),
                )
                .arg("window")
                .spawn()
                {
                    Ok(_) => {
                        tracing::info!(
                            "Launched new thermal-conductor window (standalone continuation)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Failed to launch continuation window: {e}");
                    }
                }
            }
        }

        // Try to place the new window adjacent via hyprctl.
        if let Err(e) = std::process::Command::new("hyprctl")
            .args(["dispatch", "layoutmsg", "preselect", "r"])
            .spawn()
        {
            tracing::debug!("hyprctl preselect hint failed (non-fatal): {e}");
        }
    }
}

// ── Claude session matching ───────────────────────────────────────────────────

/// Read the working directory of a process via `/proc/<pid>/cwd`.
///
/// Returns `None` if the process doesn't exist or the symlink can't be read.
fn read_proc_cwd(pid: i32) -> Option<String> {
    let link = format!("/proc/{}/cwd", pid);
    std::fs::read_link(link)
        .ok()
        .and_then(|p| p.to_str().map(String::from))
}

/// Find a Claude session whose `working_dir` matches the PTY child's cwd.
///
/// Reads the PTY child's working directory from `/proc/<pid>/cwd` and compares
/// it against each session's `working_dir` field. Returns the first match, or
/// `None` if no session matches.
fn find_matching_session(
    sessions: &[ClaudeSessionState],
    pty_child_pid: i32,
) -> Option<ClaudeSessionState> {
    if sessions.is_empty() {
        return None;
    }

    // Read the PTY child's current working directory.
    let pty_cwd = read_proc_cwd(pty_child_pid)?;

    // Try exact match first.
    for session in sessions {
        if let Some(ref working_dir) = session.working_dir
            && working_dir == &pty_cwd
        {
            return Some(session.clone());
        }
    }

    // Try prefix match: the PTY cwd may be a subdirectory of the session's
    // working_dir (e.g. PTY in /home/user/project/src, session in /home/user/project).
    for session in sessions {
        if let Some(ref working_dir) = session.working_dir
            && pty_cwd.starts_with(working_dir)
        {
            return Some(session.clone());
        }
    }

    None
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

// ── Keyboard handler ──────────────────────────────────────────────────────────

impl KeyboardHandler for ConductorWindow {
    fn enter(
        &mut self,
        _: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
        // DECSET 1004 focus reporting: send CSI I (focus in)
        {
            let handle = self.terminal.term_handle();
            let term = handle.lock();
            if term.mode().contains(TermMode::FOCUS_IN_OUT) {
                self.write_session(b"\x1b[I");
            }
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        // DECSET 1004 focus reporting: send CSI O (focus out)
        {
            let handle = self.terminal.term_handle();
            let term = handle.lock();
            if term.mode().contains(TermMode::FOCUS_IN_OUT) {
                self.write_session(b"\x1b[O");
            }
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // ── Window close: Ctrl+Shift+Q ─────────────────────────────────
        if self.modifiers.ctrl && self.modifiers.shift
            && matches!(event.keysym, Keysym::Q | Keysym::q)
        {
            tracing::info!("Ctrl+Shift+Q: closing window");
            self.exit = true;
            return;
        }

        // ── Clipboard: Ctrl+Shift+C (copy) / Ctrl+Shift+V (paste) ──────
        if self.modifiers.ctrl && self.modifiers.shift {
            match event.keysym {
                Keysym::C | Keysym::c => {
                    self.clipboard_copy();
                    self.dirty = true;
                    return;
                }
                Keysym::V | Keysym::v => {
                    self.clipboard_paste();
                    self.dirty = true;
                    return;
                }
                _ => {}
            }
        }

        // ── Agent timeline toggle: Ctrl+Shift+T ────────────────────────
        if self.modifiers.ctrl && self.modifiers.shift
            && matches!(event.keysym, Keysym::T | Keysym::t)
        {
            self.agent_timeline.toggle();
            // Recalculate terminal grid to account for the timeline bar and graph.
            let mut effective_h = self.height;
            if self.agent_timeline.visible {
                effective_h = effective_h.saturating_sub(TIMELINE_BAR_HEIGHT);
            }
            if self.agent_graph.visible {
                effective_h = effective_h.saturating_sub(GRAPH_OVERLAY_HEIGHT);
            }
            let (cols, rows) = self.grid_renderer.grid_size(self.width, effective_h);
            self.terminal.resize(
                cols,
                rows,
                self.grid_renderer.cell_width as u16,
                self.grid_renderer.cell_height as u16,
            );
            self.resize_session(cols as u16, rows as u16);
            self.dirty = true;
            return;
        }

        // ── Agent graph toggle: F3 ───────────────────────────────────────
        if matches!(event.keysym, Keysym::F3) {
            self.agent_graph.toggle();
            // Recalculate terminal grid to account for the graph overlay.
            let effective_h = if self.agent_graph.visible {
                self.height.saturating_sub(GRAPH_OVERLAY_HEIGHT)
            } else {
                self.height
            };
            // Also account for timeline if it's visible.
            let effective_h = if self.agent_timeline.visible {
                effective_h.saturating_sub(TIMELINE_BAR_HEIGHT)
            } else {
                effective_h
            };
            let (cols, rows) = self.grid_renderer.grid_size(self.width, effective_h);
            self.terminal.resize(
                cols,
                rows,
                self.grid_renderer.cell_width as u16,
                self.grid_renderer.cell_height as u16,
            );
            self.resize_session(cols as u16, rows as u16);
            self.dirty = true;
            return;
        }

        // ── Cross-pane inject: Ctrl+Shift+Enter ─────────────────────────
        // Sends the current selection to all other thermal-conductor windows.
        if self.modifiers.ctrl && self.modifiers.shift
            && matches!(event.keysym, Keysym::Return | Keysym::KP_Enter)
        {
            self.inject_selection();
            return;
        }

        // ── Context continuation: Ctrl+Shift+N ──────────────────────────
        // Spawns a new continuation session when the context window is saturated.
        if self.modifiers.ctrl && self.modifiers.shift
            && matches!(event.keysym, Keysym::N | Keysym::n)
        {
            self.spawn_continuation();
            return;
        }

        // ── Font size: Ctrl+Plus (increase), Ctrl+Minus (decrease), Ctrl+0 (reset)
        if self.modifiers.ctrl && !self.modifiers.shift {
            let font_changed = match event.keysym {
                Keysym::plus | Keysym::equal | Keysym::KP_Add => {
                    self.grid_renderer.font_config.increase()
                }
                Keysym::minus | Keysym::KP_Subtract => {
                    self.grid_renderer.font_config.decrease()
                }
                Keysym::_0 | Keysym::KP_0 => {
                    self.grid_renderer.font_config.reset()
                }
                _ => false,
            };
            if font_changed {
                tracing::info!(
                    font_size = self.grid_renderer.font_config.font_size,
                    "Font size changed"
                );
                self.grid_renderer.update_font_metrics();
                // Recalculate terminal grid dimensions for the new cell size.
                let mut effective_h = self.height;
                if self.agent_timeline.visible {
                    effective_h = effective_h.saturating_sub(TIMELINE_BAR_HEIGHT);
                }
                if self.agent_graph.visible {
                    effective_h = effective_h.saturating_sub(GRAPH_OVERLAY_HEIGHT);
                }
                let (cols, rows) = self.grid_renderer.grid_size(self.width, effective_h);
                self.terminal.resize(
                    cols,
                    rows,
                    self.grid_renderer.cell_width as u16,
                    self.grid_renderer.cell_height as u16,
                );
                self.resize_session(cols as u16, rows as u16);
                self.dirty = true;
                return;
            }
        }

        // ── Scrollback navigation (Shift+PageUp/Down/Home/End) ──────────
        // These are intercepted BEFORE encode_key so they never reach the PTY.
        if self.modifiers.shift {
            let scroll = match event.keysym {
                Keysym::Page_Up => Some(Scroll::PageUp),
                Keysym::Page_Down => Some(Scroll::PageDown),
                Keysym::Home => Some(Scroll::Top),
                Keysym::End => Some(Scroll::Bottom),
                _ => None,
            };
            if let Some(scroll) = scroll {
                let term_handle = self.terminal.term_handle();
                let mut term = term_handle.lock();
                term.scroll_display(scroll);
                self.dirty = true;
                return;
            }
        }

        // Encode the key press into bytes and send to the session.
        // Uses kitty keyboard protocol when the terminal has it enabled.
        if let Some(bytes) = self.encode_key_event(&event, input::KeyEventType::Press) {
            self.write_session(&bytes);
        }

        // Start key repeat for this key. Modifier-only keys don't repeat.
        if self.encode_key_event(&event, input::KeyEventType::Press).is_some() {
            self.repeat_key = Some(event);
            self.repeat_next = Some(std::time::Instant::now() + self.repeat_delay);
        }

        // Don't set dirty here — the PTY echo will set pty_dirty and
        // trigger a render when the shell response arrives, avoiding an
        // unnecessary extra GPU frame on every keypress.
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // Stop key repeat when any key is released.
        self.repeat_key = None;
        self.repeat_next = None;

        // When kitty keyboard protocol with REPORT_EVENTS is active, send
        // a release event to the terminal application.
        let flags = self.kitty_flags();
        if flags.contains(input::KittyFlags::REPORT_EVENTS) {
            if let Some(bytes) = input::encode_key_kitty(&event, &self.modifiers, flags, input::KeyEventType::Release) {
                self.write_session(&bytes);
            }
        }
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _layout: u32,
    ) {
        self.modifiers = modifiers;
    }
}

// ── Pointer handler (mouse selection + primary paste) ─────────────────────────

impl PointerHandler for ConductorWindow {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        // Check if the terminal program wants mouse events (SGR mouse mode).
        let mouse_mode = {
            let th = self.terminal.term_handle();
            let t = th.lock();
            let mode = t.mode();
            // Any of: MOUSE_REPORT_CLICK (1000), MOUSE_DRAG (1002),
            // MOUSE_MOTION (1003), or SGR_MOUSE (1006)
            mode.contains(TermMode::MOUSE_REPORT_CLICK)
                || mode.contains(TermMode::MOUSE_DRAG)
                || mode.contains(TermMode::MOUSE_MOTION)
        };

        for event in events {
            let (px, py) = event.position;
            let (col, line, _side) = self.pixel_to_grid(px, py);
            let cx = col.0 + 1; // SGR is 1-based
            let cy = line.0 + 1;

            // ── Ctrl+Click: open hyperlink ──────────────────────────────
            // Intercept Ctrl+Left-Click before mouse mode or selection
            // handling. If the clicked cell has a hyperlink, open it via
            // xdg-open and consume the event (matching kitty behavior).
            if let PointerEventKind::Press { button, .. } = event.kind {
                if button == BTN_LEFT && self.modifiers.ctrl {
                    let row = line.0 as usize;
                    let col_idx = col.0;
                    if let Some(url) = self.hyperlink_at(row, col_idx) {
                        self.open_url(&url);
                        continue; // Consume the click — don't start selection or forward SGR
                    }
                }
            }

            if mouse_mode {
                // Forward mouse events to session as SGR escape sequences.
                // Format: \x1b[<btn;col;row M (press) or m (release)
                let sgr = match event.kind {
                    PointerEventKind::Press { button, .. } => {
                        let btn = match button {
                            BTN_LEFT => 0,
                            BTN_MIDDLE => 1,
                            BTN_RIGHT => 2,
                            _ => continue,
                        };
                        Some(format!("\x1b[<{btn};{cx};{cy}M"))
                    }
                    PointerEventKind::Release { button, .. } => {
                        let btn = match button {
                            BTN_LEFT => 0,
                            BTN_MIDDLE => 1,
                            BTN_RIGHT => 2,
                            _ => continue,
                        };
                        Some(format!("\x1b[<{btn};{cx};{cy}m"))
                    }
                    PointerEventKind::Motion { .. } => {
                        // Motion reporting (mode 1003) or drag (1002 + button held)
                        if self.mouse_left_held {
                            Some(format!("\x1b[<32;{cx};{cy}M"))
                        } else {
                            let th = self.terminal.term_handle();
                            let t = th.lock();
                            if t.mode().contains(TermMode::MOUSE_MOTION) {
                                Some(format!("\x1b[<35;{cx};{cy}M"))
                            } else {
                                None
                            }
                        }
                    }
                    PointerEventKind::Axis { vertical, .. } => {
                        // Scroll: button 64 (up) / 65 (down) in SGR mode.
                        let btn = if vertical.discrete > 0 { 65 } else { 64 };
                        let steps = vertical.discrete.unsigned_abs().max(1);
                        let mut seq = String::new();
                        for _ in 0..steps {
                            seq.push_str(&format!("\x1b[<{btn};{cx};{cy}M"));
                        }
                        Some(seq)
                    }
                    _ => None,
                };

                if let Some(seq) = sgr {
                    self.write_session(seq.as_bytes());
                    self.dirty = true;
                }

                // Track left button state for drag reporting.
                match event.kind {
                    PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                        self.mouse_left_held = true;
                    }
                    PointerEventKind::Release { button, .. } if button == BTN_LEFT => {
                        self.mouse_left_held = false;
                    }
                    _ => {}
                }
            } else {
                // No mouse mode — use mouse for selection and scroll.
                match event.kind {
                    PointerEventKind::Press { button, .. } => {
                        if button == BTN_LEFT {
                            self.selection_start(col, line, _side);
                            self.mouse_left_held = true;
                            self.dirty = true;
                        } else if button == BTN_MIDDLE {
                            self.primary_paste();
                            self.dirty = true;
                        }
                    }
                    PointerEventKind::Release { button, .. } => {
                        if button == BTN_LEFT {
                            self.mouse_left_held = false;
                            self.selection_finalize();
                            self.dirty = true;
                        }
                    }
                    PointerEventKind::Motion { .. } => {
                        if self.mouse_left_held {
                            self.selection_update(col, line, _side);
                            self.dirty = true;
                        }
                    }
                    PointerEventKind::Axis { vertical, .. } => {
                        // Scroll the terminal scrollback when not in mouse mode.
                        let th = self.terminal.term_handle();
                        let mut t = th.lock();
                        if vertical.discrete > 0 {
                            t.scroll_display(Scroll::Delta(-3));
                        } else if vertical.discrete < 0 {
                            t.scroll_display(Scroll::Delta(3));
                        }
                        self.dirty = true;
                    }
                    _ => {}
                }
            }
        }
    }
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

// ── URL detection ─────────────────────────────────────────────────────────────

use std::sync::LazyLock;

/// Compiled regex for detecting URLs in visible terminal text.
/// Matches http:// and https:// URLs, stopping at common terminal delimiters.
static URL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"https?://[^\s<>\x00-\x1f\x7f)\]}>\"'`]+"#).unwrap()
});

/// Detect URLs via regex in the visible cell grid and annotate cells that
/// don't already have an OSC 8 hyperlink. Groups cells by row, reconstructs
/// the row text, finds URL matches, and tags matching columns.
fn detect_urls_in_cells(cells: &mut [RenderCell], screen_lines: usize) {
    // Group cell indices by row for efficient per-row scanning.
    let mut row_indices: Vec<Vec<usize>> = vec![Vec::new(); screen_lines];
    for (i, cell) in cells.iter().enumerate() {
        if cell.row < screen_lines {
            row_indices[cell.row].push(i);
        }
    }

    for row_idxs in &row_indices {
        if row_idxs.is_empty() {
            continue;
        }

        // Find max column to size the row text buffer.
        let max_col = row_idxs
            .iter()
            .map(|&i| cells[i].col)
            .max()
            .unwrap_or(0);

        // Build the row text string and a byte-offset-to-column mapping.
        // Each char in row_text corresponds to one terminal column.
        let mut row_text: Vec<char> = vec![' '; max_col + 1];
        for &idx in row_idxs {
            row_text[cells[idx].col] = cells[idx].c;
        }
        let text: String = row_text.iter().collect();

        // Build byte-offset → column-index mapping.
        // char_starts[col] = byte offset where column `col` starts in `text`.
        let char_starts: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();

        // Find URL matches in this row's text.
        for mat in URL_RE.find_iter(&text) {
            let match_byte_start = mat.start();
            let url = mat.as_str();

            // Strip common trailing punctuation that's not part of the URL.
            let url = url.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'));
            if url.len() < 10 {
                // Too short to be a real URL (at minimum "http://x.y")
                continue;
            }
            let url_byte_end = match_byte_start + url.len();

            // Convert byte offsets to column indices.
            let start_col = char_starts
                .iter()
                .position(|&b| b == match_byte_start)
                .unwrap_or(0);
            let end_col = char_starts
                .iter()
                .position(|&b| b >= url_byte_end)
                .unwrap_or(char_starts.len());

            // Tag each cell in the URL column range that doesn't already have a hyperlink.
            for &idx in row_idxs {
                let col = cells[idx].col;
                if col >= start_col && col < end_col && cells[idx].hyperlink.is_none() {
                    cells[idx].hyperlink = Some(url.to_owned());
                }
            }
        }
    }
}
