//! Image render GPU pipeline — Kitty graphics protocol inline images.

use std::collections::{HashMap, HashSet};

use tracing::debug;
use wgpu::util::DeviceExt;

use crate::kitty_graphics::ImageStore;

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
pub(crate) struct ImageRenderPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Cache of GPU textures keyed by image ID to avoid re-uploading each frame.
    texture_cache: HashMap<u32, CachedImageTexture>,
}

/// A cached GPU texture for a single image.
struct CachedImageTexture {
    // Retained for GPU resource ownership — dropping Texture deallocates the GPU memory.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    // Retained for GPU resource ownership — dropping TextureView invalidates the bind group.
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
