//! Voice assistant state management for thermal-hud.
//!
//! Watches `/tmp/thermal-voice-state.json` for voice assistant state changes
//! using the `notify` crate (same pattern as `ClaudeStatePoller` in thermal-core).
//! When the voice assistant is active, the HUD switches from agent tabs to
//! voice UI mode.

use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

/// Path to the voice assistant state file.
const VOICE_STATE_FILE: &str = "/tmp/thermal-voice-state.json";

/// Which visual mode the HUD is currently displaying.
#[derive(Debug, Clone)]
pub enum HudMode {
    /// Default: show Claude session tabs.
    AgentTabs,
    /// Voice assistant is active — show transcript / action / result.
    VoiceActive {
        transcript: String,
        state: VoiceState,
    },
}

/// Sub-states of the voice assistant UI.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceState {
    /// Microphone is listening for input.
    Listening,
    /// Audio captured, transcription in progress.
    Transcribing,
    /// Transcript sent to AI, waiting for response.
    Thinking,
    /// AI proposes an action — user must confirm (say yes/no).
    Confirming { action: String },
    /// Confirmed action is being executed.
    Executing,
    /// Action complete — show summary, auto-dim after timeout.
    Result { summary: String },
}

/// Raw JSON shape written by the voice assistant process.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct VoiceStateFile {
    listening: bool,
    last_transcript: String,
    action_pending: Option<String>,
    result: Option<String>,
}

/// Watches `/tmp/thermal-voice-state.json` for changes and derives the
/// current [`HudMode`].
pub struct VoiceStatePoller {
    _watcher: RecommendedWatcher,
    rx: mpsc::Receiver<NotifyResult<Event>>,
    state_file: PathBuf,
    /// Cached raw state from the last successful read.
    current: Option<VoiceStateFile>,
    /// When the result was first displayed (for auto-dim).
    pub result_shown_at: Option<Instant>,
}

impl VoiceStatePoller {
    /// Create a new poller watching the voice state file.
    /// Watches the parent directory since the file may not exist yet.
    pub fn new() -> NotifyResult<Self> {
        let state_file = PathBuf::from(VOICE_STATE_FILE);
        let watch_dir = state_file
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"))
            .to_path_buf();

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        // Watch the parent directory non-recursively — we filter for our file.
        watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;

        // Attempt initial read.
        let current = Self::read_file(&state_file);

        Ok(Self {
            _watcher: watcher,
            rx,
            state_file,
            current,
            result_shown_at: None,
        })
    }

    /// Drain pending file-change events, re-read if our file changed, and
    /// return the current [`HudMode`].
    pub fn poll(&mut self) -> HudMode {
        let mut dirty = false;
        let mut removed = false;

        while let Ok(result) = self.rx.try_recv() {
            if let Ok(event) = result {
                let dominated = event.paths.iter().any(|p| {
                    p == &self.state_file
                        || p.file_name()
                            .is_some_and(|n| n == "thermal-voice-state.json")
                });
                if !dominated {
                    continue;
                }
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        dirty = true;
                    }
                    EventKind::Remove(_) => {
                        removed = true;
                    }
                    _ => {}
                }
            }
        }

        if removed {
            self.current = None;
            self.result_shown_at = None;
        }

        if dirty {
            self.current = Self::read_file(&self.state_file);
        }

        self.derive_mode()
    }

    /// Derive the HUD mode from the current raw state.
    fn derive_mode(&mut self) -> HudMode {
        let state = match &self.current {
            Some(s) => s,
            None => return HudMode::AgentTabs,
        };

        // If there is a result, show it.
        if let Some(summary) = &state.result
            && !summary.is_empty()
        {
            if self.result_shown_at.is_none() {
                self.result_shown_at = Some(Instant::now());
            }
            return HudMode::VoiceActive {
                transcript: state.last_transcript.clone(),
                state: VoiceState::Result {
                    summary: summary.clone(),
                },
            };
        }

        // If there is a pending action, show confirmation.
        if let Some(action) = &state.action_pending
            && !action.is_empty()
        {
            self.result_shown_at = None;
            return HudMode::VoiceActive {
                transcript: state.last_transcript.clone(),
                state: VoiceState::Confirming {
                    action: action.clone(),
                },
            };
        }

        // If we have a transcript but no action/result, we are transcribing/thinking.
        if !state.last_transcript.is_empty() {
            self.result_shown_at = None;
            return HudMode::VoiceActive {
                transcript: state.last_transcript.clone(),
                state: VoiceState::Thinking,
            };
        }

        // If actively listening with no transcript yet.
        if state.listening {
            self.result_shown_at = None;
            return HudMode::VoiceActive {
                transcript: String::new(),
                state: VoiceState::Listening,
            };
        }

        // Voice state file exists but nothing active — fall back to tabs.
        self.result_shown_at = None;
        HudMode::AgentTabs
    }

    /// Parse the voice state JSON file.
    fn read_file(path: &PathBuf) -> Option<VoiceStateFile> {
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }
}

/// Duration after which a result display should be dimmed.
pub const RESULT_DIM_SECS: u64 = 5;
