/// Wayland layer-shell surface for thermal-bar.
///
/// Uses smithay-client-toolkit 0.19 to create a wlr-layer-shell surface
/// anchored to the top of the screen with a 32px exclusive zone.
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
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_surface},
};

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
        self.configured = true;

        tracing::debug!(
            width = self.width,
            height = BAR_HEIGHT,
            "layer surface configured"
        );

        // Commit the empty surface so the compositor maps it.
        self.layer.wl_surface().commit();
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
pub fn run() -> anyhow::Result<()> {
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

    tracing::info!("thermal-bar: entering Wayland event loop");

    loop {
        event_queue.blocking_dispatch(&mut bar)?;

        if bar.exit {
            tracing::info!("thermal-bar: exit requested");
            break;
        }
    }

    Ok(())
}
