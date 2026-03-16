use glyphon::{
    Attrs, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea, TextBounds,
};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        Capability, SeatHandler, SeatState,
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{slot::{SlotPool, Buffer as ShmBuffer}, Shm, ShmHandler},
};
use nix::libc;
use std::{ptr::NonNull, rc::Rc, time::{Instant, SystemTime, UNIX_EPOCH}};
use thermal_core::ThermalPalette;
use tracing::{info, warn, error};
use zeroize::Zeroizing;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
    Connection, Proxy, QueueHandle,
};

pub mod auth;

// ── WGSL heat-map shader ─────────────────────────────────────────────────────

const HEATMAP_SHADER: &str = r#"
struct TimeUniform {
    time: f32,
    width: f32,
    height: f32,
    _pad: f32,
}
@group(0) @binding(0)
var<uniform> u_time: TimeUniform;

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

fn thermal_color(t: f32) -> vec3<f32> {
    let cool      = vec3<f32>(0.118, 0.227, 0.541);
    let cold      = vec3<f32>(0.176, 0.106, 0.412);
    let mild      = vec3<f32>(0.051, 0.580, 0.533);
    let warm      = vec3<f32>(0.133, 0.773, 0.369);
    let hot       = vec3<f32>(0.918, 0.702, 0.031);
    let white_hot = vec3<f32>(0.996, 0.953, 0.780);
    if t < 0.2 {
        return mix(cool, cold, t / 0.2);
    } else if t < 0.4 {
        return mix(cold, mild, (t - 0.2) / 0.2);
    } else if t < 0.55 {
        return mix(mild, warm, (t - 0.4) / 0.15);
    } else if t < 0.7 {
        return mix(warm, hot, (t - 0.55) / 0.15);
    } else {
        return mix(hot, white_hot, clamp((t - 0.7) / 0.3, 0.0, 1.0));
    }
}

fn heat_noise(uv: vec2<f32>, t: f32) -> f32 {
    let p = uv * 4.0;
    var v = 0.0;
    v += sin(p.x * 1.3 + cos(p.y * 0.9 + t * 0.7)) * 0.5 + 0.5;
    v += sin(p.y * 1.1 + cos(p.x * 1.2 + t * 0.5)) * 0.5 + 0.5;
    v += sin((p.x + p.y) * 0.8 + t * 0.3) * 0.5 + 0.5;
    v += cos(length(p - vec2<f32>(2.0, 2.0)) * 1.5 - t * 0.6) * 0.5 + 0.5;
    return clamp(v / 4.0, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = frag_coord.xy / vec2<f32>(u_time.width, u_time.height);
    let t = u_time.time;
    let noise = heat_noise(uv, t);
    let color = thermal_color(noise) * 0.12;
    return vec4<f32>(color, 0.7);
}
"#;

// ── Heat-map rendering pipeline ───────────────────────────────────────────────

struct HeatmapPipeline {
    pipeline: wgpu::RenderPipeline,
    time_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    start: Instant,
}

impl HeatmapPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("heatmap"),
            source: wgpu::ShaderSource::Wgsl(HEATMAP_SHADER.into()),
        });

        let time_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("time_uniform"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("time_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("time_bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: time_buf.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("heatmap_layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("heatmap_pipeline"),
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

        Self { pipeline, time_buf, bind_group, start: Instant::now() }
    }

    fn update_time(&self, queue: &wgpu::Queue, width: u32, height: u32) {
        let elapsed = self.start.elapsed().as_secs_f32();
        let bytes: [f32; 4] = [elapsed, width as f32, height as f32, 0.0];
        queue.write_buffer(&self.time_buf, 0, bytemuck::cast_slice(&bytes));
    }

    fn elapsed_secs(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }
}

// ── Flash (failed auth) pipeline ─────────────────────────────────────────────

const FLASH_SHADER: &str = r#"
struct ColorUniform {
    color: vec4<f32>,
}
@group(0) @binding(0)
var<uniform> u_color: ColorUniform;

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

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return u_color.color;
}
"#;

fn make_flash_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::Buffer, wgpu::BindGroup) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("flash"),
        source: wgpu::ShaderSource::Wgsl(FLASH_SHADER.into()),
    });

    let color_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("flash_color"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("flash_bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("flash_bg"),
        layout: &bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: color_buf.as_entire_binding(),
        }],
    });

    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("flash_layout"),
        bind_group_layouts: &[&bgl],
        push_constant_ranges: &[],
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("flash_pipeline"),
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

    (pipeline, color_buf, bind_group)
}

// ── Auth state ────────────────────────────────────────────────────────────────

struct AuthState {
    password: Zeroizing<String>,
    failed: bool,
    shake_timer: f32,
    /// Count of consecutive failed authentication attempts (for rate limiting)
    failed_attempts: u32,
    /// Lockout until this instant; None means no lockout active
    lockout_until: Option<Instant>,
}

impl AuthState {
    fn new() -> Self {
        Self {
            password: Zeroizing::new(String::new()),
            failed: false,
            shake_timer: 0.0,
            failed_attempts: 0,
            lockout_until: None,
        }
    }

    fn masked(&self) -> String {
        "●".repeat(self.password.chars().count())
    }
}

// ── Colour helpers ────────────────────────────────────────────────────────────

fn palette_to_glyph(p: [f32; 4]) -> GlyphColor {
    GlyphColor::rgba(
        (p[0] * 255.0) as u8,
        (p[1] * 255.0) as u8,
        (p[2] * 255.0) as u8,
        (p[3] * 255.0) as u8,
    )
}

// ── Wgpu per-surface state ────────────────────────────────────────────────────

struct WgpuSurface {
    lock_surface: SessionLockSurface,
    wgpu_surface: wgpu::Surface<'static>,
    device: Rc<wgpu::Device>,
    queue: Rc<wgpu::Queue>,
    config: wgpu::SurfaceConfiguration,
    width: u32,
    height: u32,
    heatmap: HeatmapPipeline,
    text: thermal_core::ThermalTextRenderer,
    // Text buffers for UI elements
    buf_time: glyphon::Buffer,
    buf_date: glyphon::Buffer,
    buf_label: glyphon::Buffer,
    buf_prompt: glyphon::Buffer,
    buf_masked: glyphon::Buffer,
    buf_denied: glyphon::Buffer,
    last_second: u64,
    last_frame: Instant,
    /// Solid-color flash pipeline for failed-auth feedback
    flash_pipeline: wgpu::RenderPipeline,
    flash_color_buf: wgpu::Buffer,
    flash_bind_group: wgpu::BindGroup,
}

impl WgpuSurface {
    fn update_clock(&mut self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        if now == self.last_second {
            return;
        }
        self.last_second = now;

        // Use libc localtime to get timezone-aware time
        let ts = now as i64;
        let (hours, mins, secs, year, month, day) = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&ts, &mut tm);
            (
                tm.tm_hour as u64,
                tm.tm_min as u64,
                tm.tm_sec as u64,
                (tm.tm_year + 1900) as u64,
                (tm.tm_mon + 1) as u64,
                tm.tm_mday as u64,
            )
        };
        let time_str = format!("{:02}:{:02}:{:02}", hours, mins, secs);
        let date_str = format!("{:04}-{:02}-{:02}", year, month, day);

        let warm = palette_to_glyph(ThermalPalette::WARM);
        let muted = palette_to_glyph(ThermalPalette::TEXT_MUTED);

        self.buf_time.set_text(
            &mut self.text.font_system,
            &time_str,
            Attrs::new().color(warm).family(Family::Monospace),
            Shaping::Basic,
        );
        self.buf_time.shape_until_scroll(&mut self.text.font_system, false);

        self.buf_date.set_text(
            &mut self.text.font_system,
            &date_str,
            Attrs::new().color(muted).family(Family::Monospace),
            Shaping::Basic,
        );
        self.buf_date.shape_until_scroll(&mut self.text.font_system, false);
    }

    fn render_frame(&mut self, auth: &AuthState) {
        // Compute delta time for shake animation
        let now = Instant::now();
        let delta = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;

        self.heatmap.update_time(&self.queue, self.width, self.height);
        self.update_clock();

        let elapsed = self.heatmap.elapsed_secs();
        let blink = (elapsed % 1.0) < 0.5;

        // Compute shake offset
        let shake_offset = if auth.shake_timer > 0.0 {
            (auth.shake_timer * 40.0).sin() * 8.0
        } else {
            0.0
        };

        let surface_texture = match self.wgpu_surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                info!("wgpu: surface lost/outdated, reconfiguring");
                self.wgpu_surface.configure(&self.device, &self.config);
                return;
            }
            Err(e) => {
                warn!("wgpu: failed to get surface texture: {:?}", e);
                return;
            }
        };

        let view =
            surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("thermal-lock frame"),
            });

        // ── Pass 1: heat-map background ───────────────────────────────────
        {
            let bg = ThermalPalette::BG;
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("heatmap_pass"),
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
            rpass.set_pipeline(&self.heatmap.pipeline);
            rpass.set_bind_group(0, &self.heatmap.bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }

        // ── Pass 2: text ──────────────────────────────────────────────────
        let w = self.width as f32;
        let h = self.height as f32;

        // Layout positions
        let time_y = h * 0.35;
        let date_y = h * 0.42;
        let label_y = h * 0.28;
        let prompt_y = h * 0.57;
        let masked_y = h * 0.60;
        let denied_y = h * 0.66;

        // Estimate text widths (rough: ~0.6 * font_size * char_count)
        let time_chars = 8usize; // HH:MM:SS
        let time_font = 72.0f32;
        let time_w = time_font * 0.6 * time_chars as f32;
        let time_x = (w - time_w) * 0.5;

        let masked_str = if blink {
            format!("{}|", auth.masked())
        } else {
            auth.masked()
        };
        let masked_font = 24.0f32;
        let masked_w = masked_font * 0.6 * masked_str.chars().count().max(1) as f32;
        let masked_x = (w - masked_w) * 0.5 + shake_offset;

        let prompt_str = "AUTHENTICATE";
        let prompt_font = 12.0f32;
        let prompt_w = prompt_font * 0.6 * prompt_str.len() as f32;
        let prompt_x = (w - prompt_w) * 0.5;

        let label_str = "THERMAL-LOCK";
        let label_font = 11.0f32;
        let label_w = label_font * 0.6 * label_str.len() as f32;
        let label_x = (w - label_w) * 0.5;

        let date_font = 20.0f32;
        let date_w = date_font * 0.6 * 10.0; // YYYY-MM-DD
        let date_x = (w - date_w) * 0.5;

        let denied_str = "ACCESS DENIED";
        let denied_font = 16.0f32;
        let denied_w = denied_font * 0.6 * denied_str.len() as f32;
        let denied_x = (w - denied_w) * 0.5;

        // Update masked buffer with blink cursor
        let warm = palette_to_glyph(ThermalPalette::WARM);
        self.buf_masked.set_text(
            &mut self.text.font_system,
            &masked_str,
            Attrs::new().color(warm).family(Family::Monospace),
            Shaping::Basic,
        );
        self.buf_masked.shape_until_scroll(&mut self.text.font_system, false);

        // Update viewport
        self.text.viewport.update(
            &self.queue,
            Resolution { width: self.width, height: self.height },
        );

        let iw = self.width as i32;
        let ih = self.height as i32;

        let mut text_areas: Vec<TextArea> = vec![
            TextArea {
                buffer: &self.buf_label,
                left: label_x,
                top: label_y,
                scale: 1.0,
                bounds: TextBounds { left: 0, top: 0, right: iw, bottom: ih },
                default_color: palette_to_glyph(ThermalPalette::ACCENT_COLD),
                custom_glyphs: &[],
            },
            TextArea {
                buffer: &self.buf_time,
                left: time_x,
                top: time_y,
                scale: 1.0,
                bounds: TextBounds { left: 0, top: 0, right: iw, bottom: ih },
                default_color: palette_to_glyph(ThermalPalette::WARM),
                custom_glyphs: &[],
            },
            TextArea {
                buffer: &self.buf_date,
                left: date_x,
                top: date_y,
                scale: 1.0,
                bounds: TextBounds { left: 0, top: 0, right: iw, bottom: ih },
                default_color: palette_to_glyph(ThermalPalette::TEXT_MUTED),
                custom_glyphs: &[],
            },
            TextArea {
                buffer: &self.buf_prompt,
                left: prompt_x,
                top: prompt_y,
                scale: 1.0,
                bounds: TextBounds { left: 0, top: 0, right: iw, bottom: ih },
                default_color: palette_to_glyph(ThermalPalette::TEXT_MUTED),
                custom_glyphs: &[],
            },
            TextArea {
                buffer: &self.buf_masked,
                left: masked_x,
                top: masked_y,
                scale: 1.0,
                bounds: TextBounds { left: 0, top: 0, right: iw, bottom: ih },
                default_color: palette_to_glyph(ThermalPalette::WARM),
                custom_glyphs: &[],
            },
        ];

        if auth.failed {
            text_areas.push(TextArea {
                buffer: &self.buf_denied,
                left: denied_x,
                top: denied_y,
                scale: 1.0,
                bounds: TextBounds { left: 0, top: 0, right: iw, bottom: ih },
                default_color: palette_to_glyph(ThermalPalette::CRITICAL),
                custom_glyphs: &[],
            });
        }

        if let Err(e) = self.text.renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.text.font_system,
            &mut self.text.atlas,
            &self.text.viewport,
            text_areas,
            &mut self.text.swash_cache,
        ) {
            warn!("glyphon prepare error: {:?}", e);
        }

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("text_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if let Err(e) =
                self.text.renderer.render(&self.text.atlas, &self.text.viewport, &mut rpass)
            {
                warn!("glyphon render error: {:?}", e);
            }
        }

        self.queue.submit(Some(encoder.finish()));

        // ── Pass 3: critical flash overlay (separate command, alpha blend) ─
        if auth.shake_timer > 0.0 {
            let crit = ThermalPalette::CRITICAL;
            let flash_color: [f32; 4] = [crit[0], crit[1], crit[2], 0.3];
            self.queue.write_buffer(
                &self.flash_color_buf,
                0,
                bytemuck::cast_slice(&flash_color),
            );
            let mut enc2 = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("flash_enc"),
            });
            {
                let mut rpass = enc2.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("flash_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                rpass.set_pipeline(&self.flash_pipeline);
                rpass.set_bind_group(0, &self.flash_bind_group, &[]);
                rpass.draw(0..3, 0..1);
            }
            self.queue.submit(Some(enc2.finish()));
        }

        surface_texture.present();
        self.text.atlas.trim();

        let _ = delta;
    }
}

// ── Application state ─────────────────────────────────────────────────────────

struct LockApp {
    compositor_state: CompositorState,
    output_state: OutputState,
    registry_state: RegistryState,
    shm: Shm,
    seat_state: SeatState,
    session_lock_state: SessionLockState,
    session_lock: Option<SessionLock>,
    pending_surfaces: Vec<SessionLockSurface>,
    wgpu_surfaces: Vec<WgpuSurface>,
    auth: AuthState,
    exit: bool,
    denied: bool,
    username: String,
    wgpu_instance: wgpu::Instance,
    shared_device: Rc<wgpu::Device>,
    shared_queue: Rc<wgpu::Queue>,
    display_ptr: *mut std::ffi::c_void,
    last_tick: Instant,
    shm_pool: SlotPool,
    /// SHM fallback buffers kept alive until wgpu takes over rendering
    fallback_buffers: Vec<ShmBuffer>,
}

/// Persistent log path that survives reboots (unlike /run tmpfs).
fn dirs() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = format!("{}/.local/state/thermal-lock", home);
    let _ = std::fs::create_dir_all(&dir);
    format!("{}/thermal-lock.log", dir)
}

fn main() {
    // Use a persistent log path that survives reboots (tmpfs at /run is cleared)
    let log_path = dirs();

    // Install a panic hook that logs to the log file instead of stderr
    let panic_log = log_path.clone();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_log)
        {
            use std::io::Write;
            let _ = writeln!(f, "PANIC: {}", info);
        }
        eprintln!("thermal-lock PANIC: {}", info);
    }));

    // catch_unwind so a panic doesn't crash the session lock protocol
    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run_lock)) {
        eprintln!("thermal-lock caught panic: {:?}", e);
    }
}

fn run_lock() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(|| {
            use std::fs::OpenOptions;
            let path = dirs();
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap_or_else(|_| OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("/tmp/thermal-lock.log")
                    .expect("cannot open any log file"))
        })
        .init();

    let username = auth::current_username();
    info!("thermal-lock starting for user: {}", username);
    info!("thermal-lock v{}", env!("CARGO_PKG_VERSION"));

    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland display");
    let display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;

    let (globals, mut event_queue) =
        registry_queue_init(&conn).expect("Failed to init registry queue");
    let qh: QueueHandle<LockApp> = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    let shm = Shm::bind(&globals, &qh).expect("wl_shm not available");
    let seat_state = SeatState::new(&globals, &qh);
    let session_lock_state = SessionLockState::new(&globals, &qh);

    // SHM pool for immediate fallback buffers in configure (satisfies protocol timeout)
    let shm_pool = SlotPool::new(3840 * 2160 * 4, &shm)
        .expect("Failed to create SHM slot pool");

    let wgpu_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        ..Default::default()
    });

    // Pre-create shared adapter+device so all surfaces use one Vulkan device.
    // NVIDIA fails with Device(Lost) if you create multiple devices.
    let adapter = pollster::block_on(wgpu_instance.request_adapter(
        &wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        },
    )).expect("No wgpu adapter available");
    info!("adapter: {:?}", adapter.get_info());

    let (device, queue) = pollster::block_on(
        adapter.request_device(&Default::default(), None),
    ).expect("Failed to create wgpu device");
    info!("wgpu device ready (shared)");

    let shared_device = Rc::new(device);
    let shared_queue = Rc::new(queue);

    let mut app = LockApp {
        compositor_state,
        output_state,
        registry_state,
        shm,
        seat_state,
        session_lock_state,
        session_lock: None,
        pending_surfaces: Vec::new(),
        wgpu_surfaces: Vec::new(),
        auth: AuthState::new(),
        exit: false,
        denied: false,
        username,
        wgpu_instance,
        shared_device,
        shared_queue,
        display_ptr,
        last_tick: Instant::now(),
        shm_pool,
        fallback_buffers: Vec::new(),
    };

    // Roundtrip to ensure all outputs are enumerated before we lock
    event_queue.roundtrip(&mut app).expect("pre-lock roundtrip failed");
    let output_count = app.output_state.outputs().count();
    info!("pre-lock: {} outputs detected", output_count);

    let session_lock = app.session_lock_state
        .lock(&qh)
        .expect("ext-session-lock-v1 not supported by compositor");

    // Create lock surfaces IMMEDIATELY after lock() — Hyprland only sends
    // the `locked` event after all surfaces have committed buffers, so we
    // cannot wait for the `locked` callback to create them (deadlock).
    for output in app.output_state.outputs() {
        let surface = app.compositor_state.create_surface(&qh);
        let lock_surface = session_lock.create_lock_surface(surface, &output, &qh);
        app.pending_surfaces.push(lock_surface);
    }
    info!("lock request sent, {} surfaces created", app.pending_surfaces.len());

    app.session_lock = Some(session_lock);

    loop {
        if let Err(e) = event_queue.blocking_dispatch(&mut app) {
            error!("Wayland event dispatch failed: {:?}", e);
            // Try to unlock gracefully so we don't leave the session stuck
            if let Some(lock) = &app.session_lock {
                lock.unlock();
            }
            break;
        }

        if app.denied {
            info!("lock was denied, exiting gracefully");
            break;
        }

        if app.exit {
            // Drop lock surfaces BEFORE the roundtrip so the compositor
            // sees both the unlock and surface destruction in the same
            // synchronisation window.  Without this, Hyprland may not
            // re-send wl_keyboard::enter to the previously focused surface,
            // leaving existing windows unable to receive keyboard input.
            app.wgpu_surfaces.clear();
            app.pending_surfaces.clear();

            // Two roundtrips: first lets the compositor process the surface
            // destruction + unlock, second ensures any resulting focus
            // events have been dispatched.
            let _ = event_queue.roundtrip(&mut app);
            let _ = event_queue.roundtrip(&mut app);
            break;
        }

        // Tick shake timer
        let now = Instant::now();
        let dt = now.duration_since(app.last_tick).as_secs_f32();
        app.last_tick = now;

        if app.auth.shake_timer > 0.0 {
            app.auth.shake_timer -= dt;
            if app.auth.shake_timer <= 0.0 {
                app.auth.shake_timer = 0.0;
                app.auth.failed = false;
                app.auth.password.clear();
            }
        }

        // Reset failed_attempts counter once lockout has expired
        if let Some(lockout_until) = app.auth.lockout_until {
            if Instant::now() >= lockout_until {
                app.auth.lockout_until = None;
                app.auth.failed_attempts = 0;
            }
        }

        // Snapshot auth state for rendering — use masked string, never clone the real password
        let masked_str = app.auth.masked();
        let auth_snapshot = AuthState {
            password: Zeroizing::new(masked_str),
            failed: app.auth.failed,
            shake_timer: app.auth.shake_timer,
            failed_attempts: app.auth.failed_attempts,
            lockout_until: app.auth.lockout_until,
        };
        for ws in &mut app.wgpu_surfaces {
            ws.render_frame(&auth_snapshot);
        }
    }
}

// ── SessionLockHandler ───────────────────────────────────────────────────────

impl SessionLockHandler for LockApp {
    fn locked(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session_lock: SessionLock) {
        info!("LOCKED — compositor confirmed lock");
        // Surfaces were already created in run_lock() right after lock().
        // Hyprland only sends `locked` after all surfaces commit buffers,
        // so creating surfaces here would deadlock.
        self.session_lock = Some(session_lock);
    }

    fn finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session_lock: SessionLock,
    ) {
        warn!("LOCK DENIED — compositor refused or timed out");
        self.denied = true;
    }

    fn configure(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        session_lock_surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        info!("configure: surface {}x{}", width, height);
        if width == 0 || height == 0 {
            return;
        }

        let surface_id = session_lock_surface.wl_surface().id();

        if let Some(pos) = self
            .pending_surfaces
            .iter()
            .position(|s| s.wl_surface().id() == surface_id)
        {
            // Immediately commit a solid-color SHM buffer so the compositor
            // sees content before GPU init (which can exceed lockdead_screen_delay).
            {
                let stride = (width * 4) as i32;
                match self.shm_pool.create_buffer(
                    width as i32, height as i32, stride, wl_shm::Format::Argb8888,
                ) {
                    Ok((buffer, canvas)) => {
                        let bg = ThermalPalette::BG;
                        let pixel: [u8; 4] = [
                            (bg[2] * 255.0) as u8, // B
                            (bg[1] * 255.0) as u8, // G
                            (bg[0] * 255.0) as u8, // R
                            (bg[3] * 255.0) as u8, // A
                        ];
                        for chunk in canvas.chunks_exact_mut(4) {
                            chunk.copy_from_slice(&pixel);
                        }
                        let wl_surf = session_lock_surface.wl_surface();
                        match buffer.attach_to(wl_surf) {
                            Ok(()) => {
                                wl_surf.damage_buffer(0, 0, width as i32, height as i32);
                                wl_surf.commit();
                                // Flush immediately so the compositor sees the buffer
                                // before we spend seconds on GPU init below.
                                if let Err(e) = conn.flush() {
                                    error!("Wayland flush after SHM commit failed: {:?}", e);
                                }
                                info!("SHM fallback committed+flushed for {}x{}", width, height);
                                self.fallback_buffers.push(buffer);
                            }
                            Err(e) => error!("SHM attach failed: {:?}", e),
                        }
                    }
                    Err(e) => error!("SHM fallback failed for {}x{}: {:?}", width, height, e),
                }
            }

            let lock_surface = self.pending_surfaces.remove(pos);
            let raw_surface_ptr: *mut std::ffi::c_void = lock_surface.wl_surface().id().as_ptr().cast();

            let wgpu_surface = unsafe {
                match self.wgpu_instance
                    .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: RawDisplayHandle::Wayland(
                            WaylandDisplayHandle::new(NonNull::new(self.display_ptr).unwrap()),
                        ),
                        raw_window_handle: RawWindowHandle::Wayland(
                            WaylandWindowHandle::new(NonNull::new(raw_surface_ptr).unwrap()),
                        ),
                    }) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("wgpu surface creation failed for {}x{}: {:?}", width, height, e);
                        // Put it back so the protocol isn't violated
                        self.pending_surfaces.push(lock_surface);
                        return;
                    }
                }
            };

            // Use the shared device+queue (NVIDIA can't create multiple VkDevices)
            let device = Rc::clone(&self.shared_device);
            let queue = Rc::clone(&self.shared_queue);

            let caps = wgpu_surface.get_capabilities(
                &pollster::block_on(self.wgpu_instance.request_adapter(
                    &wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::HighPerformance,
                        compatible_surface: Some(&wgpu_surface),
                        force_fallback_adapter: false,
                    },
                )).expect("no adapter for surface capabilities"),
            );
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
            wgpu_surface.configure(&device, &config);

            let heatmap = HeatmapPipeline::new(&device, format);
            let (flash_pipeline, flash_color_buf, flash_bind_group) =
                make_flash_pipeline(&device, format);
            let mut text = thermal_core::ThermalTextRenderer::new(&device, &queue, format, width, height);

            // Pre-create text buffers
            let warm = palette_to_glyph(ThermalPalette::WARM);
            let muted = palette_to_glyph(ThermalPalette::TEXT_MUTED);
            let cold_accent = palette_to_glyph(ThermalPalette::ACCENT_COLD);
            let critical = palette_to_glyph(ThermalPalette::CRITICAL);

            let mut buf_time = glyphon::Buffer::new(
                &mut text.font_system,
                Metrics::new(72.0, 86.0),
            );
            buf_time.set_size(&mut text.font_system, Some(width as f32), Some(height as f32));
            buf_time.set_text(&mut text.font_system, "00:00:00",
                Attrs::new().color(warm).family(Family::Monospace), Shaping::Basic);
            buf_time.shape_until_scroll(&mut text.font_system, false);

            let mut buf_date = glyphon::Buffer::new(
                &mut text.font_system,
                Metrics::new(20.0, 24.0),
            );
            buf_date.set_size(&mut text.font_system, Some(width as f32), Some(height as f32));
            buf_date.set_text(&mut text.font_system, "1970-01-01",
                Attrs::new().color(muted).family(Family::Monospace), Shaping::Basic);
            buf_date.shape_until_scroll(&mut text.font_system, false);

            let mut buf_label = glyphon::Buffer::new(
                &mut text.font_system,
                Metrics::new(11.0, 14.0),
            );
            buf_label.set_size(&mut text.font_system, Some(width as f32), Some(height as f32));
            buf_label.set_text(&mut text.font_system, "THERMAL-LOCK",
                Attrs::new().color(cold_accent).family(Family::Monospace), Shaping::Basic);
            buf_label.shape_until_scroll(&mut text.font_system, false);

            let mut buf_prompt = glyphon::Buffer::new(
                &mut text.font_system,
                Metrics::new(12.0, 15.0),
            );
            buf_prompt.set_size(&mut text.font_system, Some(width as f32), Some(height as f32));
            buf_prompt.set_text(&mut text.font_system, "AUTHENTICATE",
                Attrs::new().color(muted).family(Family::Monospace), Shaping::Basic);
            buf_prompt.shape_until_scroll(&mut text.font_system, false);

            let mut buf_masked = glyphon::Buffer::new(
                &mut text.font_system,
                Metrics::new(24.0, 30.0),
            );
            buf_masked.set_size(&mut text.font_system, Some(width as f32), Some(height as f32));
            buf_masked.set_text(&mut text.font_system, "|",
                Attrs::new().color(warm).family(Family::Monospace), Shaping::Basic);
            buf_masked.shape_until_scroll(&mut text.font_system, false);

            let mut buf_denied = glyphon::Buffer::new(
                &mut text.font_system,
                Metrics::new(16.0, 20.0),
            );
            buf_denied.set_size(&mut text.font_system, Some(width as f32), Some(height as f32));
            buf_denied.set_text(&mut text.font_system, "ACCESS DENIED",
                Attrs::new().color(critical).family(Family::Monospace), Shaping::Basic);
            buf_denied.shape_until_scroll(&mut text.font_system, false);

            self.wgpu_surfaces.push(WgpuSurface {
                lock_surface,
                wgpu_surface,
                device,
                queue,
                config,
                width,
                height,
                heatmap,
                text,
                buf_time,
                buf_date,
                buf_label,
                buf_prompt,
                buf_masked,
                buf_denied,
                last_second: 0,
                last_frame: Instant::now(),
                flash_pipeline,
                flash_color_buf,
                flash_bind_group,
            });

            // All pending surfaces initialized — SHM fallback buffers no longer needed.
            // Clearing them lets the compositor release the SHM memory once it
            // processes the next wgpu-committed frame.
            if self.pending_surfaces.is_empty() {
                self.fallback_buffers.clear();
            }
        } else if let Some(ws) = self
            .wgpu_surfaces
            .iter_mut()
            .find(|ws| ws.lock_surface.wl_surface().id() == surface_id)
        {
            ws.width = width;
            ws.height = height;
            ws.config.width = width;
            ws.config.height = height;
            ws.wgpu_surface.configure(&ws.device, &ws.config);
            ws.text.resize(&ws.queue, width, height);
        }
    }
}

// ── CompositorHandler ────────────────────────────────────────────────────────

impl CompositorHandler for LockApp {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

// ── OutputHandler ────────────────────────────────────────────────────────────

impl OutputHandler for LockApp {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

// ── ProvidesRegistryState ────────────────────────────────────────────────────

impl ProvidesRegistryState for LockApp {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState, SeatState];
}

// ── ShmHandler ───────────────────────────────────────────────────────────────

impl ShmHandler for LockApp {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

// ── SeatHandler ──────────────────────────────────────────────────────────────

impl SeatHandler for LockApp {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        // Request keyboard from this seat
        if let Err(e) = self.seat_state.get_keyboard(qh, &seat, None) {
            warn!("Could not get keyboard: {}", e);
        }
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Err(e) = self.seat_state.get_keyboard(qh, &seat, None) {
                warn!("Could not get keyboard on capability: {}", e);
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

// ── KeyboardHandler ──────────────────────────────────────────────────────────

impl KeyboardHandler for LockApp {
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
        let sym = event.keysym;

        // Check rate-limit lockout before processing any input
        if let Some(lockout_until) = self.auth.lockout_until {
            if Instant::now() < lockout_until {
                return; // Still locked out — ignore all key input
            } else {
                self.auth.lockout_until = None; // Lockout expired
            }
        }

        if sym == Keysym::new(xkeysym::key::BackSpace) {
            // Remove last character
            self.auth.password.pop();
        } else if sym == Keysym::new(xkeysym::key::Return)
            || sym == Keysym::new(xkeysym::key::KP_Enter)
        {
            // Attempt authentication
            let username = self.username.clone();
            let ok = auth::authenticate(&username, &self.auth.password);
            if ok {
                info!("Authentication successful — unlocking");
                if let Some(lock) = &self.session_lock {
                    lock.unlock();
                }
                self.exit = true;
            } else {
                warn!("Authentication failed");
                self.auth.failed = true;
                self.auth.shake_timer = 0.5;
                self.auth.failed_attempts += 1;
                self.auth.password.clear();
                // After 3 consecutive failures, impose a 2-second lockout
                if self.auth.failed_attempts >= 3 {
                    warn!("Rate limiting: {} failed attempts — 2s lockout", self.auth.failed_attempts);
                    self.auth.lockout_until = Some(Instant::now() + std::time::Duration::from_secs(2));
                }
            }
        } else if let Some(text) = event.utf8 {
            // Printable character — append to password
            for ch in text.chars() {
                if !ch.is_control() {
                    self.auth.password.push(ch);
                }
            }
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
        _modifiers: Modifiers,
        _layout: u32,
    ) {
    }
}

// ── Delegates ────────────────────────────────────────────────────────────────

smithay_client_toolkit::delegate_compositor!(LockApp);
smithay_client_toolkit::delegate_output!(LockApp);
smithay_client_toolkit::delegate_seat!(LockApp);
smithay_client_toolkit::delegate_keyboard!(LockApp);
smithay_client_toolkit::delegate_session_lock!(LockApp);
smithay_client_toolkit::delegate_shm!(LockApp);
smithay_client_toolkit::delegate_registry!(LockApp);
