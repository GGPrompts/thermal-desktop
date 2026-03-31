//! Context heatmap GPU pipeline — edge vignette glow driven by context_percent.

use std::time::Instant;

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
