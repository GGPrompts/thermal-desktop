/// Workspace map module for thermal-bar's center zone.
///
/// Polls Hyprland via `hyprctl` to show occupied workspaces with window icons.
/// Active workspace is highlighted in hot colors; others use cool/muted tones.
use std::process::Command;

use thermal_core::ThermalPalette;

use crate::layout::{ModuleOutput, Zone};

/// Map a window class name to a Nerd Font icon.
fn class_to_icon(class: &str) -> &'static str {
    let lower = class.to_lowercase();
    match lower.as_str() {
        "kitty" | "alacritty" | "foot" | "wezterm" | "ghostty" => "\u{f120}",  // terminal
        "firefox" | "firefox-esr" | "librewolf" | "zen" | "zen-browser" => "\u{f269}",  // firefox/browser
        "chromium" | "google-chrome" | "brave-browser" => "\u{f268}",  // chrome
        "thunar" | "nautilus" | "dolphin" | "pcmanfm" | "nemo" => "\u{f07b}",  // folder
        "code" | "code-oss" | "vscodium" => "\u{e70c}",  // vscode
        "discord" => "\u{f392}",  // discord
        "slack" => "\u{f198}",  // slack
        "spotify" => "\u{f1bc}",  // spotify
        "steam" => "\u{f1b6}",  // steam
        "obs" | "obs-studio" => "\u{f03d}",  // video camera
        "gimp" | "krita" | "inkscape" => "\u{f1fc}",  // paint brush
        "vlc" | "mpv" | "celluloid" => "\u{f144}",  // play circle
        "telegram-desktop" | "telegramdesktop" => "\u{f2c6}",  // telegram
        "signal" => "\u{f4ad}",  // comment dots (messaging)
        "thunderbird" | "evolution" => "\u{f0e0}",  // envelope
        "libreoffice" | "soffice" => "\u{f15c}",  // file text
        "zathura" | "evince" | "okular" => "\u{f1c1}",  // file pdf
        "pavucontrol" | "pwvucontrol" => "\u{f028}",  // volume
        "btop" | "htop" => "\u{f080}",  // bar chart
        "eog" | "loupe" | "feh" | "imv" => "\u{f03e}",  // image
        _ => "\u{f2d0}",  // window (generic)
    }
}

/// A workspace with its windows' icons.
struct WorkspaceInfo {
    id: i64,
    icons: Vec<&'static str>,
}

/// Query Hyprland for the active workspace ID.
fn get_active_workspace_id() -> Option<i64> {
    let output = Command::new("hyprctl")
        .args(["activeworkspace", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json.get("id")?.as_i64()
}

/// Query Hyprland for all clients, grouped by workspace.
fn get_workspaces() -> Vec<WorkspaceInfo> {
    let output = match Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let clients: Vec<serde_json::Value> = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Collect windows per workspace.
    let mut map: std::collections::BTreeMap<i64, Vec<&'static str>> =
        std::collections::BTreeMap::new();

    for client in &clients {
        let ws_id = client
            .get("workspace")
            .and_then(|w| w.get("id"))
            .and_then(|id| id.as_i64())
            .unwrap_or(-1);

        // Skip special workspaces (negative IDs) and workspace 0 (unmanaged).
        if ws_id <= 0 {
            continue;
        }

        let class = client
            .get("class")
            .and_then(|c| c.as_str())
            .unwrap_or("");

        // Skip clients with empty class (e.g. layer surfaces).
        if class.is_empty() {
            continue;
        }

        let icon = class_to_icon(class);
        map.entry(ws_id).or_default().push(icon);
    }

    map.into_iter()
        .map(|(id, icons)| WorkspaceInfo { id, icons })
        .collect()
}

pub struct WorkspaceMapModule;

impl WorkspaceMapModule {
    pub fn new() -> Self {
        Self
    }

    /// Produce center-zone module outputs for the workspace map.
    pub fn render(&self) -> Vec<ModuleOutput> {
        let active_id = get_active_workspace_id().unwrap_or(-1);
        let workspaces = get_workspaces();

        if workspaces.is_empty() {
            // Fallback: show a single muted label if hyprctl is unavailable.
            return vec![ModuleOutput::new(
                Zone::Center,
                "\u{f24d} no workspaces",
                ThermalPalette::TEXT_MUTED,
            )];
        }

        workspaces
            .iter()
            .map(|ws| {
                let icons: String = ws.icons.join(" ");
                let text = format!("{} {}", ws.id, icons);

                let is_active = ws.id == active_id;
                let color = if is_active {
                    ThermalPalette::ACCENT_HOT
                } else {
                    ThermalPalette::COOL
                };

                let mut output = ModuleOutput::new(Zone::Center, text, color);

                // Give the active workspace a subtle background highlight.
                if is_active {
                    output = output.with_bg(ThermalPalette::BG_SURFACE);
                }

                output
            })
            .collect()
    }
}

impl Default for WorkspaceMapModule {
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

    #[test]
    fn class_to_icon_known_classes() {
        assert_eq!(class_to_icon("kitty"), "\u{f120}");
        assert_eq!(class_to_icon("firefox"), "\u{f269}");
        assert_eq!(class_to_icon("thunar"), "\u{f07b}");
        assert_eq!(class_to_icon("discord"), "\u{f392}");
    }

    #[test]
    fn class_to_icon_case_insensitive() {
        assert_eq!(class_to_icon("Firefox"), "\u{f269}");
        assert_eq!(class_to_icon("KITTY"), "\u{f120}");
    }

    #[test]
    fn class_to_icon_unknown_returns_generic_window() {
        assert_eq!(class_to_icon("some-random-app"), "\u{f2d0}");
    }

    #[test]
    fn render_returns_center_zone_outputs() {
        // This test runs without Hyprland, so it should hit the fallback path.
        let module = WorkspaceMapModule::new();
        let outputs = module.render();
        assert!(!outputs.is_empty());
        for m in &outputs {
            assert_eq!(m.zone, Zone::Center);
        }
    }

    #[test]
    fn render_colors_are_valid_rgba() {
        let module = WorkspaceMapModule::new();
        let outputs = module.render();
        for m in &outputs {
            for &ch in &m.color {
                assert!((0.0..=1.0).contains(&ch), "color channel out of range: {ch}");
            }
        }
    }

    #[test]
    fn default_produces_same_as_new() {
        let a = WorkspaceMapModule::new().render();
        let b = WorkspaceMapModule::default().render();
        assert_eq!(a.len(), b.len());
        for (ma, mb) in a.iter().zip(b.iter()) {
            assert_eq!(ma.text, mb.text);
        }
    }
}
