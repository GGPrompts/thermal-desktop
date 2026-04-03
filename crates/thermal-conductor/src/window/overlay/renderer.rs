//! wgpu render pipeline for overlay widgets.
//!
//! Renders semi-transparent background quads with accent side-stripes behind
//! widget content, plus glyphon text labels. Each widget gets:
//!   - A dark card background (`CARD_BG`) with rounded-corner feel (via alpha)
//!   - A colored accent stripe on the left edge (4px wide)
//!   - A primary text label (tool name, gauge label, etc.)
//!   - A secondary detail line (file path, preview, etc.)

use super::layout::WidgetRect;
use super::widgets::{ToolStatus, Widget, WidgetKind};
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics,
    Resolution, Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use thermal_core::text::glyphon_color_mode_for_surface;

/// Thermal palette colors as `[f32; 4]` RGBA for overlay rendering.
pub(super) mod colors {
    use thermal_core::palette::ThermalPalette;

    /// Semi-transparent dark background for widget cards.
    pub const CARD_BG: [f32; 4] = [0.04, 0.0, 0.06, 0.85];
    /// Active tool — searing red.
    pub const TOOL_ACTIVE: [f32; 4] = ThermalPalette::SEARING;
    /// Tool pending — accent cold.
    pub const TOOL_PENDING: [f32; 4] = ThermalPalette::ACCENT_COLD;
    /// Tool completed — status OK green.
    pub const TOOL_COMPLETED: [f32; 4] = ThermalPalette::STATUS_OK;
    /// Tool failed — status error red.
    pub const TOOL_FAILED: [f32; 4] = ThermalPalette::STATUS_ERROR;
    /// Thinking indicator — warm green.
    pub const THINKING: [f32; 4] = ThermalPalette::WARM;
    /// Context gauge fill (interpolated by usage).
    pub const GAUGE_LOW: [f32; 4] = ThermalPalette::MILD;
    pub const GAUGE_HIGH: [f32; 4] = ThermalPalette::SEARING;
    /// Permission dialog border — hot yellow.
    pub const PERMISSION_BORDER: [f32; 4] = ThermalPalette::HOT;
    /// Result success.
    pub const RESULT_OK: [f32; 4] = ThermalPalette::STATUS_OK;
    /// Result failure.
    pub const RESULT_ERROR: [f32; 4] = ThermalPalette::STATUS_ERROR;
    /// Text colors.
    pub const TEXT_PRIMARY: [f32; 4] = ThermalPalette::WHITE_HOT;
    pub const TEXT_SECONDARY: [f32; 4] = ThermalPalette::COOL;
}

/// Get the accent color for a tool call card based on its status.
fn tool_status_color(status: &ToolStatus) -> [f32; 4] {
    match status {
        ToolStatus::Pending => colors::TOOL_PENDING,
        ToolStatus::Running => colors::TOOL_ACTIVE,
        ToolStatus::Completed => colors::TOOL_COMPLETED,
        ToolStatus::Failed => colors::TOOL_FAILED,
    }
}

/// Compute the accent color for a widget.
fn widget_accent(widget: &Widget) -> [f32; 4] {
    match &widget.kind {
        WidgetKind::ToolCallCard(card) => tool_status_color(&card.status),
        WidgetKind::ThinkingIndicator(_) => colors::THINKING,
        WidgetKind::ContextGauge(gauge) => {
            let pct = if gauge.total > 0.0 {
                (gauge.used / gauge.total).clamp(0.0, 1.0)
            } else {
                0.0
            };
            [
                colors::GAUGE_LOW[0] + (colors::GAUGE_HIGH[0] - colors::GAUGE_LOW[0]) * pct,
                colors::GAUGE_LOW[1] + (colors::GAUGE_HIGH[1] - colors::GAUGE_LOW[1]) * pct,
                colors::GAUGE_LOW[2] + (colors::GAUGE_HIGH[2] - colors::GAUGE_LOW[2]) * pct,
                1.0,
            ]
        }
        WidgetKind::PermissionDialog(_) => colors::PERMISSION_BORDER,
        WidgetKind::ResultCard(card) => {
            if card.success {
                colors::RESULT_OK
            } else {
                colors::RESULT_ERROR
            }
        }
    }
}

// ── Shader + vertex layout ──────────────────────────────────────────────

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

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ColorVertex {
    position: [f32; 2],
    color: [f32; 4],
}

static RECT_VERTEX_ATTRS: &[wgpu::VertexAttribute] = &[
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

fn rect_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<ColorVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: RECT_VERTEX_ATTRS,
    }
}

/// Width of the accent stripe on the left edge of each card (pixels).
const STRIPE_WIDTH: f32 = 4.0;

/// Font size for primary widget text (tool name, status).
const TEXT_SIZE_PRIMARY: f32 = 14.0;

/// Font size for secondary widget text (file path, preview).
const TEXT_SIZE_SECONDARY: f32 = 12.0;

/// Padding inside the card (pixels).
const CARD_PAD_X: f32 = 8.0;
const CARD_PAD_Y: f32 = 6.0;

// ── Pipeline ────────────────────────────────────────────────────────────

/// GPU pipeline for rendering overlay widget quads + text.
///
/// Owns a simple colored-rect pipeline (same pattern as the grid renderer
/// and bar renderer) and a glyphon text stack for widget labels.
pub struct OverlayPipeline {
    rect_pipeline: wgpu::RenderPipeline,
    surface_format: wgpu::TextureFormat,
    // Glyphon text rendering — self-contained to avoid conflicts with
    // the grid renderer's atlas.
    font_system: FontSystem,
    swash_cache: SwashCache,
    #[allow(dead_code)]
    cache: Cache,
    atlas: TextAtlas,
    viewport: Viewport,
    text_renderer: TextRenderer,
}

impl OverlayPipeline {
    /// Create the overlay pipeline. Call once during window init.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, surface_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay_rect_shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("overlay_rect_pipeline_layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("overlay_rect_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[rect_vertex_layout()],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Ccw,
                polygon_mode: wgpu::PolygonMode::Fill,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });

        // Glyphon text rendering.
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let color_mode = glyphon_color_mode_for_surface(surface_format);
        let mut atlas =
            TextAtlas::with_color_mode(device, queue, &cache, surface_format, color_mode);
        let viewport = Viewport::new(device, &cache);
        let text_renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);

        // Pre-warm font system with a small text sample.
        let mut warmup = TextBuffer::new(&mut font_system, Metrics::new(TEXT_SIZE_PRIMARY, TEXT_SIZE_PRIMARY * 1.4));
        warmup.set_size(&mut font_system, Some(200.0), Some(30.0));
        warmup.set_text(
            &mut font_system,
            "Overlay",
            Attrs::new().family(Family::Monospace),
            Shaping::Basic,
        );

        Self {
            rect_pipeline,
            surface_format,
            font_system,
            swash_cache,
            cache,
            atlas,
            viewport,
            text_renderer,
        }
    }

    /// Render all overlay widgets in a single pass.
    ///
    /// Collects quad vertices and text areas from all widgets, then issues
    /// one rect draw call + one glyphon text render call in a single render
    /// pass with alpha blending (load existing content).
    pub fn render(
        &mut self,
        widgets: &[(Widget, WidgetRect)],
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        viewport_w: u32,
        viewport_h: u32,
    ) {
        if widgets.is_empty() {
            return;
        }

        let vw = viewport_w as f32;
        let vh = viewport_h as f32;

        // ── Build rect vertices ─────────────────────────────────────────
        let mut vertices: Vec<ColorVertex> = Vec::with_capacity(widgets.len() * 12);
        // Text buffers + placements for glyphon.
        let mut text_buffers: Vec<TextBuffer> = Vec::new();
        let mut text_placements: Vec<(usize, f32, f32, [f32; 4], f32)> = Vec::new(); // (buf_idx, left, top, color, right_bound)

        for (widget, rect) in widgets {
            let accent = widget_accent(widget);

            // Card background quad.
            let bg_verts = pixel_rect_to_ndc(rect.x, rect.y, rect.width, rect.height, vw, vh, colors::CARD_BG);
            vertices.extend_from_slice(&bg_verts);

            // Accent stripe on the left edge.
            let stripe_verts = pixel_rect_to_ndc(rect.x, rect.y, STRIPE_WIDTH, rect.height, vw, vh, accent);
            vertices.extend_from_slice(&stripe_verts);

            // ── Context gauge: fill bar ─────────────────────────────────
            if let WidgetKind::ContextGauge(gauge) = &widget.kind {
                let pct = if gauge.total > 0.0 {
                    (gauge.used / gauge.total).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                // Fill bar inside the card, after the stripe.
                let bar_x = rect.x + STRIPE_WIDTH + CARD_PAD_X;
                let bar_y = rect.y + rect.height - 8.0;
                let bar_w = (rect.width - STRIPE_WIDTH - CARD_PAD_X * 2.0) * pct;
                let bar_h = 4.0;
                let fill_color = [accent[0], accent[1], accent[2], 0.8];
                let fill_verts = pixel_rect_to_ndc(bar_x, bar_y, bar_w, bar_h, vw, vh, fill_color);
                vertices.extend_from_slice(&fill_verts);
            }

            // ── Text labels ─────────────────────────────────────────────
            let (primary, secondary) = widget_text(widget);
            let text_left = rect.x + STRIPE_WIDTH + CARD_PAD_X;
            let text_right = rect.x + rect.width - CARD_PAD_X;
            let text_width = text_right - text_left;

            // Primary text (tool name / label).
            if !primary.is_empty() {
                let mut buf = TextBuffer::new(
                    &mut self.font_system,
                    Metrics::new(TEXT_SIZE_PRIMARY, TEXT_SIZE_PRIMARY * 1.4),
                );
                buf.set_size(&mut self.font_system, Some(text_width.max(10.0)), Some(TEXT_SIZE_PRIMARY * 1.5));
                buf.set_text(
                    &mut self.font_system,
                    &primary,
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                let idx = text_buffers.len();
                text_buffers.push(buf);
                text_placements.push((idx, text_left, rect.y + CARD_PAD_Y, colors::TEXT_PRIMARY, text_right));
            }

            // Secondary text (detail line).
            if !secondary.is_empty() {
                let mut buf = TextBuffer::new(
                    &mut self.font_system,
                    Metrics::new(TEXT_SIZE_SECONDARY, TEXT_SIZE_SECONDARY * 1.4),
                );
                buf.set_size(&mut self.font_system, Some(text_width.max(10.0)), Some(TEXT_SIZE_SECONDARY * 1.5));
                buf.set_text(
                    &mut self.font_system,
                    &secondary,
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                let idx = text_buffers.len();
                text_buffers.push(buf);
                let secondary_y = rect.y + CARD_PAD_Y + TEXT_SIZE_PRIMARY * 1.4 + 2.0;
                text_placements.push((idx, text_left, secondary_y, colors::TEXT_SECONDARY, text_right));
            }
        }

        // ── Upload rect vertex buffer ───────────────────────────────────
        let rect_vbuf = if !vertices.is_empty() {
            let data = bytemuck::cast_slice::<ColorVertex, u8>(&vertices);
            let buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("overlay_rect_vbuf"),
                size: data.len() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&buf, 0, data);
            Some((buf, vertices.len() as u32))
        } else {
            None
        };

        // ── Prepare glyphon text ────────────────────────────────────────
        self.viewport.update(
            queue,
            Resolution {
                width: viewport_w,
                height: viewport_h,
            },
        );

        let has_text = !text_buffers.is_empty();
        if has_text {
            let text_areas: Vec<TextArea<'_>> = text_placements
                .iter()
                .map(|(idx, left, top, color, right_bound)| {
                    let [r, g, b, a] = color;
                    TextArea {
                        buffer: &text_buffers[*idx],
                        left: *left,
                        top: *top,
                        scale: 1.0,
                        bounds: TextBounds {
                            left: *left as i32,
                            top: *top as i32,
                            right: *right_bound as i32,
                            bottom: viewport_h as i32,
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

            // Ignore errors — text rendering is best-effort.
            let _ = self.text_renderer.prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            );
        }

        // ── Render pass (alpha-blend over existing content) ─────────────
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay_widget_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
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

            // Draw rect quads.
            if let Some((ref vbuf, count)) = rect_vbuf {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, vbuf.slice(..));
                pass.draw(0..count, 0..1);
            }

            // Draw text.
            if has_text {
                let _ = self
                    .text_renderer
                    .render(&self.atlas, &self.viewport, &mut pass);
            }
        }

        // Trim atlas periodically to free unused glyphs.
        self.atlas.trim();
    }

    /// Update the surface format (e.g., after a surface reconfigure).
    #[allow(dead_code)]
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.surface_format
    }
}

// ── Text generation for widgets ─────────────────────────────────────────

/// Generate primary and secondary text for a widget.
fn widget_text(widget: &Widget) -> (String, String) {
    match &widget.kind {
        WidgetKind::ToolCallCard(card) => {
            let status_icon = match card.status {
                ToolStatus::Pending => "\u{25cb}", // ○
                ToolStatus::Running => "\u{25cf}", // ●
                ToolStatus::Completed => "\u{2713}", // ✓
                ToolStatus::Failed => "\u{2717}",  // ✗
            };
            let primary = format!("{} {}", status_icon, card.tool);
            let secondary = card
                .file
                .as_deref()
                .unwrap_or(&card.input_preview)
                .to_owned();
            (primary, secondary)
        }
        WidgetKind::ThinkingIndicator(ind) => {
            let primary = "Thinking...".to_owned();
            let secondary = ind.content_preview.clone();
            (primary, secondary)
        }
        WidgetKind::ContextGauge(gauge) => {
            let pct = if gauge.total > 0.0 {
                (gauge.used / gauge.total * 100.0).round() as u32
            } else {
                0
            };
            let primary = format!("Context: {}%", pct);
            (primary, String::new())
        }
        WidgetKind::PermissionDialog(dialog) => {
            let primary = format!("Permission: {}", dialog.tool);
            let secondary = dialog.message.clone();
            (primary, secondary)
        }
        WidgetKind::ResultCard(card) => {
            let icon = if card.success { "\u{2713}" } else { "\u{2717}" };
            let primary = format!("{} {}", icon, card.tool);
            let secondary = card.summary.clone();
            (primary, secondary)
        }
    }
}

// ── Geometry helpers ────────────────────────────────────────────────────

/// Convert a pixel-space rectangle to 6 NDC vertices (two triangles).
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
        ColorVertex {
            position: [x0, y0],
            color,
        },
        ColorVertex {
            position: [x1, y0],
            color,
        },
        ColorVertex {
            position: [x0, y1],
            color,
        },
        ColorVertex {
            position: [x1, y0],
            color,
        },
        ColorVertex {
            position: [x1, y1],
            color,
        },
        ColorVertex {
            position: [x0, y1],
            color,
        },
    ]
}
