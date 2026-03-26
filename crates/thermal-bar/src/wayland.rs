/// Wayland layer-shell surface for thermal-bar.
///
/// Uses smithay-client-toolkit 0.19 to create a wlr-layer-shell surface
/// anchored to the top of the screen with a 32px exclusive zone.
use std::time::{Duration, Instant};

use smithay_client_toolkit as sctk;

use sctk::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_surface},
};

use crate::layout::BarLayout;
use crate::modules::claude_module::ClaudeModule;
use crate::modules::clock::ClockModule;
use crate::modules::metrics_module::MetricsModule;
use crate::modules::voice::VoiceModule;
use crate::modules::workspace_map::WorkspaceMapModule;
use crate::renderer::Renderer;
use crate::sparkline::SparklineSet;

/// Height of the bar in pixels.
pub const BAR_HEIGHT: u32 = 32;

/// State for the thermal-bar Wayland client.
pub struct BarState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,

    /// The layer-shell surface representing the bar.
    pub layer: LayerSurface,
    /// Current width, set after configure.
    pub width: u32,
    /// Whether we have received and handled the first configure.
    pub configured: bool,
    /// Set to true to exit the event loop.
    pub exit: bool,
}

impl BarState {
    /// Commit an empty (null) buffer so the compositor will send a configure.
    pub fn commit_empty(&self) {
        self.layer.commit();
    }
}

// ---------------------------------------------------------------------------
// sctk handler impls
// ---------------------------------------------------------------------------

impl CompositorHandler for BarState {
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
        // Stub: rendering will be wired in task 2.
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

impl OutputHandler for BarState {
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

impl LayerShellHandler for BarState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // Width 0 means "stretch to full output width" — compositor will pick.
        // We still record whatever the compositor tells us.
        if configure.new_size.0 != 0 {
            self.width = configure.new_size.0;
        }

        tracing::debug!(
            width = self.width,
            height = BAR_HEIGHT,
            "layer surface configured"
        );

        // Only commit on the first configure to acknowledge and map the surface.
        // Subsequent configures are handled by the render loop which commits
        // with an actual buffer attached, avoiding an infinite configure loop.
        if !self.configured {
            self.configured = true;
            self.layer.wl_surface().commit();
        }
    }
}

impl SeatHandler for BarState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
        // No keyboard/pointer needed for a status bar.
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

// ---------------------------------------------------------------------------
// delegate macros
// ---------------------------------------------------------------------------

delegate_compositor!(BarState);
delegate_output!(BarState);
delegate_seat!(BarState);
delegate_layer!(BarState);
delegate_registry!(BarState);

impl ProvidesRegistryState for BarState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Connect to the Wayland compositor, create a layer-shell bar surface, and
/// enter the event loop.  Returns when the surface is closed or an error occurs.
pub async fn run() -> anyhow::Result<()> {
    // Connect to the Wayland compositor via WAYLAND_DISPLAY.
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    // Bind Wayland globals.
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor not available: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wlr-layer-shell not available: {e}"))?;

    // Create a Wayland surface and wrap it in a layer-shell surface.
    let wl_surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        wl_surface,
        Layer::Top,
        Some("thermal-bar"),
        None, // no specific output → appears on all outputs / primary
    );

    // Configure bar geometry: full-width strip anchored to the top.
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(BAR_HEIGHT as i32);
    layer.set_size(0, BAR_HEIGHT); // width 0 → full output width
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);

    // Initial commit: no buffer attached — compositor will send a configure.
    layer.commit();

    let mut bar = BarState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        layer,
        width: 1920, // sane default until compositor configures us
        configured: false,
        exit: false,
    };

    tracing::info!("thermal-bar: waiting for compositor configure");

    // Phase 1: Block until the compositor sends the first configure event,
    // which tells us the actual surface dimensions.
    while !bar.configured {
        event_queue.blocking_dispatch(&mut bar)?;
        if bar.exit {
            tracing::info!("thermal-bar: exit before configure");
            return Ok(());
        }
    }

    // Phase 2: Initialize the wgpu renderer now that we know the surface size.
    let display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
    let surface_ptr = bar
        .layer
        .wl_surface()
        .id()
        .as_ptr()
        .cast::<std::ffi::c_void>();

    let mut renderer =
        Renderer::new_from_wayland(display_ptr, surface_ptr, bar.width, BAR_HEIGHT).await?;

    tracing::info!(
        width = bar.width,
        height = BAR_HEIGHT,
        "thermal-bar: renderer initialized, entering render loop"
    );

    // Phase 3: Render loop — poll metrics, build layout, render, dispatch events.
    let metrics_module = MetricsModule::new();
    let clock_module = ClockModule::new();
    let workspace_module = WorkspaceMapModule::new();
    let mut claude_module = ClaudeModule::new();
    let voice_module = VoiceModule::new();
    let mut sparklines = SparklineSet::new();
    let mut last_metrics = Instant::now();

    // Do an initial metrics poll to seed the CPU delta.
    let _ = crate::metrics::SystemMetrics::poll_full();

    loop {
        // Non-blocking dispatch of any pending Wayland events.
        event_queue.dispatch_pending(&mut bar)?;
        // Flush outgoing requests to the compositor.
        conn.flush()?;
        // Read any new events that arrived on the socket (non-blocking).
        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read();
            event_queue.dispatch_pending(&mut bar)?;
        }

        if bar.exit {
            tracing::info!("thermal-bar: exit requested");
            break;
        }

        // Check if the compositor resized us.
        if renderer.width != bar.width {
            renderer.resize(bar.width, BAR_HEIGHT);
        }

        // Build layout from modules.
        let mut layout = BarLayout::new(bar.width);

        // Left zone: system metrics.
        layout.left = metrics_module.render();

        // Center zone: workspace map with window icons.
        layout.center = workspace_module.render();

        // Right zone: voice status + Claude status + clock + date.
        let mut right_outputs = voice_module.render();
        right_outputs.extend(claude_module.render());
        right_outputs.extend(clock_module.render());
        layout.right = right_outputs;

        // Update sparklines once per second.
        if last_metrics.elapsed() >= Duration::from_secs(1) {
            let m = crate::metrics::SystemMetrics::poll_full();
            sparklines.push_metrics(&m);
            last_metrics = Instant::now();
        }

        // Build sparkline rects — positioned after the left-zone text labels.
        let spark_start_x = layout.left_zone_end() + 8.0;
        let spark_rects = sparklines.render_all(spark_start_x, 6.0);

        // Request the next frame callback before rendering.  This must be done
        // prior to wgpu's present() (which internally commits the wl_surface)
        // so the compositor associates the callback with the upcoming frame.
        // Without this the compositor may stop sending frame events when the
        // surface is occluded, potentially stalling the render loop.
        {
            let wl_surf = bar.layer.wl_surface();
            wl_surf.frame(&qh, wl_surf.clone());
        }

        // Render the bar with sparklines in a single pass.
        match renderer.render_layout(&layout, &spark_rects) {
            Ok(()) => {}
            Err(e) => {
                tracing::debug!("render skipped: {e}");
            }
        }

        // Sleep ~1 second between frames. A status bar doesn't need high FPS;
        // 1 Hz is sufficient for metrics updates.
        std::thread::sleep(Duration::from_secs(1));
    }

    Ok(())
}
