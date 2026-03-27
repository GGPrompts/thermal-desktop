//! thermal-face: GPU-rendered animated face avatar with thermal/FLIR aesthetic.
//!
//! Uses SDF (Signed Distance Fields) in a WGSL fragment shader to draw an
//! animated face with thermal heat mapping. Renders in a 200x200 layer-shell
//! overlay anchored to the bottom-right corner.

use std::ptr::NonNull;
use std::time::{Duration, Instant};

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
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
use smithay_client_toolkit as sctk;
use tracing::{info, warn};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_surface},
};

// ---------------------------------------------------------------------------
// WGSL Face Shader
// ---------------------------------------------------------------------------

const FACE_SHADER: &str = r#"
struct FaceUniforms {
    time: f32,
    mouth_open: f32,
    eye_blink: f32,
    expression_heat: f32,
}

@group(0) @binding(0)
var<uniform> u: FaceUniforms;

// Fullscreen triangle vertex shader
@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 3.0,  1.0),
    );
    let p = positions[idx];
    return vec4<f32>(p.x, p.y, 0.0, 1.0);
}

// --- Thermal palette (matches thermal-core palette.rs) ---

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

// --- SDF helpers ---

fn sdf_ellipse(p: vec2<f32>, center: vec2<f32>, radii: vec2<f32>) -> f32 {
    let q = (p - center) / radii;
    return (length(q) - 1.0) * min(radii.x, radii.y);
}

fn sdf_circle(p: vec2<f32>, center: vec2<f32>, radius: f32) -> f32 {
    return length(p - center) - radius;
}

fn smooth_union(d1: f32, d2: f32, k: f32) -> f32 {
    let h = clamp(0.5 + 0.5 * (d2 - d1) / k, 0.0, 1.0);
    return mix(d2, d1, h) - k * h * (1.0 - h);
}

// --- Noise ---

fn hash2(p: vec2<f32>) -> vec2<f32> {
    var q = vec2<f32>(
        dot(p, vec2<f32>(127.1, 311.7)),
        dot(p, vec2<f32>(269.5, 183.3))
    );
    return fract(sin(q) * 43758.5453) * 2.0 - 1.0;
}

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

fn noise2d(p: vec2<f32>) -> f32 {
    return gnoise(p) * 0.5 + 0.5;
}

// --- Fragment shader ---

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    // Map to -1..1 normalized coordinates (square aspect)
    let resolution = vec2<f32>(200.0, 200.0);
    let uv = frag_coord.xy / resolution;
    let p = vec2<f32>((uv.x - 0.5) * 2.0, (0.5 - uv.y) * 2.0); // flip Y for screen coords

    let time = u.time;
    let mouth_open = u.mouth_open;

    // Auto-blink: quick blink every ~4 seconds
    let blink_cycle = fract(time * 0.25);
    let auto_blink = step(blink_cycle, 0.03);
    let eye_blink = clamp(u.eye_blink + auto_blink, 0.0, 1.0);

    // --- Face geometry SDFs ---

    // Head
    let head_d = sdf_ellipse(p, vec2<f32>(0.0, 0.0), vec2<f32>(0.45, 0.55));

    // Eyes
    let eye_h = mix(0.09, 0.005, eye_blink);
    let left_eye_d = sdf_ellipse(p, vec2<f32>(-0.15, 0.12), vec2<f32>(0.07, eye_h));
    let right_eye_d = sdf_ellipse(p, vec2<f32>(0.15, 0.12), vec2<f32>(0.07, eye_h));

    // Pupils
    let left_pupil_d = sdf_circle(p, vec2<f32>(-0.15, 0.12), 0.03);
    let right_pupil_d = sdf_circle(p, vec2<f32>(0.15, 0.12), 0.03);

    // Mouth
    let mouth_h = mix(0.015, 0.06, mouth_open);
    let mouth_d = sdf_ellipse(p, vec2<f32>(0.0, -0.18), vec2<f32>(0.12, mouth_h));

    // --- Heat mapping ---

    var heat = 0.02; // background: very cold

    // Head skin
    if head_d < 0.0 {
        let skin_noise = noise2d(p * 8.0 + time * 0.5) * 0.03;
        heat = 0.35 + 0.05 * noise2d(p * 4.0 + time * 0.3) + skin_noise;
    }

    // Eye regions (inside the eye ellipses)
    let eye_glow = exp(-max(left_eye_d, 0.0) * 20.0) + exp(-max(right_eye_d, 0.0) * 20.0);
    if left_eye_d < 0.0 || right_eye_d < 0.0 {
        heat = 0.7 + 0.3 * eye_glow;
    }

    // Pupils: near white-hot
    if left_pupil_d < 0.0 || right_pupil_d < 0.0 {
        heat = 0.95;
    }

    // Mouth interior
    if mouth_d < 0.0 {
        heat = mix(0.3, 0.85, mouth_open);
    }

    // Thermal bloom: glow around hot regions
    let pupil_bloom = exp(-abs(left_pupil_d) * 10.0) * 0.95 * 0.3
                    + exp(-abs(right_pupil_d) * 10.0) * 0.95 * 0.3;
    let eye_bloom = exp(-abs(left_eye_d) * 10.0) * 0.7 * 0.3
                  + exp(-abs(right_eye_d) * 10.0) * 0.7 * 0.3;
    let mouth_bloom = exp(-abs(mouth_d) * 10.0) * mix(0.3, 0.85, mouth_open) * 0.3;
    let head_bloom = exp(-abs(head_d) * 10.0) * 0.35 * 0.3;

    heat += pupil_bloom + eye_bloom + mouth_bloom + head_bloom;

    // Subtle sensor noise over everything
    heat += noise2d(p * 8.0 + time * 0.5) * 0.03;

    heat = clamp(heat, 0.0, 1.0);

    let color = thermal_color(heat);
    return vec4<f32>(color, 1.0);
}
"#;

// ---------------------------------------------------------------------------
// GPU Pipeline
// ---------------------------------------------------------------------------

struct FacePipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    start: Instant,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FaceUniforms {
    time: f32,
    mouth_open: f32,
    eye_blink: f32,
    expression_heat: f32,
}

impl FacePipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("face_shader"),
            source: wgpu::ShaderSource::Wgsl(FACE_SHADER.into()),
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("face_uniforms"),
            size: std::mem::size_of::<FaceUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("face_bgl"),
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
            label: Some("face_bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("face_layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("face_pipeline"),
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

    fn render(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface: &wgpu::Surface<'_>,
        config: &wgpu::SurfaceConfiguration,
    ) {
        // Update uniforms
        let elapsed = self.start.elapsed().as_secs_f32();
        let uniforms = FaceUniforms {
            time: elapsed,
            mouth_open: 0.0,
            eye_blink: 0.0,
            expression_heat: 0.0,
        };
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        // Get surface texture
        let surface_texture = match surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                surface.configure(device, config);
                return;
            }
            Err(e) => {
                warn!("wgpu surface texture error: {:?}", e);
                return;
            }
        };

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("face_frame"),
        });

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("face_pass"),
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
            rpass.set_pipeline(&self.pipeline);
            rpass.set_bind_group(0, &self.bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }

        queue.submit(Some(encoder.finish()));
        surface_texture.present();
    }
}

// ---------------------------------------------------------------------------
// Wayland layer-shell state
// ---------------------------------------------------------------------------

const FACE_SIZE: u32 = 200;

struct FaceApp {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,

    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    exit: bool,
}

// -- CompositorHandler --

impl CompositorHandler for FaceApp {
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
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
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

impl OutputHandler for FaceApp {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_output::WlOutput,
    ) {
    }
}

// -- LayerShellHandler --

impl LayerShellHandler for FaceApp {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if configure.new_size.0 != 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 != 0 {
            self.height = configure.new_size.1;
        }

        tracing::debug!(
            width = self.width,
            height = self.height,
            "face layer surface configured"
        );

        if !self.configured {
            self.configured = true;
            self.layer.wl_surface().commit();
        }
    }
}

// -- SeatHandler --

impl SeatHandler for FaceApp {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
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

// -- Delegate macros --

delegate_compositor!(FaceApp);
delegate_output!(FaceApp);
delegate_seat!(FaceApp);
delegate_layer!(FaceApp);
delegate_registry!(FaceApp);

impl ProvidesRegistryState for FaceApp {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!(
        "thermal-face v{} starting",
        env!("CARGO_PKG_VERSION"),
    );

    // --- Wayland connection ---
    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland");
    let display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).expect("Failed to init registry queue");
    let qh: QueueHandle<FaceApp> = event_queue.handle();

    // Bind globals
    let compositor =
        CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let layer_shell =
        LayerShell::bind(&globals, &qh).expect("wlr-layer-shell not available");

    // Create layer surface: bottom-right, 200x200, Layer::Top
    let wl_surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        wl_surface,
        Layer::Top,
        Some("thermal-face"),
        None,
    );

    layer.set_anchor(Anchor::BOTTOM | Anchor::RIGHT);
    layer.set_size(FACE_SIZE, FACE_SIZE);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // No exclusive zone — float over other content
    layer.commit();

    let mut app = FaceApp {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        layer,
        width: FACE_SIZE,
        height: FACE_SIZE,
        configured: false,
        exit: false,
    };

    // Wait for first configure
    info!("thermal-face: waiting for compositor configure");
    while !app.configured {
        event_queue
            .blocking_dispatch(&mut app)
            .expect("dispatch failed");
        if app.exit {
            info!("thermal-face: exit before configure");
            return;
        }
    }

    // --- Initialize wgpu ---
    let wgpu_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        ..Default::default()
    });

    let raw_surface_ptr = app
        .layer
        .wl_surface()
        .id()
        .as_ptr()
        .cast::<std::ffi::c_void>();

    let wgpu_surface = unsafe {
        wgpu_instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                    NonNull::new(display_ptr).unwrap(),
                )),
                raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(
                    NonNull::new(raw_surface_ptr).unwrap(),
                )),
            })
            .expect("Failed to create wgpu surface")
    };

    let adapter = pollster::block_on(wgpu_instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: Some(&wgpu_surface),
        force_fallback_adapter: false,
    }))
    .expect("No wgpu adapter available");

    info!("wgpu adapter: {:?}", adapter.get_info());

    let (device, queue) =
        pollster::block_on(adapter.request_device(&Default::default(), None))
            .expect("Failed to create wgpu device");

    // Query surface capabilities for format selection
    let caps = wgpu_surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| *f == wgpu::TextureFormat::Bgra8UnormSrgb)
        .unwrap_or(caps.formats[0]);

    let mut surface_config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: app.width,
        height: app.height,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    wgpu_surface.configure(&device, &surface_config);

    let pipeline = FacePipeline::new(&device, format);

    info!(
        "thermal-face: renderer initialized ({}x{}), entering render loop",
        app.width, app.height
    );

    // --- Render loop (~30 FPS) ---
    loop {
        // Non-blocking Wayland dispatch
        if let Err(e) = event_queue.dispatch_pending(&mut app) {
            warn!("dispatch error: {e}");
            break;
        }
        if let Err(e) = conn.flush() {
            warn!("Wayland conn.flush() failed (DPMS/idle?): {e}");
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read();
            if let Err(e) = event_queue.dispatch_pending(&mut app) {
                warn!("dispatch error: {e}");
                break;
            }
        }

        if app.exit {
            info!("thermal-face: exit requested");
            break;
        }

        // Update surface config on resize
        if app.width != surface_config.width || app.height != surface_config.height {
            surface_config.width = app.width;
            surface_config.height = app.height;
            wgpu_surface.configure(&device, &surface_config);
        }

        // Request next frame callback
        {
            let wl_surf = app.layer.wl_surface();
            wl_surf.frame(&qh, wl_surf.clone());
        }

        // Render
        pipeline.render(&device, &queue, &wgpu_surface, &surface_config);

        // ~30 FPS
        std::thread::sleep(Duration::from_millis(33));
    }
}
