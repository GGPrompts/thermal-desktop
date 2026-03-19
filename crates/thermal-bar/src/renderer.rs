/// wgpu rendering pipeline for thermal-bar.
///
/// Provides colored rect rendering (for background fills and separators) and
/// glyphon-based text rendering with the thermal color palette.
///
/// NOTE: Surface creation requires a live Wayland compositor connection.
///       This module can only be fully tested on bare-metal with a Wayland session.
use std::ptr::NonNull;

use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use thermal_core::ThermalPalette;
use wgpu::{
    BlendState, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites,
    CommandEncoderDescriptor, Device, FragmentState, FrontFace, Instance, InstanceDescriptor,
    LoadOp, MultisampleState, Operations, PipelineLayoutDescriptor, PolygonMode, PrimitiveState,
    PrimitiveTopology, Queue, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    RenderPipelineDescriptor, RequestAdapterOptions, StoreOp, Surface, SurfaceConfiguration,
    SurfaceTargetUnsafe, TextureFormat, TextureUsages, TextureViewDescriptor, VertexAttribute,
    VertexBufferLayout, VertexState, VertexStepMode,
};
use bytemuck::{Pod, Zeroable};

use crate::layout::ModuleOutput;

// ---------------------------------------------------------------------------
// Vertex layout
// ---------------------------------------------------------------------------

/// A single colored vertex for rectangle rendering.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ColorVertex {
    position: [f32; 2], // NDC coordinates
    color: [f32; 4],    // RGBA
}

// Vertex attributes defined as static constants (avoids lifetime issues).
static RECT_VERTEX_ATTRS: &[VertexAttribute] = &[
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 8, // 2 * sizeof(f32)
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
// Renderer
// ---------------------------------------------------------------------------

/// GPU-accelerated renderer for the status bar.
pub struct Renderer {
    device: Device,
    queue: Queue,
    surface: Surface<'static>,
    surface_config: SurfaceConfiguration,
    rect_pipeline: RenderPipeline,

    // Glyphon text rendering
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
    ///
    /// # Safety
    ///
    /// - `wl_display` must be a valid `*mut wl_display` pointer that remains
    ///   valid for the lifetime of this Renderer.
    /// - `wl_surface` must be a valid `*mut wl_surface` pointer that remains
    ///   valid for the lifetime of this Renderer.
    pub async fn new_from_wayland(
        wl_display: *mut std::ffi::c_void,
        wl_surface: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        // Instance::new takes value (not reference) in wgpu 23.
        let instance = Instance::new(InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..Default::default()
        });

        // Build raw window handles for Wayland.
        let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(wl_display)
                .ok_or_else(|| anyhow::anyhow!("null wl_display pointer"))?,
        ));
        let raw_window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(wl_surface)
                .ok_or_else(|| anyhow::anyhow!("null wl_surface pointer"))?,
        ));

        // Safety: the caller guarantees the pointers remain valid.
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
                    label: Some("thermal-bar"),
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
            present_mode: wgpu::PresentMode::Mailbox,
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
        let mut warmup_buf = Buffer::new(&mut font_system, Metrics::new(16.0, 24.0));
        warmup_buf.set_size(&mut font_system, Some(width as f32), Some(height as f32));
        warmup_buf.set_text(
            &mut font_system,
            "THERMAL-BAR",
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

    /// Resize the surface to a new width (height is always BAR_HEIGHT).
    pub fn resize(&mut self, new_width: u32, new_height: u32) {
        self.width = new_width;
        self.height = new_height;
        self.surface_config.width = new_width;
        self.surface_config.height = new_height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Render all modules to the bar surface.
    ///
    /// Clears to ThermalPalette::BG, draws module backgrounds and text.
    pub fn render(&mut self, modules: &[ModuleOutput], spark_rects: &[crate::sparkline::SparkRect]) -> anyhow::Result<()> {
        let frame = self.surface.get_current_texture()?;
        let view = frame.texture.create_view(&TextureViewDescriptor::default());

        // Update viewport resolution for glyphon.
        self.viewport.update(
            &self.queue,
            Resolution { width: self.width, height: self.height },
        );

        let mut encoder =
            self.device.create_command_encoder(&CommandEncoderDescriptor { label: None });

        // Collect text buffers and their placement info.
        let mut text_buffers: Vec<Buffer> = Vec::new();
        // (buf_idx, x, y, color)
        let mut text_placements: Vec<(usize, f32, f32, [f32; 4])> = Vec::new();
        // (pixel xywh, color)
        let mut rect_quads: Vec<([f32; 4], [f32; 4])> = Vec::new();

        for module in modules {
            // Background rect.
            if let Some(bg) = module.bg_color {
                rect_quads.push(([module.x, 0.0, module.width, self.height as f32], bg));
            }

            if !module.text.is_empty() {
                let mut buf = Buffer::new(&mut self.font_system, Metrics::new(16.0, 24.0));
                buf.set_size(
                    &mut self.font_system,
                    Some(module.width.max(200.0)),
                    Some(self.height as f32),
                );
                buf.set_text(
                    &mut self.font_system,
                    &module.text,
                    Attrs::new().family(Family::Monospace),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                let idx = text_buffers.len();
                text_buffers.push(buf);
                text_placements.push((idx, module.x, 6.0, module.color));
            }
        }

        // Build vertex list for all rect quads + sparkline rects.
        let mut rect_vertices: Vec<ColorVertex> = Vec::new();
        for (xywh, color) in &rect_quads {
            let (x, y, w, h) = (xywh[0], xywh[1], xywh[2], xywh[3]);
            let verts =
                pixel_rect_to_ndc(x, y, w, h, self.width as f32, self.height as f32, *color);
            rect_vertices.extend_from_slice(&verts);
        }
        for r in spark_rects {
            let verts = pixel_rect_to_ndc(
                r.x, r.y, r.w, r.h,
                self.width as f32, self.height as f32,
                r.color,
            );
            rect_vertices.extend_from_slice(&verts);
        }

        let rect_vbuf = if !rect_vertices.is_empty() {
            let data = bytemuck::cast_slice::<ColorVertex, u8>(&rect_vertices);
            let buf = self.device.create_buffer(&BufferDescriptor {
                label: Some("rect_vbuf"),
                size: data.len() as u64,
                usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&buf, 0, data);
            Some((buf, rect_vertices.len() as u32))
        } else {
            None
        };

        // Build glyphon TextAreas referencing the buffers we just created.
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
                label: Some("bar_pass"),
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
                self.text_renderer.render(&self.atlas, &self.viewport, &mut pass)?;
            }
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.atlas.trim();

        Ok(())
    }

    /// Convenience: render a `BarLayout` with optional sparkline overlay.
    pub fn render_layout(
        &mut self,
        layout: &crate::layout::BarLayout,
        spark_rects: &[crate::sparkline::SparkRect],
    ) -> anyhow::Result<()> {
        let modules = layout.all_positioned();
        self.render(&modules, spark_rects)
    }

    /// Draw a batch of sparkline rects in a single render pass.
    ///
    /// This is called after the main `render()` pass to overlay sparkline bars
    /// on top of the already-rendered bar. In production use, sparkline rects
    /// would be merged into the main render call for efficiency.
    pub fn draw_spark_rects(&mut self, rects: &[crate::sparkline::SparkRect]) -> anyhow::Result<()> {
        if rects.is_empty() {
            return Ok(());
        }

        let frame = self.surface.get_current_texture()?;
        let view = frame.texture.create_view(&TextureViewDescriptor::default());
        let mut encoder =
            self.device.create_command_encoder(&CommandEncoderDescriptor { label: None });

        let mut vertices: Vec<ColorVertex> = Vec::with_capacity(rects.len() * 6);
        for r in rects {
            let verts = pixel_rect_to_ndc(
                r.x, r.y, r.w, r.h,
                self.width as f32, self.height as f32,
                r.color,
            );
            vertices.extend_from_slice(&verts);
        }

        let data = bytemuck::cast_slice::<ColorVertex, u8>(&vertices);
        let vbuf = self.device.create_buffer(&BufferDescriptor {
            label: Some("spark_vbuf"),
            size: data.len() as u64,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&vbuf, 0, data);

        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("spark_pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Load, // preserve existing pixels
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..vertices.len() as u32, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
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
