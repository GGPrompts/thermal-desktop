/// Voice/microphone status module for thermal-bar's right zone.
///
/// Reads `/tmp/thermal-voice-state.json` and displays a mic icon with
/// thermal-colored state:
///
/// - Cold/muted:      mic off, cool indigo
/// - Warm/listening:   mic active, green
/// - Hot/processing:   speech being processed, amber/orange
///
/// The file is polled once per render cycle (~1 Hz). Missing or
/// unreadable files are treated as "muted" (cold).
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use thermal_core::ThermalPalette;

use crate::layout::{ModuleOutput, Zone};

/// Path to the voice state file written by the voice input daemon.
const VOICE_STATE_PATH: &str = "/tmp/thermal-voice-state.json";

/// Unicode mic symbols.
const MIC_MUTED: &str = "\u{1F507}"; // speaker off (muted)
const MIC_MONITORING: &str = "\u{1F50E}"; // magnifying glass (monitoring/VAD)
const MIC_LISTENING: &str = "\u{1F3A4}"; // microphone
const MIC_PROCESSING: &str = "\u{1F525}"; // fire (processing)

// ---------------------------------------------------------------------------
// Voice state deserialization
// ---------------------------------------------------------------------------

/// Mic state as written by the voice input daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceState {
    #[default]
    Muted,
    /// Always-listening idle: VAD is active, waiting for speech.
    Monitoring,
    Listening,
    Processing,
}

/// JSON schema for `/tmp/thermal-voice-state.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct VoiceStateFile {
    state: VoiceState,
    /// Optional label (e.g. "whisper", "dictating").
    label: Option<String>,
}

impl Default for VoiceStateFile {
    fn default() -> Self {
        Self {
            state: VoiceState::Muted,
            label: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Cached voice state
// ---------------------------------------------------------------------------

struct VoiceCache {
    data: VoiceStateFile,
    last_read: Instant,
}

static VOICE_CACHE: Mutex<Option<VoiceCache>> = Mutex::new(None);

/// Re-read the state file if more than 500 ms have elapsed.
fn refresh_cache() -> VoiceStateFile {
    let mut guard = VOICE_CACHE.lock().unwrap();

    let needs_refresh = match guard.as_ref() {
        None => true,
        Some(c) => c.last_read.elapsed() > Duration::from_millis(500),
    };

    if needs_refresh {
        let data = read_voice_state();
        *guard = Some(VoiceCache {
            data: data.clone(),
            last_read: Instant::now(),
        });
        data
    } else {
        guard.as_ref().unwrap().data.clone()
    }
}

/// Read and parse the voice state JSON file. Returns default (muted) on
/// any error (missing file, bad JSON, permissions, etc.).
fn read_voice_state() -> VoiceStateFile {
    let path = Path::new(VOICE_STATE_PATH);
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => VoiceStateFile::default(),
    }
}

// ---------------------------------------------------------------------------
// VoiceModule
// ---------------------------------------------------------------------------

/// Renders a mic status indicator in the right zone.
pub struct VoiceModule;

impl VoiceModule {
    pub fn new() -> Self {
        Self
    }

    /// Produce right-zone module outputs for the current voice/mic state.
    pub fn render(&self) -> Vec<ModuleOutput> {
        let state = refresh_cache();

        let (icon, label, color) = match state.state {
            VoiceState::Muted => (MIC_MUTED, "muted", ThermalPalette::ACCENT_COLD),
            VoiceState::Monitoring => (MIC_MONITORING, "monitoring", ThermalPalette::COOL),
            VoiceState::Listening => (MIC_LISTENING, "listening", ThermalPalette::WARM),
            VoiceState::Processing => (MIC_PROCESSING, "processing", ThermalPalette::ACCENT_WARM),
        };

        // Use the daemon's label if provided, otherwise the default.
        let display_label = state.label.as_deref().unwrap_or(label);
        let text = format!("{icon} {display_label}");

        vec![ModuleOutput::new(Zone::Right, text, color)]
    }
}

impl Default for VoiceModule {
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

    // -----------------------------------------------------------------------
    // VoiceState deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn deserialize_voice_state_muted() {
        let json = r#"{"state": "muted"}"#;
        let f: VoiceStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.state, VoiceState::Muted);
        assert!(f.label.is_none());
    }

    #[test]
    fn deserialize_voice_state_listening() {
        let json = r#"{"state": "listening"}"#;
        let f: VoiceStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.state, VoiceState::Listening);
    }

    #[test]
    fn deserialize_voice_state_processing() {
        let json = r#"{"state": "processing"}"#;
        let f: VoiceStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.state, VoiceState::Processing);
    }

    #[test]
    fn deserialize_voice_state_with_label() {
        let json = r#"{"state": "listening", "label": "whisper"}"#;
        let f: VoiceStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.state, VoiceState::Listening);
        assert_eq!(f.label.as_deref(), Some("whisper"));
    }

    #[test]
    fn deserialize_voice_state_default_when_empty_json() {
        let json = r#"{}"#;
        let f: VoiceStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.state, VoiceState::Muted);
        assert!(f.label.is_none());
    }

    #[test]
    fn deserialize_voice_state_invalid_json_falls_back_to_default() {
        let result: Result<VoiceStateFile, _> = serde_json::from_str("not json at all");
        // Should fail to parse; callers use unwrap_or_default().
        let f = result.unwrap_or_default();
        assert_eq!(f.state, VoiceState::Muted);
    }

    #[test]
    fn voice_state_default_is_muted() {
        assert_eq!(VoiceState::default(), VoiceState::Muted);
    }

    #[test]
    fn voice_state_file_default_is_muted_no_label() {
        let f = VoiceStateFile::default();
        assert_eq!(f.state, VoiceState::Muted);
        assert!(f.label.is_none());
    }

    // -----------------------------------------------------------------------
    // Module output shape
    // -----------------------------------------------------------------------

    /// Build a ModuleOutput from a VoiceStateFile directly, mirroring the
    /// render() logic, so we can test it without touching the global cache.
    fn render_from_state(state_file: VoiceStateFile) -> ModuleOutput {
        let (icon, label, color) = match state_file.state {
            VoiceState::Muted => (MIC_MUTED, "muted", ThermalPalette::ACCENT_COLD),
            VoiceState::Monitoring => (MIC_MONITORING, "monitoring", ThermalPalette::COOL),
            VoiceState::Listening => (MIC_LISTENING, "listening", ThermalPalette::WARM),
            VoiceState::Processing => (MIC_PROCESSING, "processing", ThermalPalette::ACCENT_WARM),
        };
        let display_label = state_file.label.as_deref().unwrap_or(label);
        let text = format!("{icon} {display_label}");
        ModuleOutput::new(crate::layout::Zone::Right, text, color)
    }

    #[test]
    fn muted_output_contains_muted_label() {
        let f = VoiceStateFile {
            state: VoiceState::Muted,
            label: None,
        };
        let m = render_from_state(f);
        assert!(
            m.text.contains("muted"),
            "text='{}' should contain 'muted'",
            m.text
        );
    }

    #[test]
    fn listening_output_contains_listening_label() {
        let f = VoiceStateFile {
            state: VoiceState::Listening,
            label: None,
        };
        let m = render_from_state(f);
        assert!(m.text.contains("listening"), "text='{}'", m.text);
    }

    #[test]
    fn processing_output_contains_processing_label() {
        let f = VoiceStateFile {
            state: VoiceState::Processing,
            label: None,
        };
        let m = render_from_state(f);
        assert!(m.text.contains("processing"), "text='{}'", m.text);
    }

    #[test]
    fn custom_label_overrides_default_label() {
        let f = VoiceStateFile {
            state: VoiceState::Listening,
            label: Some("dictating".to_owned()),
        };
        let m = render_from_state(f);
        assert!(
            m.text.contains("dictating"),
            "text='{}' should use custom label",
            m.text
        );
        assert!(
            !m.text.contains("listening"),
            "default label should be replaced"
        );
    }

    #[test]
    fn output_zone_is_right() {
        let f = VoiceStateFile::default();
        let m = render_from_state(f);
        assert_eq!(m.zone, crate::layout::Zone::Right);
    }

    #[test]
    fn deserialize_voice_state_monitoring() {
        let json = r#"{"state": "monitoring"}"#;
        let f: VoiceStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.state, VoiceState::Monitoring);
    }

    #[test]
    fn monitoring_output_contains_monitoring_label() {
        let f = VoiceStateFile {
            state: VoiceState::Monitoring,
            label: None,
        };
        let m = render_from_state(f);
        assert!(m.text.contains("monitoring"), "text='{}'", m.text);
    }

    #[test]
    fn output_color_is_valid_rgba() {
        for state in [
            VoiceState::Muted,
            VoiceState::Monitoring,
            VoiceState::Listening,
            VoiceState::Processing,
        ] {
            let f = VoiceStateFile { state, label: None };
            let m = render_from_state(f);
            for &ch in &m.color {
                assert!((0.0..=1.0).contains(&ch), "color channel out of range: {ch}");
            }
        }
    }

    #[test]
    fn muted_and_listening_have_different_colors() {
        let muted_m = render_from_state(VoiceStateFile {
            state: VoiceState::Muted,
            label: None,
        });
        let listening_m = render_from_state(VoiceStateFile {
            state: VoiceState::Listening,
            label: None,
        });
        assert_ne!(
            muted_m.color, listening_m.color,
            "muted and listening should have distinct colors"
        );
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn mic_constants_are_non_empty() {
        assert!(!MIC_MUTED.is_empty());
        assert!(!MIC_MONITORING.is_empty());
        assert!(!MIC_LISTENING.is_empty());
        assert!(!MIC_PROCESSING.is_empty());
    }

    #[test]
    fn voice_state_path_constant_is_set() {
        assert!(!VOICE_STATE_PATH.is_empty());
        assert!(VOICE_STATE_PATH.starts_with('/'));
    }
}
