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
const MIC_MUTED: &str = "\u{1F507}";    // speaker off (muted)
const MIC_LISTENING: &str = "\u{1F3A4}"; // microphone
const MIC_PROCESSING: &str = "\u{1F525}"; // fire (processing)

// ---------------------------------------------------------------------------
// Voice state deserialization
// ---------------------------------------------------------------------------

/// Mic state as written by the voice input daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceState {
    Muted,
    Listening,
    Processing,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self::Muted
    }
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
            VoiceState::Muted => (
                MIC_MUTED,
                "muted",
                ThermalPalette::ACCENT_COLD,
            ),
            VoiceState::Listening => (
                MIC_LISTENING,
                "listening",
                ThermalPalette::WARM,
            ),
            VoiceState::Processing => (
                MIC_PROCESSING,
                "processing",
                ThermalPalette::ACCENT_WARM,
            ),
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
