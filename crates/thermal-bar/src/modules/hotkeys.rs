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
