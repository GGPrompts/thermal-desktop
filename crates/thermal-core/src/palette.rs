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
