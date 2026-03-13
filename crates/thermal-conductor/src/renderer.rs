//! wgpu rendering state for thermal-conductor.
//!
//! Provides WgpuState: initialises the wgpu device/surface pipeline and clears
//! the window to ThermalPalette::BG (#0a0010) each frame.
//!
//! NOTE: This module compiles without a Wayland compositor present (Docker /
//! CI). The window cannot actually be displayed in those environments, but
//! `cargo check` and `cargo build` succeed.

// ── WgpuState ─────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct WgpuState {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface_config: wgpu::SurfaceConfiguration,
}

#[allow(dead_code)]
impl WgpuState {
    /// Initialise wgpu from an existing winit window.
    pub async fn new(window: std::sync::Arc<winit::window::Window>) -> anyhow::Result<Self> {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        // SAFETY: surface must not outlive the window. We keep both alive for
        // the same duration via Arc.
        let surface = instance.create_surface(window)?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("No suitable wgpu adapter found"))?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("thermal-conductor device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::default(),
                },
                None,
            )
            .await?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        Ok(Self {
            surface,
            device,
            queue,
            surface_config,
        })
    }

    /// Handle a window resize — reconfigures the surface.
    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.surface_config.width = new_size.width;
        self.surface_config.height = new_size.height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Clear the frame to ThermalPalette::BG (#0a0010 — deep void purple).
    pub fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render encoder"),
            });

        {
            let _render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // ThermalPalette::BG = #0a0010
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.039,
                            g: 0.000,
                            b: 0.063,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        Ok(())
    }
}

// ── Text rendering ────────────────────────────────────────────────────────────

/// Wraps glyphon 0.7 text rendering resources.
///
/// Uses `thermal_core::ThermalTextRenderer` which handles the glyphon/wgpu
/// version alignment internally.
#[allow(dead_code)]
pub struct TextRenderer {
    inner: thermal_core::ThermalTextRenderer,
}

#[allow(dead_code)]
impl TextRenderer {
    /// Create a new text renderer.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            inner: thermal_core::ThermalTextRenderer::new(device, queue, format, width, height),
        }
    }

    /// Update the viewport resolution (call on resize).
    pub fn set_resolution(&mut self, queue: &wgpu::Queue, width: u32, height: u32) {
        self.inner.resize(queue, width, height);
    }

    /// Create a shaped buffer for the given text (using Monospace font family).
    ///
    /// Returns a glyphon Buffer ready to be included in a TextArea for prepare.
    pub fn make_buffer(
        &mut self,
        text: &str,
        font_size: f32,
        color: glyphon::Color,
    ) -> glyphon::Buffer {
        self.inner.make_buffer(text, font_size, font_size * 1.2, color)
    }

    /// Draw a text label at (x, y) in a render pass.
    ///
    /// Calls `ThermalTextRenderer`'s inner renderer to prepare and draw the
    /// text in the current render pass.
    pub fn render_label(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: glyphon::Color,
    ) -> Result<(), glyphon::RenderError> {
        let buf = self.inner.make_buffer(text, font_size, font_size * 1.2, color);

        let text_area = glyphon::TextArea {
            buffer: &buf,
            left: x,
            top: y,
            scale: 1.0,
            bounds: glyphon::TextBounds::default(),
            default_color: color,
            custom_glyphs: &[],
        };

        self.inner
            .renderer
            .prepare(
                device,
                queue,
                &mut self.inner.font_system,
                &mut self.inner.atlas,
                &self.inner.viewport,
                [text_area],
                &mut self.inner.swash_cache,
            )
            .map_err(|_| glyphon::RenderError::RemovedFromAtlas)?;

        self.inner
            .renderer
            .render(&self.inner.atlas, &self.inner.viewport, pass)
    }
}

// ── Rect ──────────────────────────────────────────────────────────────────────

/// Axis-aligned rectangle in screen-space pixels, used by layout and capture
/// rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[allow(dead_code)]
impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Returns true if the point (px, py) lies inside this rect.
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

// ── ANSI color → glyphon color ────────────────────────────────────────────────

/// Map an `AnsiColor` to a `glyphon::Color`.
///
/// - `AnsiColor::Default` → ThermalPalette::TEXT (#c4b5fd)
/// - `AnsiColor::Indexed(n)` → standard 16-colour palette for n < 16;
///   xterm-256 formula for n ≥ 16
/// - `AnsiColor::Rgb(r,g,b)` → direct pass-through
#[allow(dead_code)]
pub fn ansi_color_to_rgba(color: &crate::ansi::AnsiColor) -> glyphon::Color {
    use crate::ansi::AnsiColor;
    match *color {
        AnsiColor::Default => glyphon::Color::rgba(0xc4, 0xb5, 0xfd, 0xff), // TEXT
        AnsiColor::Rgb(r, g, b) => glyphon::Color::rgba(r, g, b, 0xff),
        AnsiColor::Indexed(n) => indexed_to_rgba(n),
    }
}

/// Convert an xterm-256 colour index to a `glyphon::Color`.
fn indexed_to_rgba(n: u8) -> glyphon::Color {
    // Standard 16 colours (ANSI order).
    const ANSI16: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00), // 0  Black
        (0x80, 0x00, 0x00), // 1  Red
        (0x00, 0x80, 0x00), // 2  Green
        (0x80, 0x80, 0x00), // 3  Yellow
        (0x00, 0x00, 0x80), // 4  Blue
        (0x80, 0x00, 0x80), // 5  Magenta
        (0x00, 0x80, 0x80), // 6  Cyan
        (0xc0, 0xc0, 0xc0), // 7  White
        (0x80, 0x80, 0x80), // 8  Bright Black (gray)
        (0xff, 0x00, 0x00), // 9  Bright Red
        (0x00, 0xff, 0x00), // 10 Bright Green
        (0xff, 0xff, 0x00), // 11 Bright Yellow
        (0x00, 0x00, 0xff), // 12 Bright Blue
        (0xff, 0x00, 0xff), // 13 Bright Magenta
        (0x00, 0xff, 0xff), // 14 Bright Cyan
        (0xff, 0xff, 0xff), // 15 Bright White
    ];

    if (n as usize) < ANSI16.len() {
        let (r, g, b) = ANSI16[n as usize];
        return glyphon::Color::rgba(r, g, b, 0xff);
    }

    if n < 232 {
        // 6×6×6 colour cube: indices 16–231.
        let idx = n - 16;
        let r = idx / 36;
        let g = (idx % 36) / 6;
        let b = idx % 6;
        let to_byte = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        return glyphon::Color::rgba(to_byte(r), to_byte(g), to_byte(b), 0xff);
    }

    // Greyscale ramp: indices 232–255.
    let level = (n - 232) * 10 + 8;
    glyphon::Color::rgba(level, level, level, 0xff)
}

// ── PaneCapture rendering ─────────────────────────────────────────────────────

#[allow(dead_code)]
impl WgpuState {
    /// Render the content of a `PaneCapture` into `viewport` using the
    /// supplied `TextRenderer`.
    ///
    /// Each line of styled characters is rendered as a text area positioned
    /// at (`viewport.x` + 0, `viewport.y` + line_index × line_height).
    /// The render pass must still be open; call this between
    /// `encoder.begin_render_pass` and the end of the pass block.
    pub fn render_capture(
        &self,
        capture: &crate::capture::PaneCapture,
        viewport: Rect,
        text_renderer: &mut TextRenderer,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass,
    ) -> Result<(), glyphon::RenderError> {
        let font_size = 14.0_f32;
        let line_height = font_size * 1.2;

        for (line_idx, line) in capture.lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            // Collect the line's text into a plain string for now.
            // Full per-character colour support requires per-span glyphon
            // rich text (future enhancement). Use the colour of the first char.
            let text: String = line.iter().map(|sc| sc.ch).collect();
            let color = if let Some(first) = line.first() {
                ansi_color_to_rgba(&first.fg)
            } else {
                glyphon::Color::rgba(0xc4, 0xb5, 0xfd, 0xff)
            };

            let y = viewport.y + line_idx as f32 * line_height;
            if y + line_height > viewport.y + viewport.h {
                break; // Clip to viewport bounds.
            }

            text_renderer.render_label(
                device,
                queue,
                pass,
                &text,
                viewport.x,
                y,
                font_size,
                color,
            )?;
        }

        Ok(())
    }
}
