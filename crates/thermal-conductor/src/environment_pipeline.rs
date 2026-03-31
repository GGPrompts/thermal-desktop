//! Environment effect GPU pipeline — context-aware border glow (Docker/worktree/SSH).

use std::time::Instant;

// ── Environment effect shader (context-aware border glow) ──────────────────

const ENVIRONMENT_EFFECT_SHADER: &str = r#"
struct EnvUniform {
    context_type: f32,
    time: f32,
    width: f32,
    height: f32,
}
@group(0) @binding(0)
var<uniform> u_env: EnvUniform;

// Simplex noise helpers (2D, gradient-based) — adapted from thermal-wallpaper
fn mod289_2(x: vec2<f32>) -> vec2<f32> { return x - floor(x * (1.0 / 289.0)) * 289.0; }
fn mod289_3(x: vec3<f32>) -> vec3<f32> { return x - floor(x * (1.0 / 289.0)) * 289.0; }
fn permute(x: vec3<f32>) -> vec3<f32> { return mod289_3((x * 34.0 + 10.0) * x); }

fn simplex2d(v: vec2<f32>) -> f32 {
    let C = vec4<f32>(0.211324865405187, 0.366025403784439, -0.577350269189626, 0.024390243902439);
    var i = floor(v + dot(v, C.yy));
    let x0 = v - i + dot(i, C.xx);
    var i1: vec2<f32>;
    if x0.x > x0.y {
        i1 = vec2<f32>(1.0, 0.0);
    } else {
        i1 = vec2<f32>(0.0, 1.0);
    }
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
}

fn fbm(p: vec2<f32>, t: f32) -> f32 {
    var val = 0.0;
    var amp = 0.5;
    var freq = 1.0;
    var pos = p;
    for (var i = 0; i < 4; i = i + 1) {
        // Circular time path to avoid seam on wrap
        let radius = 3.0;
        let time_ofs = vec2<f32>(cos(t * 0.3 * freq) * radius, sin(t * 0.3 * freq) * radius);
        val = val + amp * simplex2d(pos * freq + time_ofs);
        amp = amp * 0.5;
        freq = freq * 2.0;
    }
    return val;
}

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

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    let ctx_type = u32(u_env.context_type);
    let t = u_env.time;
    let uv = frag_coord.xy / vec2<f32>(u_env.width, u_env.height);

    // context_type 0 = MainBranch — no effect, early return.
    if ctx_type == 0u {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }

    // Edge distance: 0 at edge, 1 at center.
    let edge_x = min(uv.x, 1.0 - uv.x) * 2.0;
    let edge_y = min(uv.y, 1.0 - uv.y) * 2.0;
    let edge_dist = min(edge_x, edge_y);

    // === Docker: translucent indigo shimmer with organic bubble noise ===
    if ctx_type == 1u {
        // ACCENT_COLD: rgb(0.388, 0.400, 0.945) — indigo
        let docker_color = vec3<f32>(0.388, 0.400, 0.945);

        // Border glow falloff — extends ~6% in from edge
        let border = 1.0 - smoothstep(0.0, 0.06, edge_dist);

        // Organic bubble effect using simplex noise
        let noise_uv = uv * 8.0;
        let noise_val = fbm(noise_uv, t) * 0.5 + 0.5;

        // Slow pulse modulation
        let pulse = 0.85 + 0.15 * sin(t * 1.2);

        // Combine: border glow * noise texture * pulse
        let alpha = border * mix(0.6, 1.0, noise_val) * pulse * 0.12;

        return vec4<f32>(docker_color * alpha, alpha);
    }

    // === Worktree: amber/gold steady border glow ===
    if ctx_type == 2u {
        // ACCENT_WARM: rgb(0.961, 0.620, 0.043) — amber/gold
        let worktree_color = vec3<f32>(0.961, 0.620, 0.043);

        // Steady border glow — extends ~5% in from edge
        let border = 1.0 - smoothstep(0.0, 0.05, edge_dist);

        // Very subtle breathe (slow, minimal amplitude) — safety indicator
        let breathe = 0.92 + 0.08 * sin(t * 0.6);

        let alpha = border * breathe * 0.10;

        return vec4<f32>(worktree_color * alpha, alpha);
    }

    // === SSH: red vignette — danger zone ===
    if ctx_type == 3u {
        // ACCENT_HOT: rgb(0.937, 0.267, 0.267) — red
        let ssh_color = vec3<f32>(0.937, 0.267, 0.267);

        // Wider vignette — extends ~10% in from edge for a more ominous feel
        let vignette = 1.0 - smoothstep(0.0, 0.10, edge_dist);

        // Slow warning pulse
        let pulse = 0.8 + 0.2 * sin(t * 0.8);

        let alpha = vignette * pulse * 0.14;

        return vec4<f32>(ssh_color * alpha, alpha);
    }

    // Fallback — should not reach here.
    return vec4<f32>(0.0, 0.0, 0.0, 0.0);
}
"#;

// ── Environment effect pipeline ────────────────────────────────────────────

/// GPU pipeline for rendering environment-aware border effects.
///
/// Renders a fullscreen triangle with a WGSL fragment shader that produces
/// context-specific border glows: indigo shimmer for Docker, amber glow for
/// git worktrees, red vignette for SSH. The effect type is driven by the
/// `context_type` uniform (0=none, 1=docker, 2=worktree, 3=ssh).
pub struct EnvironmentEffectPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    start: Instant,
}

impl EnvironmentEffectPipeline {
    /// Create the environment effect pipeline. Call once during renderer init.
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("environment_effect_shader"),
            source: wgpu::ShaderSource::Wgsl(ENVIRONMENT_EFFECT_SHADER.into()),
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("environment_effect_uniform"),
            size: 16, // 4 x f32
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("environment_effect_bgl"),
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
            label: Some("environment_effect_bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("environment_effect_layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("environment_effect_pipeline"),
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

    /// Render the environment effect overlay.
    ///
    /// `context_type` is the `TerminalContext::as_uniform()` value (0-3).
    /// When context_type is 0 (MainBranch), the shader early-returns transparent.
    pub fn render(
        &self,
        context_type: u32,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) {
        if context_type == 0 {
            return; // MainBranch — no effect, skip the GPU pass entirely.
        }

        // Update uniform buffer.
        let elapsed = self.start.elapsed().as_secs_f32();
        let uniform_data: [f32; 4] = [context_type as f32, elapsed, width as f32, height as f32];
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::cast_slice(&uniform_data));

        // Render fullscreen triangle.
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("environment_effect_pass"),
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
