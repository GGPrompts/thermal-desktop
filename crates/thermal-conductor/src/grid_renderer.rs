//! Terminal cell grid renderer using glyphon.
//!
//! Reads `alacritty_terminal::Term`'s grid and renders each cell character
//! via glyphon, mapping ANSI colors to ThermalPalette where they match and
//! passing truecolor through directly. Renders the cursor as a distinct
//! visual element (inverted block).

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::RenderableCursor;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor};

use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use thermal_core::claude_state::{ClaudeSessionState, ClaudeStatus};
use thermal_core::palette::{Color as PaletteColor, thermal_gradient};
use wgpu::util::DeviceExt;

use tracing::debug;

use crate::agent_timeline::{AgentTimeline, TIMELINE_BAR_HEIGHT, ToolCategory};
use crate::kitty_graphics::ImageStore;
use crate::osc633::{CommandBlock, CommandState};

/// Near-black terminal background — neutral dark, not purple-tinted.
/// Must match the clear color in window.rs.
const TERM_BG: [f32; 4] = [0.03, 0.03, 0.04, 1.0];

// ── Constants ──────────────────────────────────────────────────────────────

/// Font size in points for the terminal grid.
const FONT_SIZE: f32 = 16.0;

/// Line height in points (typically font_size * 1.2–1.4 for monospace).
const LINE_HEIGHT: f32 = 22.0;

/// Primary font family — Nerd Font Mono variant for terminal glyphs
/// (box-drawing, Powerline, Nerd Font icons in the Private Use Area).
/// cosmic-text will fall back to other system fonts for any remaining missing glyphs.
const TERM_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

// ── RenderCell — snapshot of a single grid cell ────────────────────────────

/// A lightweight snapshot of a terminal cell, suitable for lock-free rendering.
/// Created while holding the term lock, consumed by the renderer after release.
#[derive(Clone)]
pub struct RenderCell {
    /// Viewport row index (0-based).
    pub row: usize,
    /// Column index (0-based).
    pub col: usize,
    /// The character to display.
    pub c: char,
    /// Foreground color.
    pub fg: AnsiColor,
    /// Background color.
    pub bg: AnsiColor,
    /// Cell flags (BOLD, INVERSE, WIDE_CHAR, etc.).
    pub flags: Flags,
}

// ── CachedRow — cached per-row cell data ─────────────────────────────────

/// Cached cell data for a single row, used to avoid rebuilding undamaged rows.
struct CachedRow {
    cells: Vec<RenderCell>,
}

// ── Context heatmap shader (edge vignette glow) ────────────────────────

const CONTEXT_HEATMAP_SHADER: &str = r#"
struct HeatmapUniform {
    context_percent: f32,
    time: f32,
    width: f32,
    height: f32,
}
@group(0) @binding(0)
var<uniform> u_heatmap: HeatmapUniform;

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
    // Fullscreen triangle covering the entire viewport.
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

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = frag_coord.xy / vec2<f32>(u_heatmap.width, u_heatmap.height);
    let ctx = u_heatmap.context_percent;
    let t = u_heatmap.time;

    // Vignette: distance from edge (0 at edge, 1 at center).
    let edge_x = min(uv.x, 1.0 - uv.x) * 2.0;
    let edge_y = min(uv.y, 1.0 - uv.y) * 2.0;
    let edge_dist = min(edge_x, edge_y);

    // Vignette falloff: sharper at higher context, wider glow.
    // At 50%: very narrow edge glow. At 100%: extends further in.
    let spread = mix(0.02, 0.15, (ctx - 0.5) * 2.0);
    let vignette = 1.0 - smoothstep(0.0, spread, edge_dist);

    // Intensity ramp: invisible below 50%, faint 50-80%, visible 80-100%.
    var intensity: f32;
    if ctx < 0.5 {
        intensity = 0.0;
    } else if ctx < 0.8 {
        // 50-80%: fade in gently (0.0 to 0.08).
        intensity = (ctx - 0.5) / 0.3 * 0.08;
    } else {
        // 80-100%: ramp up more (0.08 to 0.2).
        intensity = 0.08 + (ctx - 0.8) / 0.2 * 0.12;
    }

    // Subtle time-based pulse at high context (>80%), very slow.
    let pulse = 1.0 + select(0.0, sin(t * 1.5) * 0.15, ctx > 0.8);

    // Color: map context_percent to thermal gradient position.
    // Shift the gradient so 50% starts cool-ish and 100% is hot/searing.
    let heat_t = (ctx - 0.5) * 2.0; // 0.0 at 50%, 1.0 at 100%
    let color = thermal_color(clamp(heat_t, 0.0, 1.0));

    let alpha = vignette * intensity * pulse;
    return vec4<f32>(color * alpha, alpha);
}
"#;

// ── Context heatmap pipeline ────────────────────────────────────────────

/// GPU pipeline for rendering a context-aware edge vignette glow.
///
/// Renders a fullscreen triangle with a WGSL fragment shader that produces
/// a subtle thermal glow at the screen edges. The glow intensity is driven
/// by the Claude session's `context_percent` value.
pub struct ContextHeatmapPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    start: Instant,
}

impl ContextHeatmapPipeline {
    /// Create the heatmap pipeline. Call once during renderer init.
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("context_heatmap_shader"),
            source: wgpu::ShaderSource::Wgsl(CONTEXT_HEATMAP_SHADER.into()),
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("context_heatmap_uniform"),
            size: 16, // 4 x f32
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("context_heatmap_bgl"),
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
            label: Some("context_heatmap_bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("context_heatmap_layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("context_heatmap_pipeline"),
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
                    format: surface_format,
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

    /// Render the context heatmap vignette.
    ///
    /// `context_percent` is 0.0-1.0 (already normalized from the 0-100 range).
    /// Only renders when context_percent > 0.5 (effect is invisible below that).
    pub fn render(
        &self,
        context_percent: f32,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) {
        if context_percent <= 0.5 {
            return;
        }

        // Update uniform buffer.
        let elapsed = self.start.elapsed().as_secs_f32();
        let uniform_data: [f32; 4] = [context_percent, elapsed, width as f32, height as f32];
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::cast_slice(&uniform_data));

        // Render fullscreen triangle.
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("context_heatmap_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
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
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

// ── Rect rendering (for cursor and cell backgrounds) ───────────────────────

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

// ── Image rendering (Kitty graphics protocol) ──────────────────────────────

const IMAGE_SHADER: &str = r#"
struct ImageVertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
};
struct ImageVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var img_texture: texture_2d<f32>;
@group(0) @binding(1) var img_sampler: sampler;

@vertex
fn vs_main(in: ImageVertexInput) -> ImageVertexOutput {
    var out: ImageVertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_main(in: ImageVertexOutput) -> @location(0) vec4<f32> {
    return textureSample(img_texture, img_sampler, in.uv);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ImageVertex {
    position: [f32; 2],
    uv: [f32; 2],
}

static IMAGE_VERTEX_ATTRS: &[wgpu::VertexAttribute] = &[
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 8,
        shader_location: 1,
    },
];

fn image_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<ImageVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: IMAGE_VERTEX_ATTRS,
    }
}

/// GPU pipeline for rendering textured quads (inline images from Kitty graphics protocol).
pub struct ImageRenderPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Cache of GPU textures keyed by image ID to avoid re-uploading each frame.
    texture_cache: HashMap<u32, CachedImageTexture>,
}

/// A cached GPU texture for a single image.
struct CachedImageTexture {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    #[allow(dead_code)]
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

impl ImageRenderPipeline {
    /// Create the image render pipeline. Call once during renderer init.
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image_shader"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image_pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[image_vertex_layout()],
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            texture_cache: HashMap::new(),
        }
    }

    /// Upload an image to the GPU if not already cached.
    fn ensure_texture(
        &mut self,
        image_id: u32,
        rgba_data: &[u8],
        width: u32,
        height: u32,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) {
        if self.texture_cache.contains_key(&image_id) {
            return;
        }

        let texture_size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(&format!("kitty_img_{}", image_id)),
            size: texture_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba_data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            texture_size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(&format!("kitty_img_bg_{}", image_id)),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        self.texture_cache.insert(
            image_id,
            CachedImageTexture {
                texture,
                view,
                bind_group,
            },
        );

        debug!(
            id = image_id,
            width, height, "Uploaded image texture to GPU"
        );
    }

    /// Render all placed images from the ImageStore.
    ///
    /// For each placement, computes the grid-aligned position, creates a
    /// textured quad, and draws it. Images are rendered AFTER cell backgrounds
    /// but BEFORE text so text overlays remain readable.
    pub fn render(
        &mut self,
        image_store: &ImageStore,
        cell_width: f32,
        cell_height: f32,
        padding_x: f32,
        padding_y: f32,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        let placements = image_store.visible_placements();
        if placements.is_empty() {
            return;
        }

        let sw = surface_width as f32;
        let sh = surface_height as f32;

        for (placement, image) in &placements {
            // Ensure the texture is uploaded.
            self.ensure_texture(
                image.id,
                &image.rgba_data,
                image.width_px,
                image.height_px,
                device,
                queue,
            );

            let cached = match self.texture_cache.get(&image.id) {
                Some(c) => c,
                None => continue,
            };

            // Compute display dimensions in pixels.
            // If cols_span/rows_span are specified, use them.
            // Otherwise, calculate from image pixel size and cell size.
            let display_w = if placement.cols_span > 0 {
                placement.cols_span as f32 * cell_width
            } else {
                // Auto-size: use image's native pixel width, clamped to
                // a reasonable number of columns.
                let max_cols = ((sw - padding_x * 2.0) / cell_width).floor() as usize;
                let img_cols = ((image.width_px as f32) / cell_width).ceil() as usize;
                (img_cols.min(max_cols) as f32) * cell_width
            };

            let display_h = if placement.rows_span > 0 {
                placement.rows_span as f32 * cell_height
            } else {
                // Auto-size: maintain aspect ratio based on display_w.
                if image.width_px > 0 {
                    display_w * (image.height_px as f32 / image.width_px as f32)
                } else {
                    cell_height
                }
            };

            // Pixel position of the image's top-left corner.
            let px = padding_x + placement.col as f32 * cell_width;
            let py = padding_y + placement.row as f32 * cell_height;

            // Convert to NDC.
            let x0 = (px / sw) * 2.0 - 1.0;
            let x1 = ((px + display_w) / sw) * 2.0 - 1.0;
            let y0 = 1.0 - (py / sh) * 2.0;
            let y1 = 1.0 - ((py + display_h) / sh) * 2.0;

            let vertices = [
                ImageVertex {
                    position: [x0, y0],
                    uv: [0.0, 0.0],
                },
                ImageVertex {
                    position: [x1, y0],
                    uv: [1.0, 0.0],
                },
                ImageVertex {
                    position: [x0, y1],
                    uv: [0.0, 1.0],
                },
                ImageVertex {
                    position: [x1, y0],
                    uv: [1.0, 0.0],
                },
                ImageVertex {
                    position: [x1, y1],
                    uv: [1.0, 1.0],
                },
                ImageVertex {
                    position: [x0, y1],
                    uv: [0.0, 1.0],
                },
            ];

            let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("kitty_img_vbuf"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kitty_img_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
            pass.set_bind_group(0, &cached.bind_group, &[]);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..6, 0..1);
        }
    }

    /// Remove cached textures for images that no longer exist in the store.
    pub fn cleanup_cache(&mut self, image_store: &ImageStore) {
        let active_ids: HashSet<u32> = image_store
            .visible_placements()
            .iter()
            .map(|(p, _)| p.image_id)
            .collect();
        self.texture_cache.retain(|id, _| active_ids.contains(id));
    }
}

// ── GridRenderer ───────────────────────────────────────────────────────────

/// GPU-accelerated terminal grid renderer.
///
/// Renders the alacritty_terminal grid via glyphon text + colored rect pipeline
/// for backgrounds and cursor.
/// How many frames between atlas trim operations (~16s at 60fps).
const ATLAS_TRIM_INTERVAL: u64 = 1000;

/// How many frames between image cache cleanup passes (~16s at 60fps).
const IMAGE_CLEANUP_INTERVAL: u64 = 1000;

pub struct GridRenderer {
    // Glyphon state
    font_system: FontSystem,
    swash_cache: SwashCache,
    #[allow(dead_code)]
    cache: Cache,
    atlas: TextAtlas,
    viewport: Viewport,
    text_renderer: TextRenderer,

    // Separate text renderer for overlays (HUD, scroll indicator, command labels)
    // to avoid clobbering the cell text vertex buffer when prepare() is called
    // multiple times within the same frame.
    overlay_atlas: TextAtlas,
    overlay_text_renderer: TextRenderer,

    // Rect pipeline for backgrounds and cursor
    rect_pipeline: wgpu::RenderPipeline,

    // Persistent vertex buffer for cell backgrounds / cursor / selection rects.
    // Sized to hold the maximum number of rects for the current grid dimensions.
    rect_buf: wgpu::Buffer,
    /// Maximum number of **vertices** the persistent rect buffer can hold.
    rect_buf_capacity: u64,

    // Cell metrics (computed from font at init)
    pub cell_width: f32,
    pub cell_height: f32,

    // Padding from top-left corner of the window
    padding_x: f32,
    padding_y: f32,

    // Per-row cache of cell data for damage-based rendering.
    row_cache: Vec<Option<CachedRow>>,

    // Persistent per-cell glyphon Buffers — only rebuilt for damaged rows.
    // Indexed as cell_buffers[row][col]. Each non-empty, non-space cell
    // gets its own Buffer positioned at exact grid coordinates, ensuring
    // pixel-perfect alignment even with emoji/wide chars.
    cell_buffers: Vec<Vec<Option<Buffer>>>,

    // Track last cursor position to rebuild affected cell buffers when cursor moves.
    last_cursor_pos: Option<(usize, usize)>,

    // Frame counter for throttled atlas trimming.
    frame_count: u64,

    // Persistent vertex buffer (CPU-side) for cell backgrounds / cursor / selection.
    // Cleared and refilled each frame to avoid heap allocation churn.
    rect_verts_cpu: Vec<ColorVertex>,

    // Frame timing: rolling average over the last N frames.
    frame_times_us: Vec<u64>,
    frame_time_idx: usize,
    frame_time_sum: u64,

    // Kitty graphics image render pipeline.
    image_pipeline: ImageRenderPipeline,
}

/// Estimate the maximum number of vertices needed for the rect buffer.
///
/// Each cell can produce one background rect (6 vertices). The cursor can add
/// up to 4 rects (hollow block). Selection overlays can double the cell count.
/// We over-allocate by 2x + a constant to avoid frequent reallocation.
fn estimate_rect_buf_vertices(cols: usize, rows: usize) -> u64 {
    // cells + cursor (4 rects) + selection (cells again) + small margin
    let max_rects = (rows * cols) * 2 + 8;
    (max_rects as u64) * 6 // 6 vertices per rect
}

impl GridRenderer {
    /// Create a new GridRenderer.
    ///
    /// Initializes glyphon font system, text atlas, text renderer, and
    /// the colored rect pipeline for cursor/background rendering.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        // ── Glyphon setup ────────────────────────────────────────────────
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let mut atlas = TextAtlas::new(device, queue, &cache, surface_format);
        let mut overlay_atlas = TextAtlas::new(device, queue, &cache, surface_format);
        let viewport = {
            let mut vp = Viewport::new(device, &cache);
            vp.update(queue, Resolution { width, height });
            vp
        };
        let text_renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
        let overlay_text_renderer = TextRenderer::new(
            &mut overlay_atlas,
            device,
            wgpu::MultisampleState::default(),
            None,
        );

        // ── Measure cell dimensions from font metrics ────────────────────
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
        let mut measure_buf = Buffer::new(&mut font_system, metrics);
        measure_buf.set_size(&mut font_system, Some(1000.0), Some(LINE_HEIGHT * 2.0));
        measure_buf.set_text(
            &mut font_system,
            "M",
            Attrs::new().family(Family::Name(TERM_FONT_FAMILY)),
            Shaping::Basic,
        );
        measure_buf.shape_until_scroll(&mut font_system, false);

        // Extract the advance width from the first glyph.
        let cell_width = measure_buf
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first())
            .map(|g| g.w)
            .unwrap_or(FONT_SIZE * 0.6);

        let cell_height = LINE_HEIGHT;

        debug!(cell_width, cell_height, "Grid cell metrics computed");

        // ── Rect pipeline ────────────────────────────────────────────────
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grid_rect_shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grid_rect_pipeline_layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("grid_rect_pipeline"),
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

        // ── Persistent rect vertex buffer ─────────────────────────────
        let padding_x = 4.0_f32;
        let padding_y = 4.0_f32;
        let usable_w = width as f32 - padding_x * 2.0;
        let usable_h = height as f32 - padding_y * 2.0;
        let cols = (usable_w / cell_width).floor().max(2.0) as usize;
        let rows = (usable_h / cell_height).floor().max(1.0) as usize;
        let rect_buf_capacity = estimate_rect_buf_vertices(cols, rows);
        let rect_buf_size = rect_buf_capacity * std::mem::size_of::<ColorVertex>() as u64;

        let rect_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grid_rect_vbuf_persistent"),
            size: rect_buf_size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // ── Image render pipeline (Kitty graphics) ─────────────────────
        let image_pipeline = ImageRenderPipeline::new(device, surface_format);

        Self {
            font_system,
            swash_cache,
            cache,
            atlas,
            overlay_atlas,
            viewport,
            text_renderer,
            overlay_text_renderer,
            rect_pipeline,
            rect_buf,
            rect_buf_capacity,
            cell_width,
            cell_height,
            padding_x,
            padding_y,
            row_cache: Vec::new(),
            cell_buffers: Vec::new(),
            last_cursor_pos: None,
            frame_count: 0,
            rect_verts_cpu: Vec::new(),
            frame_times_us: vec![0u64; 100],
            frame_time_idx: 0,
            frame_time_sum: 0,
            image_pipeline,
        }
    }

    /// Update the viewport resolution (call on resize).
    ///
    /// Also recreates the persistent rect buffer to match the new grid
    /// dimensions and trims the atlas (since the glyph set may change).
    pub fn resize(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32) {
        self.viewport.update(queue, Resolution { width, height });

        // Recompute persistent buffer capacity for new grid size.
        let (cols, rows) = self.grid_size(width, height);
        let new_capacity = estimate_rect_buf_vertices(cols, rows);
        if new_capacity != self.rect_buf_capacity {
            let buf_size = new_capacity * std::mem::size_of::<ColorVertex>() as u64;
            self.rect_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("grid_rect_vbuf_persistent"),
                size: buf_size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.rect_buf_capacity = new_capacity;
        }

        // Trim atlases on resize — glyph set may change.
        self.atlas.trim();
        self.overlay_atlas.trim();

        // Invalidate row cache and persistent cell buffers — resize triggers a full damage anyway.
        self.row_cache.clear();
        self.cell_buffers.clear();
        self.last_cursor_pos = None;
    }

    /// Calculate terminal grid dimensions (cols, rows) for a given pixel size.
    pub fn grid_size(&self, width: u32, height: u32) -> (usize, usize) {
        let usable_w = width as f32 - self.padding_x * 2.0;
        let usable_h = height as f32 - self.padding_y * 2.0;
        let cols = (usable_w / self.cell_width).floor().max(2.0) as usize;
        let rows = (usable_h / self.cell_height).floor().max(1.0) as usize;
        (cols, rows)
    }

    /// Get the horizontal padding from the window edge.
    pub fn padding_x(&self) -> f32 {
        self.padding_x
    }

    /// Get the vertical padding from the window edge.
    pub fn padding_y(&self) -> f32 {
        self.padding_y
    }

    /// Render inline images from the Kitty graphics protocol.
    ///
    /// Should be called AFTER cell backgrounds but BEFORE text rendering.
    /// Reads the ImageStore (briefly locking it) and delegates to the
    /// ImageRenderPipeline for textured quad rendering.
    pub fn render_images(
        &mut self,
        image_store: &ImageStore,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        self.image_pipeline.render(
            image_store,
            self.cell_width,
            self.cell_height,
            self.padding_x,
            self.padding_y,
            device,
            queue,
            encoder,
            target_view,
            surface_width,
            surface_height,
        );
    }

    /// Clean up GPU texture cache for images no longer in the store.
    pub fn cleanup_image_cache(&mut self, image_store: &ImageStore) {
        self.image_pipeline.cleanup_cache(image_store);
    }

    /// Periodically clean up image caches (GPU textures + scrolled-out placements).
    ///
    /// Uses `frame_count` with `IMAGE_CLEANUP_INTERVAL` following the same periodic
    /// pattern as the atlas trim in `render_from_cache`. Call once per frame after
    /// rendering; the interval check ensures actual work only runs every ~1000 frames.
    pub fn periodic_image_cleanup(&mut self, image_store: &mut ImageStore, max_visible_row: usize) {
        if self.frame_count % IMAGE_CLEANUP_INTERVAL == 0 {
            self.cleanup_image_cache(image_store);
            image_store.cleanup_scrolled(max_visible_row);
        }
    }

    /// Render a scroll indicator overlay when the viewport is scrolled back.
    ///
    /// Draws a small "[SCROLL +N]" badge in the top-right corner of the terminal
    /// using the rect pipeline for the background and glyphon for the text.
    pub fn render_scroll_indicator(
        &mut self,
        display_offset: usize,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        if display_offset == 0 {
            return;
        }

        let sw = surface_width as f32;
        let sh = surface_height as f32;

        let label = format!(" [SCROLL +{}] ", display_offset);
        let label_chars = label.len() as f32;
        let badge_w = label_chars * self.cell_width;
        let badge_h = self.cell_height + 4.0;
        let badge_x = sw - badge_w - self.padding_x;
        let badge_y = self.padding_y;

        // ── Badge background rect ───────────────────────────────────────
        let bg_color = PaletteColor::HOT.to_f32_array();
        let verts = pixel_rect_to_ndc(badge_x, badge_y, badge_w, badge_h, sw, sh, bg_color);
        let data = bytemuck::cast_slice::<ColorVertex, u8>(&verts);
        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("scroll_indicator_bg"),
            contents: data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scroll_indicator_bg_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..6, 0..1);
        }

        // ── Badge text ──────────────────────────────────────────────────
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
        let mut buf = Buffer::new(&mut self.font_system, metrics);
        buf.set_size(
            &mut self.font_system,
            Some(badge_w + 8.0),
            Some(badge_h + 4.0),
        );
        let text_color = PaletteColor::BG.to_f32_array();
        buf.set_text(
            &mut self.font_system,
            &label,
            Attrs::new()
                .family(Family::Name(TERM_FONT_FAMILY))
                .color(f32_to_glyph_color(text_color)),
            Shaping::Basic,
        );
        buf.shape_until_scroll(&mut self.font_system, false);

        self.viewport.update(
            queue,
            Resolution {
                width: surface_width,
                height: surface_height,
            },
        );

        let text_areas = vec![TextArea {
            buffer: &buf,
            left: badge_x,
            top: badge_y + 2.0,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: surface_width as i32,
                bottom: surface_height as i32,
            },
            default_color: GlyphColor::rgba(
                PaletteColor::BG.r,
                PaletteColor::BG.g,
                PaletteColor::BG.b,
                255,
            ),
            custom_glyphs: &[],
        }];

        if let Err(e) = self.overlay_text_renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.overlay_atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
        ) {
            tracing::warn!("scroll indicator text prepare failed: {}", e);
            return;
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scroll_indicator_text_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
                self.overlay_text_renderer
                    .render(&self.overlay_atlas, &self.viewport, &mut pass)
            {
                tracing::warn!("scroll indicator text render failed: {}", e);
            }
        }
        // Atlas trimming handled by render_from_cache frame counter; no per-call trim here.
    }

    /// Render semantic command block boundaries from OSC 633 shell integration.
    ///
    /// For each CommandBlock visible in the current viewport, draws:
    /// - A left-edge color bar (green=success, red=failure, muted=in-progress)
    /// - A thin horizontal separator line between command blocks
    /// - A faint command label at the prompt line (from the E mark text)
    ///
    /// `blocks` is a snapshot of the CommandTracker's blocks taken while the
    /// tracker lock was held briefly. `display_offset` converts absolute grid
    /// line numbers to viewport coordinates. `screen_lines` is the number of
    /// visible rows in the viewport.
    pub fn render_command_blocks(
        &mut self,
        blocks: &[CommandBlock],
        _display_offset: usize,
        screen_lines: usize,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        if blocks.is_empty() {
            return;
        }

        let sw = surface_width as f32;
        let sh = surface_height as f32;

        // Width of the left-edge color bar in pixels.
        const BAR_WIDTH: f32 = 3.0;
        // Separator line height in pixels.
        const SEP_HEIGHT: f32 = 1.0;
        // Alpha for the left-edge bar.
        const BAR_ALPHA: f32 = 0.6;
        // Alpha for separator lines.
        const SEP_ALPHA: f32 = 0.3;

        let mut rect_vertices: Vec<ColorVertex> = Vec::new();
        #[allow(unused)]
        let label_entries: Vec<(f32, f32, String, [f32; 4])> = Vec::new();

        for (i, block) in blocks.iter().enumerate() {
            // Convert absolute grid line to viewport row.
            // In alacritty, line 0 is the top of the visible area when
            // display_offset is 0. With scrollback, the viewport starts at
            // `display_offset` lines back from the bottom. Command blocks
            // store absolute grid lines counted from the top of the
            // scrollback, so we convert by subtracting the offset of the
            // first visible line.
            //
            // The grid has `total_lines` of history. The viewport shows
            // lines from `total_lines - screen_lines - display_offset` to
            // `total_lines - 1 - display_offset` (inclusive). But
            // CommandTracker stores lines as cursor.point.line.0, which is
            // relative to the visible viewport (0 = first visible line in
            // the active screen area). So for blocks created while the
            // terminal was NOT scrolled, start_line is a small number
            // (0..screen_lines). When display_offset > 0, old blocks that
            // have scrolled into history would have had a line number that
            // is now screen_lines + display_offset away.
            //
            // Simplification: CommandTracker records line numbers from
            // `term.grid().cursor.point.line.0` which is the viewport-
            // relative line at the time the mark was received. To map these
            // to the current viewport, we just use the raw values. If the
            // terminal has since scrolled (display_offset > 0), blocks that
            // were at viewport row N are now at viewport row N (they refer
            // to the active screen, not scrollback). For now, only render
            // blocks whose start_line falls within 0..screen_lines.

            let start_row = block.start_line;
            let end_row = block.end_line.unwrap_or(screen_lines.saturating_sub(1));

            // Skip blocks entirely outside the viewport.
            if start_row >= screen_lines && end_row >= screen_lines {
                continue;
            }

            // Clamp to viewport bounds.
            let vis_start = start_row.min(screen_lines.saturating_sub(1));
            let vis_end = end_row.min(screen_lines.saturating_sub(1));

            // Determine color based on exit code.
            let bar_color = match (&block.state, block.exit_code) {
                (CommandState::Finished, Some(0)) => {
                    let c = PaletteColor::WARM.to_f32_array();
                    [c[0], c[1], c[2], BAR_ALPHA]
                }
                (CommandState::Finished, Some(_)) => {
                    let c = PaletteColor::SEARING.to_f32_array();
                    [c[0], c[1], c[2], BAR_ALPHA]
                }
                _ => {
                    // In-progress or no exit code yet.
                    let c = PaletteColor::TEXT_MUTED.to_f32_array();
                    [c[0], c[1], c[2], BAR_ALPHA * 0.7]
                }
            };

            // ── Left-edge color bar ────────────────────────────────────────
            let bar_x = self.padding_x;
            let bar_y = self.padding_y + vis_start as f32 * self.cell_height;
            let bar_h = (vis_end - vis_start + 1) as f32 * self.cell_height;
            let verts = pixel_rect_to_ndc(bar_x, bar_y, BAR_WIDTH, bar_h, sw, sh, bar_color);
            rect_vertices.extend_from_slice(&verts);

            // ── Separator line between this block and the next ─────────────
            // Draw a separator at the top of each block except the first.
            if i > 0 && vis_start < screen_lines {
                let sep_y = self.padding_y + vis_start as f32 * self.cell_height;
                let sep_w = surface_width as f32 - self.padding_x * 2.0;
                let sep_color = [bar_color[0], bar_color[1], bar_color[2], SEP_ALPHA];
                let sep_verts =
                    pixel_rect_to_ndc(self.padding_x, sep_y, sep_w, SEP_HEIGHT, sw, sh, sep_color);
                rect_vertices.extend_from_slice(&sep_verts);
            }

            // Command labels omitted — the left-edge color bars and separators
            // provide sufficient visual cues without overlapping cell text.
        }

        // ── Render rect pass (bars + separators) ───────────────────────────
        if !rect_vertices.is_empty() {
            let data = bytemuck::cast_slice::<ColorVertex, u8>(&rect_vertices);
            let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cmd_block_rects"),
                contents: data,
                usage: wgpu::BufferUsages::VERTEX,
            });

            let vert_count = rect_vertices.len() as u32;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cmd_block_rect_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..vert_count, 0..1);
        }

        // ── Render command labels via glyphon ──────────────────────────────
        if !label_entries.is_empty() {
            let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
            let mut label_buffers: Vec<Buffer> = Vec::with_capacity(label_entries.len());

            for (_, _, text, color) in &label_entries {
                let mut buf = Buffer::new(&mut self.font_system, metrics);
                let available_w = sw - self.padding_x;
                buf.set_size(
                    &mut self.font_system,
                    Some(available_w),
                    Some(self.cell_height + 4.0),
                );
                buf.set_text(
                    &mut self.font_system,
                    text,
                    Attrs::new()
                        .family(Family::Name(TERM_FONT_FAMILY))
                        .color(GlyphColor::rgba(
                            (color[0] * 255.0) as u8,
                            (color[1] * 255.0) as u8,
                            (color[2] * 255.0) as u8,
                            (color[3] * 255.0) as u8,
                        )),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                label_buffers.push(buf);
            }

            self.viewport.update(
                queue,
                Resolution {
                    width: surface_width,
                    height: surface_height,
                },
            );

            let text_areas: Vec<TextArea<'_>> = label_buffers
                .iter()
                .enumerate()
                .map(|(i, buf)| {
                    let (lx, ly, _, _) = &label_entries[i];
                    TextArea {
                        buffer: buf,
                        left: *lx,
                        top: *ly,
                        scale: 1.0,
                        bounds: TextBounds {
                            left: 0,
                            top: 0,
                            right: surface_width as i32,
                            bottom: surface_height as i32,
                        },
                        default_color: GlyphColor::rgba(
                            PaletteColor::TEXT_MUTED.r,
                            PaletteColor::TEXT_MUTED.g,
                            PaletteColor::TEXT_MUTED.b,
                            128,
                        ),
                        custom_glyphs: &[],
                    }
                })
                .collect();

            if let Err(e) = self.text_renderer.prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            ) {
                tracing::warn!("Command block label text prepare failed: {}", e);
                return;
            }

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("cmd_block_text_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target_view,
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

                if let Err(e) = self
                    .text_renderer
                    .render(&self.atlas, &self.viewport, &mut pass)
                {
                    tracing::warn!("Command block label text render failed: {}", e);
                }
            }
        }
    }

    /// Render a Claude session HUD overlay in the bottom-right corner.
    ///
    /// Shows status, context percentage (thermal-gradient colored), current tool,
    /// and subagent count. Only renders when a matching session is provided.
    /// Follows the same rect-bg + glyphon-text pattern as render_scroll_indicator.
    #[allow(dead_code)]
    pub fn render_claude_hud(
        &mut self,
        session: &ClaudeSessionState,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        let sw = surface_width as f32;
        let sh = surface_height as f32;

        // ── Build HUD text lines ───────────────────────────────────────────
        let status_str = match session.status {
            ClaudeStatus::Idle => "IDLE",
            ClaudeStatus::Processing => "PROCESSING",
            ClaudeStatus::ToolUse => "TOOL_USE",
            ClaudeStatus::AwaitingInput => "AWAITING",
        };

        let context_pct = session.context_percent.unwrap_or(0.0);
        let context_str = format!("CTX {:.0}%", context_pct);

        let tool_str = session
            .current_tool
            .as_deref()
            .map(|t| format!("TOOL {}", t))
            .unwrap_or_default();

        let agents = session.subagent_count.unwrap_or(0);
        let agent_str = if agents > 0 {
            format!("AGENTS {}", agents)
        } else {
            String::new()
        };

        // Build lines with owned strings for lifetime safety.
        let mut hud_lines: Vec<String> = Vec::with_capacity(4);
        hud_lines.push(format!(" {} ", status_str));
        hud_lines.push(format!(" {} ", context_str));
        if !tool_str.is_empty() {
            hud_lines.push(format!(" {} ", tool_str));
        }
        if !agent_str.is_empty() {
            hud_lines.push(format!(" {} ", agent_str));
        }

        // ── Compute badge dimensions ───────────────────────────────────────
        let max_chars = hud_lines.iter().map(|l| l.len()).max().unwrap_or(10) as f32;
        let badge_w = max_chars * self.cell_width;
        let line_count = hud_lines.len() as f32;
        let badge_h = line_count * self.cell_height + 6.0; // 6px vertical padding
        let badge_x = sw - badge_w - self.padding_x - 4.0;
        let badge_y = sh - badge_h - self.padding_y - 4.0;

        // ── Badge background rect (BG_SURFACE at ~0.85 alpha) ──────────────
        let bg = PaletteColor::BG_SURFACE.to_f32_array();
        let bg_color = [bg[0], bg[1], bg[2], 0.85];
        let verts = pixel_rect_to_ndc(badge_x, badge_y, badge_w, badge_h, sw, sh, bg_color);
        let data = bytemuck::cast_slice::<ColorVertex, u8>(&verts);
        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("claude_hud_bg"),
            contents: data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("claude_hud_bg_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..6, 0..1);
        }

        // ── Context bar (thin thermal-gradient colored strip) ──────────────
        let bar_h = 3.0_f32;
        let bar_w = (badge_w - 8.0) * (context_pct / 100.0).clamp(0.0, 1.0);
        let bar_x = badge_x + 4.0;
        let bar_y = badge_y + self.cell_height + 2.0; // below status line
        if bar_w > 0.5 {
            let heat = (context_pct / 100.0).clamp(0.0, 1.0);
            let bar_color = thermal_gradient(heat).to_f32_array();
            let bar_verts = pixel_rect_to_ndc(bar_x, bar_y, bar_w, bar_h, sw, sh, bar_color);
            let bar_data = bytemuck::cast_slice::<ColorVertex, u8>(&bar_verts);
            let bar_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("claude_hud_ctx_bar"),
                contents: bar_data,
                usage: wgpu::BufferUsages::VERTEX,
            });

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("claude_hud_ctx_bar_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target_view,
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
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, bar_vbuf.slice(..));
                pass.draw(0..6, 0..1);
            }
        }

        // ── Badge text (all lines) ─────────────────────────────────────────
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);

        // Determine per-line text colors.
        let status_color = match session.status {
            ClaudeStatus::Idle => PaletteColor::ACCENT_COLD,
            ClaudeStatus::Processing => PaletteColor::ACCENT_WARM,
            ClaudeStatus::ToolUse => PaletteColor::SEARING,
            ClaudeStatus::AwaitingInput => PaletteColor::ACCENT_COOL,
        };

        let heat = (context_pct / 100.0).clamp(0.0, 1.0);
        let ctx_color = thermal_gradient(heat);

        let line_colors: Vec<PaletteColor> = hud_lines
            .iter()
            .enumerate()
            .map(|(i, _)| match i {
                0 => status_color,
                1 => ctx_color,
                _ => PaletteColor::TEXT_MUTED,
            })
            .collect();

        // Build per-line glyphon buffers and text areas.
        let mut line_buffers: Vec<Buffer> = Vec::with_capacity(hud_lines.len());
        for (i, line) in hud_lines.iter().enumerate() {
            let color = line_colors[i];
            let mut buf = Buffer::new(&mut self.font_system, metrics);
            buf.set_size(
                &mut self.font_system,
                Some(badge_w + 8.0),
                Some(self.cell_height + 4.0),
            );
            buf.set_text(
                &mut self.font_system,
                line,
                Attrs::new()
                    .family(Family::Name(TERM_FONT_FAMILY))
                    .color(GlyphColor::rgba(color.r, color.g, color.b, 255)),
                Shaping::Basic,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            line_buffers.push(buf);
        }

        self.viewport.update(
            queue,
            Resolution {
                width: surface_width,
                height: surface_height,
            },
        );

        let text_areas: Vec<TextArea<'_>> = line_buffers
            .iter()
            .enumerate()
            .map(|(i, buf)| TextArea {
                buffer: buf,
                left: badge_x,
                top: badge_y + 3.0 + i as f32 * self.cell_height,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: surface_width as i32,
                    bottom: surface_height as i32,
                },
                default_color: GlyphColor::rgba(
                    PaletteColor::TEXT.r,
                    PaletteColor::TEXT.g,
                    PaletteColor::TEXT.b,
                    255,
                ),
                custom_glyphs: &[],
            })
            .collect();

        if let Err(e) = self.overlay_text_renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.overlay_atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
        ) {
            tracing::warn!("Claude HUD text prepare failed: {}", e);
            return;
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("claude_hud_text_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
                self.overlay_text_renderer
                    .render(&self.overlay_atlas, &self.viewport, &mut pass)
            {
                tracing::warn!("Claude HUD text render failed: {}", e);
            }
        }
    }

    /// Render a context saturation warning bar at the top of the terminal.
    ///
    /// - At 85-94%: subtle warning bar with WARM/HOT colors
    /// - At 95%+: prominent critical bar with SEARING/CRITICAL colors and
    ///   a prompt to spawn a continuation session
    pub fn render_context_warning(
        &mut self,
        context_percent: f32,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        let sw = surface_width as f32;
        let _sh = surface_height as f32;

        let critical = context_percent >= 95.0;

        // ── Build warning text ──────────────────────────────────────────
        let text = if critical {
            format!(
                " Context saturated ({:.0}%) \u{2014} Press Ctrl+Shift+N to spawn continuation ",
                context_percent
            )
        } else {
            format!(
                " Context: {:.0}% \u{2014} approaching limit ",
                context_percent
            )
        };

        // ── Bar dimensions ──────────────────────────────────────────────
        let bar_h = self.cell_height + 4.0;
        let bar_w = sw;
        let bar_x = 0.0;
        let bar_y = 0.0;

        // ── Bar background ──────────────────────────────────────────────
        let bg_color = if critical {
            let c = PaletteColor::CRITICAL.to_f32_array();
            [c[0], c[1], c[2], 0.90]
        } else {
            let c = PaletteColor::HOT.to_f32_array();
            [c[0], c[1], c[2], 0.70]
        };

        let verts = pixel_rect_to_ndc(bar_x, bar_y, bar_w, bar_h, sw, _sh, bg_color);
        let data = bytemuck::cast_slice::<ColorVertex, u8>(&verts);
        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("context_warning_bg"),
            contents: data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("context_warning_bg_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..6, 0..1);
        }

        // ── Warning text ────────────────────────────────────────────────
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
        let text_color = if critical {
            PaletteColor::WHITE_HOT
        } else {
            PaletteColor::BG
        };

        let mut buf = Buffer::new(&mut self.font_system, metrics);
        buf.set_size(&mut self.font_system, Some(sw), Some(bar_h));
        buf.set_text(
            &mut self.font_system,
            &text,
            Attrs::new()
                .family(Family::Name(TERM_FONT_FAMILY))
                .color(GlyphColor::rgba(
                    text_color.r,
                    text_color.g,
                    text_color.b,
                    255,
                )),
            Shaping::Basic,
        );
        buf.shape_until_scroll(&mut self.font_system, false);

        self.viewport.update(
            queue,
            Resolution {
                width: surface_width,
                height: surface_height,
            },
        );

        let text_areas = vec![TextArea {
            buffer: &buf,
            left: self.padding_x,
            top: 2.0,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: surface_width as i32,
                bottom: surface_height as i32,
            },
            default_color: GlyphColor::rgba(text_color.r, text_color.g, text_color.b, 255),
            custom_glyphs: &[],
        }];

        if let Err(e) = self.overlay_text_renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.overlay_atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
        ) {
            tracing::warn!("Context warning text prepare failed: {}", e);
            return;
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("context_warning_text_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
                self.overlay_text_renderer
                    .render(&self.overlay_atlas, &self.viewport, &mut pass)
            {
                tracing::warn!("Context warning text render failed: {}", e);
            }
        }
    }

    /// Render the agent timeline bar at the bottom of the window.
    ///
    /// Each tool entry is a colored horizontal segment. Time axis has newest
    /// entries on the right. The current (active) tool pulses with alpha
    /// oscillation. Tool names are rendered for entries wider than 50px.
    pub fn render_agent_timeline(
        &mut self,
        timeline: &AgentTimeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        if !timeline.visible || timeline.entries.is_empty() {
            return;
        }

        let sw = surface_width as f32;
        let sh = surface_height as f32;
        let bar_h = TIMELINE_BAR_HEIGHT as f32;
        let bar_y = sh - bar_h;

        // ── Dark background rect ──────────────────────────────────────────
        let bg = PaletteColor::BG.to_f32_array();
        let bg_color = [bg[0], bg[1], bg[2], 0.92];
        let bg_verts = pixel_rect_to_ndc(0.0, bar_y, sw, bar_h, sw, sh, bg_color);
        let bg_data = bytemuck::cast_slice::<ColorVertex, u8>(&bg_verts);
        let bg_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("timeline_bg"),
            contents: bg_data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("timeline_bg_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_vertex_buffer(0, bg_vbuf.slice(..));
            pass.draw(0..6, 0..1);
        }

        // ── Thin separator line at top of timeline bar ────────────────────
        let sep_color = PaletteColor::TEXT_MUTED.to_f32_array();
        let sep_color_dim = [sep_color[0], sep_color[1], sep_color[2], 0.4];
        let sep_verts = pixel_rect_to_ndc(0.0, bar_y, sw, 1.0, sw, sh, sep_color_dim);
        let sep_data = bytemuck::cast_slice::<ColorVertex, u8>(&sep_verts);
        let sep_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("timeline_sep"),
            contents: sep_data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("timeline_sep_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_vertex_buffer(0, sep_vbuf.slice(..));
            pass.draw(0..6, 0..1);
        }

        // ── Compute time range ────────────────────────────────────────────
        let now = Instant::now();
        let content_y = bar_y + 4.0; // top padding inside the bar
        let content_h = bar_h - 8.0; // vertical space for segments
        let content_x = 4.0; // left padding
        let content_w = sw - 8.0; // usable width for segments

        // The total visible time window: we show seconds_per_pixel * content_w seconds.
        // Use a fixed scale: 120 seconds across the full width.
        let visible_seconds: f64 = 120.0;
        let pixels_per_second = content_w as f64 / visible_seconds;

        // The right edge of the bar is "now - scroll_offset".
        let right_time = now;
        let scroll_secs = timeline.scroll_offset;

        // ── Collect segment rects and label positions ──────────────────────
        let mut segment_verts: Vec<ColorVertex> = Vec::new();
        let mut label_entries: Vec<(f32, f32, f32, String, PaletteColor)> = Vec::new();

        // Current time elapsed for pulse animation.
        // Pulse animation: oscillate alpha using the renderer's frame counter.
        let pulse_t = (self.frame_count as f32 * 0.05).sin() * 0.5 + 0.5; // 0..1 oscillation

        for entry in timeline.entries.iter() {
            let entry_end = entry.end_time.unwrap_or(now);

            // Time from right edge (in seconds). Positive = further back in time.
            let end_offset_secs = right_time.duration_since(entry_end).as_secs_f64() + scroll_secs;
            let start_offset_secs =
                right_time.duration_since(entry.start_time).as_secs_f64() + scroll_secs;

            // Convert to pixel positions from the right edge.
            let x_right = content_x + content_w - (end_offset_secs * pixels_per_second) as f32;
            let x_left = content_x + content_w - (start_offset_secs * pixels_per_second) as f32;

            // Clamp to visible area.
            let x0 = x_left.max(content_x);
            let x1 = x_right.min(content_x + content_w);

            if x1 <= x0 || x1 < content_x || x0 > content_x + content_w {
                continue; // Off-screen
            }

            let segment_w = x1 - x0;

            // Determine color from tool category.
            let base_color = match entry.category {
                ToolCategory::Read => PaletteColor::COOL,
                ToolCategory::Write => PaletteColor::HOT,
                ToolCategory::Execute => PaletteColor::HOTTER,
                ToolCategory::Thinking => PaletteColor::MILD,
                ToolCategory::Idle => PaletteColor::FREEZING,
            };

            let mut color_arr = base_color.to_f32_array();

            // Pulse the active (current) entry.
            if entry.end_time.is_none() {
                let alpha = 0.6 + 0.4 * pulse_t;
                color_arr[3] = alpha;
            } else {
                color_arr[3] = 0.75;
            }

            // Idle entries are more transparent.
            if entry.category == ToolCategory::Idle {
                color_arr[3] *= 0.3;
            }

            // Add segment rect vertices.
            let verts = pixel_rect_to_ndc(x0, content_y, segment_w, content_h, sw, sh, color_arr);
            segment_verts.extend_from_slice(&verts);

            // Add thin separator between entries (1px wide line at the right edge).
            if segment_w > 2.0 {
                let line_color = [bg[0], bg[1], bg[2], 0.6];
                let line_verts =
                    pixel_rect_to_ndc(x1 - 1.0, content_y, 1.0, content_h, sw, sh, line_color);
                segment_verts.extend_from_slice(&line_verts);
            }

            // Collect label if entry is wide enough.
            // Use dark text on bright segments (Hot/Hotter) for contrast.
            if segment_w > 50.0 {
                let text_color = match entry.category {
                    ToolCategory::Idle => PaletteColor::TEXT_MUTED,
                    ToolCategory::Execute | ToolCategory::Write => PaletteColor::BG,
                    _ => PaletteColor::TEXT_BRIGHT,
                };
                label_entries.push((
                    x0 + 4.0,
                    segment_w - 8.0,
                    content_y,
                    entry.tool_name.clone(),
                    text_color,
                ));
            }
        }

        // ── Draw segment rects ────────────────────────────────────────────
        if !segment_verts.is_empty() {
            let seg_data = bytemuck::cast_slice::<ColorVertex, u8>(&segment_verts);
            let seg_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("timeline_segments"),
                contents: seg_data,
                usage: wgpu::BufferUsages::VERTEX,
            });

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("timeline_segments_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target_view,
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
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, seg_vbuf.slice(..));
                pass.draw(0..segment_verts.len() as u32, 0..1);
            }
        }

        // ── Draw tool name labels ─────────────────────────────────────────
        if !label_entries.is_empty() {
            let metrics = Metrics::new(FONT_SIZE * 0.75, LINE_HEIGHT * 0.75);

            let mut label_buffers: Vec<Buffer> = Vec::with_capacity(label_entries.len());
            for (_, max_w, _, text, color) in &label_entries {
                let mut buf = Buffer::new(&mut self.font_system, metrics);
                buf.set_size(&mut self.font_system, Some(*max_w), Some(content_h));
                buf.set_text(
                    &mut self.font_system,
                    text,
                    Attrs::new()
                        .family(Family::Name(TERM_FONT_FAMILY))
                        .color(GlyphColor::rgba(color.r, color.g, color.b, 220)),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                label_buffers.push(buf);
            }

            self.viewport.update(
                queue,
                Resolution {
                    width: surface_width,
                    height: surface_height,
                },
            );

            let text_areas: Vec<TextArea<'_>> = label_buffers
                .iter()
                .enumerate()
                .map(|(i, buf)| {
                    let (x, _, y, _, _) = &label_entries[i];
                    TextArea {
                        buffer: buf,
                        left: *x,
                        top: *y + (content_h - LINE_HEIGHT * 0.75) / 2.0,
                        scale: 1.0,
                        bounds: TextBounds {
                            left: 0,
                            top: 0,
                            right: surface_width as i32,
                            bottom: surface_height as i32,
                        },
                        default_color: GlyphColor::rgba(
                            PaletteColor::TEXT.r,
                            PaletteColor::TEXT.g,
                            PaletteColor::TEXT.b,
                            220,
                        ),
                        custom_glyphs: &[],
                    }
                })
                .collect();

            if let Err(e) = self.overlay_text_renderer.prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.overlay_atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            ) {
                tracing::warn!("Timeline text prepare failed: {}", e);
                return;
            }

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("timeline_text_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target_view,
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

                if let Err(e) = self.overlay_text_renderer.render(
                    &self.overlay_atlas,
                    &self.viewport,
                    &mut pass,
                ) {
                    tracing::warn!("Timeline text render failed: {}", e);
                }
            }
        }
    }

    /// Render the terminal grid with damage tracking.
    ///
    /// Takes pre-collected `RenderCell` snapshots (only from damaged rows when
    /// partial damage is available) and cursor info.
    /// `damaged_rows`: None means full redraw; Some(set) means only those rows changed.
    /// Renders cell backgrounds, cursor, and text into the given encoder.
    /// The target_view should already have been cleared to BG by the caller.
    pub fn render(
        &mut self,
        cells: &[RenderCell],
        cursor: &RenderableCursor,
        screen_lines: usize,
        selection: Option<&SelectionRange>,
        display_offset: usize,
        damaged_rows: Option<&HashSet<usize>>,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        // ── Update row cache ────────────────────────────────────────────
        // Ensure the cache has the right number of rows.
        if self.row_cache.len() != screen_lines {
            self.row_cache.resize_with(screen_lines, || None);
        }

        // Group incoming cells by row and update cache.
        let mut new_row_cells: Vec<Vec<RenderCell>> = vec![Vec::new(); screen_lines];
        for cell in cells {
            if cell.row < screen_lines {
                new_row_cells[cell.row].push(RenderCell {
                    row: cell.row,
                    col: cell.col,
                    c: cell.c,
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                });
            }
        }

        // Update damaged rows in the cache.
        for (row_idx, row_cells) in new_row_cells.into_iter().enumerate() {
            let is_damaged = match damaged_rows {
                None => true, // Full redraw — update all rows.
                Some(set) => set.contains(&row_idx),
            };
            if is_damaged {
                if row_cells.is_empty() {
                    self.row_cache[row_idx] = None;
                } else {
                    self.row_cache[row_idx] = Some(CachedRow { cells: row_cells });
                }
            }
        }

        // Render from the full cache, passing damage info for buffer reuse.
        self.render_from_cache(
            cursor,
            screen_lines,
            selection,
            display_offset,
            damaged_rows,
            device,
            queue,
            encoder,
            target_view,
            surface_width,
            surface_height,
        );
    }

    /// Render using only the existing row cache (no new cell data).
    ///
    /// Used when damage tracking reports zero damaged lines — the cursor or
    /// selection may still need re-rendering but the cell content is unchanged.
    pub fn render_cached(
        &mut self,
        cursor: &RenderableCursor,
        screen_lines: usize,
        selection: Option<&SelectionRange>,
        display_offset: usize,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        // No cell damage — pass empty set so only cursor-affected rows rebuild.
        let empty = HashSet::new();
        self.render_from_cache(
            cursor,
            screen_lines,
            selection,
            display_offset,
            Some(&empty),
            device,
            queue,
            encoder,
            target_view,
            surface_width,
            surface_height,
        );
    }

    /// Internal: render the terminal grid from the row cache.
    ///
    /// `damaged_rows`: `None` = full redraw (all buffers rebuilt),
    /// `Some(set)` = only those rows (plus cursor-affected rows) are rebuilt.
    fn render_from_cache(
        &mut self,
        cursor: &RenderableCursor,
        screen_lines: usize,
        selection: Option<&SelectionRange>,
        display_offset: usize,
        damaged_rows: Option<&HashSet<usize>>,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        let frame_start = Instant::now();

        let sw = surface_width as f32;
        let sh = surface_height as f32;

        // ── Collect background rects from all cached rows ───────────────
        let mut bg_rects: Vec<([f32; 4], [f32; 4])> = Vec::new();

        for cached in &self.row_cache {
            if let Some(row) = cached {
                for cell in &row.cells {
                    let bg_color = cell_bg_color(cell);
                    if let Some(bg) = bg_color {
                        let x = self.padding_x + cell.col as f32 * self.cell_width;
                        let y = self.padding_y + cell.row as f32 * self.cell_height;
                        let w = if cell.flags.contains(Flags::WIDE_CHAR) {
                            self.cell_width * 2.0
                        } else {
                            self.cell_width
                        };
                        bg_rects.push(([x, y, w, self.cell_height], bg));
                    }
                }
            }
        }

        // ── Cursor rect ──────────────────────────────────────────────────
        if cursor.shape != CursorShape::Hidden {
            let cursor_row = cursor.point.line.0 as usize;
            if cursor_row < screen_lines {
                let col_idx = cursor.point.column.0;
                let cx = self.padding_x + col_idx as f32 * self.cell_width;
                let cy = self.padding_y + cursor_row as f32 * self.cell_height;
                let cursor_color = PaletteColor::TEXT_BRIGHT.to_f32_array();

                match cursor.shape {
                    CursorShape::Block => {
                        bg_rects.push(([cx, cy, self.cell_width, self.cell_height], cursor_color));
                    }
                    CursorShape::Underline => {
                        let h = 2.0;
                        bg_rects.push((
                            [cx, cy + self.cell_height - h, self.cell_width, h],
                            cursor_color,
                        ));
                    }
                    CursorShape::Beam => {
                        bg_rects.push(([cx, cy, 2.0, self.cell_height], cursor_color));
                    }
                    CursorShape::HollowBlock => {
                        let t = 1.0;
                        bg_rects.push(([cx, cy, self.cell_width, t], cursor_color));
                        bg_rects.push((
                            [cx, cy + self.cell_height - t, self.cell_width, t],
                            cursor_color,
                        ));
                        bg_rects.push(([cx, cy, t, self.cell_height], cursor_color));
                        bg_rects.push((
                            [cx + self.cell_width - t, cy, t, self.cell_height],
                            cursor_color,
                        ));
                    }
                    CursorShape::Hidden => {}
                }
            }
        }

        // ── Selection highlight rects ────────────────────────────────────
        // Draw a semi-transparent highlight over selected cells.
        if let Some(sel) = selection {
            let sel_color = PaletteColor::ACCENT_COOL.to_f32_array();
            let sel_highlight = [sel_color[0], sel_color[1], sel_color[2], 0.35];

            for cached in &self.row_cache {
                if let Some(row) = cached {
                    for cell in &row.cells {
                        let grid_line = Line(cell.row as i32 - display_offset as i32);
                        let point = Point::new(grid_line, Column(cell.col));
                        if sel.contains(point) {
                            let x = self.padding_x + cell.col as f32 * self.cell_width;
                            let y = self.padding_y + cell.row as f32 * self.cell_height;
                            let w = if cell.flags.contains(Flags::WIDE_CHAR) {
                                self.cell_width * 2.0
                            } else {
                                self.cell_width
                            };
                            bg_rects.push(([x, y, w, self.cell_height], sel_highlight));
                        }
                    }
                }
            }
        }

        // ── Write rect vertices into persistent buffer ──────────────────
        // Reuse the persistent CPU-side Vec to avoid allocation each frame.
        self.rect_verts_cpu.clear();
        for (xywh, color) in &bg_rects {
            let verts = pixel_rect_to_ndc(xywh[0], xywh[1], xywh[2], xywh[3], sw, sh, *color);
            self.rect_verts_cpu.extend_from_slice(&verts);
        }

        let rect_vertex_count = self.rect_verts_cpu.len() as u32;

        if !self.rect_verts_cpu.is_empty() {
            let needed = self.rect_verts_cpu.len() as u64;

            // If the persistent buffer is too small, reallocate it.
            if needed > self.rect_buf_capacity {
                let new_capacity = needed * 2; // double to avoid frequent realloc
                let buf_size = new_capacity * std::mem::size_of::<ColorVertex>() as u64;
                self.rect_buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("grid_rect_vbuf_persistent"),
                    size: buf_size,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.rect_buf_capacity = new_capacity;
            }

            let data = bytemuck::cast_slice::<ColorVertex, u8>(&self.rect_verts_cpu);
            queue.write_buffer(&self.rect_buf, 0, data);
        }

        // ── Rebuild only damaged per-cell glyphon Buffers ────────────────
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);

        let cursor_row = cursor.point.line.0 as usize;
        let cursor_col = cursor.point.column.0;

        // Ensure cell_buffers has enough rows.
        while self.cell_buffers.len() < screen_lines {
            self.cell_buffers.push(Vec::new());
        }
        self.cell_buffers.truncate(screen_lines);

        // Determine which rows need their cell Buffers rebuilt.
        let prev_cursor = self.last_cursor_pos;
        let full_rebuild = damaged_rows.is_none();

        for (row_idx, cached) in self.row_cache.iter().enumerate() {
            if row_idx >= screen_lines {
                break;
            }

            let needs_rebuild = if full_rebuild {
                true
            } else {
                let in_damage_set = damaged_rows
                    .map(|set| set.contains(&row_idx))
                    .unwrap_or(false);
                let is_cursor_row = row_idx == cursor_row;
                let was_cursor_row = prev_cursor.map(|(r, _)| r == row_idx).unwrap_or(false);
                in_damage_set || is_cursor_row || was_cursor_row
            };

            if !needs_rebuild {
                continue;
            }

            let row = match cached {
                Some(r) => r,
                None => {
                    self.cell_buffers[row_idx].clear();
                    continue;
                }
            };

            let row_cells = &row.cells;
            if row_cells.is_empty() {
                self.cell_buffers[row_idx].clear();
                continue;
            }

            // Determine max column to size the cell buffer row.
            let max_col = row_cells.iter().map(|c| c.col).max().unwrap_or(0) + 1;
            while self.cell_buffers[row_idx].len() < max_col {
                self.cell_buffers[row_idx].push(None);
            }

            // Mark all columns as empty first (for cells that disappeared).
            for slot in self.cell_buffers[row_idx].iter_mut() {
                *slot = None;
            }

            // (debug logging removed)

            // Build per-cell Buffers.
            for cell in row_cells {
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                let ch = if cell.c == '\0' { ' ' } else { cell.c };

                // Determine foreground color (with cursor inversion).
                let is_block_cursor = cursor.shape == CursorShape::Block
                    && cursor_col == cell.col
                    && cursor_row == cell.row;
                let fg = if is_block_cursor {
                    TERM_BG
                } else if cell.flags.contains(Flags::INVERSE) {
                    ansi_to_glyphon_bg(&cell.bg).unwrap_or(TERM_BG)
                } else {
                    ansi_to_glyphon_fg(&cell.fg)
                };

                // Skip pure spaces (no need to render — background handles them).
                // Exception: cursor cell (needs inverted text rendered).
                if ch == ' ' && !is_block_cursor {
                    continue;
                }

                let buf_width = if cell.flags.contains(Flags::WIDE_CHAR) {
                    self.cell_width * 2.0
                } else {
                    self.cell_width
                };

                let buf = self.cell_buffers[row_idx][cell.col]
                    .get_or_insert_with(|| Buffer::new(&mut self.font_system, metrics));
                buf.set_metrics(&mut self.font_system, metrics);
                buf.set_size(
                    &mut self.font_system,
                    Some(buf_width + 4.0),
                    Some(self.cell_height + 4.0),
                );

                let s: String = ch.to_string();
                let attrs = Attrs::new()
                    .family(Family::Name(TERM_FONT_FAMILY))
                    .color(f32_to_glyph_color(fg));
                buf.set_text(&mut self.font_system, &s, attrs, Shaping::Basic);
                buf.shape_until_scroll(&mut self.font_system, false);
            }
        }

        // Update cursor tracking for next frame.
        self.last_cursor_pos = Some((cursor_row, cursor_col));

        // ── Update viewport ──────────────────────────────────────────────
        self.viewport.update(
            queue,
            Resolution {
                width: surface_width,
                height: surface_height,
            },
        );

        // ── Prepare glyphon text from persistent cell_buffers ────────────
        let pad_x = self.padding_x;
        let pad_y = self.padding_y;
        let cw = self.cell_width;
        let ch = self.cell_height;
        let text_areas: Vec<TextArea<'_>> = self
            .cell_buffers
            .iter()
            .enumerate()
            .flat_map(|(row_idx, row)| {
                row.iter()
                    .enumerate()
                    .filter_map(move |(col_idx, opt_buf)| {
                        let buf = opt_buf.as_ref()?;
                        Some(TextArea {
                            buffer: buf,
                            left: pad_x + col_idx as f32 * cw,
                            top: pad_y + row_idx as f32 * ch,
                            scale: 1.0,
                            bounds: TextBounds {
                                left: 0,
                                top: 0,
                                right: surface_width as i32,
                                bottom: surface_height as i32,
                            },
                            default_color: GlyphColor::rgba(
                                PaletteColor::TEXT.r,
                                PaletteColor::TEXT.g,
                                PaletteColor::TEXT.b,
                                255,
                            ),
                            custom_glyphs: &[],
                        })
                    })
            })
            .collect();

        let has_text = !text_areas.is_empty();
        if has_text {
            if let Err(e) = self.text_renderer.prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            ) {
                tracing::warn!("glyphon prepare failed: {}", e);
            }
        }

        // ── Render pass: backgrounds + cursor rects ──────────────────────
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("grid_rect_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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

            if rect_vertex_count > 0 {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, self.rect_buf.slice(..));
                pass.draw(0..rect_vertex_count, 0..1);
            }
        }

        // ── Render pass: text on top ─────────────────────────────────────
        if has_text {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("grid_text_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
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

            if let Err(e) = self
                .text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
            {
                tracing::warn!("glyphon render failed: {}", e);
            }
        }

        // Trim atlas periodically to free unused glyphs (not every frame).
        self.frame_count += 1;
        if self.frame_count % ATLAS_TRIM_INTERVAL == 0 {
            self.atlas.trim();
            self.overlay_atlas.trim();
        }

        // ── Frame timing ────────────────────────────────────────────────
        let elapsed_us = frame_start.elapsed().as_micros() as u64;

        // Update rolling average (circular buffer of 100 samples).
        let idx = self.frame_time_idx % self.frame_times_us.len();
        self.frame_time_sum = self
            .frame_time_sum
            .wrapping_sub(self.frame_times_us[idx])
            .wrapping_add(elapsed_us);
        self.frame_times_us[idx] = elapsed_us;
        self.frame_time_idx = self.frame_time_idx.wrapping_add(1);

        // Log if this frame exceeds 2ms.
        if elapsed_us > 2000 {
            debug!(
                elapsed_us,
                frame = self.frame_count,
                "grid render frame exceeded 2ms"
            );
        }

        // Log rolling average every 100 frames.
        if self.frame_count % 100 == 0 {
            let n = self.frame_times_us.len() as u64;
            let avg_us = self.frame_time_sum / n;
            debug!(
                avg_us,
                frame = self.frame_count,
                "grid render 100-frame avg"
            );
        }
    }
}

// ── Color mapping helpers ──────────────────────────────────────────────────

/// Map an alacritty_terminal ANSI Color to an [f32; 4] RGBA array.
fn ansi_to_glyphon_fg(color: &AnsiColor) -> [f32; 4] {
    match color {
        AnsiColor::Named(named) => named_to_thermal_fg(*named),
        AnsiColor::Spec(rgb) => [
            rgb.r as f32 / 255.0,
            rgb.g as f32 / 255.0,
            rgb.b as f32 / 255.0,
            1.0,
        ],
        AnsiColor::Indexed(idx) => indexed_color(*idx),
    }
}

fn ansi_to_glyphon_bg(color: &AnsiColor) -> Option<[f32; 4]> {
    match color {
        // Default/Background and Black (index 0) both mean "no background rect" —
        // let the clear-pass BG (ThermalPalette::BG / 0x0a0010) show through.
        AnsiColor::Named(NamedColor::Background) => None,
        AnsiColor::Named(NamedColor::Black) => None,
        AnsiColor::Named(named) => Some(named_to_thermal_bg(*named)),
        AnsiColor::Spec(rgb) => Some([
            rgb.r as f32 / 255.0,
            rgb.g as f32 / 255.0,
            rgb.b as f32 / 255.0,
            1.0,
        ]),
        AnsiColor::Indexed(idx) => {
            if *idx == 0 {
                None
            } else {
                Some(indexed_color(*idx))
            }
        }
    }
}

/// Map named ANSI colors to thermal palette foreground colors.
///
/// Spread across the full thermal spectrum: dark bg → blue → teal → green →
/// yellow → orange → red → white-hot.  Avoids clustering everything in the
/// purple/indigo range.
fn named_to_thermal_fg(named: NamedColor) -> [f32; 4] {
    match named {
        NamedColor::Black => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::Red => PaletteColor::SEARING.to_f32_array(),
        NamedColor::Green => PaletteColor::WARM.to_f32_array(),
        NamedColor::Yellow => PaletteColor::HOT.to_f32_array(),
        NamedColor::Blue => PaletteColor::ACCENT_COOL.to_f32_array(),
        NamedColor::Magenta => PaletteColor::HOTTER.to_f32_array(),
        NamedColor::Cyan => PaletteColor::ACCENT_NEUTRAL.to_f32_array(),
        NamedColor::White | NamedColor::Foreground => PaletteColor::TEXT_BRIGHT.to_f32_array(),

        NamedColor::BrightBlack => [0.40, 0.38, 0.45, 1.0], // neutral gray with slight warmth
        NamedColor::BrightRed => PaletteColor::CRITICAL.to_f32_array(),
        NamedColor::BrightGreen => PaletteColor::WARM.to_f32_array(),
        NamedColor::BrightYellow => PaletteColor::WHITE_HOT.to_f32_array(),
        NamedColor::BrightBlue => PaletteColor::ACCENT_COOL.to_f32_array(),
        NamedColor::BrightMagenta => PaletteColor::ACCENT_WARM.to_f32_array(),
        NamedColor::BrightCyan => PaletteColor::MILD.to_f32_array(),
        NamedColor::BrightWhite | NamedColor::BrightForeground => {
            PaletteColor::WHITE_HOT.to_f32_array()
        }

        NamedColor::DimBlack => TERM_BG,
        NamedColor::DimRed => PaletteColor::SEARING.to_f32_array(),
        NamedColor::DimGreen => PaletteColor::MILD.to_f32_array(),
        NamedColor::DimYellow => PaletteColor::HOTTER.to_f32_array(),
        NamedColor::DimBlue => PaletteColor::COOL.to_f32_array(),
        NamedColor::DimMagenta => PaletteColor::ACCENT_COLD.to_f32_array(),
        NamedColor::DimCyan => [0.08, 0.45, 0.42, 1.0], // muted teal
        NamedColor::DimWhite | NamedColor::DimForeground => PaletteColor::TEXT_MUTED.to_f32_array(),

        NamedColor::Background => TERM_BG,
        NamedColor::Cursor => PaletteColor::WHITE_HOT.to_f32_array(),
    }
}

/// Map named ANSI colors to thermal palette background colors.
///
/// Bright/Dim variants use muted/dark palette entries so they don't paint
/// vivid colored backgrounds. `Black` and `Background` are handled upstream
/// in `ansi_to_glyphon_bg` (both return `None` → transparent), so those arms
/// are retained here only as a safety fallback.
fn named_to_thermal_bg(named: NamedColor) -> [f32; 4] {
    match named {
        // Standard backgrounds
        NamedColor::Black => TERM_BG,
        NamedColor::Red => PaletteColor::SEARING.to_f32_array(),
        NamedColor::Green => PaletteColor::WARM.to_f32_array(),
        NamedColor::Yellow => PaletteColor::HOT.to_f32_array(),
        NamedColor::Blue => PaletteColor::COOL.to_f32_array(),
        NamedColor::Magenta => PaletteColor::HOTTER.to_f32_array(),
        NamedColor::Cyan => [0.05, 0.36, 0.33, 1.0], // dark teal
        NamedColor::White => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::Foreground => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::Background => TERM_BG,
        NamedColor::Cursor => PaletteColor::BG_SURFACE.to_f32_array(),

        // Bright backgrounds — use muted/dark variants, never vivid foreground colors
        NamedColor::BrightBlack => PaletteColor::BG_LIGHT.to_f32_array(),
        NamedColor::BrightRed => PaletteColor::CRITICAL.to_f32_array(),
        NamedColor::BrightGreen => PaletteColor::MILD.to_f32_array(),
        NamedColor::BrightYellow => PaletteColor::HOT.to_f32_array(),
        NamedColor::BrightBlue => PaletteColor::COOL.to_f32_array(),
        NamedColor::BrightMagenta => PaletteColor::FREEZING.to_f32_array(),
        NamedColor::BrightCyan => [0.04, 0.28, 0.26, 1.0], // muted dark teal
        NamedColor::BrightWhite => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::BrightForeground => PaletteColor::TEXT_MUTED.to_f32_array(),

        // Dim backgrounds — use deep dark palette entries
        NamedColor::DimBlack => TERM_BG,
        NamedColor::DimRed => PaletteColor::FREEZING.to_f32_array(),
        NamedColor::DimGreen => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::DimYellow => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::DimBlue => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::DimMagenta => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::DimCyan => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::DimWhite => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::DimForeground => PaletteColor::TEXT_MUTED.to_f32_array(),
    }
}

/// Standard xterm-256 color palette lookup.
fn indexed_color(idx: u8) -> [f32; 4] {
    match idx {
        0 => named_to_thermal_fg(NamedColor::Black),
        1 => named_to_thermal_fg(NamedColor::Red),
        2 => named_to_thermal_fg(NamedColor::Green),
        3 => named_to_thermal_fg(NamedColor::Yellow),
        4 => named_to_thermal_fg(NamedColor::Blue),
        5 => named_to_thermal_fg(NamedColor::Magenta),
        6 => named_to_thermal_fg(NamedColor::Cyan),
        7 => named_to_thermal_fg(NamedColor::White),
        8 => named_to_thermal_fg(NamedColor::BrightBlack),
        9 => named_to_thermal_fg(NamedColor::BrightRed),
        10 => named_to_thermal_fg(NamedColor::BrightGreen),
        11 => named_to_thermal_fg(NamedColor::BrightYellow),
        12 => named_to_thermal_fg(NamedColor::BrightBlue),
        13 => named_to_thermal_fg(NamedColor::BrightMagenta),
        14 => named_to_thermal_fg(NamedColor::BrightCyan),
        15 => named_to_thermal_fg(NamedColor::BrightWhite),

        // 216-color cube (indices 16..=231).
        16..=231 => {
            let idx = idx - 16;
            let r_idx = idx / 36;
            let g_idx = (idx % 36) / 6;
            let b_idx = idx % 6;
            let r = if r_idx == 0 { 0 } else { 55 + r_idx * 40 };
            let g = if g_idx == 0 { 0 } else { 55 + g_idx * 40 };
            let b = if b_idx == 0 { 0 } else { 55 + b_idx * 40 };
            [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
        }

        // 24-step grayscale (indices 232..=255).
        232..=255 => {
            let level = 8 + (idx - 232) * 10;
            let v = level as f32 / 255.0;
            [v, v, v, 1.0]
        }
    }
}

/// Determine the background color for a cell (returns None for default BG).
fn cell_bg_color(cell: &RenderCell) -> Option<[f32; 4]> {
    if cell.flags.contains(Flags::INVERSE) {
        Some(ansi_to_glyphon_fg(&cell.fg))
    } else {
        ansi_to_glyphon_bg(&cell.bg)
    }
}

/// Convert pixel-space rect to 6 NDC vertices (two triangles).
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

/// Convert an [f32; 4] RGBA color to a glyphon Color.
fn f32_to_glyph_color(c: [f32; 4]) -> GlyphColor {
    GlyphColor::rgba(
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8,
        (c[3] * 255.0) as u8,
    )
}
