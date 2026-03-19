/// wgpu rendering pipeline for thermal-hud.
///
/// Renders a horizontal tab strip showing per-agent status tabs with
/// session ID, current tool, status dot, and context % progress bar.
/// Adapted from thermal-bar's renderer.rs pattern.
use std::ptr::NonNull;

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use thermal_core::{ClaudeSessionState, ClaudeStatus, ThermalPalette};

use crate::voice::{HudMode, VoiceState, RESULT_DIM_SECS};
use wgpu::{
    BlendState, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites,
    CommandEncoderDescriptor, Device, FragmentState, FrontFace, Instance, InstanceDescriptor,
    LoadOp, MultisampleState, Operations, PipelineLayoutDescriptor, PolygonMode, PrimitiveState,
    PrimitiveTopology, Queue, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    RenderPipelineDescriptor, RequestAdapterOptions, StoreOp, Surface, SurfaceConfiguration,
    SurfaceTargetUnsafe, TextureFormat, TextureUsages, TextureViewDescriptor, VertexAttribute,
    VertexBufferLayout, VertexState, VertexStepMode,
};

// ---------------------------------------------------------------------------
// Vertex layout
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ColorVertex {
    position: [f32; 2],
    color: [f32; 4],
}

static RECT_VERTEX_ATTRS: &[VertexAttribute] = &[
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 8,
        shader_location: 1,
    },
];

fn rect_vertex_layout() -> VertexBufferLayout<'static> {
    VertexBufferLayout {
        array_stride: std::mem::size_of::<ColorVertex>() as u64,
        step_mode: VertexStepMode::Vertex,
        attributes: RECT_VERTEX_ATTRS,
    }
}

// ---------------------------------------------------------------------------
// WGSL shader source
// ---------------------------------------------------------------------------

const RECT_SHADER: &str = r#"
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

// ---------------------------------------------------------------------------
// Tab layout constants
// ---------------------------------------------------------------------------

/// Minimum tab width in pixels.
const TAB_MIN_WIDTH: f32 = 200.0;
/// Maximum tab width in pixels.
const TAB_MAX_WIDTH: f32 = 400.0;
/// Horizontal padding inside each tab.
const TAB_PADDING: f32 = 12.0;
/// Gap between tabs.
const TAB_GAP: f32 = 2.0;
/// Height of the context % progress bar at the bottom of each tab.
const CONTEXT_BAR_HEIGHT: f32 = 4.0;
/// Radius of the status dot (rendered as a small square).
const STATUS_DOT_SIZE: f32 = 8.0;
/// Left margin before first tab.
const LEFT_MARGIN: f32 = 8.0;

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

pub struct Renderer {
    device: Device,
    queue: Queue,
    surface: Surface<'static>,
    surface_config: SurfaceConfiguration,
    rect_pipeline: RenderPipeline,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,

    pub width: u32,
    pub height: u32,
}

impl Renderer {
    /// Create a new Renderer from raw Wayland display and surface pointers.
    pub async fn new_from_wayland(
        wl_display: *mut std::ffi::c_void,
        wl_surface: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        let instance = Instance::new(InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..Default::default()
        });

        let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(wl_display)
                .ok_or_else(|| anyhow::anyhow!("null wl_display pointer"))?,
        ));
        let raw_window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(wl_surface)
                .ok_or_else(|| anyhow::anyhow!("null wl_surface pointer"))?,
        ));

        let surface = unsafe {
            instance.create_surface_unsafe(SurfaceTargetUnsafe::RawHandle {
                raw_display_handle,
                raw_window_handle,
            })?
        };

        let adapter = instance
            .request_adapter(&RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("no compatible wgpu adapter found"))?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("thermal-hud"),
                    ..Default::default()
                },
                None,
            )
            .await?;

        let surface_format = TextureFormat::Bgra8Unorm;
        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        // Build the colored-rect pipeline.
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rect_shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("rect_pipeline_layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let rect_pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("rect_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[rect_vertex_layout()],
                compilation_options: Default::default(),
            },
            primitive: PrimitiveState {
                topology: PrimitiveTopology::TriangleList,
                front_face: FrontFace::Ccw,
                polygon_mode: PolygonMode::Fill,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: MultisampleState::default(),
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(ColorTargetState {
                    format: surface_format,
                    blend: Some(BlendState::ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });

        // Set up glyphon text rendering.
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, surface_format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);

        // Pre-warm font system.
        let mut warmup_buf = Buffer::new(&mut font_system, Metrics::new(14.0, 20.0));
        warmup_buf.set_size(&mut font_system, Some(width as f32), Some(height as f32));
        warmup_buf.set_text(
            &mut font_system,
            "THERMAL-HUD",
            Attrs::new().family(Family::Monospace),
            Shaping::Basic,
        );

        Ok(Self {
            device,
            queue,
            surface,
            surface_config,
            rect_pipeline,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            width,
            height,
        })
    }

    /// Resize the surface.
    pub fn resize(&mut self, new_width: u32, new_height: u32) {
        self.width = new_width;
        self.height = new_height;
        self.surface_config.width = new_width;
        self.surface_config.height = new_height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Render the tab strip for all active Claude sessions.
    ///
    /// Each tab shows:
    /// - Status dot (color-coded by ClaudeStatus)
    /// - Truncated session ID
    /// - Current tool name (if any)
    /// - Context % as a horizontal progress bar at the bottom
    ///
    /// Active tab gets a SEARING border, inactive tabs get COOL/DIM colors.
    pub fn render_tabs(
        &mut self,
        sessions: &[ClaudeSessionState],
        active_tab: usize,
    ) -> anyhow::Result<()> {
        let frame = self.surface.get_current_texture()?;
        let view = frame.texture.create_view(&TextureViewDescriptor::default());

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.width,
                height: self.height,
            },
        );

        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor { label: None });

        let mut rect_quads: Vec<([f32; 4], [f32; 4])> = Vec::new();
        let mut text_buffers: Vec<Buffer> = Vec::new();
        let mut text_placements: Vec<(usize, f32, f32, [f32; 4])> = Vec::new();

        let screen_w = self.width as f32;
        let screen_h = self.height as f32;

        if sessions.is_empty() {
            // No sessions — render a dim placeholder message.
            let placeholder = "THERMAL-HUD  no active sessions";
            let mut buf = Buffer::new(&mut self.font_system, Metrics::new(14.0, 20.0));
            buf.set_size(&mut self.font_system, Some(screen_w), Some(screen_h));
            buf.set_text(
                &mut self.font_system,
                placeholder,
                Attrs::new().family(Family::Monospace),
                Shaping::Basic,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            let idx = text_buffers.len();
            text_buffers.push(buf);
            text_placements.push((idx, LEFT_MARGIN, 14.0, ThermalPalette::TEXT_MUTED));
        } else {
            // Compute tab width: distribute evenly, clamped to min/max.
            let available = screen_w - LEFT_MARGIN * 2.0;
            let count = sessions.len() as f32;
            let tab_width = ((available - TAB_GAP * (count - 1.0)) / count)
                .clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH);

            for (i, session) in sessions.iter().enumerate() {
                let is_active = i == active_tab;
                let tab_x = LEFT_MARGIN + i as f32 * (tab_width + TAB_GAP);

                // Tab background.
                let bg_color = if is_active {
                    ThermalPalette::BG_SURFACE
                } else {
                    ThermalPalette::BG_LIGHT
                };
                rect_quads.push(([tab_x, 0.0, tab_width, screen_h], bg_color));

                // Active tab border (top 2px strip in SEARING).
                if is_active {
                    rect_quads.push((
                        [tab_x, 0.0, tab_width, 2.0],
                        ThermalPalette::SEARING,
                    ));
                }

                // Status dot — small square indicating session status.
                let dot_color = status_color(&session.status);
                let dot_x = tab_x + TAB_PADDING;
                let dot_y = (screen_h - CONTEXT_BAR_HEIGHT) / 2.0 - STATUS_DOT_SIZE / 2.0;
                rect_quads.push((
                    [dot_x, dot_y, STATUS_DOT_SIZE, STATUS_DOT_SIZE],
                    dot_color,
                ));

                // Build tab text: "session_id  ToolName"
                let session_label = truncate_session_id(&session.session_id, 12);
                let tool_label = session
                    .current_tool
                    .as_deref()
                    .unwrap_or("")
                    .to_string();
                let tab_text = if tool_label.is_empty() {
                    format!("{session_label}  {}", status_label(&session.status))
                } else {
                    format!("{session_label}  {tool_label}")
                };

                let text_x = dot_x + STATUS_DOT_SIZE + 6.0;
                let text_y = 6.0;
                let text_color = if is_active {
                    ThermalPalette::TEXT_BRIGHT
                } else {
                    ThermalPalette::TEXT
                };

                let mut buf = Buffer::new(&mut self.font_system, Metrics::new(14.0, 20.0));
                buf.set_size(
                    &mut self.font_system,
                    Some((tab_width - TAB_PADDING * 2.0 - STATUS_DOT_SIZE - 6.0).max(50.0)),
                    Some(screen_h - CONTEXT_BAR_HEIGHT),
                );
                buf.set_text(
                    &mut self.font_system,
                    &tab_text,
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                let idx = text_buffers.len();
                text_buffers.push(buf);
                text_placements.push((idx, text_x, text_y, text_color));

                // Context % secondary line.
                let ctx_pct = session.context_percent.unwrap_or(0.0);
                let ctx_text = format!("ctx {:.0}%", ctx_pct);
                let mut ctx_buf = Buffer::new(&mut self.font_system, Metrics::new(11.0, 14.0));
                ctx_buf.set_size(
                    &mut self.font_system,
                    Some((tab_width - TAB_PADDING * 2.0 - STATUS_DOT_SIZE - 6.0).max(50.0)),
                    Some(20.0),
                );
                ctx_buf.set_text(
                    &mut self.font_system,
                    &ctx_text,
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                ctx_buf.shape_until_scroll(&mut self.font_system, false);
                let ctx_idx = text_buffers.len();
                text_buffers.push(ctx_buf);
                text_placements.push((ctx_idx, text_x, 26.0, ThermalPalette::TEXT_MUTED));

                // Context % progress bar at the bottom of the tab.
                let bar_y = screen_h - CONTEXT_BAR_HEIGHT;
                let bar_width = (tab_width * (ctx_pct / 100.0).clamp(0.0, 1.0)).max(0.0);

                // Background track (dim).
                rect_quads.push((
                    [tab_x, bar_y, tab_width, CONTEXT_BAR_HEIGHT],
                    ThermalPalette::FREEZING,
                ));

                // Filled portion — color based on context usage.
                let bar_color = context_bar_color(ctx_pct);
                if bar_width > 0.0 {
                    rect_quads.push((
                        [tab_x, bar_y, bar_width, CONTEXT_BAR_HEIGHT],
                        bar_color,
                    ));
                }
            }
        }

        // Build vertex list for all rect quads.
        let mut rect_vertices: Vec<ColorVertex> = Vec::new();
        for (xywh, color) in &rect_quads {
            let verts = pixel_rect_to_ndc(
                xywh[0], xywh[1], xywh[2], xywh[3], screen_w, screen_h, *color,
            );
            rect_vertices.extend_from_slice(&verts);
        }

        let rect_vbuf = if !rect_vertices.is_empty() {
            let data = bytemuck::cast_slice::<ColorVertex, u8>(&rect_vertices);
            let buf = self.device.create_buffer(&BufferDescriptor {
                label: Some("hud_rect_vbuf"),
                size: data.len() as u64,
                usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&buf, 0, data);
            Some((buf, rect_vertices.len() as u32))
        } else {
            None
        };

        // Build glyphon TextAreas.
        let has_text = !text_buffers.is_empty();
        if has_text {
            let text_areas: Vec<TextArea<'_>> = text_placements
                .iter()
                .map(|(idx, x, y, color)| {
                    let [r, g, b, a] = color;
                    TextArea {
                        buffer: &text_buffers[*idx],
                        left: *x,
                        top: *y,
                        scale: 1.0,
                        bounds: TextBounds {
                            left: *x as i32,
                            top: 0,
                            right: self.width as i32,
                            bottom: self.height as i32,
                        },
                        default_color: GlyphColor::rgba(
                            (*r * 255.0) as u8,
                            (*g * 255.0) as u8,
                            (*b * 255.0) as u8,
                            (*a * 255.0) as u8,
                        ),
                        custom_glyphs: &[],
                    }
                })
                .collect();

            self.text_renderer.prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )?;
        }

        let bg = ThermalPalette::BG;
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("hud_pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: 1.0,
                        }),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // Draw background rects.
            if let Some((vbuf, count)) = &rect_vbuf {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, vbuf.slice(..));
                pass.draw(0..*count, 0..1);
            }

            // Render text.
            if has_text {
                self.text_renderer
                    .render(&self.atlas, &self.viewport, &mut pass)?;
            }
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.atlas.trim();

        Ok(())
    }

    /// Render the voice assistant UI state instead of tabs.
    ///
    /// Layout varies by voice state:
    /// - LISTENING:   pulsing "MIC" label in ACCENT_HOT
    /// - TRANSCRIBING / THINKING: partial transcript in TEXT_BRIGHT
    /// - CONFIRMING:  action text in ACCENT_WARM + "SAY YES/NO" label
    /// - EXECUTING:   action text + spinner-style label
    /// - RESULT:      summary in WARM, dimmed after RESULT_DIM_SECS
    pub fn render_voice_state(
        &mut self,
        mode: &HudMode,
        result_age_secs: Option<u64>,
    ) -> anyhow::Result<()> {
        let (transcript, voice_state) = match mode {
            HudMode::VoiceActive { transcript, state } => (transcript.as_str(), state),
            HudMode::AgentTabs => {
                // Should not be called in this mode, but handle gracefully.
                return Ok(());
            }
        };

        let frame = self.surface.get_current_texture()?;
        let view = frame.texture.create_view(&TextureViewDescriptor::default());

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.width,
                height: self.height,
            },
        );

        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor { label: None });

        let mut rect_quads: Vec<([f32; 4], [f32; 4])> = Vec::new();
        let mut text_buffers: Vec<Buffer> = Vec::new();
        let mut text_placements: Vec<(usize, f32, f32, [f32; 4])> = Vec::new();

        let screen_w = self.width as f32;
        let screen_h = self.height as f32;

        match voice_state {
            VoiceState::Listening => {
                // Pulsing "MIC" indicator — bright ACCENT_HOT block + label.
                let mic_width = 80.0;
                let mic_x = LEFT_MARGIN;
                rect_quads.push(([mic_x, 4.0, mic_width, screen_h - 8.0], ThermalPalette::ACCENT_HOT));

                let mut buf = Buffer::new(&mut self.font_system, Metrics::new(18.0, 24.0));
                buf.set_size(&mut self.font_system, Some(mic_width), Some(screen_h));
                buf.set_text(
                    &mut self.font_system,
                    "  MIC",
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                let idx = text_buffers.len();
                text_buffers.push(buf);
                text_placements.push((idx, mic_x, 12.0, ThermalPalette::BG));

                // "Listening..." label.
                let label_x = mic_x + mic_width + 16.0;
                let mut lbl = Buffer::new(&mut self.font_system, Metrics::new(14.0, 20.0));
                lbl.set_size(&mut self.font_system, Some(screen_w - label_x), Some(screen_h));
                lbl.set_text(
                    &mut self.font_system,
                    "LISTENING...",
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                lbl.shape_until_scroll(&mut self.font_system, false);
                let lbl_idx = text_buffers.len();
                text_buffers.push(lbl);
                text_placements.push((lbl_idx, label_x, 14.0, ThermalPalette::TEXT_MUTED));
            }

            VoiceState::Transcribing => {
                // Show partial transcript.
                let label_x = LEFT_MARGIN;
                let mut buf = Buffer::new(&mut self.font_system, Metrics::new(14.0, 20.0));
                buf.set_size(&mut self.font_system, Some(screen_w - label_x * 2.0), Some(screen_h));
                let display = if transcript.is_empty() {
                    "TRANSCRIBING..."
                } else {
                    transcript
                };
                buf.set_text(
                    &mut self.font_system,
                    display,
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                let idx = text_buffers.len();
                text_buffers.push(buf);
                text_placements.push((idx, label_x, 14.0, ThermalPalette::TEXT_BRIGHT));
            }

            VoiceState::Thinking => {
                // Show transcript + "THINKING..." indicator.
                let label_x = LEFT_MARGIN;

                // Transcript line.
                if !transcript.is_empty() {
                    let mut buf = Buffer::new(&mut self.font_system, Metrics::new(13.0, 18.0));
                    buf.set_size(&mut self.font_system, Some(screen_w - label_x * 2.0), Some(screen_h));
                    buf.set_text(
                        &mut self.font_system,
                        transcript,
                        Attrs::new().family(Family::Monospace),
                        Shaping::Basic,
                    );
                    buf.shape_until_scroll(&mut self.font_system, false);
                    let idx = text_buffers.len();
                    text_buffers.push(buf);
                    text_placements.push((idx, label_x, 4.0, ThermalPalette::TEXT_BRIGHT));
                }

                // "THINKING..." label on second line.
                let mut lbl = Buffer::new(&mut self.font_system, Metrics::new(11.0, 14.0));
                lbl.set_size(&mut self.font_system, Some(screen_w - label_x * 2.0), Some(20.0));
                lbl.set_text(
                    &mut self.font_system,
                    "THINKING...",
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                lbl.shape_until_scroll(&mut self.font_system, false);
                let lbl_idx = text_buffers.len();
                text_buffers.push(lbl);
                text_placements.push((lbl_idx, label_x, 28.0, ThermalPalette::ACCENT_WARM));
            }

            VoiceState::Confirming { action } => {
                // Show action text in ACCENT_WARM + "SAY YES/NO" label.
                let label_x = LEFT_MARGIN;

                // Action text.
                let mut buf = Buffer::new(&mut self.font_system, Metrics::new(13.0, 18.0));
                buf.set_size(&mut self.font_system, Some(screen_w - label_x * 2.0 - 140.0), Some(screen_h));
                buf.set_text(
                    &mut self.font_system,
                    action,
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                let idx = text_buffers.len();
                text_buffers.push(buf);
                text_placements.push((idx, label_x, 6.0, ThermalPalette::ACCENT_WARM));

                // "SAY YES/NO" confirmation badge on the right.
                let badge_w = 120.0;
                let badge_x = screen_w - badge_w - LEFT_MARGIN;
                rect_quads.push(([badge_x, 8.0, badge_w, screen_h - 16.0], ThermalPalette::ACCENT_WARM));

                let mut badge = Buffer::new(&mut self.font_system, Metrics::new(13.0, 18.0));
                badge.set_size(&mut self.font_system, Some(badge_w), Some(screen_h));
                badge.set_text(
                    &mut self.font_system,
                    " SAY YES/NO",
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                badge.shape_until_scroll(&mut self.font_system, false);
                let badge_idx = text_buffers.len();
                text_buffers.push(badge);
                text_placements.push((badge_idx, badge_x, 14.0, ThermalPalette::BG));

                // Transcript below action (smaller, muted).
                if !transcript.is_empty() {
                    let mut tbuf = Buffer::new(&mut self.font_system, Metrics::new(11.0, 14.0));
                    tbuf.set_size(&mut self.font_system, Some(screen_w - label_x * 2.0 - 140.0), Some(20.0));
                    tbuf.set_text(
                        &mut self.font_system,
                        transcript,
                        Attrs::new().family(Family::Monospace),
                        Shaping::Basic,
                    );
                    tbuf.shape_until_scroll(&mut self.font_system, false);
                    let tbuf_idx = text_buffers.len();
                    text_buffers.push(tbuf);
                    text_placements.push((tbuf_idx, label_x, 28.0, ThermalPalette::TEXT_MUTED));
                }
            }

            VoiceState::Executing => {
                // Show "EXECUTING..." with a warm accent.
                let label_x = LEFT_MARGIN;
                let mut buf = Buffer::new(&mut self.font_system, Metrics::new(14.0, 20.0));
                buf.set_size(&mut self.font_system, Some(screen_w - label_x * 2.0), Some(screen_h));
                buf.set_text(
                    &mut self.font_system,
                    "EXECUTING...",
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                let idx = text_buffers.len();
                text_buffers.push(buf);
                text_placements.push((idx, label_x, 14.0, ThermalPalette::HOTTER));
            }

            VoiceState::Result { summary } => {
                // Show result summary — dim after RESULT_DIM_SECS.
                let dimmed = result_age_secs.map_or(false, |age| age >= RESULT_DIM_SECS);
                let text_color = if dimmed {
                    ThermalPalette::TEXT_MUTED
                } else {
                    ThermalPalette::WARM
                };

                let label_x = LEFT_MARGIN;
                let mut buf = Buffer::new(&mut self.font_system, Metrics::new(14.0, 20.0));
                buf.set_size(&mut self.font_system, Some(screen_w - label_x * 2.0), Some(screen_h));
                buf.set_text(
                    &mut self.font_system,
                    summary,
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                let idx = text_buffers.len();
                text_buffers.push(buf);
                text_placements.push((idx, label_x, 6.0, text_color));

                // "DONE" label below.
                let mut lbl = Buffer::new(&mut self.font_system, Metrics::new(11.0, 14.0));
                lbl.set_size(&mut self.font_system, Some(screen_w - label_x * 2.0), Some(20.0));
                lbl.set_text(
                    &mut self.font_system,
                    if dimmed { "DONE" } else { "RESULT" },
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                lbl.shape_until_scroll(&mut self.font_system, false);
                let lbl_idx = text_buffers.len();
                text_buffers.push(lbl);
                let lbl_color = if dimmed {
                    ThermalPalette::FREEZING
                } else {
                    ThermalPalette::TEXT_MUTED
                };
                text_placements.push((lbl_idx, label_x, 28.0, lbl_color));
            }
        }

        // Build vertex list for all rect quads.
        let mut rect_vertices: Vec<ColorVertex> = Vec::new();
        for (xywh, color) in &rect_quads {
            let verts = pixel_rect_to_ndc(
                xywh[0], xywh[1], xywh[2], xywh[3], screen_w, screen_h, *color,
            );
            rect_vertices.extend_from_slice(&verts);
        }

        let rect_vbuf = if !rect_vertices.is_empty() {
            let data = bytemuck::cast_slice::<ColorVertex, u8>(&rect_vertices);
            let buf = self.device.create_buffer(&BufferDescriptor {
                label: Some("hud_voice_rect_vbuf"),
                size: data.len() as u64,
                usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&buf, 0, data);
            Some((buf, rect_vertices.len() as u32))
        } else {
            None
        };

        // Build glyphon TextAreas.
        let has_text = !text_buffers.is_empty();
        if has_text {
            let text_areas: Vec<TextArea<'_>> = text_placements
                .iter()
                .map(|(idx, x, y, color)| {
                    let [r, g, b, a] = color;
                    TextArea {
                        buffer: &text_buffers[*idx],
                        left: *x,
                        top: *y,
                        scale: 1.0,
                        bounds: TextBounds {
                            left: *x as i32,
                            top: 0,
                            right: self.width as i32,
                            bottom: self.height as i32,
                        },
                        default_color: GlyphColor::rgba(
                            (*r * 255.0) as u8,
                            (*g * 255.0) as u8,
                            (*b * 255.0) as u8,
                            (*a * 255.0) as u8,
                        ),
                        custom_glyphs: &[],
                    }
                })
                .collect();

            self.text_renderer.prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )?;
        }

        let bg = ThermalPalette::BG;
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("hud_voice_pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: 1.0,
                        }),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if let Some((vbuf, count)) = &rect_vbuf {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, vbuf.slice(..));
                pass.draw(0..*count, 0..1);
            }

            if has_text {
                self.text_renderer
                    .render(&self.atlas, &self.viewport, &mut pass)?;
            }
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.atlas.trim();

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a pixel-space rect to 6 NDC vertices (two triangles).
fn pixel_rect_to_ndc(
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
    screen_w: f32,
    screen_h: f32,
    color: [f32; 4],
) -> [ColorVertex; 6] {
    let x0 = (px / screen_w) * 2.0 - 1.0;
    let x1 = ((px + pw) / screen_w) * 2.0 - 1.0;
    let y0 = 1.0 - (py / screen_h) * 2.0;
    let y1 = 1.0 - ((py + ph) / screen_h) * 2.0;

    [
        ColorVertex { position: [x0, y0], color },
        ColorVertex { position: [x1, y0], color },
        ColorVertex { position: [x0, y1], color },
        ColorVertex { position: [x1, y0], color },
        ColorVertex { position: [x1, y1], color },
        ColorVertex { position: [x0, y1], color },
    ]
}

/// Map ClaudeStatus to a thermal color for the status dot.
fn status_color(status: &ClaudeStatus) -> [f32; 4] {
    match status {
        ClaudeStatus::ToolUse => ThermalPalette::ACCENT_HOT,
        ClaudeStatus::Processing => ThermalPalette::ACCENT_WARM,
        ClaudeStatus::AwaitingInput => ThermalPalette::ACCENT_COOL,
        ClaudeStatus::Idle => ThermalPalette::ACCENT_COLD,
    }
}

/// Map ClaudeStatus to a short label.
fn status_label(status: &ClaudeStatus) -> &'static str {
    match status {
        ClaudeStatus::ToolUse => "TOOL",
        ClaudeStatus::Processing => "RUN",
        ClaudeStatus::AwaitingInput => "WAIT",
        ClaudeStatus::Idle => "IDLE",
    }
}

/// Truncate a session ID to `max_len` characters with ellipsis.
fn truncate_session_id(id: &str, max_len: usize) -> String {
    if id.len() <= max_len {
        id.to_string()
    } else {
        format!("{}...", &id[..max_len.saturating_sub(3)])
    }
}

/// Map context percentage to a thermal color for the progress bar.
fn context_bar_color(pct: f32) -> [f32; 4] {
    if pct >= 90.0 {
        ThermalPalette::SEARING
    } else if pct >= 70.0 {
        ThermalPalette::HOT
    } else if pct >= 50.0 {
        ThermalPalette::ACCENT_WARM
    } else if pct >= 30.0 {
        ThermalPalette::MILD
    } else {
        ThermalPalette::COOL
    }
}
