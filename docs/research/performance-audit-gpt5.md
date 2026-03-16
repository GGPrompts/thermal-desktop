# Performance Audit — GPT-5.4 Ultra High (March 2026)

## Root Cause: Full-Frame Text Rebuild Every Dirty Update

The biggest loss is in the frame path, not PTY I/O. Every dirty update:
1. Lock Term, copy full visible grid into `Vec<RenderCell>` (window.rs)
2. Regroup by row, sort again (grid_renderer.rs) — redundant, display_iter is row-major
3. Allocate new glyphon Buffers per row (grid_renderer.rs)
4. `set_rich_text()` + `Shaping::Advanced` (HarfBuzz) per row
5. `prepare()` all text again

This is why htop/btop feel slow: full-screen churn = full viewport re-layout every burst.

## Fix Priority

### 1. Use Term::damage() API (highest impact)
alacritty_terminal 0.25 exposes `term.damage()` / `term.reset_damage()`. Keep persistent
per-row caches, update only damaged rows, reset damage after snapshotting. This is what
Alacritty itself does (`DamageTracker`).

### 2. Persistent per-row glyphon Buffers
Don't recreate Buffers every frame. One Buffer per visible row, rebuild only rows whose
damage says they changed. Row hash or damage index tracks staleness.

### 3. Remove redundant per-row sort
`display_iter()` is already row-major in alacritty's grid iterator. The sort in
grid_renderer.rs is unnecessary work.

### 4. Don't render on keypress (input latency)
Normal terminal input has no local echo. Marking dirty on every keypress forces an extra
empty frame before the PTY response arrives. Only render when PTY output arrives.

### 5. Lower swapchain frame latency
Current: `desired_maximum_frame_latency: 2`. Lower to 1 to reduce missed-vblank visibility.

### 6. Persistent GPU rect buffers
Creating GPU buffers every frame for cell backgrounds/cursor. Replace with persistent
upload/instance buffers.

### 7. Stop trimming glyph atlas every frame
`atlas.trim()` every frame works against keeping glyph caches hot. Only trim on explicit
memory pressure or very infrequent maintenance.

### 8. Switch to Shaping::Basic for grid
HarfBuzz shaping is overkill for monospace terminal grids and may cause glyph position
drift. Use Basic for cell rendering (also fixes Unicode width misalignment).
Keep Advanced only for font fallback if needed.

## Architecture (longer term)

### Instanced glyph atlas renderer
Move from "text layout engine every frame" to "retained glyph renderer with damage."
Glyph atlas + instanced quads for fg glyphs, backgrounds, cursor. Update only damaged
row ranges. This is what kitty, alacritty, and rio do.

### Simplify threading
Drop tokio entirely. Two threads: blocking PTY/parser thread that reads and feeds Term
directly, and UI/render thread with eventfd (cleaner than pipe). Removes Vec<u8> channel
churn and tokio runtime.

## Event loop correctness
window.rs prepare_read() usage doesn't match Wayland contract: should read only if fd is
readable, otherwise drop the guard. Treat as correctness cleanup, not main perf issue.

## References
- kitty performance: https://sw.kovidgoyal.net/kitty/performance/
- Alacritty display: https://github.com/alacritty/alacritty/blob/master/alacritty/src/display/mod.rs
- Rio 0.2.18 notes: https://rioterm.com/docs/upgrade-notes-0.2.18
