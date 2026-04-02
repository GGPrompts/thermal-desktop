/// Wayland layer-shell surface for thermal-hud.
///
/// Uses smithay-client-toolkit 0.19 to create a wlr-layer-shell surface
/// anchored to the top of the screen with a 48px exclusive zone.
/// Adapted from thermal-bar's wayland.rs pattern.
use std::time::Duration;

use smithay_client_toolkit as sctk;

use sctk::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{BTN_LEFT, PointerEvent, PointerEventKind, PointerHandler},
    },
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
    protocol::{wl_output, wl_pointer::WlPointer, wl_seat, wl_surface},
};

use thermal_core::ClaudeStatePoller;

use crate::daemon_subscriber;
use crate::renderer::Renderer;
use crate::voice::{HudMode, VoiceStatePoller};

/// Height of the HUD header bar in pixels.
pub const HUD_HEIGHT: u32 = 48;

// ---------------------------------------------------------------------------
// Click interaction types
// ---------------------------------------------------------------------------

/// An action to execute when a HUD region is clicked.
#[derive(Debug, Clone)]
pub enum ClickAction {
    /// Focus the session tab at this index.
    SelectTab(usize),
    /// Select tab at index and focus the session's workspace via hyprctl.
    SessionFocus(usize, i64),
}

/// A rectangular click target on the HUD surface.
#[derive(Debug, Clone)]
pub struct ClickRegion {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub action: ClickAction,
}

impl ClickRegion {
    /// Test whether a point falls within this region.
    fn hit_test(&self, px: f64, py: f64) -> bool {
        let px = px as f32;
        let py = py as f32;
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

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

    // Pointer interaction state
    pointer: Option<WlPointer>,
    pointer_position: (f64, f64),
    /// Click regions rebuilt each render cycle.
    pub click_regions: Vec<ClickRegion>,
    /// Pending click action to execute after event dispatch.
    pub pending_click: Option<ClickAction>,
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
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(ptr) => self.pointer = Some(ptr),
                Err(e) => tracing::warn!("failed to get pointer from seat: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for HudState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.pointer_position = event.position;
                }
                PointerEventKind::Leave { .. } => {
                    self.pointer_position = (-1.0, -1.0);
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    let (px, py) = self.pointer_position;
                    for region in &self.click_regions {
                        if region.hit_test(px, py) {
                            self.pending_click = Some(region.action.clone());
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// delegate macros
// ---------------------------------------------------------------------------

delegate_compositor!(HudState);
delegate_output!(HudState);
delegate_seat!(HudState);
delegate_pointer!(HudState);
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
        pointer: None,
        pointer_position: (-1.0, -1.0),
        click_regions: Vec::new(),
        pending_click: None,
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
        .cast::<std::ffi::c_void>();

    let mut renderer =
        Renderer::new_from_wayland(display_ptr, surface_ptr, hud.width, HUD_HEIGHT).await?;

    tracing::info!(
        width = hud.width,
        height = HUD_HEIGHT,
        "thermal-hud: renderer initialized, entering render loop"
    );

    // Phase 3: Set up agent session state source.
    //
    // Prefer daemon semantic subscriptions (real-time, event-driven) over
    // file-watching (ClaudeStatePoller).  Falls back to the poller when the
    // daemon is not running.
    let daemon_rx = daemon_subscriber::try_spawn_subscriber();
    let mut poller = if daemon_rx.is_some() {
        tracing::info!("Using daemon semantic subscription for agent state (source: daemon)");
        None
    } else {
        tracing::info!(
            "Daemon not available — using ClaudeStatePoller fallback (source: file-derived)"
        );
        Some(
            ClaudeStatePoller::new()
                .map_err(|e| anyhow::anyhow!("failed to create ClaudeStatePoller: {e}"))?,
        )
    };

    // Phase 4: Set up the VoiceStatePoller for voice assistant UI.
    let mut voice_poller = VoiceStatePoller::new()
        .map_err(|e| anyhow::anyhow!("failed to create VoiceStatePoller: {e}"))?;

    // Track which tab is "active" (index into sessions list).
    let mut active_tab: usize = 0;

    loop {
        // Dispatch Wayland events. We poll multiple times per render cycle
        // to keep click response snappy (see sleep loop below).
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

        // Execute any pending click action from the previous dispatch.
        if let Some(action) = hud.pending_click.take() {
            match &action {
                ClickAction::SelectTab(idx) => {
                    active_tab = *idx;
                    tracing::info!(tab = idx, "click: selected HUD tab");
                }
                ClickAction::SessionFocus(idx, ws) => {
                    active_tab = *idx;
                    tracing::info!(
                        tab = idx,
                        workspace = ws,
                        "click: focusing session workspace"
                    );
                    let _ = std::process::Command::new("hyprctl")
                        .args(["dispatch", "workspace", &ws.to_string()])
                        .spawn();
                }
            }
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
                // Clear click regions when in voice mode — no tabs to click.
                hud.click_regions.clear();
                // Compute how long the result has been shown (for auto-dim).
                let result_age = voice_poller.result_shown_at.map(|t| t.elapsed().as_secs());
                tracing::debug!(?voice_mode, "rendering voice state");
                renderer.render_voice_state(&voice_mode, result_age)
            }
            HudMode::AgentTabs => {
                // Get sessions from daemon subscription or file poller.
                let mut sessions = if let Some(ref rx) = daemon_rx {
                    rx.borrow().clone()
                } else if let Some(ref mut p) = poller {
                    p.poll()
                } else {
                    Vec::new()
                };

                // Sort by workspace (same order as renderer) so click
                // regions line up with rendered tabs.
                sessions
                    .sort_by_key(|s| (s.workspace.map_or(i64::MAX, |w| w), s.session_id.clone()));

                // Partition into parent sessions and subagents.
                // Only parent tabs are rendered full-width; subagents become
                // compact emoji icons on their parent's tab.
                let mut subagent_map: std::collections::HashMap<
                    String,
                    Vec<thermal_core::ClaudeSessionState>,
                > = std::collections::HashMap::new();
                let mut parents: Vec<thermal_core::ClaudeSessionState> = Vec::new();

                for s in sessions {
                    if let Some(ref parent_id) = s.parent_session_id {
                        subagent_map.entry(parent_id.clone()).or_default().push(s);
                    } else {
                        parents.push(s);
                    }
                }

                // Clamp active tab index.
                if !parents.is_empty() && active_tab >= parents.len() {
                    active_tab = parents.len() - 1;
                }

                // Rebuild click regions from parent session tab layout.
                build_tab_click_regions(&parents, hud.width as f32, &mut hud.click_regions);

                renderer.render_tabs(&parents, active_tab, &subagent_map)
            }
        };

        match render_result {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("render error: {e}");
            }
        }

        // Sleep ~1s for status updates but poll Wayland events every 100ms
        // so clicks are processed within ~100ms instead of waiting a full second.
        for _ in 0..9 {
            std::thread::sleep(Duration::from_millis(100));
            // Drain any click events that arrived during sleep.
            event_queue.dispatch_pending(&mut hud)?;
            if let Ok(()) = conn.flush() {
                if let Some(guard) = conn.prepare_read() {
                    let _ = guard.read();
                    event_queue.dispatch_pending(&mut hud)?;
                }
            }
            // If a click arrived, break out early to process + re-render.
            if hud.pending_click.is_some() {
                break;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Click region helpers
// ---------------------------------------------------------------------------

/// Tab layout constants — must match those in renderer.rs.
const TAB_MIN_WIDTH: f32 = 200.0;
const TAB_MAX_WIDTH: f32 = 400.0;
const TAB_GAP: f32 = 2.0;
const LEFT_MARGIN: f32 = 8.0;

/// Rebuild click regions from the current session tab layout.
///
/// Each session tab becomes a click region. Clicking a tab both selects it
/// (visual highlight) and, if the session has a workspace, focuses that
/// workspace via hyprctl.
fn build_tab_click_regions(
    sessions: &[thermal_core::ClaudeSessionState],
    screen_w: f32,
    regions: &mut Vec<ClickRegion>,
) {
    regions.clear();

    if sessions.is_empty() {
        return;
    }

    let available = screen_w - LEFT_MARGIN * 2.0;
    let count = sessions.len() as f32;
    let tab_width =
        ((available - TAB_GAP * (count - 1.0)) / count).clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH);
    let tab_h = HUD_HEIGHT as f32;

    for (i, session) in sessions.iter().enumerate() {
        let tab_x = LEFT_MARGIN + i as f32 * (tab_width + TAB_GAP);

        // Clicking always selects the tab. If the session also has a known
        // workspace, focus that workspace via hyprctl.
        let action = if let Some(ws) = session.workspace {
            ClickAction::SessionFocus(i, ws)
        } else {
            ClickAction::SelectTab(i)
        };
        regions.push(ClickRegion {
            x: tab_x,
            y: 0.0,
            width: tab_width,
            height: tab_h,
            action,
        });
    }
}
