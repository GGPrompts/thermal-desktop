//! ANSI-to-thermal color mapping helpers for the GPU terminal renderer.

use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use glyphon::Color as GlyphColor;
use thermal_core::palette::Color as PaletteColor;

use crate::grid_renderer::{ColorVertex, RenderCell, TERM_BG};

/// Map an alacritty_terminal ANSI Color to an [f32; 4] RGBA array.
pub(crate) fn ansi_to_glyphon_fg(color: &AnsiColor) -> [f32; 4] {
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

pub(crate) fn ansi_to_glyphon_bg(color: &AnsiColor) -> Option<[f32; 4]> {
    match color {
        AnsiColor::Named(NamedColor::Background) => None,
        AnsiColor::Named(NamedColor::Black) => None,
        AnsiColor::Named(named) => Some(named_to_thermal_bg(*named)),
        AnsiColor::Spec(rgb) => {
            if (rgb.r == 0 && rgb.g == 0 && rgb.b == 0)
                || (rgb.r == 10 && rgb.g == 0 && rgb.b == 16)
            {
                None
            } else {
                Some([
                    rgb.r as f32 / 255.0,
                    rgb.g as f32 / 255.0,
                    rgb.b as f32 / 255.0,
                    1.0,
                ])
            }
        }
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
/// Spread across the full thermal spectrum: dark bg -> blue -> teal -> green ->
/// yellow -> orange -> red -> white-hot.  Avoids clustering everything in the
/// purple/indigo range.
pub(crate) fn named_to_thermal_fg(named: NamedColor) -> [f32; 4] {
    // Aligned with kitty.conf in thermal-os-dotfiles/config/kitty/kitty.conf
    match named {
        NamedColor::Black => PaletteColor::BG.to_f32_array(),           // color0  #0a0010
        NamedColor::Red => PaletteColor::SEARING.to_f32_array(),        // color1  #ef4444
        NamedColor::Green => PaletteColor::WARM.to_f32_array(),         // color2  #22c55e
        NamedColor::Yellow => PaletteColor::HOT.to_f32_array(),         // color3  #eab308
        NamedColor::Blue => PaletteColor::ACCENT_COOL.to_f32_array(),   // color4  #3b82f6
        NamedColor::Magenta => PaletteColor::FREEZING.to_f32_array(),   // color5  #1a0030
        NamedColor::Cyan => PaletteColor::ACCENT_NEUTRAL.to_f32_array(),// color6  #14b8a6
        NamedColor::White | NamedColor::Foreground => PaletteColor::TEXT_BRIGHT.to_f32_array(), // color7 #e9e0ff

        NamedColor::BrightBlack => PaletteColor::COLD.to_f32_array(),   // color8  #4a3a8a
        NamedColor::BrightRed => PaletteColor::CRITICAL.to_f32_array(), // color9  #dc2626
        NamedColor::BrightGreen => PaletteColor::MILD.to_f32_array(),   // color10 #0d9488
        NamedColor::BrightYellow => PaletteColor::HOTTER.to_f32_array(),// color11 #f97316
        NamedColor::BrightBlue => PaletteColor::ACCENT_COLD.to_f32_array(), // color12 #818cf8
        NamedColor::BrightMagenta => PaletteColor::TEXT.to_f32_array(), // color13 #c4b5fd
        NamedColor::BrightCyan => PaletteColor::MILD.to_f32_array(),    // color14 #0d9488
        NamedColor::BrightWhite | NamedColor::BrightForeground => {
            PaletteColor::WHITE_HOT.to_f32_array()                      // color15 #fef3c7
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
/// in `ansi_to_glyphon_bg` (both return `None` -> transparent), so those arms
/// are retained here only as a safety fallback.
pub(crate) fn named_to_thermal_bg(named: NamedColor) -> [f32; 4] {
    // Aligned with kitty.conf — backgrounds use the same base colors as fg,
    // but muted for bright/dim variants so backgrounds don't overpower text.
    match named {
        NamedColor::Black => TERM_BG,
        NamedColor::Red => PaletteColor::SEARING.to_f32_array(),
        NamedColor::Green => PaletteColor::WARM.to_f32_array(),
        NamedColor::Yellow => PaletteColor::HOT.to_f32_array(),
        NamedColor::Blue => PaletteColor::ACCENT_COOL.to_f32_array(),
        NamedColor::Magenta => PaletteColor::FREEZING.to_f32_array(),   // #1a0030 (was HOTTER)
        NamedColor::Cyan => PaletteColor::ACCENT_NEUTRAL.to_f32_array(),
        NamedColor::White => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::Foreground => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::Background => TERM_BG,
        NamedColor::Cursor => PaletteColor::BG_SURFACE.to_f32_array(),

        // Bright backgrounds — muted variants
        NamedColor::BrightBlack => PaletteColor::COLD.to_f32_array(),   // #4a3a8a (was BG_LIGHT)
        NamedColor::BrightRed => PaletteColor::CRITICAL.to_f32_array(),
        NamedColor::BrightGreen => PaletteColor::MILD.to_f32_array(),
        NamedColor::BrightYellow => PaletteColor::HOTTER.to_f32_array(),
        NamedColor::BrightBlue => PaletteColor::COOL.to_f32_array(),
        NamedColor::BrightMagenta => PaletteColor::FREEZING.to_f32_array(),
        NamedColor::BrightCyan => PaletteColor::MILD.to_f32_array(),
        NamedColor::BrightWhite => PaletteColor::TEXT_MUTED.to_f32_array(),
        NamedColor::BrightForeground => PaletteColor::TEXT_MUTED.to_f32_array(),

        // Dim backgrounds — deep dark palette entries
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
pub(crate) fn indexed_color(idx: u8) -> [f32; 4] {
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
pub(crate) fn cell_bg_color(cell: &RenderCell) -> Option<[f32; 4]> {
    if cell.flags.contains(alacritty_terminal::term::cell::Flags::INVERSE) {
        Some(ansi_to_glyphon_fg(&cell.fg))
    } else {
        ansi_to_glyphon_bg(&cell.bg)
    }
}

/// Convert pixel-space rect to 6 NDC vertices (two triangles).
pub(crate) fn pixel_rect_to_ndc(
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
pub(crate) fn f32_to_glyph_color(c: [f32; 4]) -> GlyphColor {
    GlyphColor::rgba(
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8,
        (c[3] * 255.0) as u8,
    )
}

/// Convert a line between two pixel coordinates into a thin rectangle (6 vertices).
///
/// The rectangle is `thickness` pixels wide, oriented along the line direction.
/// Returns vertices in NDC for the rect pipeline.
pub(crate) fn line_to_rect_verts(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    thickness: f32,
    screen_w: f32,
    screen_h: f32,
    color: [f32; 4],
) -> [ColorVertex; 6] {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt().max(0.001);

    // Perpendicular unit vector.
    let nx = -dy / len * thickness * 0.5;
    let ny = dx / len * thickness * 0.5;

    // Four corners of the thin rectangle in pixel coordinates.
    let corners = [
        (x0 + nx, y0 + ny),
        (x0 - nx, y0 - ny),
        (x1 + nx, y1 + ny),
        (x1 - nx, y1 - ny),
    ];

    // Convert to NDC.
    let to_ndc = |px: f32, py: f32| -> [f32; 2] {
        [(px / screen_w) * 2.0 - 1.0, 1.0 - (py / screen_h) * 2.0]
    };

    let p0 = to_ndc(corners[0].0, corners[0].1);
    let p1 = to_ndc(corners[1].0, corners[1].1);
    let p2 = to_ndc(corners[2].0, corners[2].1);
    let p3 = to_ndc(corners[3].0, corners[3].1);

    [
        ColorVertex { position: p0, color },
        ColorVertex { position: p2, color },
        ColorVertex { position: p1, color },
        ColorVertex { position: p1, color },
        ColorVertex { position: p2, color },
        ColorVertex { position: p3, color },
    ]
}
