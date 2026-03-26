//! Kitty graphics protocol support for inline image rendering.
//!
//! Implements a subset of the [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/):
//! - Direct transmission (`t=d`) with base64-encoded payloads
//! - PNG format (`f=100`) decoded via the `image` crate
//! - Direct RGBA (`f=32`) and RGB (`f=24`) pixel data
//! - Transmit and display (`a=T`), transmit only (`a=t`), delete (`a=d`)
//! - Multi-chunk transmission (`m=1` for continuation, `m=0` for final chunk)
//!
//! ## APC Sequence Format
//!
//! ```text
//! ESC _G <key>=<value>,... ; <base64-payload> ESC \
//! ```
//!
//! Images are placed at the current cursor position and span the specified
//! number of grid columns and rows. The renderer creates wgpu textures on
//! demand and draws textured quads at the correct grid positions.

use std::collections::HashMap;

use base64::prelude::*;
use tracing::{debug, warn};

// ── Public types ─────────────────────────────────────────────────────────────

/// A decoded image stored in memory, ready to be uploaded to the GPU.
#[derive(Debug, Clone)]
pub struct StoredImage {
    /// Unique image ID (from the `i` parameter, or auto-assigned).
    pub id: u32,
    /// RGBA pixel data (4 bytes per pixel).
    pub rgba_data: Vec<u8>,
    /// Image width in pixels.
    pub width_px: u32,
    /// Image height in pixels.
    pub height_px: u32,
}

/// A placed image — where in the terminal grid an image should be displayed.
#[derive(Debug, Clone)]
pub struct PlacedImage {
    /// Image ID referencing a `StoredImage`.
    pub image_id: u32,
    /// Grid row where the image's top-left corner is placed.
    pub row: usize,
    /// Grid column where the image's top-left corner is placed.
    pub col: usize,
    /// Number of grid columns the image should span.
    pub cols_span: usize,
    /// Number of grid rows the image should span.
    pub rows_span: usize,
}

/// Parsed Kitty graphics command extracted from an APC sequence.
#[derive(Debug, Clone)]
pub struct GraphicsCommand {
    /// Action: 't' = transmit, 'T' = transmit+display, 'd' = delete, 'p' = put/display.
    pub action: char,
    /// Format: 32 = RGBA, 24 = RGB, 100 = PNG.
    pub format: u32,
    /// Transmission medium: 'd' = direct (base64 in payload).
    pub transmission: char,
    /// Image ID (0 means auto-assign).
    pub image_id: u32,
    /// Source image width in pixels (for raw formats).
    pub src_width: u32,
    /// Source image height in pixels (for raw formats).
    pub src_height: u32,
    /// Display columns to span in the grid.
    pub display_cols: u32,
    /// Display rows to span in the grid.
    pub display_rows: u32,
    /// More data flag: 1 = more chunks follow, 0 = final chunk.
    pub more: u32,
    /// The base64-encoded payload data.
    pub payload: Vec<u8>,
    /// Whether to suppress the OK response to the application.
    pub quiet: u32,
}

impl Default for GraphicsCommand {
    fn default() -> Self {
        Self {
            action: 'T', // Default: transmit and display
            format: 32,  // Default: RGBA
            transmission: 'd',
            image_id: 0,
            src_width: 0,
            src_height: 0,
            display_cols: 0,
            display_rows: 0,
            more: 0,
            payload: Vec::new(),
            quiet: 0,
        }
    }
}

// ── Image store ──────────────────────────────────────────────────────────────

/// Manages stored images and their grid placements.
///
/// Images are keyed by their ID. Placements record where each image should
/// appear in the terminal grid. The renderer queries this store each frame
/// to determine which textures to draw and where.
pub struct ImageStore {
    /// Stored images keyed by image ID.
    images: HashMap<u32, StoredImage>,
    /// Active placements — images currently visible in the grid.
    placements: Vec<PlacedImage>,
    /// Auto-incrementing ID counter for images without an explicit ID.
    next_id: u32,
    /// Accumulator for multi-chunk transmissions keyed by image ID.
    /// When `m=1`, payload chunks are appended here. On `m=0` (final),
    /// the accumulated data is processed.
    pending_chunks: HashMap<u32, PendingTransmission>,
}

/// Accumulated state for a multi-chunk image transmission.
struct PendingTransmission {
    command: GraphicsCommand,
    payload_accum: Vec<u8>,
}

impl ImageStore {
    pub fn new() -> Self {
        Self {
            images: HashMap::new(),
            placements: Vec::new(),
            next_id: 1,
            pending_chunks: HashMap::new(),
        }
    }

    /// Process a parsed graphics command. Returns an optional response string
    /// that should be sent back to the PTY (for protocol compliance).
    pub fn process(
        &mut self,
        cmd: GraphicsCommand,
        cursor_row: usize,
        cursor_col: usize,
    ) -> Option<String> {
        match cmd.action {
            'T' | 't' => self.handle_transmit(cmd, cursor_row, cursor_col),
            'd' => {
                self.handle_delete(&cmd);
                None
            }
            'p' => {
                // Put/display: place an already-transmitted image.
                self.handle_put(&cmd, cursor_row, cursor_col);
                None
            }
            _ => {
                debug!(action = %cmd.action, "Ignoring unknown graphics action");
                None
            }
        }
    }

    /// Handle transmit (`a=t`) and transmit+display (`a=T`) commands.
    fn handle_transmit(
        &mut self,
        mut cmd: GraphicsCommand,
        cursor_row: usize,
        cursor_col: usize,
    ) -> Option<String> {
        // Assign an ID if none was given.
        if cmd.image_id == 0 {
            cmd.image_id = self.next_id;
            self.next_id += 1;
        }

        let image_id = cmd.image_id;

        // Multi-chunk handling: if `m=1`, accumulate and wait for more.
        if cmd.more == 1 {
            let entry =
                self.pending_chunks
                    .entry(image_id)
                    .or_insert_with(|| PendingTransmission {
                        command: cmd.clone(),
                        payload_accum: Vec::new(),
                    });
            entry.payload_accum.extend_from_slice(&cmd.payload);
            return None;
        }

        // Final chunk (m=0 or not set). Combine with any pending chunks.
        let (final_cmd, full_payload) =
            if let Some(mut pending) = self.pending_chunks.remove(&image_id) {
                pending.payload_accum.extend_from_slice(&cmd.payload);
                // Use the original command's parameters but with the full payload.
                pending.command.payload = pending.payload_accum;
                pending.command.more = 0;
                let full_payload = pending.command.payload.clone();
                (pending.command, full_payload)
            } else {
                let payload = cmd.payload.clone();
                (cmd, payload)
            };

        // Decode base64 payload.
        let raw_data = match BASE64_STANDARD.decode(&full_payload) {
            Ok(data) => data,
            Err(e) => {
                warn!("Kitty graphics: base64 decode failed: {}", e);
                return None;
            }
        };

        // Decode image data based on format.
        let (rgba_data, width, height) = match final_cmd.format {
            100 => {
                // PNG format — decode using the image crate.
                match image::load_from_memory_with_format(&raw_data, image::ImageFormat::Png) {
                    Ok(img) => {
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        (rgba.into_raw(), w, h)
                    }
                    Err(e) => {
                        warn!("Kitty graphics: PNG decode failed: {}", e);
                        return None;
                    }
                }
            }
            32 => {
                // Direct RGBA — use payload as-is.
                let w = final_cmd.src_width;
                let h = final_cmd.src_height;
                if w == 0 || h == 0 {
                    warn!("Kitty graphics: RGBA format requires s= and v= dimensions");
                    return None;
                }
                let expected = (w * h * 4) as usize;
                if raw_data.len() < expected {
                    warn!(
                        "Kitty graphics: RGBA data too short (got {} bytes, expected {})",
                        raw_data.len(),
                        expected
                    );
                    return None;
                }
                (raw_data[..expected].to_vec(), w, h)
            }
            24 => {
                // Direct RGB — convert to RGBA.
                let w = final_cmd.src_width;
                let h = final_cmd.src_height;
                if w == 0 || h == 0 {
                    warn!("Kitty graphics: RGB format requires s= and v= dimensions");
                    return None;
                }
                let expected_rgb = (w * h * 3) as usize;
                if raw_data.len() < expected_rgb {
                    warn!(
                        "Kitty graphics: RGB data too short (got {} bytes, expected {})",
                        raw_data.len(),
                        expected_rgb
                    );
                    return None;
                }
                let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                for chunk in raw_data[..expected_rgb].chunks_exact(3) {
                    rgba.push(chunk[0]);
                    rgba.push(chunk[1]);
                    rgba.push(chunk[2]);
                    rgba.push(255);
                }
                (rgba, w, h)
            }
            _ => {
                warn!(
                    format = final_cmd.format,
                    "Kitty graphics: unsupported format"
                );
                return None;
            }
        };

        debug!(
            id = image_id,
            width,
            height,
            bytes = rgba_data.len(),
            "Kitty graphics: stored image"
        );

        // Store the image.
        self.images.insert(
            image_id,
            StoredImage {
                id: image_id,
                rgba_data,
                width_px: width,
                height_px: height,
            },
        );

        // If action is 'T' (transmit and display), also place it.
        if final_cmd.action == 'T' {
            let cols_span = if final_cmd.display_cols > 0 {
                final_cmd.display_cols as usize
            } else {
                // Will be computed by the renderer based on image/cell dimensions.
                0
            };
            let rows_span = if final_cmd.display_rows > 0 {
                final_cmd.display_rows as usize
            } else {
                0
            };

            self.placements.push(PlacedImage {
                image_id,
                row: cursor_row,
                col: cursor_col,
                cols_span,
                rows_span,
            });

            debug!(
                id = image_id,
                row = cursor_row,
                col = cursor_col,
                "Kitty graphics: placed image"
            );
        }

        // Build OK response (unless quiet).
        if final_cmd.quiet == 0 {
            Some(format!("\x1b_Gi={};OK\x1b\\", image_id))
        } else {
            None
        }
    }

    /// Handle delete command (`a=d`).
    fn handle_delete(&mut self, cmd: &GraphicsCommand) {
        if cmd.image_id > 0 {
            // Delete specific image and its placements.
            self.images.remove(&cmd.image_id);
            self.placements.retain(|p| p.image_id != cmd.image_id);
            debug!(id = cmd.image_id, "Kitty graphics: deleted image");
        } else {
            // Delete all images.
            self.images.clear();
            self.placements.clear();
            debug!("Kitty graphics: deleted all images");
        }
    }

    /// Handle put/display command (`a=p`) — place an already-transmitted image.
    fn handle_put(&mut self, cmd: &GraphicsCommand, cursor_row: usize, cursor_col: usize) {
        if !self.images.contains_key(&cmd.image_id) {
            warn!(
                id = cmd.image_id,
                "Kitty graphics: put for unknown image ID"
            );
            return;
        }

        self.placements.push(PlacedImage {
            image_id: cmd.image_id,
            row: cursor_row,
            col: cursor_col,
            cols_span: cmd.display_cols as usize,
            rows_span: cmd.display_rows as usize,
        });
    }

    /// Get all current placements with their image data.
    ///
    /// Returns references to placed images and their pixel data for rendering.
    pub fn visible_placements(&self) -> Vec<(&PlacedImage, &StoredImage)> {
        self.placements
            .iter()
            .filter_map(|p| self.images.get(&p.image_id).map(|img| (p, img)))
            .collect()
    }

    /// Remove placements that have scrolled out of the visible area.
    pub fn cleanup_scrolled(&mut self, max_visible_row: usize) {
        self.placements.retain(|p| {
            let bottom = p.row + p.rows_span.max(1);
            // Keep if any part could still be visible.
            // Be generous — keep images even if partially scrolled.
            bottom > 0 && p.row <= max_visible_row + 50
        });
    }

    /// Return the number of stored images (for debug/status).
    #[allow(dead_code)]
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    /// Return the number of active placements.
    #[allow(dead_code)]
    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }
}

// ── APC sequence parser ──────────────────────────────────────────────────────

/// Stateful scanner for Kitty graphics APC sequences in a raw byte stream.
///
/// Scans for `ESC _G` sequences, extracts the key=value parameters and
/// base64 payload, and returns parsed `GraphicsCommand`s. Non-graphics APC
/// sequences are ignored.
///
/// Unlike the OSC 633 parser, this parser *strips* matched graphics sequences
/// from the byte stream, returning the filtered bytes that should be forwarded
/// to alacritty_terminal.
#[derive(Debug)]
pub struct KittyGraphicsParser {
    /// State machine state.
    state: ParserState,
    /// Accumulated bytes of the current APC body.
    buf: Vec<u8>,
    /// Bytes that were part of the APC prefix and need to be replayed if
    /// the sequence turns out to not be a graphics command.
    prefix_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    /// Normal pass-through mode.
    Ground,
    /// Saw ESC (0x1B), waiting for `_` (0x5F) to confirm APC.
    SawEsc,
    /// Inside an APC sequence: saw `ESC _`, now reading the body.
    /// If the first body byte is 'G', it's a Kitty graphics command.
    InApc,
    /// Confirmed Kitty graphics APC — accumulating the body until ST.
    InGraphicsApc,
    /// Inside APC body, saw ESC — expecting `\` (0x5C) for ST terminator.
    ApcSawEsc,
    /// Inside graphics APC body, saw ESC — expecting `\` for ST.
    GraphicsApcSawEsc,
}

/// Result of feeding bytes through the parser.
pub struct ParseResult {
    /// Graphics commands found in this chunk.
    pub commands: Vec<GraphicsCommand>,
    /// Filtered bytes that should be forwarded to alacritty_terminal.
    /// Graphics APC sequences have been stripped out.
    pub passthrough: Vec<u8>,
}

impl KittyGraphicsParser {
    pub fn new() -> Self {
        Self {
            state: ParserState::Ground,
            buf: Vec::with_capacity(4096),
            prefix_bytes: Vec::with_capacity(8),
        }
    }

    /// Feed a chunk of raw PTY bytes. Returns parsed graphics commands and
    /// the filtered bytes (with graphics sequences stripped) to forward to
    /// alacritty_terminal.
    pub fn feed(&mut self, bytes: &[u8]) -> ParseResult {
        let mut commands = Vec::new();
        let mut passthrough = Vec::with_capacity(bytes.len());

        for &byte in bytes {
            match self.state {
                ParserState::Ground => {
                    if byte == 0x1B {
                        // Potential start of APC or other escape sequence.
                        self.state = ParserState::SawEsc;
                        self.prefix_bytes.clear();
                        self.prefix_bytes.push(byte);
                    } else {
                        passthrough.push(byte);
                    }
                }

                ParserState::SawEsc => {
                    if byte == 0x5F {
                        // ESC _ = APC start. Buffer the body.
                        self.state = ParserState::InApc;
                        self.buf.clear();
                        self.prefix_bytes.push(byte);
                    } else {
                        // Not APC — replay the ESC and this byte.
                        passthrough.extend_from_slice(&self.prefix_bytes);
                        passthrough.push(byte);
                        self.state = ParserState::Ground;
                        self.prefix_bytes.clear();
                    }
                }

                ParserState::InApc => {
                    if self.buf.is_empty() && byte == b'G' {
                        // First byte is 'G' — this is a Kitty graphics APC.
                        self.state = ParserState::InGraphicsApc;
                        self.buf.clear();
                        // Don't push 'G' into buf — it's just the identifier.
                    } else if byte == 0x1B {
                        // ESC inside non-graphics APC — check for ST.
                        self.state = ParserState::ApcSawEsc;
                    } else if byte == 0x07 {
                        // BEL terminates APC (some terminals accept this).
                        // Not a graphics APC — replay everything.
                        passthrough.extend_from_slice(&self.prefix_bytes);
                        passthrough.extend_from_slice(&self.buf);
                        passthrough.push(byte);
                        self.reset();
                    } else {
                        self.buf.push(byte);
                        // If we've accumulated enough bytes to know this isn't 'G',
                        // switch to pass-through for the rest of this APC.
                        // (We already checked buf.is_empty() above.)
                    }
                }

                ParserState::ApcSawEsc => {
                    if byte == 0x5C {
                        // ST (ESC \) terminates non-graphics APC — replay everything.
                        passthrough.extend_from_slice(&self.prefix_bytes);
                        passthrough.extend_from_slice(&self.buf);
                        passthrough.push(0x1B);
                        passthrough.push(0x5C);
                        self.reset();
                    } else {
                        // Not ST — continue buffering.
                        self.buf.push(0x1B);
                        self.buf.push(byte);
                        self.state = ParserState::InApc;
                    }
                }

                ParserState::InGraphicsApc => {
                    if byte == 0x1B {
                        self.state = ParserState::GraphicsApcSawEsc;
                    } else if byte == 0x07 {
                        // BEL terminates the graphics APC.
                        if let Some(cmd) = parse_graphics_body(&self.buf) {
                            commands.push(cmd);
                        }
                        self.reset();
                    } else {
                        self.buf.push(byte);
                    }
                }

                ParserState::GraphicsApcSawEsc => {
                    if byte == 0x5C {
                        // ST terminates the graphics APC.
                        if let Some(cmd) = parse_graphics_body(&self.buf) {
                            commands.push(cmd);
                        }
                        self.reset();
                    } else {
                        // Not ST — ESC is part of the body (unusual but possible).
                        self.buf.push(0x1B);
                        self.buf.push(byte);
                        self.state = ParserState::InGraphicsApc;
                    }
                }
            }
        }

        ParseResult {
            commands,
            passthrough,
        }
    }

    fn reset(&mut self) {
        self.state = ParserState::Ground;
        self.buf.clear();
        self.prefix_bytes.clear();
    }
}

/// Parse the body of a Kitty graphics APC (everything after `ESC _G` and before ST).
///
/// Format: `key=value,key=value,...;base64payload`
fn parse_graphics_body(body: &[u8]) -> Option<GraphicsCommand> {
    // Split on `;` to separate control keys from payload.
    let body_str = std::str::from_utf8(body).ok()?;

    let (params_str, payload_str) = if let Some(idx) = body_str.find(';') {
        (&body_str[..idx], &body_str[idx + 1..])
    } else {
        (body_str, "")
    };

    let mut cmd = GraphicsCommand {
        payload: payload_str.as_bytes().to_vec(),
        ..Default::default()
    };

    // Parse key=value pairs.
    for pair in params_str.split(',') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "a" => {
                    if let Some(ch) = value.chars().next() {
                        cmd.action = ch;
                    }
                }
                "f" => {
                    cmd.format = value.parse().unwrap_or(32);
                }
                "t" => {
                    if let Some(ch) = value.chars().next() {
                        cmd.transmission = ch;
                    }
                }
                "i" => {
                    cmd.image_id = value.parse().unwrap_or(0);
                }
                "s" => {
                    cmd.src_width = value.parse().unwrap_or(0);
                }
                "v" => {
                    cmd.src_height = value.parse().unwrap_or(0);
                }
                "c" => {
                    cmd.display_cols = value.parse().unwrap_or(0);
                }
                "r" => {
                    cmd.display_rows = value.parse().unwrap_or(0);
                }
                "m" => {
                    cmd.more = value.parse().unwrap_or(0);
                }
                "q" => {
                    cmd.quiet = value.parse().unwrap_or(0);
                }
                _ => {
                    // Unknown parameter — ignore.
                }
            }
        }
    }

    Some(cmd)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Kitty graphics APC sequence terminated by ST (ESC \).
    fn graphics_apc(body: &str) -> Vec<u8> {
        let mut v = b"\x1b_G".to_vec();
        v.extend_from_slice(body.as_bytes());
        v.extend_from_slice(b"\x1b\\");
        v
    }

    /// Build a Kitty graphics APC sequence terminated by BEL.
    fn graphics_apc_bel(body: &str) -> Vec<u8> {
        let mut v = b"\x1b_G".to_vec();
        v.extend_from_slice(body.as_bytes());
        v.push(0x07);
        v
    }

    #[test]
    fn parse_simple_transmit_display() {
        let mut parser = KittyGraphicsParser::new();
        // A minimal PNG transmit+display: a=T,f=100;base64data
        let seq = graphics_apc("a=T,f=100;dGVzdA==");
        let result = parser.feed(&seq);

        assert_eq!(result.commands.len(), 1);
        let cmd = &result.commands[0];
        assert_eq!(cmd.action, 'T');
        assert_eq!(cmd.format, 100);
        assert_eq!(cmd.payload, b"dGVzdA==");
        // Graphics sequence should be stripped from passthrough.
        assert!(result.passthrough.is_empty());
    }

    #[test]
    fn parse_with_bel_terminator() {
        let mut parser = KittyGraphicsParser::new();
        let seq = graphics_apc_bel("a=t,f=32,s=2,v=2,i=42;AAAA");
        let result = parser.feed(&seq);

        assert_eq!(result.commands.len(), 1);
        let cmd = &result.commands[0];
        assert_eq!(cmd.action, 't');
        assert_eq!(cmd.format, 32);
        assert_eq!(cmd.src_width, 2);
        assert_eq!(cmd.src_height, 2);
        assert_eq!(cmd.image_id, 42);
    }

    #[test]
    fn non_graphics_apc_passes_through() {
        let mut parser = KittyGraphicsParser::new();
        // A non-graphics APC (doesn't start with G).
        let seq = b"\x1b_Xsomething\x1b\\";
        let result = parser.feed(seq);

        assert!(result.commands.is_empty());
        // Should pass through the entire sequence.
        assert_eq!(result.passthrough.len(), seq.len());
    }

    #[test]
    fn mixed_content_strips_graphics() {
        let mut parser = KittyGraphicsParser::new();
        let mut bytes = b"Hello".to_vec();
        bytes.extend_from_slice(&graphics_apc("a=T,f=100;AAAA"));
        bytes.extend_from_slice(b"World");

        let result = parser.feed(&bytes);

        assert_eq!(result.commands.len(), 1);
        assert_eq!(&result.passthrough, b"HelloWorld");
    }

    #[test]
    fn split_across_chunks() {
        let mut parser = KittyGraphicsParser::new();
        let full = graphics_apc("a=T,f=100;AAAA");
        let mid = full.len() / 2;

        let r1 = parser.feed(&full[..mid]);
        let r2 = parser.feed(&full[mid..]);

        let total_commands = r1.commands.len() + r2.commands.len();
        assert_eq!(total_commands, 1);
    }

    #[test]
    fn image_store_transmit_and_display() {
        let mut store = ImageStore::new();

        // Create a tiny 1x1 white PNG.
        let png_data = create_test_png(1, 1);
        let b64 = BASE64_STANDARD.encode(&png_data);

        let cmd = GraphicsCommand {
            action: 'T',
            format: 100,
            payload: b64.into_bytes(),
            ..Default::default()
        };

        let response = store.process(cmd, 5, 10);

        // Should have stored and placed the image.
        assert_eq!(store.image_count(), 1);
        assert_eq!(store.placement_count(), 1);

        // Should return an OK response.
        assert!(response.is_some());
        assert!(response.unwrap().contains("OK"));

        // Check placement position.
        let placements = store.visible_placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].0.row, 5);
        assert_eq!(placements[0].0.col, 10);
    }

    #[test]
    fn image_store_delete() {
        let mut store = ImageStore::new();

        let png_data = create_test_png(1, 1);
        let b64 = BASE64_STANDARD.encode(&png_data);

        let cmd = GraphicsCommand {
            action: 'T',
            format: 100,
            image_id: 7,
            payload: b64.into_bytes(),
            ..Default::default()
        };
        store.process(cmd, 0, 0);
        assert_eq!(store.image_count(), 1);

        // Delete.
        let del = GraphicsCommand {
            action: 'd',
            image_id: 7,
            ..Default::default()
        };
        store.process(del, 0, 0);
        assert_eq!(store.image_count(), 0);
        assert_eq!(store.placement_count(), 0);
    }

    #[test]
    fn multi_chunk_transmission() {
        let mut store = ImageStore::new();

        let png_data = create_test_png(2, 2);
        let b64 = BASE64_STANDARD.encode(&png_data);

        // Split the base64 data in half.
        let mid = b64.len() / 2;
        let chunk1 = &b64[..mid];
        let chunk2 = &b64[mid..];

        // First chunk (m=1).
        let cmd1 = GraphicsCommand {
            action: 'T',
            format: 100,
            image_id: 99,
            more: 1,
            payload: chunk1.as_bytes().to_vec(),
            ..Default::default()
        };
        let resp1 = store.process(cmd1, 0, 0);
        assert!(resp1.is_none()); // No response for intermediate chunks.
        assert_eq!(store.image_count(), 0); // Not stored yet.

        // Final chunk (m=0).
        let cmd2 = GraphicsCommand {
            action: 'T',
            format: 100,
            image_id: 99,
            more: 0,
            payload: chunk2.as_bytes().to_vec(),
            ..Default::default()
        };
        let resp2 = store.process(cmd2, 3, 5);
        assert!(resp2.is_some());
        assert_eq!(store.image_count(), 1);
        assert_eq!(store.placement_count(), 1);
    }

    /// Create a minimal PNG image for testing.
    fn create_test_png(width: u32, height: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut buf);
        use image::ImageEncoder;
        let rgba: Vec<u8> = vec![255; (width * height * 4) as usize];
        encoder
            .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
            .expect("PNG encode failed");
        buf
    }
}
