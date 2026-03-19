/// Hotkey cheat-sheet module for thermal-bar's center zone.
///
/// Renders a strip of thermal-colored hotkey hints so the user can learn
/// keybindings at a glance. Each hint is a small `ModuleOutput` with a
/// unicode icon + key combo, colored by action "heat":
///
/// - Searing/hot: destructive or exit actions
/// - Warm/accent: primary workflow (terminal, launcher)
/// - Cool/mild: navigation and utility
/// - Cold: informational / monitoring
use thermal_core::ThermalPalette;

use crate::layout::{ModuleOutput, Zone};

/// A single hotkey hint definition.
struct Hotkey {
    icon: &'static str,
    keys: &'static str,
    label: &'static str,
    color: [f32; 4],
}

/// All hotkey hints to display, ordered left-to-right.
const HOTKEYS: &[Hotkey] = &[
    Hotkey { icon: "\u{25b8}", keys: "\u{2318}\u{23ce}", label: "term",   color: ThermalPalette::WARM },
    Hotkey { icon: "\u{2715}", keys: "\u{2318}Q",        label: "close",  color: ThermalPalette::SEARING },
    Hotkey { icon: "\u{25c9}", keys: "\u{2318}D",        label: "launch", color: ThermalPalette::ACCENT_WARM },
    Hotkey { icon: "\u{2261}", keys: "\u{2318}E",        label: "files",  color: ThermalPalette::MILD },
    Hotkey { icon: "\u{25a3}", keys: "\u{2318}F",        label: "full",   color: ThermalPalette::ACCENT_COOL },
    Hotkey { icon: "\u{25f1}", keys: "\u{2318}V",        label: "float",  color: ThermalPalette::ACCENT_COOL },
    Hotkey { icon: "\u{2691}", keys: "\u{2318}N",        label: "notif",  color: ThermalPalette::TEXT_BRIGHT },
    Hotkey { icon: "\u{2327}", keys: "\u{2318}M",        label: "exit",   color: ThermalPalette::SEARING },
    Hotkey { icon: "\u{2603}", keys: "\u{2318}B",        label: "btop",   color: ThermalPalette::ACCENT_COOL },
    Hotkey { icon: "\u{2388}", keys: "\u{2318}T",        label: "therm",  color: ThermalPalette::ACCENT_HOT },
    Hotkey { icon: "\u{2399}", keys: "PrtSc",            label: "snap",   color: ThermalPalette::WARM },
    Hotkey { icon: "\u{25a8}", keys: "\u{2318}S",        label: "stash",  color: ThermalPalette::ACCENT_COOL },
    Hotkey { icon: "\u{2753}", keys: "\u{2318}/",        label: "help",   color: ThermalPalette::TEXT_BRIGHT },
];

pub struct HotkeysModule;

impl HotkeysModule {
    pub fn new() -> Self {
        Self
    }

    /// Produce center-zone module outputs for the hotkey legend strip.
    pub fn render(&self) -> Vec<ModuleOutput> {
        HOTKEYS
            .iter()
            .map(|hk| {
                let text = format!("{} {} {}", hk.icon, hk.keys, hk.label);
                ModuleOutput::new(Zone::Center, text, hk.color)
            })
            .collect()
    }
}

impl Default for HotkeysModule {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Zone;

    #[test]
    fn render_returns_one_output_per_hotkey() {
        let outputs = HotkeysModule::new().render();
        assert_eq!(outputs.len(), HOTKEYS.len());
    }

    #[test]
    fn render_all_outputs_are_center_zone() {
        let outputs = HotkeysModule::new().render();
        for m in &outputs {
            assert_eq!(m.zone, Zone::Center, "expected Center zone, got {:?}", m.zone);
        }
    }

    #[test]
    fn render_text_contains_icon_keys_and_label() {
        let outputs = HotkeysModule::new().render();
        // Spot-check: the first entry should contain "term".
        assert!(outputs[0].text.contains("term"),
            "first hotkey text '{}' should contain 'term'", outputs[0].text);
    }

    #[test]
    fn render_text_contains_close_label() {
        let outputs = HotkeysModule::new().render();
        let has_close = outputs.iter().any(|m| m.text.contains("close"));
        assert!(has_close, "expected a 'close' hotkey in outputs");
    }

    #[test]
    fn render_text_is_non_empty() {
        let outputs = HotkeysModule::new().render();
        for m in &outputs {
            assert!(!m.text.is_empty(), "hotkey text should not be empty");
        }
    }

    #[test]
    fn render_colors_are_valid_rgba() {
        let outputs = HotkeysModule::new().render();
        for m in &outputs {
            for &ch in &m.color {
                assert!(ch >= 0.0 && ch <= 1.0, "color channel out of range: {ch}");
            }
        }
    }

    #[test]
    fn default_produces_same_output_as_new() {
        let a = HotkeysModule::new().render();
        let b = HotkeysModule::default().render();
        assert_eq!(a.len(), b.len());
        for (ma, mb) in a.iter().zip(b.iter()) {
            assert_eq!(ma.text, mb.text);
            assert_eq!(ma.color, mb.color);
        }
    }

    #[test]
    fn hotkeys_count_is_correct() {
        // Verify the constant HOTKEYS array has the expected entries.
        assert_eq!(HOTKEYS.len(), 13, "expected 13 hotkey entries");
    }

    #[test]
    fn render_each_text_has_three_parts() {
        // Each ModuleOutput text is "{icon} {keys} {label}" — at least 2 spaces.
        let outputs = HotkeysModule::new().render();
        for m in &outputs {
            let parts: Vec<&str> = m.text.splitn(3, ' ').collect();
            assert_eq!(parts.len(), 3,
                "expected 3 space-delimited parts in '{}', got {}", m.text, parts.len());
        }
    }
}
