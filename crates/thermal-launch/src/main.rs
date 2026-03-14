use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphonColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_registry,
    delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_surface},
    Connection, Proxy, QueueHandle,
};

use bytemuck::{Pod, Zeroable};
use clap::Parser;
use std::ptr::NonNull;
use thermal_core::ThermalPalette;

pub mod desktop;
use desktop::{DesktopEntry, fuzzy_filter};

const WIDTH: u32 = 700;
const HEIGHT: u32 = 500;

/// Thermal-launch — fuzzy app launcher with targeting reticle UI.
#[derive(Parser, Debug)]
#[command(name = "thermal-launch", version, about)]
struct Args {
    /// Start hidden; the surface becomes visible on a D-Bus show signal.
    #[arg(long)]
    hidden: bool,
}

fn main() {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    tracing::info!(
        "thermal-launch v{} starting (hidden={})",
        env!("CARGO_PKG_VERSION"),
        args.hidden,
    );

    if args.hidden {
        tracing::info!("--hidden flag set; D-Bus signal handler is a stub — showing immediately");
    }

    // Load desktop entries at startup
    let entries = desktop::load_desktop_entries();
    tracing::info!("Loaded {} desktop entries", entries.len());

    // Connect to the Wayland display
    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland display");

    // Enumerate globals
    let (globals, mut event_queue) = registry_queue_init(&conn).expect("Failed to init registry");
    let qh = event_queue.handle();

    // Bind the compositor
    let compositor =
        CompositorState::bind(&globals, &qh).expect("wl_compositor is not available");

    // Bind the wlr-layer-shell
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell is not available");

    // Create a wl_surface
    let surface = compositor.create_surface(&qh);

    // Create layer surface on the OVERLAY layer with keyboard interactivity exclusive
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("thermal-launch"),
        None,
    );

    // Configure: center anchor, fixed 700x500 size, exclusive keyboard
    layer.set_anchor(Anchor::empty());
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(WIDTH, HEIGHT);

    // Initial commit — compositor will respond with a configure event
    layer.commit();

    // ── wgpu setup ───────────────────────────────────────────────────────────

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        ..Default::default()
    });

    let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        NonNull::new(conn.backend().display_ptr() as *mut _)
            .expect("Wayland display ptr is null"),
    ));
    let raw_window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
        NonNull::new(layer.wl_surface().id().as_ptr().cast::<std::ffi::c_void>())
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
        width: WIDTH,
        height: HEIGHT,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: wgpu::CompositeAlphaMode::Auto,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    wgpu_surface.configure(&device, &surface_config);

    // ── glyphon text renderer ─────────────────────────────────────────────────

    let font_system = FontSystem::new();
    let swash_cache = SwashCache::new();
    let glyphon_cache = Cache::new(&device);
    let mut viewport = Viewport::new(&device, &glyphon_cache);
    viewport.update(&queue, Resolution { width: WIDTH, height: HEIGHT });
    let mut atlas = TextAtlas::new(&device, &queue, &glyphon_cache, surface_format);
    let text_renderer =
        TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

    let text_state = TextState {
        font_system,
        swash_cache,
        atlas,
        viewport,
        renderer: text_renderer,
    };

    // ── Reticle pipeline ─────────────────────────────────────────────────────

    let reticle_pipeline = ReticlePipeline::new(&device, surface_format);

    // ── Launcher setup ───────────────────────────────────────────────────────

    let launcher_state = LauncherState::new(entries);

    let mut launcher = LauncherSurface {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        layer,
        configured: false,
        exit: false,
        dismiss_mode: DismissMode::Escape,
        keyboard: None,
        state: launcher_state,
        wgpu: WgpuState {
            device,
            queue,
            surface: wgpu_surface,
            config: surface_config,
        },
        text: text_state,
        reticle: reticle_pipeline,
        cached_query_buf: None,
        cached_query: String::new(),
        cached_entry_bufs: Vec::new(),
        cached_results: Vec::new(),
        cached_selected: 0,
    };

    // Event loop
    loop {
        event_queue
            .blocking_dispatch(&mut launcher)
            .expect("Wayland event dispatch failed");

        if launcher.configured {
            launcher.render_frame();
        }

        if launcher.exit {
            match launcher.dismiss_mode {
                DismissMode::Launch => {
                    launcher.layer.set_size(0, 0);
                    launcher.layer.wl_surface().commit();
                    tracing::info!("Launcher hidden after launch");
                }
                DismissMode::Escape => {
                    launcher.layer.wl_surface().destroy();
                    tracing::info!("Launcher destroyed on Escape");
                }
            }
            tracing::info!("thermal-launch exiting");
            break;
        }
    }
}

// ── wgpu state ────────────────────────────────────────────────────────────────

struct WgpuState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    #[allow(dead_code)]
    config: wgpu::SurfaceConfiguration,
}

// ── Text state ────────────────────────────────────────────────────────────────

struct TextState {
    font_system: FontSystem,
    swash_cache: SwashCache,
    atlas: TextAtlas,
    viewport: Viewport,
    renderer: TextRenderer,
}

// ── Reticle pipeline ──────────────────────────────────────────────────────────

/// A simple colored-quad pipeline for drawing the targeting reticle.
///
/// The reticle is drawn as 8 quads (2 per L-bracket corner, 4 corners total):
/// one horizontal bar and one vertical bar per corner, in ACCENT_HOT red.
struct ReticlePipeline {
    pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
}

/// Vertex for the reticle: NDC position + RGBA color.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct ReticleVertex {
    position: [f32; 2],
    color: [f32; 4],
}

impl ReticleVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x4
    ];

    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ReticleVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

impl ReticlePipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("reticle shader"),
            source: wgpu::ShaderSource::Wgsl(RETICLE_SHADER.into()),
        });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("reticle layout"),
                bind_group_layouts: &[],
                push_constant_ranges: &[],
            });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("reticle pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[ReticleVertex::desc()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
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

        // Pre-allocate buffer for 8 quads × 6 vertices each = 48 vertices max
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("reticle verts"),
            size: (48 * std::mem::size_of::<ReticleVertex>()) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self { pipeline, vertex_buffer }
    }

    /// Build vertices for 4 L-bracket corners around a row bounding box.
    ///
    /// `x0`, `y0` are top-left, `x1`, `y1` are bottom-right (in pixels).
    /// Arm length = 12px, thickness = 2px. Returns a flat list of quads
    /// (each quad = 6 vertices for 2 triangles) in NDC coordinates.
    fn build_reticle_verts(
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        w: f32,
        h: f32,
        color: [f32; 4],
    ) -> Vec<ReticleVertex> {
        const ARM: f32 = 12.0;
        const THICK: f32 = 2.0;

        // 4 corners, each has 2 bars (horizontal + vertical) = 8 quads
        // Corners: TL, TR, BR, BL
        let corners: [(f32, f32, f32, f32, f32, f32, f32, f32); 4] = [
            // Top-left: horiz bar going right, vert bar going down
            (x0, y0, x0 + ARM, y0 + THICK, x0, y0, x0 + THICK, y0 + ARM),
            // Top-right: horiz bar going left, vert bar going down
            (x1 - ARM, y0, x1, y0 + THICK, x1 - THICK, y0, x1, y0 + ARM),
            // Bottom-right: horiz bar going left, vert bar going up
            (x1 - ARM, y1 - THICK, x1, y1, x1 - THICK, y1 - ARM, x1, y1),
            // Bottom-left: horiz bar going right, vert bar going up
            (x0, y1 - THICK, x0 + ARM, y1, x0, y1 - ARM, x0 + THICK, y1),
        ];

        let mut verts = Vec::with_capacity(8 * 6);

        for (hx0, hy0, hx1, hy1, vx0, vy0, vx1, vy1) in corners {
            // Horizontal quad
            verts.extend_from_slice(&quad_verts(hx0, hy0, hx1, hy1, w, h, color));
            // Vertical quad
            verts.extend_from_slice(&quad_verts(vx0, vy0, vx1, vy1, w, h, color));
        }

        verts
    }
}

/// Convert a pixel-space rect to 6 NDC vertices (2 triangles = 1 quad).
fn quad_verts(
    px0: f32, py0: f32, px1: f32, py1: f32,
    w: f32, h: f32,
    color: [f32; 4],
) -> [ReticleVertex; 6] {
    let to_ndc = |px: f32, py: f32| -> [f32; 2] {
        [px / w * 2.0 - 1.0, -(py / h * 2.0 - 1.0)]
    };
    let tl = to_ndc(px0, py0);
    let tr = to_ndc(px1, py0);
    let bl = to_ndc(px0, py1);
    let br = to_ndc(px1, py1);

    [
        ReticleVertex { position: tl, color },
        ReticleVertex { position: tr, color },
        ReticleVertex { position: bl, color },
        ReticleVertex { position: tr, color },
        ReticleVertex { position: br, color },
        ReticleVertex { position: bl, color },
    ]
}

const RETICLE_SHADER: &str = r#"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

// ── Launcher application state ────────────────────────────────────────────────

struct LauncherState {
    all_entries: Vec<DesktopEntry>,
    query: String,
    results: Vec<usize>,
    selected: usize,
}

impl LauncherState {
    fn new(all_entries: Vec<DesktopEntry>) -> Self {
        let initial_results: Vec<usize> =
            all_entries.iter().take(8).enumerate().map(|(i, _)| i).collect();
        Self {
            all_entries,
            query: String::new(),
            results: initial_results,
            selected: 0,
        }
    }

    fn update_results(&mut self) {
        let matches = fuzzy_filter(&self.all_entries, &self.query);
        self.results = matches
            .iter()
            .filter_map(|(_, entry)| {
                self.all_entries.iter().position(|e| std::ptr::eq(*entry, e))
            })
            .collect();
        if self.selected >= self.results.len() {
            self.selected = self.results.len().saturating_sub(1);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DismissMode {
    Launch,
    Escape,
}

// ── Main surface struct ───────────────────────────────────────────────────────

struct LauncherSurface {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    layer: LayerSurface,
    configured: bool,
    exit: bool,
    dismiss_mode: DismissMode,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    state: LauncherState,
    wgpu: WgpuState,
    text: TextState,
    reticle: ReticlePipeline,
    // ── Text buffer cache ────────────────────────────────────────────────────
    /// Cached glyphon buffer for the query line — rebuilt only when query changes.
    cached_query_buf: Option<Buffer>,
    /// Last query string used to build cached_query_buf.
    cached_query: String,
    /// Cached glyphon buffers for the result entry rows.
    cached_entry_bufs: Vec<Buffer>,
    /// Last result set (indices) used to build cached_entry_bufs.
    cached_results: Vec<usize>,
    /// Last selected index used to build cached_entry_bufs (selection changes color).
    cached_selected: usize,
}

impl LauncherSurface {
    /// Render the launcher UI: background clear, text list, targeting reticle.
    fn render_frame(&mut self) {
        let output = match self.wgpu.surface.get_current_texture() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("Failed to acquire surface texture: {}", e);
                return;
            }
        };

        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.wgpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("thermal-launch frame"),
            });

        // Layout constants
        const PADDING_X: f32 = 30.0;
        const QUERY_Y: f32 = 30.0;
        const ENTRY_START_Y: f32 = 80.0;
        const ROW_HEIGHT: f32 = 40.0;
        const FONT_SIZE: f32 = 18.0;
        const LINE_HEIGHT: f32 = 22.0;

        // ── Build text buffers (cached) ──────────────────────────────────────
        //
        // Buffers are only rebuilt when the query, results, or selection change.
        // At ~60fps this eliminates ~540 glyphon allocations/sec.

        let warm = ThermalPalette::WARM;
        let warm_color = GlyphonColor::rgba(
            (warm[0] * 255.0) as u8,
            (warm[1] * 255.0) as u8,
            (warm[2] * 255.0) as u8,
            255,
        );

        let query_text = if self.state.query.is_empty() {
            "Type to search...".to_string()
        } else {
            format!("> {}", self.state.query)
        };

        // Rebuild query buffer only when the query string changed.
        if self.cached_query != self.state.query {
            let mut buf = Buffer::new(
                &mut self.text.font_system,
                Metrics::new(FONT_SIZE, LINE_HEIGHT),
            );
            buf.set_size(
                &mut self.text.font_system,
                Some(WIDTH as f32 - PADDING_X * 2.0),
                None,
            );
            buf.set_text(
                &mut self.text.font_system,
                &query_text,
                Attrs::new().color(warm_color).family(Family::Monospace),
                Shaping::Advanced,
            );
            buf.shape_until_scroll(&mut self.text.font_system, false);
            self.cached_query_buf = Some(buf);
            self.cached_query = self.state.query.clone();
        }

        // Rebuild entry buffers when results list or selection changes.
        let entries_dirty = self.cached_results != self.state.results
            || self.cached_selected != self.state.selected;
        if entries_dirty {
            self.cached_entry_bufs.clear();
            for (i, &entry_idx) in self.state.results.iter().enumerate() {
                let entry = &self.state.all_entries[entry_idx];
                let is_selected = i == self.state.selected;

                let color_arr = if is_selected {
                    ThermalPalette::WHITE_HOT
                } else {
                    ThermalPalette::TEXT
                };
                let color = GlyphonColor::rgba(
                    (color_arr[0] * 255.0) as u8,
                    (color_arr[1] * 255.0) as u8,
                    (color_arr[2] * 255.0) as u8,
                    255,
                );

                let mut buf = Buffer::new(
                    &mut self.text.font_system,
                    Metrics::new(FONT_SIZE, LINE_HEIGHT),
                );
                buf.set_size(
                    &mut self.text.font_system,
                    Some(WIDTH as f32 - PADDING_X * 2.0),
                    None,
                );
                buf.set_text(
                    &mut self.text.font_system,
                    &entry.name,
                    Attrs::new().color(color).family(Family::SansSerif),
                    Shaping::Advanced,
                );
                buf.shape_until_scroll(&mut self.text.font_system, false);
                self.cached_entry_bufs.push(buf);
            }
            self.cached_results = self.state.results.clone();
            self.cached_selected = self.state.selected;
        }

        // Build text areas referencing the cached buffers (which live in self).
        let mut text_areas: Vec<TextArea> = Vec::new();

        // Query text area
        if let Some(query_buf) = &self.cached_query_buf {
            let query_top = QUERY_Y as i32;
            text_areas.push(TextArea {
                buffer: query_buf,
                left: PADDING_X,
                top: QUERY_Y,
                scale: 1.0,
                bounds: TextBounds {
                    left: PADDING_X as i32,
                    top: query_top,
                    right: (WIDTH as f32 - PADDING_X) as i32,
                    bottom: (QUERY_Y + LINE_HEIGHT) as i32,
                },
                default_color: warm_color,
                custom_glyphs: &[],
            });
        }

        for (i, buf) in self.cached_entry_bufs.iter().enumerate() {
            let row_y = ENTRY_START_Y + i as f32 * ROW_HEIGHT;
            let color_arr = if i == self.cached_selected {
                ThermalPalette::WHITE_HOT
            } else {
                ThermalPalette::TEXT
            };
            let color = GlyphonColor::rgba(
                (color_arr[0] * 255.0) as u8,
                (color_arr[1] * 255.0) as u8,
                (color_arr[2] * 255.0) as u8,
                255,
            );
            text_areas.push(TextArea {
                buffer: buf,
                left: PADDING_X,
                top: row_y,
                scale: 1.0,
                bounds: TextBounds {
                    left: PADDING_X as i32,
                    top: row_y as i32,
                    right: (WIDTH as f32 - PADDING_X) as i32,
                    bottom: (row_y + ROW_HEIGHT) as i32,
                },
                default_color: color,
                custom_glyphs: &[],
            });
        }

        self.text.viewport.update(
            &self.wgpu.queue,
            Resolution { width: WIDTH, height: HEIGHT },
        );

        if let Err(e) = self.text.renderer.prepare(
            &self.wgpu.device,
            &self.wgpu.queue,
            &mut self.text.font_system,
            &mut self.text.atlas,
            &self.text.viewport,
            text_areas,
            &mut self.text.swash_cache,
        ) {
            tracing::warn!("glyphon prepare failed: {}", e);
        }

        // ── Build reticle verts for selected row ──────────────────────────────

        let selected_row_y = ENTRY_START_Y + self.state.selected as f32 * ROW_HEIGHT;
        let rx0 = PADDING_X - 8.0;
        let ry0 = selected_row_y - 4.0;
        let rx1 = WIDTH as f32 - PADDING_X + 8.0;
        let ry1 = selected_row_y + ROW_HEIGHT + 4.0;
        let hot = ThermalPalette::ACCENT_HOT;

        let verts = if !self.state.results.is_empty() {
            ReticlePipeline::build_reticle_verts(
                rx0, ry0, rx1, ry1,
                WIDTH as f32, HEIGHT as f32,
                hot,
            )
        } else {
            vec![]
        };

        // Upload reticle verts
        if !verts.is_empty() {
            self.wgpu.queue.write_buffer(
                &self.reticle.vertex_buffer,
                0,
                bytemuck::cast_slice(&verts),
            );
        }

        // ── Record render pass ────────────────────────────────────────────────

        let bg = ThermalPalette::BG;
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("launch pass"),
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

            // Draw reticle quads
            if !verts.is_empty() {
                pass.set_pipeline(&self.reticle.pipeline);
                pass.set_vertex_buffer(0, self.reticle.vertex_buffer.slice(..));
                pass.draw(0..verts.len() as u32, 0..1);
            }

            // Draw text
            if let Err(e) = self.text.renderer.render(
                &self.text.atlas,
                &self.text.viewport,
                &mut pass,
            ) {
                tracing::warn!("glyphon render failed: {}", e);
            }
        }

        self.wgpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        self.text.atlas.trim();
    }
}

// ── Compositor handler ────────────────────────────────────────────────────────

impl CompositorHandler for LauncherSurface {
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

impl OutputHandler for LauncherSurface {
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

// ── Layer shell handler ───────────────────────────────────────────────────────

impl LayerShellHandler for LauncherSurface {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
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
        tracing::debug!("Layer surface configured");
    }
}

// ── Seat handler ──────────────────────────────────────────────────────────────

impl SeatHandler for LauncherSurface {
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
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

// ── Keyboard handler ──────────────────────────────────────────────────────────

impl KeyboardHandler for LauncherSurface {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let raw = event.raw_code;

        if raw == 1 || event.keysym == Keysym::Escape {
            tracing::info!("Escape — dismissing launcher");
            self.dismiss_mode = DismissMode::Escape;
            self.exit = true;
            return;
        }

        if raw == 14 || event.keysym == Keysym::BackSpace {
            self.state.query.pop();
            self.state.update_results();
            tracing::debug!("Query: {:?}", self.state.query);
            return;
        }

        if raw == 28 || event.keysym == Keysym::Return {
            if !self.state.results.is_empty() {
                let idx = self.state.results[self.state.selected];
                let exec = self.state.all_entries[idx].exec.clone();
                launch_app(&exec);
                self.dismiss_mode = DismissMode::Launch;
                self.exit = true;
            }
            return;
        }

        if raw == 103 || event.keysym == Keysym::Up {
            if self.state.selected > 0 {
                self.state.selected -= 1;
            }
            return;
        }

        if raw == 108 || event.keysym == Keysym::Down {
            let max = self.state.results.len().saturating_sub(1);
            if self.state.selected < max {
                self.state.selected += 1;
            }
            return;
        }

        if let Some(ch) = keysym_to_char(event.keysym) {
            self.state.query.push(ch);
            self.state.update_results();
            tracing::debug!("Query: {:?}", self.state.query);
        }
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
        _modifiers: Modifiers,
        _layout: u32,
    ) {
    }
}

// ── Helper functions ──────────────────────────────────────────────────────────

fn launch_app(exec: &str) {
    let mut cleaned = exec.to_string();
    for code in &["%f", "%F", "%u", "%U", "%d", "%D", "%n", "%N", "%i", "%c", "%k"] {
        cleaned = cleaned.replace(code, "");
    }
    let cleaned = cleaned.trim().to_string();
    tracing::info!("Launching: {:?}", cleaned);
    use std::process::{Command, Stdio};
    match Command::new("sh")
        .args(["-c", &cleaned])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => tracing::info!("Launched successfully"),
        Err(e) => tracing::error!("Failed to launch {:?}: {}", cleaned, e),
    }
}

fn keysym_to_char(keysym: Keysym) -> Option<char> {
    let raw: u32 = keysym.into();
    if (0x0020..=0x007e).contains(&raw) { char::from_u32(raw) } else { None }
}

// ── Delegate macros ───────────────────────────────────────────────────────────

delegate_compositor!(LauncherSurface);
delegate_output!(LauncherSurface);
delegate_seat!(LauncherSurface);
delegate_keyboard!(LauncherSurface);
delegate_layer!(LauncherSurface);
delegate_registry!(LauncherSurface);

impl ProvidesRegistryState for LauncherSurface {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
