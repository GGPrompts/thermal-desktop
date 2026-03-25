/// Wayland layer-shell surface for thermal-hud.
///
/// Uses smithay-client-toolkit 0.19 to create a wlr-layer-shell surface
/// anchored to the top of the screen with a 48px exclusive zone.
/// Adapted from thermal-bar's wayland.rs pattern.
use std::time::Duration;

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

use thermal_core::ClaudeStatePoller;

use crate::renderer::Renderer;
use crate::voice::{HudMode, VoiceStatePoller};

/// Height of the HUD header bar in pixels.
pub const HUD_HEIGHT: u32 = 48;

/// State for the thermal-hud Wayland client.
pub struct HudState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,

    /// The layer-shell surface representing the HUD bar.
    pub layer: LayerSurface,
    /// Current width, set after configure.
    pub width: u32,
    /// Whether we have received and handled the first configure.
    pub configured: bool,
    /// Set to true to exit the event loop.
    pub exit: bool,
}

impl HudState {
    /// Commit an empty (null) buffer so the compositor will send a configure.
    pub fn commit_empty(&self) {
        self.layer.commit();
    }
}

// ---------------------------------------------------------------------------
// sctk handler impls
// ---------------------------------------------------------------------------

impl CompositorHandler for HudState {
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

impl OutputHandler for HudState {
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

impl LayerShellHandler for HudState {
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
        if configure.new_size.0 != 0 {
            self.width = configure.new_size.0;
        }

        tracing::debug!(
            width = self.width,
            height = HUD_HEIGHT,
            "layer surface configured"
        );

        // Only commit on the first configure to acknowledge and map the surface.
        if !self.configured {
            self.configured = true;
            self.layer.wl_surface().commit();
        }
    }
}

impl SeatHandler for HudState {
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

delegate_compositor!(HudState);
delegate_output!(HudState);
delegate_seat!(HudState);
delegate_layer!(HudState);
delegate_registry!(HudState);

impl ProvidesRegistryState for HudState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Connect to the Wayland compositor, create a layer-shell HUD surface, and
/// enter the event loop. Returns when the surface is closed or an error occurs.
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
        Some("thermal-hud"),
        None, // no specific output
    );

    // Configure HUD geometry: full-width strip anchored to the top.
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(HUD_HEIGHT as i32);
    layer.set_size(0, HUD_HEIGHT); // width 0 = full output width
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);

    // Initial commit: no buffer attached — compositor will send a configure.
    layer.commit();

    let mut hud = HudState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        layer,
        width: 1920, // sane default until compositor configures us
        configured: false,
        exit: false,
    };

    tracing::info!("thermal-hud: waiting for compositor configure");

    // Phase 1: Block until the compositor sends the first configure event.
    while !hud.configured {
        event_queue.blocking_dispatch(&mut hud)?;
        if hud.exit {
            tracing::info!("thermal-hud: exit before configure");
            return Ok(());
        }
    }

    // Phase 2: Initialize the wgpu renderer now that we know the surface size.
    let display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
    let surface_ptr = hud
        .layer
        .wl_surface()
        .id()
        .as_ptr()
        .cast::<std::ffi::c_void>() as *mut std::ffi::c_void;

    let mut renderer =
        Renderer::new_from_wayland(display_ptr, surface_ptr, hud.width, HUD_HEIGHT).await?;

    tracing::info!(
        width = hud.width,
        height = HUD_HEIGHT,
        "thermal-hud: renderer initialized, entering render loop"
    );

    // Phase 3: Set up the ClaudeStatePoller for agent sessions.
    let mut poller = ClaudeStatePoller::new()
        .map_err(|e| anyhow::anyhow!("failed to create ClaudeStatePoller: {e}"))?;

    // Phase 4: Set up the VoiceStatePoller for voice assistant UI.
    let mut voice_poller = VoiceStatePoller::new()
        .map_err(|e| anyhow::anyhow!("failed to create VoiceStatePoller: {e}"))?;

    // Track which tab is "active" (index into sessions list).
    let mut active_tab: usize = 0;

    loop {
        // Non-blocking dispatch of any pending Wayland events.
        event_queue.dispatch_pending(&mut hud)?;
        conn.flush()?;
        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read();
            event_queue.dispatch_pending(&mut hud)?;
        }

        if hud.exit {
            tracing::info!("thermal-hud: exit requested");
            break;
        }

        // Check if the compositor resized us.
        if renderer.width != hud.width {
            renderer.resize(hud.width, HUD_HEIGHT);
        }

        // Poll voice state first — it takes priority over agent tabs.
        let voice_mode = voice_poller.poll();

        // Request the next frame callback before rendering.
        {
            let wl_surf = hud.layer.wl_surface();
            wl_surf.frame(&qh, wl_surf.clone());
        }

        // Render based on the current HUD mode.
        let render_result = match &voice_mode {
            HudMode::VoiceActive { .. } => {
                // Compute how long the result has been shown (for auto-dim).
                let result_age = voice_poller.result_shown_at.map(|t| t.elapsed().as_secs());
                tracing::debug!(?voice_mode, "rendering voice state");
                renderer.render_voice_state(&voice_mode, result_age)
            }
            HudMode::AgentTabs => {
                // Fall back to agent tab rendering.
                let sessions = poller.poll();

                // Clamp active tab index.
                if !sessions.is_empty() && active_tab >= sessions.len() {
                    active_tab = sessions.len() - 1;
                }

                renderer.render_tabs(&sessions, active_tab)
            }
        };

        match render_result {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("render error: {e}");
            }
        }

        // Sleep ~1 second between frames — status HUD doesn't need high FPS.
        std::thread::sleep(Duration::from_secs(1));
    }

    Ok(())
}
