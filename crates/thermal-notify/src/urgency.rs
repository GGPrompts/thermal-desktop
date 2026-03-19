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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Urgency::from_byte ───────────────────────────────────────────────────

    #[test]
    fn from_byte_zero_is_low() {
        assert_eq!(Urgency::from_byte(0), Urgency::Low);
    }

    #[test]
    fn from_byte_one_is_normal() {
        assert_eq!(Urgency::from_byte(1), Urgency::Normal);
    }

    #[test]
    fn from_byte_two_is_critical() {
        assert_eq!(Urgency::from_byte(2), Urgency::Critical);
    }

    #[test]
    fn from_byte_unknown_values_are_normal() {
        for b in [3u8, 10, 50, 100, 255] {
            assert_eq!(
                Urgency::from_byte(b),
                Urgency::Normal,
                "byte {b} should map to Normal"
            );
        }
    }

    // ── Urgency::from_hints ──────────────────────────────────────────────────

    #[test]
    fn from_hints_missing_key_defaults_to_normal() {
        let hints = HashMap::new();
        assert_eq!(Urgency::from_hints(&hints), Urgency::Normal);
    }

    #[test]
    fn from_hints_low_urgency() {
        let mut hints = HashMap::new();
        hints.insert("urgency".to_string(), Value::U8(0));
        assert_eq!(Urgency::from_hints(&hints), Urgency::Low);
    }

    #[test]
    fn from_hints_normal_urgency() {
        let mut hints = HashMap::new();
        hints.insert("urgency".to_string(), Value::U8(1));
        assert_eq!(Urgency::from_hints(&hints), Urgency::Normal);
    }

    #[test]
    fn from_hints_critical_urgency() {
        let mut hints = HashMap::new();
        hints.insert("urgency".to_string(), Value::U8(2));
        assert_eq!(Urgency::from_hints(&hints), Urgency::Critical);
    }

    #[test]
    fn from_hints_wrong_value_type_defaults_to_normal() {
        // urgency hint present but not a U8 — should fall back to Normal
        let mut hints = HashMap::new();
        hints.insert("urgency".to_string(), Value::Str("critical".into()));
        assert_eq!(Urgency::from_hints(&hints), Urgency::Normal);
    }

    // ── Urgency::to_color ────────────────────────────────────────────────────

    #[test]
    fn to_color_low_returns_cool() {
        assert_eq!(Urgency::Low.to_color(), ThermalPalette::COOL);
    }

    #[test]
    fn to_color_normal_returns_warm() {
        assert_eq!(Urgency::Normal.to_color(), ThermalPalette::WARM);
    }

    #[test]
    fn to_color_critical_returns_searing() {
        assert_eq!(Urgency::Critical.to_color(), ThermalPalette::SEARING);
    }

    #[test]
    fn to_color_returns_four_component_rgba() {
        for urgency in [Urgency::Low, Urgency::Normal, Urgency::Critical] {
            let c = urgency.to_color();
            assert_eq!(c.len(), 4, "color must have 4 components");
            // Alpha must be 1.0 (fully opaque from palette constants)
            assert!(
                (c[3] - 1.0).abs() < 1e-6,
                "{urgency:?} color alpha was {}", c[3]
            );
        }
    }

    // ── Urgency::default_timeout_ms ──────────────────────────────────────────

    #[test]
    fn default_timeout_low_is_5000() {
        assert_eq!(Urgency::Low.default_timeout_ms(), 5000);
    }

    #[test]
    fn default_timeout_normal_is_8000() {
        assert_eq!(Urgency::Normal.default_timeout_ms(), 8000);
    }

    #[test]
    fn default_timeout_critical_is_zero_persistent() {
        assert_eq!(Urgency::Critical.default_timeout_ms(), 0);
    }

    // ── Urgency derives ──────────────────────────────────────────────────────

    #[test]
    fn urgency_equality() {
        assert_eq!(Urgency::Low, Urgency::Low);
        assert_ne!(Urgency::Low, Urgency::Normal);
        assert_ne!(Urgency::Normal, Urgency::Critical);
    }

    #[test]
    fn urgency_copy() {
        let u = Urgency::Critical;
        let v = u; // Copy
        assert_eq!(u, v);
    }

    #[test]
    fn urgency_clone() {
        let u = Urgency::Normal;
        assert_eq!(u.clone(), u);
    }
}
