/// Workspace map module for the bar's center zone.
///
/// Polls Hyprland via `hyprctl` to show occupied workspaces with window icons.
use std::process::Command;

use thermal_core::ThermalPalette;

use crate::bar::layout::{ModuleOutput, Zone};

/// Map a window class name to a Nerd Font icon.
fn class_to_icon(class: &str) -> &'static str {
    let lower = class.to_lowercase();
    match lower.as_str() {
        "kitty" | "alacritty" | "foot" | "wezterm" | "ghostty" => "\u{f120}",
        "firefox" | "firefox-esr" | "librewolf" | "zen" | "zen-browser" => "\u{f269}",
        "chromium" | "google-chrome" | "brave-browser" => "\u{f268}",
        "thunar" | "nautilus" | "dolphin" | "pcmanfm" | "nemo" => "\u{f07b}",
        "code" | "code-oss" | "vscodium" => "\u{e70c}",
        "discord" => "\u{f392}",
        "slack" => "\u{f198}",
        "spotify" => "\u{f1bc}",
        "steam" => "\u{f1b6}",
        "obs" | "obs-studio" => "\u{f03d}",
        "gimp" | "krita" | "inkscape" => "\u{f1fc}",
        "vlc" | "mpv" | "celluloid" => "\u{f144}",
        "telegram-desktop" | "telegramdesktop" => "\u{f2c6}",
        "signal" => "\u{f4ad}",
        "thunderbird" | "evolution" => "\u{f0e0}",
        "libreoffice" | "soffice" => "\u{f15c}",
        "zathura" | "evince" | "okular" => "\u{f1c1}",
        "pavucontrol" | "pwvucontrol" => "\u{f028}",
        "btop" | "htop" => "\u{f080}",
        "eog" | "loupe" | "feh" | "imv" => "\u{f03e}",
        _ => "\u{f2d0}",
    }
}

struct WorkspaceInfo {
    id: i64,
    icons: Vec<&'static str>,
}

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

fn get_workspaces() -> Vec<WorkspaceInfo> {
    let output = match Command::new("hyprctl").args(["clients", "-j"]).output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let clients: Vec<serde_json::Value> = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut map: std::collections::BTreeMap<i64, Vec<&'static str>> =
        std::collections::BTreeMap::new();

    for client in &clients {
        let ws_id = client
            .get("workspace")
            .and_then(|w| w.get("id"))
            .and_then(|id| id.as_i64())
            .unwrap_or(-1);

        if ws_id <= 0 {
            continue;
        }

        let class = client.get("class").and_then(|c| c.as_str()).unwrap_or("");

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

    pub fn render(&self) -> Vec<ModuleOutput> {
        let active_id = get_active_workspace_id().unwrap_or(-1);
        let workspaces = get_workspaces();

        if workspaces.is_empty() {
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
                    ThermalPalette::WARM
                } else {
                    ThermalPalette::ACCENT_HOT
                };

                let mut output = ModuleOutput::new(Zone::Center, text, color);

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
