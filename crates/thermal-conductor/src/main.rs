//! Thermal Conductor — Native GPU-rendered agent dashboard.
//!
//! A wall of terminal panes, each running a Claude agent session.
//! Thermal state indicators, PipeWire audio cues, git diff awareness.
//!
//! Uses smithay-client-toolkit for a Wayland toplevel surface and wgpu for
//! GPU-accelerated rendering of tmux pane captures.

mod ansi;
mod audio;
mod capture;
mod conductor;
mod dbus;
mod git_watcher;
mod hud;
mod input;
mod layout;
mod renderer;
mod session;
mod state_detector;
mod tmux;

use std::time::{Duration, Instant};

use smithay_client_toolkit as sctk;

use sctk::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler, BTN_LEFT},
    },
    shell::{
        WaylandSurface,
        xdg::{XdgShell, window::{Window, WindowConfigure, WindowHandler, WindowDecorations}},
    },
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
};

use thermal_core::{ConductorConfig, Layout};

// ── Wayland application state ─────────────────────────────────────────────────

/// State for the thermal-conductor Wayland client.
struct ConductorState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,

    /// The XDG toplevel window.
    window: Window,
    /// Current surface dimensions.
    width: u32,
    height: u32,
    /// Whether we have received the first configure.
    configured: bool,
    /// Set to true to exit the event loop.
    exit: bool,

    /// Pending key events to send to tmux.
    /// Each entry is (keys_string, literal) — if literal is true, use `send-keys -l`.
    pending_keys: Vec<(String, bool)>,
    /// Current keyboard modifiers.
    modifiers: Modifiers,
    /// Which pane has keyboard focus.
    focused_pane: usize,
    /// Last known cursor position.
    cursor_pos: (f64, f64),
    /// Pane index that was clicked (processed in render loop where layout is available).
    click_pending: bool,
}

// ── sctk handler impls ───────────────────────────────────────────────────────

impl CompositorHandler for ConductorState {
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

impl OutputHandler for ConductorState {
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

impl WindowHandler for ConductorState {
    fn request_close(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
    ) {
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
        if let Some(w) = configure.new_size.0 {
            self.width = w.get();
        }
        if let Some(h) = configure.new_size.1 {
            self.height = h.get();
        }

        tracing::debug!(
            width = self.width,
            height = self.height,
            "window configured"
        );

        if !self.configured {
            self.configured = true;
            self.window.wl_surface().commit();
        }
    }
}

impl SeatHandler for ConductorState {
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
        if capability == Capability::Keyboard {
            if let Err(e) = self.seat_state.get_keyboard(qh, &seat, None) {
                tracing::warn!("failed to get keyboard: {e}");
            }
        }
        if capability == Capability::Pointer {
            if let Err(e) = self.seat_state.get_pointer(qh, &seat) {
                tracing::warn!("failed to get pointer: {e}");
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for ConductorState {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        // Ctrl-Q: quit
        if self.modifiers.ctrl && event.keysym == Keysym::q {
            self.exit = true;
            return;
        }

        // Map keysym to tmux key name (not raw escape sequences).
        // tmux send-keys expects key names for special keys, and -l for literal text.
        let key_entry: Option<(String, bool)> = match event.keysym {
            Keysym::Return | Keysym::KP_Enter => Some(("Enter".to_owned(), false)),
            Keysym::BackSpace => Some(("BSpace".to_owned(), false)),
            Keysym::Tab => Some(("Tab".to_owned(), false)),
            Keysym::Escape => Some(("Escape".to_owned(), false)),
            Keysym::Up => Some(("Up".to_owned(), false)),
            Keysym::Down => Some(("Down".to_owned(), false)),
            Keysym::Right => Some(("Right".to_owned(), false)),
            Keysym::Left => Some(("Left".to_owned(), false)),
            Keysym::Home => Some(("Home".to_owned(), false)),
            Keysym::End => Some(("End".to_owned(), false)),
            Keysym::Delete => Some(("DC".to_owned(), false)),
            Keysym::Page_Up => Some(("PageUp".to_owned(), false)),
            Keysym::Page_Down => Some(("PageDown".to_owned(), false)),
            _ => {
                if self.modifiers.ctrl {
                    // Ctrl+letter → send as C-<letter> tmux key name.
                    if let Some(ref utf8) = event.utf8 {
                        if utf8.len() == 1 {
                            let ch = utf8.chars().next().unwrap();
                            if ch.is_ascii_alphabetic() {
                                Some((format!("C-{}", ch.to_ascii_lowercase()), false))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else if let Some(ref utf8) = event.utf8 {
                    // Regular text — send literally.
                    if !utf8.is_empty() {
                        Some((utf8.clone(), true))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        if let Some(entry) = key_entry {
            tracing::info!(keys = %entry.0, literal = entry.1, "key pressed");
            self.pending_keys.push(entry);
        }
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _layout: u32,
    ) {
        self.modifiers = modifiers;
    }
}

impl PointerHandler for ConductorState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Motion { .. } => {
                    self.cursor_pos = event.position;
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    self.cursor_pos = event.position;
                    self.click_pending = true;
                }
                _ => {}
            }
        }
    }
}

// ── sctk delegate macros ──────────────────────────────────────────────────────

delegate_compositor!(ConductorState);
delegate_output!(ConductorState);
delegate_seat!(ConductorState);
delegate_registry!(ConductorState);
sctk::delegate_keyboard!(ConductorState);
sctk::delegate_pointer!(ConductorState);

sctk::delegate_xdg_shell!(ConductorState);
sctk::delegate_xdg_window!(ConductorState);

impl ProvidesRegistryState for ConductorState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("thermal_conductor=debug".parse().unwrap()),
        )
        .init();

    tracing::info!("THERMAL CONDUCTOR v{} — Initializing...", env!("CARGO_PKG_VERSION"));

    // ── Session setup ─────────────────────────────────────────────────────────
    let config = ConductorConfig::default();
    let session_mgr = session::SessionManager::start(config).map_err(|e| {
        anyhow::anyhow!("SessionManager error: {e}")
    })?;

    tracing::info!(
        session = %session_mgr.session.session_name,
        panes = session_mgr.pane_ids().len(),
        "tmux session ready"
    );

    // ── Wayland connection ────────────────────────────────────────────────────
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    // Bind Wayland globals.
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor not available: {e}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("xdg_wm_base not available: {e}"))?;

    // Create a toplevel window.
    let wl_surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(
        wl_surface,
        WindowDecorations::RequestServer,
        &qh,
    );
    window.set_title("THERMAL CONDUCTOR".to_string());
    window.set_app_id("thermal-conductor".to_string());
    window.commit();

    let default_width = 1920u32;
    let default_height = 1080u32;

    let mut state = ConductorState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        window,
        width: default_width,
        height: default_height,
        configured: false,
        exit: false,
        pending_keys: Vec::new(),
        modifiers: Modifiers::default(),
        focused_pane: 0,
        cursor_pos: (0.0, 0.0),
        click_pending: false,
    };

    tracing::info!("waiting for compositor configure...");

    // Phase 1: Block until the compositor sends the first configure event.
    while !state.configured {
        event_queue.blocking_dispatch(&mut state)?;
        if state.exit {
            tracing::info!("exit before configure");
            let _ = session_mgr.shutdown(false);
            return Ok(());
        }
    }

    // Phase 2: Initialize the wgpu renderer now that we know the surface size.
    let display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
    let surface_ptr = state.window.wl_surface().id().as_ptr().cast::<std::ffi::c_void>()
        as *mut std::ffi::c_void;

    let mut renderer = renderer::WgpuState::new_from_wayland(
        display_ptr,
        surface_ptr,
        state.width,
        state.height,
    )
    .await?;

    tracing::info!(
        width = state.width,
        height = state.height,
        "renderer initialized"
    );

    // Phase 3: Create the Conductor and enter the render loop.
    let layout_engine = layout::LayoutEngine::new(
        Layout::Grid,
        state.width as f32,
        state.height as f32,
    );
    let mut cond = conductor::Conductor::new(session_mgr, layout_engine);
    cond.layout.pane_count = cond.session.pane_ids().len();

    // Create text renderer for pane content.
    let mut text_renderer = renderer::TextRenderer::new(
        &renderer.device,
        &renderer.queue,
        renderer.surface_config.format,
        state.width,
        state.height,
    );

    tracing::info!(
        panes = cond.session.pane_ids().len(),
        "entering render loop"
    );

    let target_frame_time = Duration::from_millis(16); // ~60 fps
    let poll_interval = Duration::from_millis(100); // poll tmux at ~10 Hz
    let mut last_poll = Instant::now();

    loop {
        let frame_start = Instant::now();

        // Non-blocking dispatch of pending Wayland events.
        event_queue.dispatch_pending(&mut state)?;
        conn.flush()?;
        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read();
            event_queue.dispatch_pending(&mut state)?;
        }

        if state.exit {
            tracing::info!("exit requested — shutting down");
            break;
        }

        // Handle resize.
        if renderer.width != state.width || renderer.height != state.height {
            renderer.resize(state.width, state.height);
            text_renderer.set_resolution(&renderer.queue, state.width, state.height);
            cond.layout.window_width = state.width as f32;
            cond.layout.window_height = state.height as f32;
        }

        // Handle click-to-focus.
        if state.click_pending {
            state.click_pending = false;
            let (cx, cy) = state.cursor_pos;
            if let Some(idx) = cond.layout.pane_at(cx as f32, cy as f32) {
                state.focused_pane = idx;
                tracing::info!(pane = idx, "focus changed");
            }
        }

        // Send pending key events to the focused tmux pane.
        if !state.pending_keys.is_empty() {
            let pane_ids = cond.session.pane_ids().to_owned();
            let focused = state.focused_pane.min(pane_ids.len().saturating_sub(1));
            if let Some(pane_id) = pane_ids.get(focused) {
                for (keys, literal) in state.pending_keys.drain(..) {
                    let result = if literal {
                        cond.session.session.send_keys_literal(pane_id, &keys)
                    } else {
                        cond.session.session.send_keys(pane_id, &keys)
                    };
                    if let Err(e) = result {
                        tracing::warn!("send_keys error: {e}");
                    }
                }
            } else {
                state.pending_keys.clear();
            }
        }

        // Poll tmux panes at the poll interval.
        if last_poll.elapsed() >= poll_interval {
            let dirty_count = cond.poll();
            if dirty_count > 0 {
                tracing::trace!(dirty = dirty_count, "panes updated");
            }
            last_poll = Instant::now();
        }

        // Compute layout rects and render.
        let rects = cond.layout.compute_rects();
        // Clear dirty flags.
        for d in cond.dirty.iter_mut() {
            *d = false;
        }

        match renderer.render(&cond.captures, &rects, &mut text_renderer) {
            Ok(()) => {}
            Err(wgpu::SurfaceError::Lost) => {
                renderer.resize(state.width, state.height);
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                tracing::error!("wgpu: out of memory");
                break;
            }
            Err(e) => {
                tracing::warn!("render error: {e}");
            }
        }

        // Commit the surface so the compositor displays the frame.
        state.window.wl_surface().commit();

        // Sleep to maintain target frame rate.
        let elapsed = frame_start.elapsed();
        if elapsed < target_frame_time {
            std::thread::sleep(target_frame_time - elapsed);
        }
    }

    // Leave the tmux session alive so the user can `tmux a` into it.
    let _ = cond.session.shutdown(false);

    Ok(())
}
