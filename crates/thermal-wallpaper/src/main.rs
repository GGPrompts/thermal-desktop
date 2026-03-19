/// thermal-wallpaper: animated WGSL thermal shader wallpaper daemon for Wayland.
///
/// Renders a simplex-noise heat field mapped through the thermal gradient LUT,
/// modulated by real-time system metrics (CPU, GPU, memory). Low system load
/// produces cool/blue drifting noise; high load produces hot/red turbulence.
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
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
use thermal_core::palette::thermal_gradient_lut;
use tracing::{debug, info, warn};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_surface},
};

// ── System metrics ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct CpuTimes {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
}

impl CpuTimes {
    fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq
    }
    fn idle_total(&self) -> u64 {
        self.idle + self.iowait
    }
}

static PREV_CPU: Mutex<Option<CpuTimes>> = Mutex::new(None);

fn parse_cpu_times(line: &str) -> Option<CpuTimes> {
    let mut parts = line.split_ascii_whitespace();
    parts.next()?; // skip "cpu"
    let user = parts.next()?.parse().ok()?;
    let nice = parts.next()?.parse().ok()?;
    let system = parts.next()?.parse().ok()?;
    let idle = parts.next()?.parse().ok()?;
    let iowait = parts.next()?.parse().ok()?;
    let irq = parts.next()?.parse().ok()?;
    let softirq = parts.next()?.parse().ok()?;
    Some(CpuTimes { user, nice, system, idle, iowait, irq, softirq })
}

/// Read CPU usage as a fraction 0.0 - 1.0.
fn read_cpu_load() -> f32 {
    let Ok(contents) = std::fs::read_to_string("/proc/stat") else {
        return 0.0;
    };
    let first_line = contents.lines().next().unwrap_or("");
    let Some(current) = parse_cpu_times(first_line) else {
        return 0.0;
    };

    let mut guard = PREV_CPU.lock().unwrap();
    let usage = match *guard {
        None => 0.0,
        Some(prev) => {
            let delta_total = current.total().saturating_sub(prev.total());
            let delta_idle = current.idle_total().saturating_sub(prev.idle_total());
            if delta_total == 0 {
                0.0
            } else {
                (delta_total - delta_idle) as f32 / delta_total as f32
            }
        }
    };
    *guard = Some(current);
    usage
}

fn parse_meminfo_kb(contents: &str, key: &str) -> Option<u64> {
    for line in contents.lines() {
        if line.starts_with(key) {
            return line.split_ascii_whitespace().nth(1)?.parse().ok();
        }
    }
    None
}

/// Read memory usage as a fraction 0.0 - 1.0.
fn read_mem_load() -> f32 {
    let Ok(contents) = std::fs::read_to_string("/proc/meminfo") else {
        return 0.0;
    };
    let total = parse_meminfo_kb(&contents, "MemTotal:").unwrap_or(1);
    let available = parse_meminfo_kb(&contents, "MemAvailable:").unwrap_or(0);
    if total == 0 {
        return 0.0;
    }
    (total.saturating_sub(available)) as f32 / total as f32
}

/// Read GPU usage as a fraction 0.0 - 1.0.
fn read_gpu_load() -> f32 {
    // AMD: /sys/class/drm/card0/device/gpu_busy_percent
    if let Ok(raw) = std::fs::read_to_string("/sys/class/drm/card0/device/gpu_busy_percent") {
        if let Ok(pct) = raw.trim().parse::<f32>() {
            return (pct / 100.0).clamp(0.0, 1.0);
        }
    }
    // NVIDIA: nvidia-smi
    if let Ok(output) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
    {
        if let Ok(text) = String::from_utf8(output.stdout) {
            if let Ok(pct) = text.trim().parse::<f32>() {
                return (pct / 100.0).clamp(0.0, 1.0);
            }
        }
    }
    0.0
}

#[derive(Clone, Copy)]
struct SystemLoad {
    cpu: f32,
    gpu: f32,
    mem: f32,
}

impl SystemLoad {
    fn poll() -> Self {
        Self {
            cpu: read_cpu_load(),
            gpu: read_gpu_load(),
            mem: read_mem_load(),
        }
    }
}

// ── WGSL Shader ─────────────────────────────────────────────────────────────

/// Build the WGSL shader source, embedding the thermal gradient LUT.
fn build_shader_source() -> String {
    let lut = thermal_gradient_lut(64);
    let mut lut_entries = String::new();
    for (i, color) in lut.iter().enumerate() {
        let [r, g, b, _a] = color.to_f32_array();
        lut_entries.push_str(&format!(
            "    vec3<f32>({:.6}, {:.6}, {:.6})",
            r, g, b
        ));
        if i < lut.len() - 1 {
            lut_entries.push_str(",\n");
        } else {
            lut_entries.push('\n');
        }
    }

    format!(
        r#"
struct Uniforms {{
    time: f32,
    cpu_load: f32,
    gpu_load: f32,
    mem_load: f32,
    resolution: vec2<f32>,
    _pad: vec2<f32>,
}}

@group(0) @binding(0)
var<uniform> u: Uniforms;

const LUT_SIZE: u32 = 64u;

var<private> thermal_lut: array<vec3<f32>, 64> = array<vec3<f32>, 64>(
{lut_entries});

fn thermal_sample(t: f32) -> vec3<f32> {{
    let tc = clamp(t, 0.0, 1.0);
    let idx_f = tc * f32(LUT_SIZE - 1u);
    let lo = u32(floor(idx_f));
    let hi = min(lo + 1u, LUT_SIZE - 1u);
    let frac = idx_f - floor(idx_f);
    return mix(thermal_lut[lo], thermal_lut[hi], frac);
}}

// Simplex-style noise helpers (2D, gradient-based)
fn mod289_2(x: vec2<f32>) -> vec2<f32> {{ return x - floor(x * (1.0 / 289.0)) * 289.0; }}
fn mod289_3(x: vec3<f32>) -> vec3<f32> {{ return x - floor(x * (1.0 / 289.0)) * 289.0; }}
fn permute(x: vec3<f32>) -> vec3<f32> {{ return mod289_3((x * 34.0 + 10.0) * x); }}

fn simplex2d(v: vec2<f32>) -> f32 {{
    let C = vec4<f32>(0.211324865405187, 0.366025403784439, -0.577350269189626, 0.024390243902439);
    var i = floor(v + dot(v, C.yy));
    let x0 = v - i + dot(i, C.xx);
    var i1: vec2<f32>;
    if x0.x > x0.y {{
        i1 = vec2<f32>(1.0, 0.0);
    }} else {{
        i1 = vec2<f32>(0.0, 1.0);
    }}
    let x12 = vec4<f32>(x0.xy + C.xx - i1, x0.xy + C.zz);
    i = mod289_2(i);
    let p = permute(permute(i.y + vec3<f32>(0.0, i1.y, 1.0)) + i.x + vec3<f32>(0.0, i1.x, 1.0));
    var m = max(vec3<f32>(0.5) - vec3<f32>(dot(x0, x0), dot(x12.xy, x12.xy), dot(x12.zw, x12.zw)), vec3<f32>(0.0));
    m = m * m;
    m = m * m;
    let x = 2.0 * fract(p * C.www) - 1.0;
    let h = abs(x) - 0.5;
    let ox = floor(x + 0.5);
    let a0 = x - ox;
    m = m * (1.79284291400159 - 0.85373472095314 * (a0 * a0 + h * h));
    let g0 = a0.x * x0.x + h.x * x0.y;
    let g1 = a0.y * x12.x + h.y * x12.y;
    let g2 = a0.z * x12.z + h.z * x12.w;
    return 130.0 * dot(m, vec3<f32>(g0, g1, g2));
}}

fn fbm(p: vec2<f32>, octaves: i32, speed: f32) -> f32 {{
    var val = 0.0;
    var amp = 0.5;
    var freq = 1.0;
    var pos = p;
    for (var i = 0; i < octaves; i = i + 1) {{
        val = val + amp * simplex2d(pos * freq + vec2<f32>(u.time * speed * freq * 0.1, u.time * speed * freq * 0.07));
        amp = amp * 0.5;
        freq = freq * 2.0;
    }}
    return val;
}}

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {{
    // Full-screen triangle
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 3.0,  1.0),
    );
    let p = positions[idx];
    return vec4<f32>(p.x, p.y, 0.0, 1.0);
}}

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {{
    let uv = frag_coord.xy / u.resolution;

    // Composite system load drives turbulence and heat bias
    let load = max(max(u.cpu_load, u.gpu_load), u.mem_load);
    let avg_load = (u.cpu_load + u.gpu_load + u.mem_load) / 3.0;

    // Animation speed: slow drift at idle, fast turbulence under load
    let speed = mix(0.3, 2.5, load);

    // Noise octaves: more detail under high load
    let octaves = select(4, 6, load > 0.5);

    // Spatial frequency: tighter patterns when hot
    let freq = mix(1.5, 4.0, avg_load);

    // Multi-layer noise for organic feel
    let n1 = fbm(uv * freq, octaves, speed);
    let n2 = fbm(uv * freq * 0.7 + vec2<f32>(5.3, 1.7), max(octaves - 1, 2), speed * 0.8);
    let n3 = simplex2d(uv * freq * 0.3 + vec2<f32>(u.time * 0.05, u.time * 0.03));

    // Combine noise layers
    var heat = (n1 * 0.5 + n2 * 0.3 + n3 * 0.2) * 0.5 + 0.5;

    // Bias toward hot colors under load
    heat = mix(heat * 0.5, heat * 0.5 + 0.5 * avg_load, avg_load);

    // Clamp to valid LUT range
    heat = clamp(heat, 0.0, 1.0);

    // Sample thermal gradient
    let color = thermal_sample(heat);

    // Slight vignette darkening at edges
    let center = uv - vec2<f32>(0.5);
    let vignette = 1.0 - dot(center, center) * 0.5;

    return vec4<f32>(color * vignette, 1.0);
}}
"#
    )
}

// ── Uniforms ────────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    time: f32,
    cpu_load: f32,
    gpu_load: f32,
    mem_load: f32,
    resolution: [f32; 2],
    _pad: [f32; 2],
}

// ── Render pipeline ─────────────────────────────────────────────────────────

struct WallpaperPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    start: Instant,
}

impl WallpaperPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, shader_src: &str) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("thermal_wallpaper"),
            source: wgpu::ShaderSource::Wgsl(shader_src.into()),
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wallpaper_uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wallpaper_bgl"),
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
            label: Some("wallpaper_bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wallpaper_layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("wallpaper_pipeline"),
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
                    blend: None,
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

    fn update_uniforms(
        &self,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        load: &SystemLoad,
    ) {
        let uniforms = Uniforms {
            time: self.start.elapsed().as_secs_f32(),
            cpu_load: load.cpu,
            gpu_load: load.gpu,
            mem_load: load.mem,
            resolution: [width as f32, height as f32],
            _pad: [0.0; 2],
        };
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::cast_slice(&[uniforms]));
    }
}

// ── Wayland state ───────────────────────────────────────────────────────────

struct WallpaperState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    layer: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
    exit: bool,
}

impl CompositorHandler for WallpaperState {
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

impl OutputHandler for WallpaperState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
    }
}

impl LayerShellHandler for WallpaperState {
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
        if configure.new_size.1 != 0 {
            self.height = configure.new_size.1;
        }

        debug!(
            width = self.width,
            height = self.height,
            "wallpaper surface configured"
        );

        if !self.configured {
            self.configured = true;
            self.layer.wl_surface().commit();
        }
    }
}

impl SeatHandler for WallpaperState {
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

// ── sctk delegate macros ────────────────────────────────────────────────────

delegate_compositor!(WallpaperState);
delegate_output!(WallpaperState);
delegate_seat!(WallpaperState);
delegate_layer!(WallpaperState);
delegate_registry!(WallpaperState);

impl ProvidesRegistryState for WallpaperState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("thermal-wallpaper starting");

    // ── Connect to Wayland ──────────────────────────────────────────────
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wl_compositor not available: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| anyhow::anyhow!("wlr-layer-shell not available: {e}"))?;

    // Create background layer surface anchored to all edges.
    let wl_surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        wl_surface,
        Layer::Background,
        Some("thermal-wallpaper"),
        None, // primary output
    );

    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1); // no exclusive zone — cover everything
    layer.set_size(0, 0); // stretch to full output
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();

    let mut state = WallpaperState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        layer,
        width: 1920,
        height: 1080,
        configured: false,
        exit: false,
    };

    info!("waiting for compositor configure");

    // Block until first configure.
    while !state.configured {
        event_queue.blocking_dispatch(&mut state)?;
        if state.exit {
            info!("exit before configure");
            return Ok(());
        }
    }

    // ── Initialize wgpu ─────────────────────────────────────────────────
    let display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
    let surface_ptr = state
        .layer
        .wl_surface()
        .id()
        .as_ptr()
        .cast::<std::ffi::c_void>() as *mut std::ffi::c_void;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        ..Default::default()
    });

    let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        NonNull::new(display_ptr).ok_or_else(|| anyhow::anyhow!("null wl_display"))?,
    ));
    let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
        NonNull::new(surface_ptr).ok_or_else(|| anyhow::anyhow!("null wl_surface"))?,
    ));

    let wgpu_surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: raw_display,
            raw_window_handle: raw_window,
        })?
    };

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: Some(&wgpu_surface),
        power_preference: wgpu::PowerPreference::LowPower,
        ..Default::default()
    }))
    .ok_or_else(|| anyhow::anyhow!("no compatible wgpu adapter"))?;

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("thermal-wallpaper"),
            ..Default::default()
        },
        None,
    ))?;

    let surface_format = wgpu::TextureFormat::Bgra8Unorm;
    let mut surface_config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width: state.width,
        height: state.height,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: wgpu::CompositeAlphaMode::Opaque,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    wgpu_surface.configure(&device, &surface_config);

    // Build shader with embedded thermal LUT.
    let shader_src = build_shader_source();
    let wp_pipeline = WallpaperPipeline::new(&device, surface_format, &shader_src);

    info!(
        width = state.width,
        height = state.height,
        "renderer initialized, entering render loop (~30fps)"
    );

    // ── Render loop ─────────────────────────────────────────────────────
    let frame_duration = Duration::from_millis(33); // ~30fps
    let metrics_interval = Duration::from_secs(1);
    let mut last_metrics_poll = Instant::now() - metrics_interval; // force immediate first poll
    let mut load = SystemLoad { cpu: 0.0, gpu: 0.0, mem: 0.0 };

    // Seed CPU delta computation.
    let _ = read_cpu_load();

    loop {
        let frame_start = Instant::now();

        // Dispatch Wayland events (non-blocking).
        event_queue.dispatch_pending(&mut state)?;
        conn.flush()?;
        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read();
            event_queue.dispatch_pending(&mut state)?;
        }

        if state.exit {
            info!("exit requested");
            break;
        }

        // Poll system metrics every second.
        if last_metrics_poll.elapsed() >= metrics_interval {
            load = SystemLoad::poll();
            last_metrics_poll = Instant::now();
            debug!(cpu = load.cpu, gpu = load.gpu, mem = load.mem, "metrics");
        }

        // Handle resize.
        if surface_config.width != state.width || surface_config.height != state.height {
            surface_config.width = state.width;
            surface_config.height = state.height;
            wgpu_surface.configure(&device, &surface_config);
            info!(
                width = state.width,
                height = state.height,
                "surface resized"
            );
        }

        // Update uniforms.
        wp_pipeline.update_uniforms(&queue, state.width, state.height, &load);

        // Acquire surface texture and render.
        let surface_texture = match wgpu_surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                wgpu_surface.configure(&device, &surface_config);
                continue;
            }
            Err(e) => {
                warn!("surface texture error: {:?}", e);
                continue;
            }
        };

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("wallpaper_frame"),
        });

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("wallpaper_pass"),
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
            rpass.set_pipeline(&wp_pipeline.pipeline);
            rpass.set_bind_group(0, &wp_pipeline.bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }

        // Request frame callback before present.
        {
            let wl_surf = state.layer.wl_surface();
            wl_surf.frame(&qh, wl_surf.clone());
        }

        queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();

        // Sleep to maintain ~30fps.
        let elapsed = frame_start.elapsed();
        if elapsed < frame_duration {
            std::thread::sleep(frame_duration - elapsed);
        }
    }

    info!("thermal-wallpaper exiting");
    Ok(())
}
