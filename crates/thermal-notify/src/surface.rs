use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::wlr_layer::{
        Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
        LayerSurfaceConfigure,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_surface},
    Connection, Proxy, QueueHandle,
};

// ── NotifySurface ─────────────────────────────────────────────────────────────

pub struct NotifySurface {
    pub width: u32,
    pub height: u32,

    // Wayland state
    conn: Connection,
    queue_handle: QueueHandle<NotifySurfaceState>,
    state: NotifySurfaceState,

    // wgpu
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface_config: wgpu::SurfaceConfiguration,
}

impl NotifySurface {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let conn = Connection::connect_to_env().context("failed to connect to Wayland display")?;

        let (globals, mut event_queue) =
            registry_queue_init::<NotifySurfaceState>(&conn).context("registry_queue_init failed")?;

        let qh = event_queue.handle();

        let compositor_state =
            CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
        let layer_shell =
            LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1 not available")?;
        let output_state = OutputState::new(&globals, &qh);
        let registry_state = RegistryState::new(&globals);
        let seat_state = SeatState::new(&globals, &qh);

        // Create a wl_surface and promote it to a layer surface
        let wl_surface = compositor_state.create_surface(&qh);

        let layer_surface = layer_shell.create_layer_surface(
            &qh,
            wl_surface.clone(),
            Layer::Overlay,
            Some("thermal-notify"),
            None,
        );

        // Configure anchoring: top-right corner
        layer_surface.set_anchor(Anchor::TOP | Anchor::RIGHT);
        layer_surface.set_size(width, height);
        layer_surface.set_margin(16, 16, 0, 0); // top, right, bottom, left
        layer_surface.set_exclusive_zone(-1); // don't reserve space
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        wl_surface.commit();

        let mut state = NotifySurfaceState {
            compositor_state,
            layer_shell,
            output_state,
            registry_state,
            seat_state,
            layer_surface: layer_surface.clone(),
            configured: false,
            closed: false,
        };

        // Run the event loop until the surface is configured
        while !state.configured {
            event_queue
                .blocking_dispatch(&mut state)
                .context("wayland dispatch failed")?;
        }

        // ── wgpu setup ────────────────────────────────────────────────────────
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..Default::default()
        });

        // Safety: the raw Wayland handles come from our connection and surface
        // which both outlive this function's setup. The wgpu surface is kept
        // inside NotifySurface which owns the connection too.
        let raw_display = conn.backend().display_ptr() as *mut _;
        let raw_window = wl_surface.id().as_ptr() as *mut _;

        let wgpu_surface = unsafe {
            instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: raw_window_handle::RawDisplayHandle::Wayland(
                        raw_window_handle::WaylandDisplayHandle::new(
                            std::ptr::NonNull::new(raw_display)
                                .context("null Wayland display pointer")?,
                        ),
                    ),
                    raw_window_handle: raw_window_handle::RawWindowHandle::Wayland(
                        raw_window_handle::WaylandWindowHandle::new(
                            std::ptr::NonNull::new(raw_window)
                                .context("null wl_surface pointer")?,
                        ),
                    ),
                })
                .context("create_surface_unsafe failed")?
        };

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&wgpu_surface),
            force_fallback_adapter: false,
        }))
        .context("no suitable wgpu adapter found")?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("thermal-notify"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: Default::default(),
            },
            None,
        ))
        .context("request_device failed")?;

        let surface_caps = wgpu_surface.get_capabilities(&adapter);
        let format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8UnormSrgb)
            .or_else(|| surface_caps.formats.first().copied())
            .context("no suitable texture format")?;

        let present_mode = if surface_caps
            .present_modes
            .contains(&wgpu::PresentMode::Mailbox)
        {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            alpha_mode: wgpu::CompositeAlphaMode::PreMultiplied,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        wgpu_surface.configure(&device, &surface_config);

        Ok(Self {
            width,
            height,
            conn,
            queue_handle: qh,
            state,
            instance,
            surface: wgpu_surface,
            adapter,
            device,
            queue,
            surface_config,
        })
    }

    /// Resize the Wayland surface and reconfigure wgpu.
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.width = w;
        self.height = h;
        self.state.layer_surface.set_size(w, h);
        self.surface_config.width = w;
        self.surface_config.height = h;
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Dispatch pending Wayland events (non-blocking).
    pub fn dispatch(&mut self) -> Result<()> {
        self.conn
            .prepare_read()
            .context("prepare_read failed")?
            .read()
            .ok(); // ignore read errors (EAGAIN etc.)
        Ok(())
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn surface_config(&self) -> &wgpu::SurfaceConfiguration {
        &self.surface_config
    }

    pub fn get_current_texture(&self) -> Result<wgpu::SurfaceTexture> {
        self.surface
            .get_current_texture()
            .context("get_current_texture failed")
    }
}

// ── Wayland state machine ─────────────────────────────────────────────────────

struct NotifySurfaceState {
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    output_state: OutputState,
    registry_state: RegistryState,
    seat_state: SeatState,
    layer_surface: LayerSurface,
    configured: bool,
    closed: bool,
}

// ── smithay-client-toolkit delegate impls ─────────────────────────────────────

impl CompositorHandler for NotifySurfaceState {
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

impl LayerShellHandler for NotifySurfaceState {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
    ) {
        self.closed = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        _configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        self.configured = true;
    }
}

impl OutputHandler for NotifySurfaceState {
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

impl SeatHandler for NotifySurfaceState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }

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
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }
}

impl ProvidesRegistryState for NotifySurfaceState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(NotifySurfaceState);
delegate_output!(NotifySurfaceState);
delegate_seat!(NotifySurfaceState);
delegate_layer!(NotifySurfaceState);
delegate_registry!(NotifySurfaceState);
