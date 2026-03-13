/// A single RGBA color with u8 components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    /// Construct from a packed 24-bit hex value (0xRRGGBB), alpha = 255.
    pub const fn from_hex(hex: u32) -> Self {
        Self {
            r: ((hex >> 16) & 0xFF) as u8,
            g: ((hex >> 8) & 0xFF) as u8,
            b: (hex & 0xFF) as u8,
            a: 0xFF,
        }
    }

    /// Construct from individual u8 components.
    pub const fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Return as a `[f32; 4]` RGBA array (each channel 0.0–1.0).
    pub fn to_f32_array(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }

    /// Return as a `(u8, u8, u8, u8)` tuple.
    pub const fn to_rgba_u8(self) -> (u8, u8, u8, u8) {
        (self.r, self.g, self.b, self.a)
    }

    /// Return a 24-bit ANSI foreground escape sequence: `\x1b[38;2;R;G;Bm`.
    pub fn to_ansi_escape(self) -> String {
        format!("\x1b[38;2;{};{};{}m", self.r, self.g, self.b)
    }
}

// ---------------------------------------------------------------------------
// Color constants matching the thermal palette
// ---------------------------------------------------------------------------

impl Color {
    // Void / Background
    pub const BG: Color = Color::from_hex(0x0a0010);
    pub const BG_LIGHT: Color = Color::from_hex(0x0f0018);
    pub const BG_SURFACE: Color = Color::from_hex(0x120822);

    // Cold spectrum
    pub const FREEZING: Color = Color::from_hex(0x1a0030);
    pub const COLD: Color = Color::from_hex(0x2d1b69);
    pub const COOL: Color = Color::from_hex(0x1e3a8a);

    // Neutral
    pub const MILD: Color = Color::from_hex(0x0d9488);
    pub const WARM: Color = Color::from_hex(0x22c55e);

    // Hot spectrum
    pub const HOT: Color = Color::from_hex(0xeab308);
    pub const HOTTER: Color = Color::from_hex(0xf97316);
    pub const SEARING: Color = Color::from_hex(0xef4444);
    pub const CRITICAL: Color = Color::from_hex(0xdc2626);

    // White-hot
    pub const WHITE_HOT: Color = Color::from_hex(0xfef3c7);

    // Text
    pub const TEXT: Color = Color::from_hex(0xc4b5fd);
    pub const TEXT_BRIGHT: Color = Color::from_hex(0xe9e0ff);
    pub const TEXT_MUTED: Color = Color::from_hex(0x7c6faa);

    // Accents
    pub const ACCENT_COLD: Color = Color::from_hex(0x6366f1);
    pub const ACCENT_COOL: Color = Color::from_hex(0x3b82f6);
    pub const ACCENT_NEUTRAL: Color = Color::from_hex(0x14b8a6);
    pub const ACCENT_WARM: Color = Color::from_hex(0xf59e0b);
    pub const ACCENT_HOT: Color = Color::from_hex(0xef4444);
}

// ---------------------------------------------------------------------------
// Gradient interpolation
// ---------------------------------------------------------------------------

/// Linearly interpolate between two u8 channel values.
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

/// Interpolate a `Color` between two `Color` values.
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: lerp_u8(a.r, b.r, t),
        g: lerp_u8(a.g, b.g, t),
        b: lerp_u8(a.b, b.b, t),
        a: lerp_u8(a.a, b.a, t),
    }
}

/// Map a heat value `t` in `[0.0, 1.0]` to a thermal-spectrum `Color`.
///
/// The gradient runs:
///   0.00 → deep-cold blue   (`COOL`)
///   0.20 → cold purple      (`COLD`)
///   0.40 → teal/mild        (`MILD`)
///   0.55 → warm green       (`WARM`)
///   0.70 → hot yellow       (`HOT`)
///   0.80 → hotter orange    (`HOTTER`)
///   0.90 → searing red      (`SEARING`)
///   1.00 → white-hot        (`WHITE_HOT`)
pub fn thermal_gradient(t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);

    // Gradient stops: (position, Color)
    const STOPS: [(f32, Color); 8] = [
        (0.00, Color::COOL),
        (0.20, Color::COLD),
        (0.40, Color::MILD),
        (0.55, Color::WARM),
        (0.70, Color::HOT),
        (0.80, Color::HOTTER),
        (0.90, Color::SEARING),
        (1.00, Color::WHITE_HOT),
    ];

    // Find the surrounding pair of stops.
    for i in 0..STOPS.len() - 1 {
        let (t0, c0) = STOPS[i];
        let (t1, c1) = STOPS[i + 1];
        if t <= t1 {
            let local = (t - t0) / (t1 - t0);
            return lerp_color(c0, c1, local);
        }
    }

    Color::WHITE_HOT
}

// ---------------------------------------------------------------------------
// Legacy [f32; 4] palette (kept for wgpu compatibility)
// ---------------------------------------------------------------------------

/// The thermal/FLIR color palette used across all components.
pub struct ThermalPalette;

impl ThermalPalette {
    // Void / Background
    pub const BG: [f32; 4] = Self::hex(0x0a, 0x00, 0x10);
    pub const BG_LIGHT: [f32; 4] = Self::hex(0x0f, 0x00, 0x18);
    pub const BG_SURFACE: [f32; 4] = Self::hex(0x12, 0x08, 0x22);

    // Cold spectrum
    pub const FREEZING: [f32; 4] = Self::hex(0x1a, 0x00, 0x30);
    pub const COLD: [f32; 4] = Self::hex(0x2d, 0x1b, 0x69);
    pub const COOL: [f32; 4] = Self::hex(0x1e, 0x3a, 0x8a);

    // Neutral
    pub const MILD: [f32; 4] = Self::hex(0x0d, 0x94, 0x88);
    pub const WARM: [f32; 4] = Self::hex(0x22, 0xc5, 0x5e);

    // Hot spectrum
    pub const HOT: [f32; 4] = Self::hex(0xea, 0xb3, 0x08);
    pub const HOTTER: [f32; 4] = Self::hex(0xf9, 0x73, 0x16);
    pub const SEARING: [f32; 4] = Self::hex(0xef, 0x44, 0x44);
    pub const CRITICAL: [f32; 4] = Self::hex(0xdc, 0x26, 0x26);

    // White-hot
    pub const WHITE_HOT: [f32; 4] = Self::hex(0xfe, 0xf3, 0xc7);

    // Text
    pub const TEXT: [f32; 4] = Self::hex(0xc4, 0xb5, 0xfd);
    pub const TEXT_BRIGHT: [f32; 4] = Self::hex(0xe9, 0xe0, 0xff);
    pub const TEXT_MUTED: [f32; 4] = Self::hex(0x7c, 0x6f, 0xaa);

    // Accents
    pub const ACCENT_COLD: [f32; 4] = Self::hex(0x63, 0x66, 0xf1);
    pub const ACCENT_COOL: [f32; 4] = Self::hex(0x3b, 0x82, 0xf6);
    pub const ACCENT_NEUTRAL: [f32; 4] = Self::hex(0x14, 0xb8, 0xa6);
    pub const ACCENT_WARM: [f32; 4] = Self::hex(0xf5, 0x9e, 0x0b);
    pub const ACCENT_HOT: [f32; 4] = Self::hex(0xef, 0x44, 0x44);

    const fn hex(r: u8, g: u8, b: u8) -> [f32; 4] {
        [
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            1.0,
        ]
    }
}
