//! Settings display and editing for the Services tab.
//!
//! Reads `~/.config/thermal/settings.toml` (creating a default template if absent),
//! parses per-component sections, and provides summary strings for inline display.
//! The 'e' hotkey opens the config file in `$EDITOR` (defaulting to `micro`).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

/// Per-component configuration summary extracted from settings.toml.
#[derive(Debug, Clone, Default)]
pub struct ServiceSettings {
    /// Map from section name (e.g. "audio", "voice") to key-value pairs.
    sections: HashMap<String, Vec<(String, String)>>,
}

impl ServiceSettings {
    /// Return a reference to the raw section data for direct key lookups.
    pub fn sections_raw(&self) -> &HashMap<String, Vec<(String, String)>> {
        &self.sections
    }

    /// Return a one-line summary for a service binary name, or `None` if no
    /// section matches.
    pub fn summary_for(&self, binary: &str) -> Option<String> {
        let section = binary_to_section(binary)?;
        let pairs = self.sections.get(section)?;
        if pairs.is_empty() {
            return None;
        }
        let parts: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
        Some(parts.join(", "))
    }
}

/// Map a service binary name to its settings.toml section name.
fn binary_to_section(binary: &str) -> Option<&'static str> {
    match binary {
        "thermal-audio" => Some("audio"),
        "thermal-voice" => Some("voice"),
        "thermal-dispatcher" => Some("dispatcher"),
        "thermal-bar" => Some("bar"),
        "thermal-conductor" | "thermal-conductor-tui" => Some("conductor"),
        // thermal-hud is now built into thermal-conductor.
        "thermal-hud" => Some("hud"),
        "thermal-notify" => Some("notify"),
        _ => None,
    }
}

/// Return the path to the unified settings file.
pub fn settings_path() -> PathBuf {
    let config_dir = dirs_fallback();
    config_dir.join("settings.toml")
}

/// XDG config dir for thermal, with fallback.
fn dirs_fallback() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("thermal")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("thermal")
    } else {
        PathBuf::from("/tmp/thermal-config")
    }
}

/// Default template written when no settings.toml exists.
const DEFAULT_SETTINGS: &str = r#"# Thermal Desktop — unified settings
# Edit this file to configure thermal components.
# Changes take effect on daemon restart (or immediately for daemons
# that support file-watching).

[audio]
# voice = "en-US-GuyNeural"    # default TTS voice
# speed = 1.0                  # TTS speech rate
# volume = 1.0                 # master volume (0.0 – 1.0)

[voice]
# mode = "vad"                 # "vad" (always-listen) or "ptt" (push-to-talk)
# sensitivity = 0.6            # VAD energy threshold (0.0 – 1.0)
# stt_model = "base.en"        # whisper model name

[dispatcher]
# backend = "ollama"           # "ollama", "claude", or "copilot"
# model = "qwen3:8b"           # model name for the selected backend

[bar]
# position = "top"             # "top" or "bottom"
# update_interval_ms = 1000    # status polling interval
# modules = "cpu,gpu,mem,net,workspaces,agents,voice"

[conductor]
# backend = "auto"             # "auto", "kitty", or "daemon"
# preview_refresh_ms = 500     # live terminal preview interval

[hud]
# enabled = true               # show the layer-shell HUD overlay

[notify]
# timeout_ms = 5000            # default notification display time

[messages]
# persist = false              # write JSONL log to disk
# ring_size = 500              # in-memory ring buffer capacity
"#;

/// Ensure settings.toml exists (create with defaults if missing).
/// Returns the path.
pub fn ensure_settings_file() -> PathBuf {
    let path = settings_path();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, DEFAULT_SETTINGS);
    }
    path
}

/// Parse settings.toml and return per-component summaries.
pub fn load_settings() -> ServiceSettings {
    let path = ensure_settings_file();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return ServiceSettings::default(),
    };

    let table: toml::Table = match content.parse() {
        Ok(t) => t,
        Err(_) => return ServiceSettings::default(),
    };

    let mut sections = HashMap::new();
    for (section_name, value) in &table {
        if let toml::Value::Table(inner) = value {
            let pairs: Vec<(String, String)> = inner
                .iter()
                .map(|(k, v)| {
                    let display = match v {
                        toml::Value::String(s) => s.clone(),
                        toml::Value::Integer(i) => i.to_string(),
                        toml::Value::Float(f) => format!("{f:.2}"),
                        toml::Value::Boolean(b) => b.to_string(),
                        other => other.to_string(),
                    };
                    (k.clone(), display)
                })
                .collect();
            sections.insert(section_name.clone(), pairs);
        }
    }

    ServiceSettings { sections }
}

/// Open settings.toml in the user's editor. This temporarily leaves the
/// alternate screen so the editor can render normally, then returns to TUI.
///
/// Returns `Ok(true)` if the editor ran successfully, `Err` on failure.
pub fn open_in_editor() -> Result<bool, String> {
    struct EditorSuspendGuard {
        suspended: bool,
    }

    impl EditorSuspendGuard {
        fn suspend() -> Result<Self, String> {
            crossterm::execute!(
                io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::event::DisableMouseCapture
            )
            .map_err(|e| format!("failed to leave alternate screen: {e}"))?;

            if let Err(e) = crossterm::terminal::disable_raw_mode() {
                let _ = crossterm::execute!(
                    io::stdout(),
                    crossterm::terminal::EnterAlternateScreen,
                    crossterm::event::EnableMouseCapture,
                    crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
                );
                return Err(format!("failed to disable raw mode: {e}"));
            }

            Ok(Self { suspended: true })
        }

        fn resume(&mut self) -> Result<(), String> {
            if !self.suspended {
                return Ok(());
            }

            crossterm::terminal::enable_raw_mode()
                .map_err(|e| format!("failed to re-enable raw mode: {e}"))?;
            crossterm::execute!(
                io::stdout(),
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableMouseCapture,
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
            )
            .map_err(|e| format!("failed to re-enter alternate screen: {e}"))?;

            self.suspended = false;
            Ok(())
        }
    }

    impl Drop for EditorSuspendGuard {
        fn drop(&mut self) {
            if !self.suspended {
                return;
            }

            let _ = crossterm::terminal::enable_raw_mode();
            let _ = crossterm::execute!(
                io::stdout(),
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableMouseCapture,
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
            );
        }
    }

    let path = ensure_settings_file();
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "micro".to_string());
    let mut suspended = EditorSuspendGuard::suspend()?;

    let status = std::process::Command::new(&editor)
        .arg(path.to_str().unwrap_or(""))
        .status()
        .map_err(|e| format!("failed to launch {editor}: {e}"))?;
    suspended.resume()?;

    Ok(status.success())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_to_section_maps_known_services() {
        assert_eq!(binary_to_section("thermal-audio"), Some("audio"));
        assert_eq!(binary_to_section("thermal-voice"), Some("voice"));
        assert_eq!(binary_to_section("thermal-dispatcher"), Some("dispatcher"));
        assert_eq!(binary_to_section("thermal-bar"), Some("bar"));
        assert_eq!(binary_to_section("thermal-conductor"), Some("conductor"));
        assert_eq!(binary_to_section("thermal-hud"), Some("hud"));
        assert_eq!(binary_to_section("thermal-notify"), Some("notify"));
    }

    #[test]
    fn binary_to_section_returns_none_for_unknown() {
        assert_eq!(binary_to_section("codex-state-adapter"), None);
        assert_eq!(binary_to_section("thermal-lock"), None);
        assert_eq!(binary_to_section("random-thing"), None);
    }

    #[test]
    fn settings_path_is_under_config() {
        let path = settings_path();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("thermal") && path_str.ends_with("settings.toml"),
            "unexpected settings path: {path_str}"
        );
    }

    #[test]
    fn default_settings_parses_as_valid_toml() {
        let result: Result<toml::Table, _> = DEFAULT_SETTINGS.parse();
        assert!(
            result.is_ok(),
            "DEFAULT_SETTINGS is not valid TOML: {result:?}"
        );
    }

    #[test]
    fn empty_settings_returns_no_summaries() {
        let settings = ServiceSettings::default();
        assert!(settings.summary_for("thermal-audio").is_none());
    }

    #[test]
    fn summary_for_formats_key_value_pairs() {
        let mut sections = HashMap::new();
        sections.insert(
            "audio".to_string(),
            vec![
                ("volume".to_string(), "0.80".to_string()),
                ("voice".to_string(), "en-US-GuyNeural".to_string()),
            ],
        );
        let settings = ServiceSettings { sections };
        let summary = settings.summary_for("thermal-audio").unwrap();
        assert!(summary.contains("volume=0.80"));
        assert!(summary.contains("voice=en-US-GuyNeural"));
    }
}
