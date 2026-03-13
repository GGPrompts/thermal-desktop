use std::collections::HashMap;
use thermal_core::palette::ThermalPalette;
use zbus::zvariant::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => Urgency::Low,
            2 => Urgency::Critical,
            _ => Urgency::Normal,
        }
    }

    pub fn from_hints(hints: &HashMap<String, Value<'_>>) -> Self {
        hints
            .get("urgency")
            .and_then(|v| match v {
                Value::U8(b) => Some(*b),
                _ => None,
            })
            .map(Self::from_byte)
            .unwrap_or(Urgency::Normal)
    }

    /// Returns a `[f32; 4]` RGBA color from ThermalPalette for this urgency level.
    pub fn to_color(self) -> [f32; 4] {
        match self {
            Urgency::Low => ThermalPalette::COOL,      // cold blue
            Urgency::Normal => ThermalPalette::WARM,   // warm green
            Urgency::Critical => ThermalPalette::SEARING, // searing red
        }
    }

    /// Default timeout in ms: low=5000, normal=8000, critical=0 (persistent)
    pub fn default_timeout_ms(self) -> i32 {
        match self {
            Urgency::Low => 5000,
            Urgency::Normal => 8000,
            Urgency::Critical => 0,
        }
    }
}
