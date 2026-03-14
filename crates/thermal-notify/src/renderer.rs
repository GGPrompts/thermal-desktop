use std::sync::Arc;

use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use thermal_core::palette::ThermalPalette;
use wgpu::util::DeviceExt;

use crate::dbus::Notification;

// ── WGSL shader ─────────────────────────────────────────────────────────────

const SHADER_SRC: &str = r#"
struct Rect {
    min: vec2<f32>,
    size: vec2<f32>,
    color: vec4<f32>,
    radius: f32,
    _pad1: f32,
    _pad2: f32,
    _pad3: f32,
};

@group(0) @binding(0)
var<uniform> rect: Rect;

struct VertexOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VertexOut {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let c = corners[vi];
    var out: VertexOut;
    out.pos = vec4<f32>(rect.min + c * rect.size, 0.0, 1.0);
    out.color = rect.color;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

// ── Uniform buffer layout ────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RectUniform {
    min: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
    radius: f32,
    _pad: [f32; 3], // pad to 48 bytes (WGSL uniform alignment rounds up to vec4 = 16 bytes)
}

// ── NotificationRenderer ─────────────────────────────────────────────────────

pub struct NotificationRenderer {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    font_system: FontSystem,
    swash_cache: SwashCache,
    cache: Cache,
    text_atlas: TextAtlas,
    viewport: Viewport,
    text_renderer: TextRenderer,
    pub alpha: f32,
}

impl NotificationRenderer {
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        // ── Shader ───────────────────────────────────────────────────────────
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("thermal-notify quad shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        // ── Bind group layout ─────────────────────────────────────────────────
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("rect bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        // ── Pipeline ─────────────────────────────────────────────────────────
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("notify pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("notify quad pipeline"),
            layout: Some(&pipeline_layout),
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
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // ── Glyphon setup (glyphon 0.7 API) ──────────────────────────────────
        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let mut text_atlas = TextAtlas::new(&device, &queue, &cache, format);
        let viewport = {
            let mut vp = Viewport::new(&device, &cache);
            vp.update(&queue, Resolution { width, height });
            vp
        };
        let text_renderer =
            TextRenderer::new(&mut text_atlas, &device, wgpu::MultisampleState::default(), None);

        Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            font_system,
            swash_cache,
            cache,
            text_atlas,
            viewport,
            text_renderer,
            alpha: 1.0,
        }
    }

    pub fn set_alpha(&mut self, a: f32) {
        self.alpha = a.clamp(0.0, 1.0);
    }

    /// Draw a notification card into `surface_view`.
    pub fn render(
        &mut self,
        surface_view: &wgpu::TextureView,
        notif: &Notification,
        urgency_color: [f32; 4],
        width: u32,
        height: u32,
    ) {
        let a = self.alpha;

        // Update viewport resolution
        self.viewport.update(
            &self.queue,
            Resolution { width, height },
        );

        let mut encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("notify render encoder"),
                });

        // ── Background clear ─────────────────────────────────────────────────
        let bg = ThermalPalette::BG_SURFACE;
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("bg clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: surface_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: (bg[0] * a) as f64,
                            g: (bg[1] * a) as f64,
                            b: (bg[2] * a) as f64,
                            a: a as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }

        let w = width as f32;
        let h = height as f32;

        // ── Accent bar (left 8 px) ────────────────────────────────────────────
        self.draw_rect(
            &mut encoder,
            surface_view,
            [-1.0, -1.0],
            [2.0 * 8.0 / w, 2.0],
            apply_alpha(urgency_color, a),
        );

        // ── Card background (right of accent bar) ─────────────────────────────
        let bg_light = ThermalPalette::BG_LIGHT;
        self.draw_rect(
            &mut encoder,
            surface_view,
            [-1.0 + 2.0 * 8.0 / w, -1.0],
            [2.0 - 2.0 * 8.0 / w, 2.0],
            apply_alpha(bg_light, a),
        );

        // ── Text ──────────────────────────────────────────────────────────────
        let text_bright = ThermalPalette::TEXT_BRIGHT;
        let text_color = ThermalPalette::TEXT;

        let mut summary_buf = Buffer::new(&mut self.font_system, Metrics::new(16.0, 20.0));
        summary_buf.set_size(&mut self.font_system, Some(w - 20.0), Some(24.0));
        summary_buf.set_text(
            &mut self.font_system,
            &notif.summary,
            Attrs::new().family(Family::SansSerif).color(GlyphColor::rgba(
                (text_bright[0] * 255.0) as u8,
                (text_bright[1] * 255.0) as u8,
                (text_bright[2] * 255.0) as u8,
                (a * 255.0) as u8,
            )),
            Shaping::Advanced,
        );
        summary_buf.shape_until_scroll(&mut self.font_system, false);

        let mut body_buf = Buffer::new(&mut self.font_system, Metrics::new(13.0, 17.0));
        body_buf.set_size(&mut self.font_system, Some(w - 20.0), Some(h - 36.0));
        body_buf.set_text(
            &mut self.font_system,
            &notif.body,
            Attrs::new().family(Family::SansSerif).color(GlyphColor::rgba(
                (text_color[0] * 255.0) as u8,
                (text_color[1] * 255.0) as u8,
                (text_color[2] * 255.0) as u8,
                (a * 255.0) as u8,
            )),
            Shaping::Advanced,
        );
        body_buf.shape_until_scroll(&mut self.font_system, false);

        let text_areas = [
            TextArea {
                buffer: &summary_buf,
                left: 16.0,
                top: 8.0,
                scale: 1.0,
                bounds: TextBounds {
                    left: 16,
                    top: 8,
                    right: width as i32 - 8,
                    bottom: 32,
                },
                default_color: GlyphColor::rgba(
                    (text_bright[0] * 255.0) as u8,
                    (text_bright[1] * 255.0) as u8,
                    (text_bright[2] * 255.0) as u8,
                    (a * 255.0) as u8,
                ),
                custom_glyphs: &[],
            },
            TextArea {
                buffer: &body_buf,
                left: 16.0,
                top: 32.0,
                scale: 1.0,
                bounds: TextBounds {
                    left: 16,
                    top: 32,
                    right: width as i32 - 8,
                    bottom: height as i32 - 8,
                },
                default_color: GlyphColor::rgba(
                    (text_color[0] * 255.0) as u8,
                    (text_color[1] * 255.0) as u8,
                    (text_color[2] * 255.0) as u8,
                    (a * 255.0) as u8,
                ),
                custom_glyphs: &[],
            },
        ];

        let _ = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.text_atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
        );

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("text pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: surface_view,
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
            let _ = self.text_renderer.render(&self.text_atlas, &self.viewport, &mut pass);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
    }

    fn draw_rect(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        ndc_min: [f32; 2],
        ndc_size: [f32; 2],
        color: [f32; 4],
    ) {
        let uniform = RectUniform {
            min: ndc_min,
            size: ndc_size,
            color,
            radius: 4.0,
            _pad: [0.0; 3],
        };

        let buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("rect uniform"),
                contents: bytemuck::bytes_of(&uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rect bg"),
            layout: &self.bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buf.as_entire_binding(),
            }],
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rect pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
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

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..6, 0..1);
    }
}

fn apply_alpha(color: [f32; 4], a: f32) -> [f32; 4] {
    [color[0], color[1], color[2], color[3] * a]
}
