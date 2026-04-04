//! Helper functions: cell/color conversion, grid snapshots, name generation.

use crate::kitty::next_unique_name;
use crate::protocol::{CellData, ColorData};
use crate::terminal::Terminal;

/// Convert an alacritty terminal cell to our wire format `CellData`.
pub(super) fn cell_to_data(cell: &alacritty_terminal::term::cell::Cell) -> CellData {
    CellData {
        ch: cell.c,
        fg: color_to_data(cell.fg),
        bg: color_to_data(cell.bg),
        flags: cell.flags.bits(),
    }
}

/// Convert an alacritty Rgb/Color to our wire format `ColorData`.
fn color_to_data(color: alacritty_terminal::vte::ansi::Color) -> ColorData {
    // alacritty_terminal::vte::ansi::Color can be Named, Spec, or Indexed.
    // For the wire protocol we resolve to a default RGB value.
    match color {
        alacritty_terminal::vte::ansi::Color::Spec(rgb) => ColorData {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        },
        alacritty_terminal::vte::ansi::Color::Named(name) => {
            // Map named colors to reasonable defaults.
            let (r, g, b) = named_color_rgb(name);
            ColorData { r, g, b }
        }
        alacritty_terminal::vte::ansi::Color::Indexed(idx) => {
            // Use the standard 256-color palette approximation.
            let (r, g, b) = indexed_color_rgb(idx);
            ColorData { r, g, b }
        }
    }
}

/// Map a named color to an approximate RGB value.
fn named_color_rgb(name: alacritty_terminal::vte::ansi::NamedColor) -> (u8, u8, u8) {
    use alacritty_terminal::vte::ansi::NamedColor;
    match name {
        NamedColor::Black => (0, 0, 0),
        NamedColor::Red => (204, 0, 0),
        NamedColor::Green => (78, 154, 6),
        NamedColor::Yellow => (196, 160, 0),
        NamedColor::Blue => (52, 101, 164),
        NamedColor::Magenta => (117, 80, 123),
        NamedColor::Cyan => (6, 152, 154),
        NamedColor::White => (211, 215, 207),
        NamedColor::BrightBlack => (85, 87, 83),
        NamedColor::BrightRed => (239, 41, 41),
        NamedColor::BrightGreen => (138, 226, 52),
        NamedColor::BrightYellow => (252, 233, 79),
        NamedColor::BrightBlue => (114, 159, 207),
        NamedColor::BrightMagenta => (173, 127, 168),
        NamedColor::BrightCyan => (52, 226, 226),
        NamedColor::BrightWhite => (238, 238, 236),
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor => {
            (211, 215, 207)
        }
        NamedColor::Background => (0, 0, 0),
        NamedColor::DimBlack => (40, 40, 40),
        NamedColor::DimRed => (150, 0, 0),
        NamedColor::DimGreen => (50, 100, 4),
        NamedColor::DimYellow => (140, 110, 0),
        NamedColor::DimBlue => (35, 70, 110),
        NamedColor::DimMagenta => (80, 55, 85),
        NamedColor::DimCyan => (4, 105, 106),
        NamedColor::DimWhite | NamedColor::DimForeground => (150, 152, 147),
    }
}

/// Map a 256-color index to RGB.
fn indexed_color_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0..=15 => {
            // Standard 16 colors — map via named.
            let named = [
                (0, 0, 0),
                (204, 0, 0),
                (78, 154, 6),
                (196, 160, 0),
                (52, 101, 164),
                (117, 80, 123),
                (6, 152, 154),
                (211, 215, 207),
                (85, 87, 83),
                (239, 41, 41),
                (138, 226, 52),
                (252, 233, 79),
                (114, 159, 207),
                (173, 127, 168),
                (52, 226, 226),
                (238, 238, 236),
            ];
            named[idx as usize]
        }
        16..=231 => {
            // 6x6x6 color cube.
            let idx = idx - 16;
            let r = idx / 36;
            let g = (idx % 36) / 6;
            let b = idx % 6;
            let to_val = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            (to_val(r), to_val(g), to_val(b))
        }
        232..=255 => {
            // Grayscale ramp.
            let v = 8 + 10 * (idx - 232);
            (v, v, v)
        }
    }
}

/// Generate a display name from the shell path.
///
/// Extracts the basename of the shell binary (e.g. "/bin/zsh" -> "zsh",
/// "/usr/bin/bash" -> "bash"). Falls back to "session-N" if the path
/// has no recognizable basename.
pub(super) fn generate_name_from_shell(shell_path: &str, id_num: u64) -> String {
    std::path::Path::new(shell_path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("session-{id_num}"))
}

/// Assign a unique name given a base and a list of existing names.
///
/// - If `base` is not taken, returns it as-is (e.g. "zsh").
/// - If taken, appends a dedup suffix: "zsh-2", "zsh-3", etc.
///
/// Delegates to the shared `next_unique_name()` helper in kitty.rs.
pub(super) fn assign_unique_name(base: &str, existing: &[String]) -> String {
    next_unique_name(base, |candidate| existing.iter().any(|n| n == candidate))
}

/// Create a full grid snapshot from a Terminal.
pub(super) fn snapshot_cells(terminal: &Terminal, screen_lines: usize, cols: usize) -> Vec<CellData> {
    let term_handle = terminal.term_handle();
    let term = term_handle.lock();
    let content = term.renderable_content();

    let mut grid = vec![
        CellData {
            ch: ' ',
            fg: ColorData {
                r: 211,
                g: 215,
                b: 207
            },
            bg: ColorData { r: 0, g: 0, b: 0 },
            flags: 0,
        };
        screen_lines * cols
    ];

    for indexed in content.display_iter {
        let point = indexed.point;
        let cell = indexed.cell;
        let viewport_line = point.line.0 + content.display_offset as i32;
        let row = match usize::try_from(viewport_line) {
            Ok(r) if r < screen_lines => r,
            _ => continue,
        };
        let col = point.column.0;
        if col < cols {
            grid[row * cols + col] = cell_to_data(cell);
        }
    }

    grid
}
