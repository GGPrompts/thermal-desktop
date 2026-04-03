//! Frame rendering for ConductorWindow.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::TermDamage;
use alacritty_terminal::term::cell::Flags;

use crate::grid_renderer::{self, RenderCell, cell_display_text};

use super::url_detection::detect_urls_in_cells;
use super::{ConductorWindow, RenderStatus};

impl ConductorWindow {
    /// Render a frame: clear to BG, then render the terminal grid.
    // TODO: [code-review] extract render_terminal_grid, render_overlays, render_hud sub-methods
    pub(super) fn render_frame(&mut self) -> RenderStatus {
        // Garbage-collect expired overlay result cards each frame.
        if self.overlay.gc_expired() {
            self.dirty = true;
        }

        let output = match self.wgpu.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                self.wgpu
                    .surface
                    .configure(&self.wgpu.device, &self.wgpu.config);
                return RenderStatus::Retry;
            }
            Err(wgpu::SurfaceError::Timeout) => {
                tracing::debug!("Timed out acquiring surface texture; retrying");
                return RenderStatus::Retry;
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                tracing::error!("Out of memory while acquiring surface texture");
                return RenderStatus::Fatal;
            }
        };

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.wgpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("conductor frame"),
                });

        // ── Clear pass ───────────────────────────────────────────────────
        // Palette BG (#0a0010) with compositor transparency.
        // Pre-multiply RGB by alpha for PreMultiplied/Inherit modes.
        let pre_mul = matches!(
            self.wgpu.config.alpha_mode,
            wgpu::CompositeAlphaMode::PreMultiplied | wgpu::CompositeAlphaMode::Inherit
        );
        let bg: [f32; 4] = grid_renderer::clear_color_for_mode(pre_mul);
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("conductor clear pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Pass drops here — just a clear
        }

        // ── Context heatmap vignette (renders BEFORE grid so text is on top) ──
        if let Some(ref session) = self.claude_session
            && let Some(ctx_pct) = session.context_percent
        {
            // Normalize from 0-100 to 0.0-1.0.
            let normalized = (ctx_pct as f32 / 100.0).clamp(0.0, 1.0);
            self.context_heatmap.render(
                normalized,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
        }

        // ── Environment effect (renders BEFORE grid so text is on top) ──
        self.environment_effect.render(
            self.terminal_context.as_uniform(),
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // ── Render terminal grid ─────────────────────────────────────────
        // Lock the terminal and read renderable content.
        let term_handle = self.terminal.term_handle();
        let mut term = term_handle.lock();

        // Query damage BEFORE reading content — damage() requires &mut self.
        let damaged_rows: Option<HashSet<usize>> =
            if self.force_full_redraw.swap(false, Ordering::AcqRel) {
                None
            } else {
                match term.damage() {
                    TermDamage::Full => None, // None means "full redraw"
                    TermDamage::Partial(iter) => {
                        let set: HashSet<usize> = iter
                            .filter(|bounds| bounds.is_damaged())
                            .map(|bounds| bounds.line)
                            .collect();
                        if set.is_empty() {
                            // Nothing damaged — reuse entire cache, skip cell collection.
                            let screen_lines = term.screen_lines();
                            let content = term.renderable_content();
                            let display_offset = content.display_offset;
                            let cursor = content.cursor;
                            let selection_range = content.selection;
                            term.reset_damage();
                            drop(term);

                            self.grid_renderer.render_cached(
                                &cursor,
                                screen_lines,
                                selection_range.as_ref(),
                                display_offset,
                                &self.wgpu.device,
                                &self.wgpu.queue,
                                &mut encoder,
                                &view,
                                self.width,
                                self.height,
                            );

                            // ── Kitty graphics inline images ─────────────────────────
                            {
                                let store = self.terminal.image_store();
                                let mut store_guard = store.lock();
                                self.grid_renderer.render_images(
                                    &store_guard,
                                    &self.wgpu.device,
                                    &self.wgpu.queue,
                                    &mut encoder,
                                    &view,
                                    self.width,
                                    self.height,
                                );
                                self.grid_renderer
                                    .periodic_image_cleanup(&mut store_guard, screen_lines);
                            }

                            // ── Command block overlays ──────────────────────────────
                            {
                                let tracker = self.terminal.command_tracker();
                                let blocks = tracker.lock().blocks.clone();
                                self.grid_renderer.render_command_blocks(
                                    &blocks,
                                    display_offset,
                                    screen_lines,
                                    &self.wgpu.device,
                                    &self.wgpu.queue,
                                    &mut encoder,
                                    &view,
                                    self.width,
                                    self.height,
                                );
                            }

                            // ── Scroll indicator overlay ─────────────────────────────
                            self.grid_renderer.render_scroll_indicator(
                                display_offset,
                                &self.wgpu.device,
                                &self.wgpu.queue,
                                &mut encoder,
                                &view,
                                self.width,
                                self.height,
                            );

                            // Claude HUD overlay disabled — redundant with Claude's
                            // built-in statusline and thermal-monitor dashboard.

                            // ── Context saturation warning overlay ─────────────────
                            if self.context_warning_active {
                                let ctx_pct =
                                    self.claude_session
                                        .as_ref()
                                        .and_then(|s| s.context_percent)
                                        .unwrap_or(0.0) as f32;
                                self.grid_renderer.render_context_warning(
                                    ctx_pct,
                                    &self.wgpu.device,
                                    &self.wgpu.queue,
                                    &mut encoder,
                                    &view,
                                    self.width,
                                    self.height,
                                );
                            }

                            // ── Agent timeline overlay ─────────────────────────────
                            self.grid_renderer.render_agent_timeline(
                                &self.agent_timeline,
                                &self.wgpu.device,
                                &self.wgpu.queue,
                                &mut encoder,
                                &view,
                                self.width,
                                self.height,
                            );

                            // ── Agent graph overlay ────────────────────────────────
                            self.grid_renderer.render_agent_graph(
                                &self.agent_graph,
                                &self.wgpu.device,
                                &self.wgpu.queue,
                                &mut encoder,
                                &view,
                                self.width,
                                self.height,
                            );

                            // ── Bell flash overlay ─────────────────────────────────
                            if self.bell_flash_until.is_some() {
                                self.grid_renderer.render_bell_flash(
                                    &self.wgpu.device,
                                    &self.wgpu.queue,
                                    &mut encoder,
                                    &view,
                                    self.width,
                                    self.height,
                                );
                            }

                            // ── Agent overlay widgets ─────────────────────────────
                            if self.overlay.has_widgets() {
                                self.overlay.render(
                                    &self.wgpu.device,
                                    &self.wgpu.queue,
                                    &mut encoder,
                                    &view,
                                    self.width,
                                    self.height,
                                );
                            }

                            self.wgpu.queue.submit(std::iter::once(encoder.finish()));
                            output.present();
                            return RenderStatus::Presented;
                        }
                        Some(set)
                    }
                }
            };

        let content = term.renderable_content();

        let screen_lines = term.screen_lines();
        let display_offset = content.display_offset;
        let cursor = content.cursor;
        let selection_range = content.selection;

        // Collect cells into RenderCell snapshots while holding the lock.
        // When we have partial damage, only collect cells from damaged rows.
        let cells: Vec<RenderCell> = content
            .display_iter
            .filter_map(|indexed| {
                let point = indexed.point;
                let cell = indexed.cell;

                // Convert grid line to viewport row index.
                let viewport_line = point.line.0 + display_offset as i32;
                let row = usize::try_from(viewport_line).ok()?;
                if row >= screen_lines {
                    return None;
                }

                // Skip rows that aren't damaged (partial damage only).
                if let Some(ref damaged) = damaged_rows
                    && !damaged.contains(&row)
                {
                    return None;
                }

                // Skip wide char spacers.
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    return None;
                }

                let hyperlink = cell.hyperlink().map(|h| h.uri().to_owned());

                Some(RenderCell {
                    row,
                    col: point.column.0,
                    c: cell.c,
                    text: cell_display_text(cell.c, cell.zerowidth()),
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                    hyperlink,
                })
            })
            .collect();

        // Reset damage while we still hold the lock.
        term.reset_damage();

        // Release the term lock before the (potentially slow) GPU work.
        drop(term);

        // ── Regex URL detection ──────────────────────────────────────────
        // For cells that don't already have an OSC 8 hyperlink, detect
        // URLs via regex and annotate them as clickable.
        let mut cells = cells;
        detect_urls_in_cells(&mut cells, screen_lines);

        self.grid_renderer.render(
            &cells,
            &cursor,
            screen_lines,
            selection_range.as_ref(),
            display_offset,
            damaged_rows.as_ref(),
            &self.wgpu.device,
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // ── Kitty graphics inline images ──────────────────────────────────
        {
            let store = self.terminal.image_store();
            let mut store_guard = store.lock();
            self.grid_renderer.render_images(
                &store_guard,
                &self.wgpu.device,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
            self.grid_renderer
                .periodic_image_cleanup(&mut store_guard, screen_lines);
        }

        // ── Command block overlays ──────────────────────────────────────
        {
            let tracker = self.terminal.command_tracker();
            let blocks = tracker.lock().blocks.clone();
            self.grid_renderer.render_command_blocks(
                &blocks,
                display_offset,
                screen_lines,
                &self.wgpu.device,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
        }

        // ── Scroll indicator overlay ─────────────────────────────────────
        self.grid_renderer.render_scroll_indicator(
            display_offset,
            &self.wgpu.device,
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // Claude HUD overlay disabled — redundant with Claude's
        // built-in statusline and thermal-monitor dashboard.

        // ── Context saturation warning overlay ──────────────────────────
        if self.context_warning_active {
            let ctx_pct = self
                .claude_session
                .as_ref()
                .and_then(|s| s.context_percent)
                .unwrap_or(0.0) as f32;
            self.grid_renderer.render_context_warning(
                ctx_pct,
                &self.wgpu.device,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
        }

        // ── Agent timeline overlay ──────────────────────────────────────
        self.grid_renderer.render_agent_timeline(
            &self.agent_timeline,
            &self.wgpu.device,
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // ── Agent graph overlay ─────────────────────────────────────────
        self.grid_renderer.render_agent_graph(
            &self.agent_graph,
            &self.wgpu.device,
            &self.wgpu.queue,
            &mut encoder,
            &view,
            self.width,
            self.height,
        );

        // ── Bell flash overlay ──────────────────────────────────────────
        if self.bell_flash_until.is_some() {
            self.grid_renderer.render_bell_flash(
                &self.wgpu.device,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
        }

        // ── Agent overlay widgets ──────────────────────────────────────
        if self.overlay.has_widgets() {
            self.overlay.render(
                &self.wgpu.device,
                &self.wgpu.queue,
                &mut encoder,
                &view,
                self.width,
                self.height,
            );
        }

        self.wgpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        RenderStatus::Presented
    }
}
