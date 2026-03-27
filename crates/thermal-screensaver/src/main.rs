//! thermal-screensaver: idle-triggered thermal fluid simulation overlay for Wayland.
//!
//! Uses ext-idle-notify-v1 for idle detection and wlr-layer-shell for
//! fullscreen overlay rendering with a reaction-diffusion WGSL shader.

use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use clap::Parser;
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use sctk::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
};
use smithay_client_toolkit as sctk;
use tracing::{error, info, warn};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_surface},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1, ext_idle_notifier_v1,
};

// -- CLI --

#[derive(Parser)]
#[command(
    name = "thermal-screensaver",
    about = "Idle-triggered thermal fluid simulation"
)]
struct Cli {
    /// Idle timeout in seconds before screensaver activates
    #[arg(long, default_value = "300")]
    timeout: u64,
}

// -- WGSL shader: reaction-diffusion thermal fluid simulation --

const FLUID_SHADER: &str = r#"
struct Uniforms {
    time: f32,
    width: f32,
    height: f32,
    opacity: f32,
}
@group(0) @binding(0)
var<uniform> u: Uniforms;

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
    // Fullscreen triangle
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 3.0,  1.0),
    );
    let p = positions[idx];
    return vec4<f32>(p.x, p.y, 0.0, 1.0);
}

// Thermal palette (matches thermal-core palette.rs)
fn thermal_color(t: f32) -> vec3<f32> {
    let cool      = vec3<f32>(0.118, 0.227, 0.541);   // COOL
    let cold      = vec3<f32>(0.176, 0.106, 0.412);   // COLD
    let mild      = vec3<f32>(0.051, 0.580, 0.533);   // MILD
    let warm      = vec3<f32>(0.133, 0.773, 0.369);   // WARM
    let hot       = vec3<f32>(0.918, 0.702, 0.031);   // HOT
    let hotter    = vec3<f32>(0.976, 0.451, 0.086);   // HOTTER
    let searing   = vec3<f32>(0.937, 0.267, 0.267);   // SEARING
    let white_hot = vec3<f32>(0.996, 0.953, 0.780);   // WHITE_HOT
    if t < 0.15 {
        return mix(cool, cold, t / 0.15);
    } else if t < 0.30 {
        return mix(cold, mild, (t - 0.15) / 0.15);
    } else if t < 0.45 {
        return mix(mild, warm, (t - 0.30) / 0.15);
    } else if t < 0.60 {
        return mix(warm, hot, (t - 0.45) / 0.15);
    } else if t < 0.75 {
        return mix(hot, hotter, (t - 0.60) / 0.15);
    } else if t < 0.90 {
        return mix(hotter, searing, (t - 0.75) / 0.15);
    } else {
        return mix(searing, white_hot, (t - 0.90) / 0.10);
    }
}

// Pseudo-random hash
fn hash2(p: vec2<f32>) -> vec2<f32> {
    var q = vec2<f32>(
        dot(p, vec2<f32>(127.1, 311.7)),
        dot(p, vec2<f32>(269.5, 183.3))
    );
    return fract(sin(q) * 43758.5453) * 2.0 - 1.0;
}

// Gradient noise
fn gnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let uu = f * f * (3.0 - 2.0 * f);

    return mix(
        mix(dot(hash2(i + vec2<f32>(0.0, 0.0)), f - vec2<f32>(0.0, 0.0)),
            dot(hash2(i + vec2<f32>(1.0, 0.0)), f - vec2<f32>(1.0, 0.0)), uu.x),
        mix(dot(hash2(i + vec2<f32>(0.0, 1.0)), f - vec2<f32>(0.0, 1.0)),
            dot(hash2(i + vec2<f32>(1.0, 1.0)), f - vec2<f32>(1.0, 1.0)), uu.x),
        uu.y
    );
}

// Fractal Brownian motion
fn fbm(p: vec2<f32>, t: f32) -> f32 {
    var v = 0.0;
    var a = 0.5;
    let shift = vec2<f32>(100.0, 100.0);
    var q = p;
    // Slow rotation matrix per octave
    let cs = cos(0.5);
    let sn = sin(0.5);

    for (var i = 0; i < 5; i++) {
        v += a * gnoise(q + t * 0.08);
        q = vec2<f32>(q.x * cs - q.y * sn, q.x * sn + q.y * cs) * 2.0 + shift;
        a *= 0.5;
    }
    return v;
}

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    let resolution = vec2<f32>(u.width, u.height);
    let uv = frag_coord.xy / resolution;
    let t = u.time;

    // Aspect-corrected coordinates
    let aspect = resolution.x / resolution.y;
    let p = vec2<f32>((uv.x - 0.5) * aspect, uv.y - 0.5) * 3.0;

    // Reaction-diffusion-like pattern via warped domain FBM
    // First layer: base warp field
    let q = vec2<f32>(
        fbm(p + vec2<f32>(0.0, 0.0), t),
        fbm(p + vec2<f32>(5.2, 1.3), t)
    );

    // Second layer: warp the warp
    let r = vec2<f32>(
        fbm(p + 4.0 * q + vec2<f32>(1.7, 9.2), t * 0.7),
        fbm(p + 4.0 * q + vec2<f32>(8.3, 2.8), t * 0.7)
    );

    // Final value with dual warp creating organic fluid motion
    let f = fbm(p + 4.0 * r, t * 0.5);

    // Map to 0..1 range for thermal coloring
    let heat = clamp(f * 0.5 + 0.5, 0.0, 1.0);

    // Apply thermal color palette
    let color = thermal_color(heat);

    // Slight vignette for depth
    let vignette = 1.0 - 0.3 * length(uv - 0.5);

    return vec4<f32>(color * vignette, u.opacity);
}
"#;

// -- GPU pipeline --

struct FluidPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    start: Instant,
}

impl FluidPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fluid_shader"),
            source: wgpu::ShaderSource::Wgsl(FLUID_SHADER.into()),
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fluid_uniforms"),
            size: 16, // time(f32) + width(f32) + height(f32) + opacity(f32)
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fluid_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fluid_bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fluid_layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fluid_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Self {
            pipeline,
            uniform_buf,
            bind_group,
            start: Instant::now(),
        }
    }

    fn update_uniforms(&self, queue: &wgpu::Queue, width: u32, height: u32, opacity: f32) {
        let elapsed = self.start.elapsed().as_secs_f32();
        let data: [f32; 4] = [elapsed, width as f32, height as f32, opacity];
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::cast_slice(&data));
    }
}

// -- Per-output GPU surface --

struct ScreensaverSurface {
    layer: LayerSurface,
    wgpu_surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    width: u32,
    height: u32,
    pipeline: FluidPipeline,
    qh: QueueHandle<App>,
}

impl ScreensaverSurface {
    fn render(&self, device: &wgpu::Device, queue: &wgpu::Queue, opacity: f32) {
        self.pipeline
            .update_uniforms(queue, self.width, self.height, opacity);

        let surface_texture = match self.wgpu_surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.wgpu_surface.configure(device, &self.config);
                return;
            }
            Err(e) => {
                warn!("wgpu: surface texture error: {:?}", e);
                return;
            }
        };

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("screensaver_frame"),
        });

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fluid_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rpass.set_pipeline(&self.pipeline.pipeline);
            rpass.set_bind_group(0, &self.pipeline.bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }

        queue.submit(Some(encoder.finish()));

        // Request next frame callback before present
        let wl_surf = self.layer.wl_surface();
        wl_surf.frame(&self.qh, wl_surf.clone());

        surface_texture.present();
    }
}

// -- Screensaver state machine --

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Waiting for idle notification
    WaitingForIdle,
    /// Screensaver is active and rendering
    Active,
    /// Fading out (dismiss triggered)
    FadingOut { fade_start: Instant },
}

// -- Application state --

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,

    // Idle notification protocol objects
    idle_notifier: ext_idle_notifier_v1::ExtIdleNotifierV1,
    idle_notification: Option<ext_idle_notification_v1::ExtIdleNotificationV1>,

    // GPU state (initialized on first idle)
    wgpu_instance: wgpu::Instance,
    device: Option<Rc<wgpu::Device>>,
    queue: Option<Rc<wgpu::Queue>>,
    display_ptr: *mut std::ffi::c_void,

    // Layer surfaces pending configure
    pending_layers: Vec<LayerSurface>,
    // Active surfaces
    surfaces: Vec<ScreensaverSurface>,

    phase: Phase,
    exit: bool,

    /// Tracks last user input (keyboard/pointer) for watchdog timeout.
    last_input: Instant,
}

impl App {
    /// Initialize GPU device lazily on first activation.
    fn ensure_gpu(&mut self) {
        if self.device.is_some() {
            return;
        }

        let adapter = pollster::block_on(self.wgpu_instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: None,
                force_fallback_adapter: false,
            },
        ))
        .expect("No wgpu adapter available");
        info!("wgpu adapter: {:?}", adapter.get_info());

        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default(), None))
            .expect("Failed to create wgpu device");

        self.device = Some(Rc::new(device));
        self.queue = Some(Rc::new(queue));
    }

    /// Create layer-shell surfaces on all outputs.
    fn activate(&mut self, qh: &QueueHandle<App>) {
        info!("screensaver activating");
        self.ensure_gpu();
        self.phase = Phase::Active;

        for output in self.output_state.outputs() {
            let wl_surface = self.compositor_state.create_surface(qh);
            let layer = self.layer_shell.create_layer_surface(
                qh,
                wl_surface,
                Layer::Overlay,
                Some("thermal-screensaver"),
                Some(&output),
            );

            // Fullscreen: anchor all edges
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_exclusive_zone(-1); // don't push other surfaces
            layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            layer.set_size(0, 0); // compositor picks fullscreen size

            // Initial commit to get configure
            layer.commit();

            self.pending_layers.push(layer);
        }

        info!("created {} layer surfaces", self.pending_layers.len());
    }

    /// Destroy all surfaces.
    fn deactivate(&mut self) {
        info!("screensaver deactivating");
        self.surfaces.clear();
        self.pending_layers.clear();
        self.phase = Phase::WaitingForIdle;
    }

    /// Begin fade-out transition.
    fn begin_dismiss(&mut self) {
        if self.phase == Phase::Active {
            info!("screensaver dismissing (fade-out)");
            self.phase = Phase::FadingOut {
                fade_start: Instant::now(),
            };
        }
    }

    /// Get current opacity based on phase.
    fn opacity(&self) -> f32 {
        match self.phase {
            Phase::WaitingForIdle => 0.0,
            Phase::Active => 1.0,
            Phase::FadingOut { fade_start } => {
                let elapsed = fade_start.elapsed().as_secs_f32();
                let fade_duration = 0.5;
                (1.0 - elapsed / fade_duration).max(0.0)
            }
        }
    }

    /// Check if fade-out is complete.
    fn fade_complete(&self) -> bool {
        match self.phase {
            Phase::FadingOut { fade_start } => fade_start.elapsed().as_secs_f32() >= 0.5,
            _ => false,
        }
    }
}

// -- Dispatch for ext_idle_notifier_v1 (global binding) --

impl Dispatch<ext_idle_notifier_v1::ExtIdleNotifierV1, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &ext_idle_notifier_v1::ExtIdleNotifierV1,
        _event: ext_idle_notifier_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The notifier global has no events
    }
}

// -- Dispatch for ext_idle_notification_v1 --

impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &ext_idle_notification_v1::ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                info!("idle notification: user is idle");
                if state.phase == Phase::WaitingForIdle {
                    state.activate(qh);
                }
            }
            ext_idle_notification_v1::Event::Resumed => {
                info!("idle notification: user resumed");
                if state.phase == Phase::Active {
                    state.begin_dismiss();
                }
            }
            _ => {}
        }
    }
}

// -- Dispatch for wl_registry (needed for binding idle notifier) --

impl Dispatch<wl_registry::WlRegistry, GlobalList> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalList,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Handled by sctk RegistryState
    }
}

// -- CompositorHandler --

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

// -- OutputHandler --

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

// -- LayerShellHandler --

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let id = layer.wl_surface().id();
        self.surfaces.retain(|s| s.layer.wl_surface().id() != id);
        self.pending_layers.retain(|l| l.wl_surface().id() != id);
        if self.surfaces.is_empty()
            && self.pending_layers.is_empty()
            && self.phase != Phase::WaitingForIdle
        {
            self.phase = Phase::WaitingForIdle;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        if width == 0 || height == 0 {
            return;
        }

        let surface_id = layer.wl_surface().id();

        // Check if this is an existing surface being reconfigured
        if let Some(existing) = self
            .surfaces
            .iter_mut()
            .find(|s| s.layer.wl_surface().id() == surface_id)
        {
            existing.width = width;
            existing.height = height;
            existing.config.width = width;
            existing.config.height = height;
            existing
                .wgpu_surface
                .configure(self.device.as_ref().unwrap(), &existing.config);
            info!("screensaver surface reconfigured: {}x{}", width, height);
            return;
        }

        // New surface from pending_layers
        let pos = match self
            .pending_layers
            .iter()
            .position(|l| l.wl_surface().id() == surface_id)
        {
            Some(p) => p,
            None => {
                warn!("configure for unknown surface");
                return;
            }
        };

        let device = match &self.device {
            Some(d) => d,
            None => {
                error!("GPU not initialized at configure time");
                return;
            }
        };

        let owned_layer = self.pending_layers.remove(pos);
        let raw_surface_ptr = owned_layer
            .wl_surface()
            .id()
            .as_ptr()
            .cast::<std::ffi::c_void>();

        let wgpu_surface = unsafe {
            self.wgpu_instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                        NonNull::new(self.display_ptr).unwrap(),
                    )),
                    raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(
                        NonNull::new(raw_surface_ptr).unwrap(),
                    )),
                })
                .expect("Failed to create wgpu surface")
        };

        let adapter = pollster::block_on(self.wgpu_instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&wgpu_surface),
                force_fallback_adapter: false,
            },
        ))
        .expect("No adapter for surface");

        let caps = wgpu_surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8UnormSrgb)
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        wgpu_surface.configure(device, &config);

        let pipeline = FluidPipeline::new(device, format);

        self.surfaces.push(ScreensaverSurface {
            layer: owned_layer,
            wgpu_surface,
            config,
            width,
            height,
            pipeline,
            qh: qh.clone(),
        });

        info!("screensaver surface ready: {}x{}", width, height);
    }
}

// -- SeatHandler --

impl SeatHandler for App {
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
        if capability == Capability::Keyboard
            && let Err(e) = self.seat_state.get_keyboard(qh, &seat, None)
        {
            warn!("Could not get keyboard: {}", e);
        }
        if capability == Capability::Pointer
            && let Err(e) = self.seat_state.get_pointer(qh, &seat)
        {
            warn!("Could not get pointer: {}", e);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

// -- KeyboardHandler --

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
        // Any keypress dismisses the screensaver
        self.last_input = Instant::now();
        self.begin_dismiss();
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: u32,
    ) {
    }
}

// -- PointerHandler --

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Press { .. }
                | PointerEventKind::Release { .. }
                | PointerEventKind::Motion { .. } => {
                    // Any mouse activity dismisses the screensaver
                    self.last_input = Instant::now();
                    self.begin_dismiss();
                }
                _ => {}
            }
        }
    }
}

// -- Delegate macros --

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_layer!(App);
delegate_registry!(App);
sctk::delegate_keyboard!(App);
sctk::delegate_pointer!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// -- Main --

fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!(
        "thermal-screensaver v{} starting (timeout={}s)",
        env!("CARGO_PKG_VERSION"),
        cli.timeout
    );

    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland");
    let display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).expect("Failed to init registry queue");
    let qh: QueueHandle<App> = event_queue.handle();

    // Bind standard SCTK globals
    let compositor_state =
        CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    let seat_state = SeatState::new(&globals, &qh);
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell not available");

    // Bind ext-idle-notifier-v1 manually from the global list
    let idle_notifier: ext_idle_notifier_v1::ExtIdleNotifierV1 = globals
        .bind(&qh, 1..=2, ())
        .expect("ext_idle_notifier_v1 not available (compositor must support ext-idle-notify-v1)");

    let wgpu_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        ..Default::default()
    });

    let timeout_ms = (cli.timeout * 1000) as u32;

    let mut app = App {
        registry_state,
        seat_state,
        output_state,
        compositor_state,
        layer_shell,
        idle_notifier,
        idle_notification: None,
        wgpu_instance,
        device: None,
        queue: None,
        display_ptr,
        pending_layers: Vec::new(),
        surfaces: Vec::new(),
        phase: Phase::WaitingForIdle,
        exit: false,
        last_input: Instant::now(),
    };

    // Roundtrip to discover outputs and seats
    event_queue.roundtrip(&mut app).expect("roundtrip failed");

    // Create the idle notification with the first seat
    let seats: Vec<_> = app.seat_state.seats().collect();
    if seats.is_empty() {
        error!("No seats available");
        return;
    }
    let seat = &seats[0];

    let idle_notification = app
        .idle_notifier
        .get_idle_notification(timeout_ms, seat, &qh, ());
    app.idle_notification = Some(idle_notification);

    info!(
        "idle notification created, timeout={}ms, entering event loop",
        timeout_ms
    );

    // Event loop
    loop {
        // Process pending events
        event_queue
            .dispatch_pending(&mut app)
            .expect("dispatch failed");
        if let Err(e) = conn.flush() {
            warn!("Wayland conn.flush() failed (DPMS/idle?): {e}");
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read();
            event_queue
                .dispatch_pending(&mut app)
                .expect("dispatch failed");
        }

        if app.exit {
            break;
        }

        // Watchdog: if Exclusive keyboard grab held >5 min with no input, exit gracefully
        // to avoid locking out the user after DPMS/idle transitions.
        if app.phase == Phase::Active
            && app.last_input.elapsed() > Duration::from_secs(5 * 60)
        {
            warn!(
                "screensaver watchdog: Exclusive grab held >5 min without input, exiting"
            );
            break;
        }

        // Render if active
        match app.phase {
            Phase::Active | Phase::FadingOut { .. } => {
                let opacity = app.opacity();

                if let (Some(device), Some(queue)) = (&app.device, &app.queue) {
                    for surface in &app.surfaces {
                        surface.render(device, queue, opacity);
                    }
                }

                if app.fade_complete() {
                    app.deactivate();
                }

                // ~60 FPS when active
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            Phase::WaitingForIdle => {
                // Low-power: just wait for events
                if let Err(e) = event_queue.blocking_dispatch(&mut app) {
                    error!("blocking dispatch error: {:?}", e);
                    break;
                }
            }
        }
    }

    // Cleanup
    if let Some(notification) = app.idle_notification.take() {
        notification.destroy();
    }
}
