/// Voice/microphone status module for the bar's right zone.
///
/// Reads `/tmp/thermal-voice-state.json` (written by the unified thermal-audio
/// daemon) and displays a mic icon with thermal-colored state. Polled once per
/// render cycle (~1 Hz).
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use thermal_core::ThermalPalette;

use crate::bar::layout::{ModuleOutput, Zone};

const VOICE_STATE_PATH: &str = "/tmp/thermal-voice-state.json";

const MIC_MUTED: &str = "\u{1F507}";
const MIC_MONITORING: &str = "\u{1F50E}";
const MIC_LISTENING: &str = "\u{1F3A4}";
const MIC_PROCESSING: &str = "\u{1F525}";

// ---------------------------------------------------------------------------
// Voice state deserialization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceState {
    #[default]
    Muted,
    #[serde(alias = "wake_word")]
    Monitoring,
    Listening,
    Processing,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct VoiceStateFile {
    state: VoiceState,
    label: Option<String>,
    level: Option<f32>,
}

impl Default for VoiceStateFile {
    fn default() -> Self {
        Self {
            state: VoiceState::Muted,
            label: None,
            level: None,
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

fn read_voice_state() -> VoiceStateFile {
    let path = Path::new(VOICE_STATE_PATH);
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => VoiceStateFile::default(),
    }
}

// ---------------------------------------------------------------------------
// Level meter
// ---------------------------------------------------------------------------

const METER_BLOCKS: [char; 8] = ['\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];
const METER_WIDTH: usize = 5;

fn level_meter(rms: f32) -> String {
    if rms <= 0.0 {
        return METER_BLOCKS[0].to_string().repeat(METER_WIDTH);
    }
    let log_level = ((rms.max(0.001).log10() + 3.0) / 3.0).clamp(0.0, 1.0);
    let idx = ((log_level * (METER_BLOCKS.len() - 1) as f32).round() as usize)
        .min(METER_BLOCKS.len() - 1);
    METER_BLOCKS[idx].to_string().repeat(METER_WIDTH)
}

// ---------------------------------------------------------------------------
// VoiceModule
// ---------------------------------------------------------------------------

pub struct VoiceModule;

impl VoiceModule {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self) -> Vec<ModuleOutput> {
        let state = refresh_cache();

        let (icon, label, color) = match state.state {
            VoiceState::Muted => (MIC_MUTED, "muted", ThermalPalette::ACCENT_COLD),
            VoiceState::Monitoring => (MIC_MONITORING, "monitoring", ThermalPalette::WARM),
            VoiceState::Listening => (MIC_LISTENING, "recording", ThermalPalette::ACCENT_WARM),
            VoiceState::Processing => (MIC_PROCESSING, "processing", ThermalPalette::HOT),
        };

        let display_label = state.label.as_deref().unwrap_or(label);
        let meter = level_meter(state.level.unwrap_or(0.0));
        let text = format!("{icon} {meter} {display_label}");

        vec![ModuleOutput::new(Zone::Right, text, color)]
    }
}

impl Default for VoiceModule {
    fn default() -> Self {
        Self::new()
    }
}
