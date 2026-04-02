//! Overlay HUD rendering for the GPU terminal window.
//!
//! Contains `impl GridRenderer` methods for overlay elements:
//! scroll indicator, command blocks, Claude session HUD, context warning,
//! bell flash, agent timeline, and agent graph.

use std::time::Instant;

use glyphon::{
    Attrs, Buffer, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea, TextBounds,
};
use thermal_core::claude_state::{ClaudeSessionState, ClaudeStatus};
use thermal_core::palette::{Color as PaletteColor, thermal_gradient};

use wgpu::util::DeviceExt;

use crate::agent_graph::{AgentGraph, GRAPH_OVERLAY_HEIGHT};
use crate::agent_timeline::{AgentTimeline, TIMELINE_BAR_HEIGHT, ToolCategory};
use crate::color_mapping::*;
use crate::grid_renderer::{ColorVertex, GridRenderer};
use crate::osc633::{CommandBlock, CommandState};

impl GridRenderer {
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
        let metrics = Metrics::new(self.font_config.font_size, self.font_config.line_height);
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
                .family(Family::Name(&self.font_config.family))
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
            let metrics = Metrics::new(self.font_config.font_size, self.font_config.line_height);
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
                        .family(Family::Name(&self.font_config.family))
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

            if let Err(e) = self.overlay_text_renderer.prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.overlay_atlas,
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

                if let Err(e) = self.overlay_text_renderer.render(
                    &self.overlay_atlas,
                    &self.viewport,
                    &mut pass,
                ) {
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

        let context_pct = session.context_percent.unwrap_or(0.0) as f32;
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
        let metrics = Metrics::new(self.font_config.font_size, self.font_config.line_height);

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
                    .family(Family::Name(&self.font_config.family))
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
        let metrics = Metrics::new(self.font_config.font_size, self.font_config.line_height);
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
                .family(Family::Name(&self.font_config.family))
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

    /// Render a brief translucent flash overlay for the visual bell.
    ///
    /// Covers the entire terminal area with `ACCENT_WARM` at ~18% opacity.
    /// Called from `render_frame()` when `bell_flash_until` is active.
    pub fn render_bell_flash(
        &self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        let sw = surface_width as f32;
        let sh = surface_height as f32;

        let c = PaletteColor::ACCENT_WARM.to_f32_array();
        let color = [c[0], c[1], c[2], 0.18];

        let verts = pixel_rect_to_ndc(0.0, 0.0, sw, sh, sw, sh, color);
        let data = bytemuck::cast_slice::<ColorVertex, u8>(&verts);
        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("bell_flash_overlay"),
            contents: data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("bell_flash_pass"),
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
            let metrics = Metrics::new(
                self.font_config.font_size * 0.75,
                self.font_config.line_height * 0.75,
            );

            let mut label_buffers: Vec<Buffer> = Vec::with_capacity(label_entries.len());
            for (_, max_w, _, text, color) in &label_entries {
                let mut buf = Buffer::new(&mut self.font_system, metrics);
                buf.set_size(&mut self.font_system, Some(*max_w), Some(content_h));
                buf.set_text(
                    &mut self.font_system,
                    text,
                    Attrs::new()
                        .family(Family::Name(&self.font_config.family))
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
                        top: *y + (content_h - self.font_config.line_height * 0.75) / 2.0,
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

    /// Render the agent communication graph overlay.
    ///
    /// Draws nodes as filled circles (approximated with rect segments) colored by
    /// agent status, context-percent circular gauges around each node, animated
    /// message arcs between agents, and text labels via glyphon.
    pub fn render_agent_graph(
        &mut self,
        graph: &AgentGraph,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        if !graph.visible || graph.nodes.is_empty() {
            return;
        }

        let sw = surface_width as f32;
        let sh = surface_height as f32;
        let graph_h = GRAPH_OVERLAY_HEIGHT as f32;
        let graph_y = sh - graph_h;

        // ── Dark background rect ────────────────────────────────────────────
        let bg = PaletteColor::BG.to_f32_array();
        let bg_color = [bg[0], bg[1], bg[2], 0.90];
        let bg_verts = pixel_rect_to_ndc(0.0, graph_y, sw, graph_h, sw, sh, bg_color);
        let bg_data = bytemuck::cast_slice::<ColorVertex, u8>(&bg_verts);
        let bg_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("graph_bg"),
            contents: bg_data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("graph_bg_pass"),
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

        // ── Top separator line ──────────────────────────────────────────────
        let sep_color = PaletteColor::TEXT_MUTED.to_f32_array();
        let sep_color_dim = [sep_color[0], sep_color[1], sep_color[2], 0.5];
        let sep_verts = pixel_rect_to_ndc(0.0, graph_y, sw, 1.0, sw, sh, sep_color_dim);
        let sep_data = bytemuck::cast_slice::<ColorVertex, u8>(&sep_verts);
        let sep_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("graph_sep"),
            contents: sep_data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("graph_sep_pass"),
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

        // ── "AGENT GRAPH" title label ───────────────────────────────────────
        let title_color = PaletteColor::TEXT_MUTED.to_f32_array();
        let title_color_dim = [title_color[0], title_color[1], title_color[2], 0.6];

        // ── Collect all rect vertices for nodes, arcs, gauges ───────────────
        let mut all_verts: Vec<ColorVertex> = Vec::new();
        let mut label_entries: Vec<(f32, f32, String, PaletteColor)> = Vec::new();

        let nodes = graph.node_list();

        // ── Render message arcs (lines between nodes) ───────────────────────
        for arc in &graph.arcs {
            if arc.alpha < 0.01 {
                continue;
            }

            let from_pos = graph.nodes.get(&arc.from_session).map(|n| n.pos);
            let to_pos = graph.nodes.get(&arc.to_session).map(|n| n.pos);

            if let (Some(from), Some(to)) = (from_pos, to_pos) {
                // Draw line as a thin rotated rect (2px wide).
                let arc_verts = line_to_rect_verts(
                    from[0],
                    graph_y + from[1],
                    to[0],
                    graph_y + to[1],
                    2.0,
                    sw,
                    sh,
                    {
                        let c = PaletteColor::ACCENT_WARM.to_f32_array();
                        [c[0], c[1], c[2], arc.alpha * 0.7]
                    },
                );
                all_verts.extend_from_slice(&arc_verts);
            }
        }

        // ── Render parent-child edges (persistent connection lines) ─────────
        for node in &nodes {
            if let Some(ref parent_id) = node.parent_session_id {
                if let Some(parent) = graph.nodes.get(parent_id) {
                    let edge_verts = line_to_rect_verts(
                        parent.pos[0],
                        graph_y + parent.pos[1],
                        node.pos[0],
                        graph_y + node.pos[1],
                        1.0,
                        sw,
                        sh,
                        {
                            let c = PaletteColor::TEXT_MUTED.to_f32_array();
                            [c[0], c[1], c[2], 0.3]
                        },
                    );
                    all_verts.extend_from_slice(&edge_verts);
                }
            }
        }

        // ── Render nodes ────────────────────────────────────────────────────
        let node_radius: f32 = 20.0;
        let pulse_t = (self.frame_count as f32 * 0.05).sin() * 0.5 + 0.5;

        for node in &nodes {
            let cx = node.pos[0];
            let cy = graph_y + node.pos[1];

            // Node color based on status.
            let node_color = match node.status {
                ClaudeStatus::Processing => {
                    let c = PaletteColor::HOT.to_f32_array();
                    let alpha = 0.7 + 0.3 * pulse_t;
                    [c[0], c[1], c[2], alpha]
                }
                ClaudeStatus::ToolUse => {
                    let c = PaletteColor::WARM.to_f32_array();
                    [c[0], c[1], c[2], 0.85]
                }
                ClaudeStatus::Idle => {
                    let c = PaletteColor::COOL.to_f32_array();
                    [c[0], c[1], c[2], 0.6]
                }
                ClaudeStatus::AwaitingInput => {
                    let c = PaletteColor::ACCENT_COLD.to_f32_array();
                    let alpha = 0.5 + 0.3 * pulse_t;
                    [c[0], c[1], c[2], alpha]
                }
            };

            // Approximate circle with 8 rectangular segments (octagonal fill).
            let segments = 8;
            for i in 0..segments {
                let angle0 = (i as f32) * std::f32::consts::TAU / segments as f32;
                let angle1 = ((i + 1) as f32) * std::f32::consts::TAU / segments as f32;

                let x0 = cx + node_radius * angle0.cos();
                let y0 = cy + node_radius * angle0.sin();
                let x1 = cx + node_radius * angle1.cos();
                let y1 = cy + node_radius * angle1.sin();

                // Triangle from center to edge segment.
                let ndc_cx = (cx / sw) * 2.0 - 1.0;
                let ndc_cy = 1.0 - (cy / sh) * 2.0;
                let ndc_x0 = (x0 / sw) * 2.0 - 1.0;
                let ndc_y0 = 1.0 - (y0 / sh) * 2.0;
                let ndc_x1 = (x1 / sw) * 2.0 - 1.0;
                let ndc_y1 = 1.0 - (y1 / sh) * 2.0;

                // Two triangles to fill the segment (we need 6 verts for the
                // rect pipeline, but for a triangle fan from center we use 3).
                // Since the rect pipeline expects full quads (6 verts = 2 triangles),
                // we emit two degenerate triangles forming a pie slice.
                all_verts.push(ColorVertex {
                    position: [ndc_cx, ndc_cy],
                    color: node_color,
                });
                all_verts.push(ColorVertex {
                    position: [ndc_x0, ndc_y0],
                    color: node_color,
                });
                all_verts.push(ColorVertex {
                    position: [ndc_x1, ndc_y1],
                    color: node_color,
                });
            }

            // ── Context percent gauge ring ──────────────────────────────────
            let ctx_pct = node.context_percent / 100.0;
            if ctx_pct > 0.01 {
                let gauge_radius = node_radius + 4.0;
                let gauge_thickness = 3.0;
                let gauge_segments = ((segments as f32 * ctx_pct).ceil() as usize).max(1);
                let heat = ctx_pct.clamp(0.0, 1.0);
                let gauge_color_base = thermal_gradient(heat).to_f32_array();
                let gauge_color = [
                    gauge_color_base[0],
                    gauge_color_base[1],
                    gauge_color_base[2],
                    0.9,
                ];

                for i in 0..gauge_segments {
                    let total_angle = std::f32::consts::TAU * ctx_pct;
                    let a0 = -std::f32::consts::FRAC_PI_2
                        + (i as f32 / gauge_segments as f32) * total_angle;
                    let a1 = -std::f32::consts::FRAC_PI_2
                        + ((i + 1) as f32 / gauge_segments as f32) * total_angle;

                    let outer_x0 = cx + gauge_radius * a0.cos();
                    let outer_y0 = cy + gauge_radius * a0.sin();
                    let outer_x1 = cx + gauge_radius * a1.cos();
                    let outer_y1 = cy + gauge_radius * a1.sin();

                    let inner_r = gauge_radius - gauge_thickness;
                    let inner_x0 = cx + inner_r * a0.cos();
                    let inner_y0 = cy + inner_r * a0.sin();
                    let inner_x1 = cx + inner_r * a1.cos();
                    let inner_y1 = cy + inner_r * a1.sin();

                    // Two triangles for the arc segment strip.
                    let verts = [
                        ColorVertex {
                            position: [(inner_x0 / sw) * 2.0 - 1.0, 1.0 - (inner_y0 / sh) * 2.0],
                            color: gauge_color,
                        },
                        ColorVertex {
                            position: [(outer_x0 / sw) * 2.0 - 1.0, 1.0 - (outer_y0 / sh) * 2.0],
                            color: gauge_color,
                        },
                        ColorVertex {
                            position: [(outer_x1 / sw) * 2.0 - 1.0, 1.0 - (outer_y1 / sh) * 2.0],
                            color: gauge_color,
                        },
                        ColorVertex {
                            position: [(inner_x0 / sw) * 2.0 - 1.0, 1.0 - (inner_y0 / sh) * 2.0],
                            color: gauge_color,
                        },
                        ColorVertex {
                            position: [(outer_x1 / sw) * 2.0 - 1.0, 1.0 - (outer_y1 / sh) * 2.0],
                            color: gauge_color,
                        },
                        ColorVertex {
                            position: [(inner_x1 / sw) * 2.0 - 1.0, 1.0 - (inner_y1 / sh) * 2.0],
                            color: gauge_color,
                        },
                    ];
                    all_verts.extend_from_slice(&verts);
                }
            }

            // ── Token budget bar inside node ────────────────────────────────
            let bar_w = node_radius * 1.2;
            let bar_h = 4.0;
            let bar_x = cx - bar_w / 2.0;
            let bar_y_pos = cy + 2.0; // slightly below center

            // Background bar.
            let bar_bg = [bg[0], bg[1], bg[2], 0.5];
            let bar_bg_verts = pixel_rect_to_ndc(bar_x, bar_y_pos, bar_w, bar_h, sw, sh, bar_bg);
            all_verts.extend_from_slice(&bar_bg_verts);

            // Filled portion.
            let fill_w = bar_w * (1.0 - ctx_pct.clamp(0.0, 1.0)); // depleting: full = unused
            if fill_w > 0.5 {
                let fill_color = {
                    let remaining = 1.0 - ctx_pct.clamp(0.0, 1.0);
                    if remaining > 0.5 {
                        PaletteColor::WARM.to_f32_array()
                    } else if remaining > 0.2 {
                        PaletteColor::HOT.to_f32_array()
                    } else {
                        PaletteColor::SEARING.to_f32_array()
                    }
                };
                let fill_color_alpha = [fill_color[0], fill_color[1], fill_color[2], 0.8];
                let fill_verts =
                    pixel_rect_to_ndc(bar_x, bar_y_pos, fill_w, bar_h, sw, sh, fill_color_alpha);
                all_verts.extend_from_slice(&fill_verts);
            }

            // ── Label (session name) ────────────────────────────────────────
            let label = AgentGraph::node_label(node);
            let label_color = match node.status {
                ClaudeStatus::Processing => PaletteColor::TEXT_BRIGHT,
                ClaudeStatus::ToolUse => PaletteColor::TEXT_BRIGHT,
                _ => PaletteColor::TEXT,
            };
            label_entries.push((cx, cy + node_radius + 8.0, label, label_color));

            // Status/tool sub-label.
            if let Some(ref tool) = node.current_tool {
                let sub_label = tool.clone();
                label_entries.push((
                    cx,
                    cy + node_radius + 22.0,
                    sub_label,
                    PaletteColor::TEXT_MUTED,
                ));
            }
        }

        // ── Draw all rect/triangle vertices ─────────────────────────────────
        if !all_verts.is_empty() {
            let data = bytemuck::cast_slice::<ColorVertex, u8>(&all_verts);
            let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("graph_nodes"),
                contents: data,
                usage: wgpu::BufferUsages::VERTEX,
            });

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("graph_nodes_pass"),
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
                pass.draw(0..all_verts.len() as u32, 0..1);
            }
        }

        // ── Draw title text ─────────────────────────────────────────────────
        // Title + node labels via glyphon.
        if !label_entries.is_empty() || !nodes.is_empty() {
            let metrics = Metrics::new(
                self.font_config.font_size * 0.7,
                self.font_config.line_height * 0.7,
            );
            let small_metrics = Metrics::new(
                self.font_config.font_size * 0.6,
                self.font_config.line_height * 0.6,
            );

            let mut text_buffers: Vec<(Buffer, f32, f32)> = Vec::new();

            // Title: "AGENT GRAPH" in top-left of the overlay area.
            {
                let mut buf = Buffer::new(&mut self.font_system, metrics);
                buf.set_size(
                    &mut self.font_system,
                    Some(200.0),
                    Some(self.font_config.line_height),
                );
                buf.set_text(
                    &mut self.font_system,
                    "AGENT GRAPH",
                    Attrs::new()
                        .family(Family::Name(&self.font_config.family))
                        .color(GlyphColor::rgba(
                            (title_color_dim[0] * 255.0) as u8,
                            (title_color_dim[1] * 255.0) as u8,
                            (title_color_dim[2] * 255.0) as u8,
                            (title_color_dim[3] * 255.0) as u8,
                        )),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                text_buffers.push((buf, 8.0, graph_y + 4.0));
            }

            // Node labels.
            for (cx, ly, text, color) in &label_entries {
                let is_sub = text.len() < 20 && *ly > graph_y + node_radius + 20.0;
                let m = if is_sub { small_metrics } else { metrics };

                let mut buf = Buffer::new(&mut self.font_system, m);
                let max_w = 120.0;
                buf.set_size(
                    &mut self.font_system,
                    Some(max_w),
                    Some(self.font_config.line_height),
                );
                buf.set_text(
                    &mut self.font_system,
                    text,
                    Attrs::new()
                        .family(Family::Name(&self.font_config.family))
                        .color(GlyphColor::rgba(color.r, color.g, color.b, 200)),
                    Shaping::Basic,
                );
                buf.shape_until_scroll(&mut self.font_system, false);

                // Center the label horizontally under the node.
                let text_w = text.len() as f32 * self.cell_width * 0.7;
                let left = cx - text_w / 2.0;
                text_buffers.push((buf, left.max(2.0), *ly));
            }

            self.viewport.update(
                queue,
                Resolution {
                    width: surface_width,
                    height: surface_height,
                },
            );

            let text_areas: Vec<TextArea<'_>> = text_buffers
                .iter()
                .map(|(buf, x, y)| TextArea {
                    buffer: buf,
                    left: *x,
                    top: *y,
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
                        200,
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
                tracing::warn!("Graph text prepare failed: {}", e);
                return;
            }

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("graph_text_pass"),
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
                    tracing::warn!("Graph text render failed: {}", e);
                }
            }
        }
    }
}
