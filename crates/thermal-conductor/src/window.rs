//! SCTK + wgpu window for thermal-conductor.
//!
//! Creates an xdg_toplevel window with a wgpu render pipeline that renders
//! a live terminal via glyphon. Spawns a PTY shell process and connects it
//! to an alacritty_terminal::Term for VT parsing. The grid renderer reads
//! the term's renderable content each frame and renders it via GPU.
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
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler, BTN_LEFT, BTN_MIDDLE},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
};

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use std::ptr::NonNull;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use thermal_core::ThermalPalette;

use crate::grid_renderer::{GridRenderer, RenderCell};
use crate::input;
use crate::pty::PtySession;
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
    let compositor =
        CompositorState::bind(&globals, &qh).expect("wl_compositor is not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg_wm_base is not available");

    // ── Keyboard shortcuts inhibit (optional) ──────────────────────────────────
    // Bind zwp_keyboard_shortcuts_inhibit_manager_v1 so the compositor
    // forwards all key combos (Ctrl+Alt, Super, etc.) to us when focused.
    let shortcuts_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1> =
        match globals.bind::<ZwpKeyboardShortcutsInhibitManagerV1, _, _>(&qh, 1..=1, ()) {
            Ok(manager) => {
                tracing::info!("Keyboard shortcuts inhibit protocol available");
                Some(manager)
            }
            Err(_) => {
                tracing::warn!(
                    "zwp_keyboard_shortcuts_inhibit_manager_v1 not available — \
                     compositor may intercept key combos"
                );
                None
            }
        };

    // ── Create xdg toplevel window ────────────────────────────────────────────
    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("Thermal Conductor");
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
        NonNull::new(conn.backend().display_ptr() as *mut _)
            .expect("Wayland display ptr is null"),
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

    let surface_format = wgpu::TextureFormat::Bgra8UnormSrgb;
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

    // ── Grid renderer ─────────────────────────────────────────────────────────
    let grid_renderer = GridRenderer::new(
        &device,
        &queue,
        surface_format,
        DEFAULT_WIDTH,
        DEFAULT_HEIGHT,
    );

    // ── Terminal + PTY ────────────────────────────────────────────────────────
    // Calculate initial grid size from the renderer's cell metrics.
    let (init_cols, init_rows) = grid_renderer.grid_size(DEFAULT_WIDTH, DEFAULT_HEIGHT);
    let terminal = Terminal::with_size(init_cols, init_rows);

    // Start a tokio runtime for the async PTY reader and byte processor.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .expect("Failed to create tokio runtime");

    // Spawn the PTY inside the tokio runtime.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut pty = tokio_rt
        .block_on(async { PtySession::spawn(&shell) })
        .expect("Failed to spawn PTY");

    // Connect PTY output to the terminal byte processor.
    let pty_output_rx = pty.take_output();
    // Shared dirty flag: the byte processor sets this to true whenever new
    // PTY output has been processed, so the render loop knows to re-render.
    let pty_dirty = Arc::new(AtomicBool::new(false));
    // The byte processor needs the tokio runtime to spawn its task.
    let _guard = tokio_rt.enter();
    terminal.spawn_byte_processor(pty_output_rx, Arc::clone(&pty_dirty));

    // Resize PTY to match grid.
    let _ = pty.resize(init_cols as u16, init_rows as u16);

    tracing::info!(cols = init_cols, rows = init_rows, "Terminal initialized");

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
        terminal,
        pty,
        _tokio_rt: tokio_rt,
        configured: false,
        dirty: true,
        pty_dirty,
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        exit: false,
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
        shortcuts_inhibit_manager,
        shortcuts_inhibitor: None,
    };

    // ── Event loop ────────────────────────────────────────────────────────────
    // Use non-blocking dispatch so we can render when PTY output arrives,
    // not just when Wayland events come in. We dispatch pending events first,
    // then use prepare_read + poll with a short timeout to wait for new data
    // from either Wayland or the PTY (via the 16ms timeout).
    loop {
        // Flush outgoing Wayland requests.
        if let Err(e) = conn.flush() {
            tracing::warn!("Wayland flush failed: {e}");
        }

        // Try to prepare a read guard. If None, there are already pending events.
        if let Some(guard) = conn.prepare_read() {
            // Poll the Wayland fd with a 16ms timeout (~60fps) so we also
            // wake up to render PTY output even without Wayland events.
            use std::os::fd::AsRawFd;
            let fd = guard.connection_fd().as_raw_fd();
            let mut pollfd = [nix::poll::PollFd::new(
                unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
                nix::poll::PollFlags::POLLIN,
            )];
            let _ = nix::poll::poll(&mut pollfd, nix::poll::PollTimeout::from(16u16));
            // Read any new Wayland data (non-blocking after poll).
            let _ = guard.read();
        }

        // Dispatch all pending Wayland events.
        event_queue
            .dispatch_pending(&mut state)
            .expect("Wayland event dispatch failed");

        // Check whether the byte processor has produced new PTY output.
        if state.pty_dirty.swap(false, Ordering::AcqRel) {
            state.dirty = true;
        }

        if state.configured && state.dirty {
            // Check if terminal has content (debug).
            {
                let th = state.terminal.term_handle();
                let t = th.lock();
                let cols = t.columns();
                let lines = t.screen_lines();
                let cursor = t.grid().cursor.point;
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::info!(cols, lines, ?cursor, "First render: term grid state");
                });
            }
            state.render_frame();
            state.dirty = false;
        }

        // Exit if the shell process died (e.g. user typed `exit`).
        if state.pty.has_exited() {
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
    terminal: Terminal,
    pty: PtySession,
    _tokio_rt: tokio::runtime::Runtime,
    configured: bool,
    /// Whether the window needs to be redrawn this iteration.
    dirty: bool,
    /// Set to `true` by the PTY byte processor when new terminal output has
    /// been processed; cleared each time the render loop checks it.
    pty_dirty: Arc<AtomicBool>,
    width: u32,
    height: u32,
    exit: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The seat associated with our keyboard, needed for shortcuts inhibit.
    seat: Option<wl_seat::WlSeat>,
    modifiers: Modifiers,
    // Mouse / pointer state
    pointer: Option<wl_pointer::WlPointer>,
    /// Whether the left mouse button is currently held (for drag selection).
    mouse_left_held: bool,
    // Keyboard shortcuts inhibit
    /// Manager global — kept alive for the session.
    shortcuts_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    /// Active inhibitor — created on keyboard focus, destroyed on blur.
    shortcuts_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

impl ConductorWindow {
    /// Render a frame: clear to BG, then render the terminal grid.
    fn render_frame(&mut self) {
        let output = match self.wgpu.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Outdated) => {
                self.wgpu.surface.configure(&self.wgpu.device, &self.wgpu.config);
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
        let bg = ThermalPalette::BG;
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

        // ── Render terminal grid ─────────────────────────────────────────
        // Lock the terminal and read renderable content.
        let term_handle = self.terminal.term_handle();
        let term = term_handle.lock();
        let content = term.renderable_content();

        let screen_lines = term.screen_lines();
        let display_offset = content.display_offset;
        let cursor = content.cursor;
        let selection_range = content.selection;

        // Collect cells into RenderCell snapshots while holding the lock.
        // The display_iter borrows the grid, so we must collect before releasing.
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

                // Skip wide char spacers.
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    return None;
                }

                Some(RenderCell {
                    row,
                    col: point.column.0,
                    c: cell.c,
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                })
            })
            .collect();

        // Release the term lock before the (potentially slow) GPU work.
        drop(term);

        self.grid_renderer.render(
            &cells,
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

    /// Paste from the primary selection (middle-click) into the PTY.
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
            if let Err(e) = self.pty.write(&payload) {
                tracing::warn!("Failed to write bracketed primary paste to PTY: {}", e);
            }
        } else if let Err(e) = self.pty.write(text) {
            tracing::warn!("Failed to write primary paste to PTY: {}", e);
        }

        tracing::debug!(
            len = text.len(),
            bracketed,
            "Primary paste: sent to PTY"
        );
    }

    /// Paste from the Wayland clipboard into the PTY, with bracketed paste
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
            if let Err(e) = self.pty.write(&payload) {
                tracing::warn!("Failed to write bracketed paste to PTY: {}", e);
            }
        } else {
            if let Err(e) = self.pty.write(text) {
                tracing::warn!("Failed to write paste to PTY: {}", e);
            }
        }

        tracing::debug!(
            len = text.len(),
            bracketed,
            "Clipboard paste: sent to PTY"
        );
    }
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
    fn request_close(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
    ) {
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
            self.wgpu.surface.configure(&self.wgpu.device, &self.wgpu.config);

            // Resize the grid renderer viewport.
            self.grid_renderer.resize(&self.wgpu.queue, w, h);

            // Recalculate terminal grid dimensions and resize.
            let (cols, rows) = self.grid_renderer.grid_size(w, h);
            self.terminal.resize(
                cols,
                rows,
                self.grid_renderer.cell_width as u16,
                self.grid_renderer.cell_height as u16,
            );
            let _ = self.pty.resize(cols as u16, rows as u16);

            tracing::debug!("Window configured: {}x{} (grid: {}x{})", w, h, cols, rows);
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
        if capability == Capability::Keyboard {
            if let Some(kb) = self.keyboard.take() {
                kb.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

// ── Keyboard handler ──────────────────────────────────────────────────────────

impl KeyboardHandler for ConductorWindow {
    fn enter(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
        // Create a keyboard shortcuts inhibitor so the compositor forwards
        // all key combos (Ctrl+Alt, Super, etc.) to us while focused.
        if self.shortcuts_inhibitor.is_none() {
            if let (Some(manager), Some(seat)) =
                (&self.shortcuts_inhibit_manager, &self.seat)
            {
                let inhibitor = manager.inhibit_shortcuts(surface, seat, qh, ());
                tracing::debug!("Keyboard shortcuts inhibitor created");
                self.shortcuts_inhibitor = Some(inhibitor);
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
        // Destroy the shortcuts inhibitor when we lose keyboard focus.
        if let Some(inhibitor) = self.shortcuts_inhibitor.take() {
            inhibitor.destroy();
            tracing::debug!("Keyboard shortcuts inhibitor destroyed");
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
        // keyboard-shortcuts-inhibit eats Super+Q, so we need our own close.
        if self.modifiers.ctrl && self.modifiers.shift {
            if matches!(event.keysym, Keysym::Q | Keysym::q) {
                tracing::info!("Ctrl+Shift+Q: closing window");
                self.exit = true;
                return;
            }
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

        // Encode the key press into PTY bytes and send to the shell.
        if let Some(bytes) = input::encode_key(&event, &self.modifiers) {
            if let Err(e) = self.pty.write(&bytes) {
                tracing::warn!("Failed to write to PTY: {}", e);
            }
        }

        self.dirty = true;
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
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
        for event in events {
            let (px, py) = event.position;

            match event.kind {
                PointerEventKind::Press { button, .. } => {
                    if button == BTN_LEFT {
                        // Start a new selection at the click position.
                        let (col, line, side) = self.pixel_to_grid(px, py);
                        self.selection_start(col, line, side);
                        self.mouse_left_held = true;
                        self.dirty = true;
                    } else if button == BTN_MIDDLE {
                        // Middle-click: paste from primary selection.
                        self.primary_paste();
                        self.dirty = true;
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if button == BTN_LEFT {
                        self.mouse_left_held = false;
                        // Finalize: copy selection text to primary clipboard.
                        self.selection_finalize();
                        self.dirty = true;
                    }
                }
                PointerEventKind::Motion { .. } => {
                    if self.mouse_left_held {
                        // Update selection end point while dragging.
                        let (col, line, side) = self.pixel_to_grid(px, py);
                        self.selection_update(col, line, side);
                        self.dirty = true;
                    }
                }
                _ => {}
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

// ── Keyboard shortcuts inhibit dispatch ──────────────────────────────────────

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for ConductorWindow {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpKeyboardShortcutsInhibitManagerV1,
        _event: <ZwpKeyboardShortcutsInhibitManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The manager has no events — it is a pure request interface.
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for ConductorWindow {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpKeyboardShortcutsInhibitorV1,
        event: <ZwpKeyboardShortcutsInhibitorV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {
                tracing::debug!("Keyboard shortcuts inhibitor: active");
            }
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                tracing::debug!("Keyboard shortcuts inhibitor: inactive (compositor reclaimed shortcuts)");
            }
            _ => {}
        }
    }
}
