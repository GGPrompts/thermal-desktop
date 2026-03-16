//! Terminal cell grid renderer using glyphon.
//!
//! Reads `alacritty_terminal::Term`'s grid and renders each cell character
//! via glyphon, mapping ANSI colors to ThermalPalette where they match and
//! passing truecolor through directly. Renders the cursor as a distinct
//! visual element (inverted block).

use std::collections::HashSet;

use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::RenderableCursor;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor};

use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use thermal_core::palette::Color as PaletteColor;
use wgpu::util::DeviceExt;

use tracing::debug;

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

// ── GridRenderer ───────────────────────────────────────────────────────────

/// GPU-accelerated terminal grid renderer.
///
/// Renders the alacritty_terminal grid via glyphon text + colored rect pipeline
/// for backgrounds and cursor.
/// How many frames between atlas trim operations (~16s at 60fps).
const ATLAS_TRIM_INTERVAL: u64 = 1000;

pub struct GridRenderer {
    // Glyphon state
    font_system: FontSystem,
    swash_cache: SwashCache,
    #[allow(dead_code)]
    cache: Cache,
    atlas: TextAtlas,
    viewport: Viewport,
    text_renderer: TextRenderer,

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

    // Persistent per-row glyphon Buffers — only rebuilt for damaged rows.
    row_buffers: Vec<Option<Buffer>>,

    // Track last cursor row to rebuild affected buffers when cursor moves.
    last_cursor_row: Option<usize>,

    // Frame counter for throttled atlas trimming.
    frame_count: u64,
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
        let viewport = {
            let mut vp = Viewport::new(device, &cache);
            vp.update(queue, Resolution { width, height });
            vp
        };
        let text_renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);

        // ── Measure cell dimensions from font metrics ────────────────────
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
        let mut measure_buf = Buffer::new(&mut font_system, metrics);
        measure_buf.set_size(&mut font_system, Some(1000.0), Some(LINE_HEIGHT * 2.0));
        measure_buf.set_text(
            &mut font_system,
            "M",
            Attrs::new().family(Family::Name(TERM_FONT_FAMILY)),
            Shaping::Advanced,
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

        Self {
            font_system,
            swash_cache,
            cache,
            atlas,
            viewport,
            text_renderer,
            rect_pipeline,
            rect_buf,
            rect_buf_capacity,
            cell_width,
            cell_height,
            padding_x,
            padding_y,
            row_cache: Vec::new(),
            row_buffers: Vec::new(),
            last_cursor_row: None,
            frame_count: 0,
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

        // Trim atlas on resize — glyph set may change.
        self.atlas.trim();

        // Invalidate row cache and persistent text buffers — resize triggers a full damage anyway.
        self.row_cache.clear();
        self.row_buffers.clear();
        self.last_cursor_row = None;
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
        buf.set_size(&mut self.font_system, Some(badge_w + 8.0), Some(badge_h + 4.0));
        let text_color = PaletteColor::BG.to_f32_array();
        buf.set_text(
            &mut self.font_system,
            &label,
            Attrs::new()
                .family(Family::Name(TERM_FONT_FAMILY))
                .color(f32_to_glyph_color(text_color)),
            Shaping::Advanced,
        );
        buf.shape_until_scroll(&mut self.font_system, false);

        self.viewport.update(queue, Resolution { width: surface_width, height: surface_height });

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

        if let Err(e) = self.text_renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
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

            if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, &mut pass) {
                tracing::warn!("scroll indicator text render failed: {}", e);
            }
        }
        // Atlas trimming handled by render_from_cache frame counter; no per-call trim here.
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
                        bg_rects.push(([cx, cy + self.cell_height - h, self.cell_width, h], cursor_color));
                    }
                    CursorShape::Beam => {
                        bg_rects.push(([cx, cy, 2.0, self.cell_height], cursor_color));
                    }
                    CursorShape::HollowBlock => {
                        let t = 1.0;
                        bg_rects.push(([cx, cy, self.cell_width, t], cursor_color));
                        bg_rects.push(([cx, cy + self.cell_height - t, self.cell_width, t], cursor_color));
                        bg_rects.push(([cx, cy, t, self.cell_height], cursor_color));
                        bg_rects.push(([cx + self.cell_width - t, cy, t, self.cell_height], cursor_color));
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
        let mut rect_vertices: Vec<ColorVertex> = Vec::new();
        for (xywh, color) in &bg_rects {
            let verts = pixel_rect_to_ndc(xywh[0], xywh[1], xywh[2], xywh[3], sw, sh, *color);
            rect_vertices.extend_from_slice(&verts);
        }

        let rect_vertex_count = rect_vertices.len() as u32;

        if !rect_vertices.is_empty() {
            let needed = rect_vertices.len() as u64;

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

            let data = bytemuck::cast_slice::<ColorVertex, u8>(&rect_vertices);
            queue.write_buffer(&self.rect_buf, 0, data);
        }

        // ── Rebuild only damaged per-row glyphon Buffers ────────────────
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);

        let cursor_row = cursor.point.line.0 as usize;
        let cursor_col = cursor.point.column.0;

        // Ensure row_buffers is sized to screen_lines.
        if self.row_buffers.len() != screen_lines {
            self.row_buffers.resize_with(screen_lines, || None);
        }

        // Determine which rows need their glyphon Buffer rebuilt.
        // Cursor row always needs rebuild (block cursor inverts fg color).
        // Previous cursor row also needs rebuild (cursor moved away).
        let prev_cursor_row = self.last_cursor_row;
        let full_rebuild = damaged_rows.is_none();

        for (row_idx, cached) in self.row_cache.iter().enumerate() {
            // Decide if this row's Buffer needs rebuilding.
            let needs_rebuild = if full_rebuild {
                true
            } else {
                let in_damage_set = damaged_rows
                    .map(|set| set.contains(&row_idx))
                    .unwrap_or(false);
                let is_cursor_row = row_idx == cursor_row;
                let was_cursor_row = prev_cursor_row.map(|r| r == row_idx).unwrap_or(false);
                in_damage_set || is_cursor_row || was_cursor_row
            };

            if !needs_rebuild {
                // Reuse existing Buffer (if any).
                continue;
            }

            let row = match cached {
                Some(r) => r,
                None => {
                    // Row is empty — drop any existing buffer.
                    self.row_buffers[row_idx] = None;
                    continue;
                }
            };

            let row_cells = &row.cells;
            if row_cells.is_empty() {
                self.row_buffers[row_idx] = None;
                continue;
            }

            // Build rich text spans with per-character colors.
            let mut rich_spans: Vec<(String, Attrs<'_>)> = Vec::new();
            let mut current_fg: Option<[f32; 4]> = None;
            let mut current_span = String::new();
            let mut last_col: usize = 0;

            for cell in row_cells {
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                // Fill gap spaces with default color.
                while last_col < cell.col {
                    let default_fg = PaletteColor::TEXT.to_f32_array();
                    if current_fg.is_some() && current_fg != Some(default_fg) {
                        if !current_span.is_empty() {
                            let c = current_fg.unwrap_or(default_fg);
                            rich_spans.push((
                                std::mem::take(&mut current_span),
                                Attrs::new()
                                    .family(Family::Name(TERM_FONT_FAMILY))
                                    .color(f32_to_glyph_color(c)),
                            ));
                        }
                    }
                    current_fg = Some(default_fg);
                    current_span.push(' ');
                    last_col += 1;
                }

                // Determine foreground color (with cursor inversion).
                let is_block_cursor = cursor.shape == CursorShape::Block
                    && cursor_col == cell.col
                    && cursor_row == cell.row;
                let fg = if is_block_cursor {
                    PaletteColor::BG.to_f32_array()
                } else if cell.flags.contains(Flags::INVERSE) {
                    ansi_to_glyphon_bg(&cell.bg).unwrap_or(PaletteColor::BG.to_f32_array())
                } else {
                    ansi_to_glyphon_fg(&cell.fg)
                };

                if current_fg.is_some() && current_fg != Some(fg) {
                    if !current_span.is_empty() {
                        let c = current_fg.unwrap_or(fg);
                        rich_spans.push((
                            std::mem::take(&mut current_span),
                            Attrs::new()
                                .family(Family::Name(TERM_FONT_FAMILY))
                                .color(f32_to_glyph_color(c)),
                        ));
                    }
                }
                current_fg = Some(fg);

                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                current_span.push(ch);

                if cell.flags.contains(Flags::WIDE_CHAR) {
                    current_span.push(' ');
                    last_col += 2;
                } else {
                    last_col += 1;
                }
            }

            // Flush remaining span.
            if !current_span.is_empty() {
                let fg = current_fg.unwrap_or(PaletteColor::TEXT.to_f32_array());
                rich_spans.push((
                    current_span,
                    Attrs::new()
                        .family(Family::Name(TERM_FONT_FAMILY))
                        .color(f32_to_glyph_color(fg)),
                ));
            }

            if rich_spans.is_empty() {
                self.row_buffers[row_idx] = None;
                continue;
            }

            // Reuse existing Buffer if available, otherwise create a new one.
            let buf = self.row_buffers[row_idx]
                .get_or_insert_with(|| Buffer::new(&mut self.font_system, metrics));
            buf.set_metrics(&mut self.font_system, metrics);
            buf.set_size(
                &mut self.font_system,
                Some(sw),
                Some(self.cell_height + 4.0),
            );

            let borrowed_spans: Vec<(&str, Attrs<'_>)> = rich_spans
                .iter()
                .map(|(s, a)| (s.as_str(), *a))
                .collect();
            buf.set_rich_text(
                &mut self.font_system,
                borrowed_spans,
                Attrs::new().family(Family::Name(TERM_FONT_FAMILY)),
                Shaping::Advanced,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
        }

        // Update cursor tracking for next frame.
        self.last_cursor_row = Some(cursor_row);

        // ── Update viewport ──────────────────────────────────────────────
        self.viewport.update(queue, Resolution { width: surface_width, height: surface_height });

        // ── Prepare glyphon text from persistent row_buffers ────────────
        let text_areas: Vec<TextArea<'_>> = self
            .row_buffers
            .iter()
            .enumerate()
            .filter_map(|(row_idx, opt_buf)| {
                let buf = opt_buf.as_ref()?;
                Some(TextArea {
                    buffer: buf,
                    left: self.padding_x,
                    top: self.padding_y + row_idx as f32 * self.cell_height,
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

            if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, &mut pass) {
                tracing::warn!("glyphon render failed: {}", e);
            }
        }

        // Trim atlas periodically to free unused glyphs (not every frame).
        self.frame_count += 1;
        if self.frame_count % ATLAS_TRIM_INTERVAL == 0 {
            self.atlas.trim();
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
fn named_to_thermal_fg(named: NamedColor) -> [f32; 4] {
    match named {
        NamedColor::Black => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::Red => PaletteColor::SEARING.to_f32_array(),
        NamedColor::Green => PaletteColor::WARM.to_f32_array(),
        NamedColor::Yellow => PaletteColor::HOT.to_f32_array(),
        NamedColor::Blue => PaletteColor::ACCENT_COOL.to_f32_array(),
        NamedColor::Magenta => PaletteColor::ACCENT_COLD.to_f32_array(),
        NamedColor::Cyan => PaletteColor::COLD.to_f32_array(),
        NamedColor::White | NamedColor::Foreground => PaletteColor::TEXT.to_f32_array(),

        NamedColor::BrightBlack => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::BrightRed => PaletteColor::CRITICAL.to_f32_array(),
        NamedColor::BrightGreen => PaletteColor::WARM.to_f32_array(),
        NamedColor::BrightYellow => PaletteColor::WHITE_HOT.to_f32_array(),
        NamedColor::BrightBlue => PaletteColor::ACCENT_COOL.to_f32_array(),
        NamedColor::BrightMagenta => PaletteColor::ACCENT_COLD.to_f32_array(),
        NamedColor::BrightCyan => PaletteColor::MILD.to_f32_array(),
        NamedColor::BrightWhite | NamedColor::BrightForeground => PaletteColor::TEXT_BRIGHT.to_f32_array(),

        NamedColor::DimBlack => PaletteColor::BG.to_f32_array(),
        NamedColor::DimRed => PaletteColor::SEARING.to_f32_array(),
        NamedColor::DimGreen => PaletteColor::COOL.to_f32_array(),
        NamedColor::DimYellow => PaletteColor::HOTTER.to_f32_array(),
        NamedColor::DimBlue => PaletteColor::COOL.to_f32_array(),
        NamedColor::DimMagenta => PaletteColor::FREEZING.to_f32_array(),
        NamedColor::DimCyan => PaletteColor::COLD.to_f32_array(),
        NamedColor::DimWhite | NamedColor::DimForeground => PaletteColor::TEXT_MUTED.to_f32_array(),

        NamedColor::Background => PaletteColor::BG.to_f32_array(),
        NamedColor::Cursor => PaletteColor::TEXT_BRIGHT.to_f32_array(),
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
        NamedColor::Black => PaletteColor::BG.to_f32_array(),
        NamedColor::Red => PaletteColor::SEARING.to_f32_array(),
        NamedColor::Green => PaletteColor::WARM.to_f32_array(),
        NamedColor::Yellow => PaletteColor::HOT.to_f32_array(),
        NamedColor::Blue => PaletteColor::COOL.to_f32_array(),
        NamedColor::Magenta => PaletteColor::COLD.to_f32_array(),
        NamedColor::Cyan => PaletteColor::COLD.to_f32_array(),
        NamedColor::White => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::Foreground => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::Background => PaletteColor::BG.to_f32_array(),
        NamedColor::Cursor => PaletteColor::BG_SURFACE.to_f32_array(),

        // Bright backgrounds — use muted/dark variants, never vivid foreground colors
        NamedColor::BrightBlack => PaletteColor::BG_LIGHT.to_f32_array(),
        NamedColor::BrightRed => PaletteColor::CRITICAL.to_f32_array(),
        NamedColor::BrightGreen => PaletteColor::MILD.to_f32_array(),
        NamedColor::BrightYellow => PaletteColor::HOT.to_f32_array(),
        NamedColor::BrightBlue => PaletteColor::COOL.to_f32_array(),
        NamedColor::BrightMagenta => PaletteColor::FREEZING.to_f32_array(),
        NamedColor::BrightCyan => PaletteColor::COLD.to_f32_array(),
        NamedColor::BrightWhite => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::BrightForeground => PaletteColor::TEXT_MUTED.to_f32_array(),

        // Dim backgrounds — use deep dark palette entries
        NamedColor::DimBlack => PaletteColor::BG.to_f32_array(),
        NamedColor::DimRed => PaletteColor::FREEZING.to_f32_array(),
        NamedColor::DimGreen => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::DimYellow => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::DimBlue => PaletteColor::BG_SURFACE.to_f32_array(),
        NamedColor::DimMagenta => PaletteColor::BG.to_f32_array(),
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
        ColorVertex { position: [x0, y0], color },
        ColorVertex { position: [x1, y0], color },
        ColorVertex { position: [x0, y1], color },
        ColorVertex { position: [x1, y0], color },
        ColorVertex { position: [x1, y1], color },
        ColorVertex { position: [x0, y1], color },
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
