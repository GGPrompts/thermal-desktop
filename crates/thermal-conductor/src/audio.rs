use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Audio notification manager for agent state changes
pub struct AudioManager {
    assets_dir: PathBuf,
    enabled: bool,
    sounds: HashMap<SoundEvent, PathBuf>,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum SoundEvent {
    AgentStarted,   // idle → running
    AgentComplete,  // running → complete (success chime)
    AgentError,     // → error state (alert tone)
    PaneCreated,    // New pane opened
    PaneDestroyed,  // Pane closed
    FocusChanged,   // Clicked different pane
}

impl AudioManager {
    pub fn new(assets_dir: impl Into<PathBuf>) -> Self {
        let assets_dir = assets_dir.into();
        let mut sounds = HashMap::new();

        // Map events to sound files (will be in assets/sounds/)
        sounds.insert(SoundEvent::AgentStarted, assets_dir.join("agent-start.ogg"));
        sounds.insert(SoundEvent::AgentComplete, assets_dir.join("agent-complete.ogg"));
        sounds.insert(SoundEvent::AgentError, assets_dir.join("agent-error.ogg"));
        sounds.insert(SoundEvent::PaneCreated, assets_dir.join("pane-create.ogg"));
        sounds.insert(SoundEvent::PaneDestroyed, assets_dir.join("pane-destroy.ogg"));
        sounds.insert(SoundEvent::FocusChanged, assets_dir.join("focus-change.ogg"));

        Self {
            assets_dir,
            enabled: true,
            sounds,
        }
    }

    /// Play a sound for an event (non-blocking)
    pub fn play(&self, event: SoundEvent) {
        if !self.enabled {
            return;
        }

        if let Some(path) = self.sounds.get(&event) {
            if path.exists() {
                // Spawn a thread to play audio so it doesn't block
                let path = path.clone();
                std::thread::spawn(move || {
                    if let Err(e) = play_sound(&path) {
                        tracing::warn!("Failed to play sound {:?}: {}", path, e);
                    }
                });
            } else {
                tracing::debug!("Sound file not found: {:?}", path);
            }
        }
    }

    /// Enable or disable audio
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn assets_dir(&self) -> &Path {
        &self.assets_dir
    }
}

/// Play a sound file using rodio
#[cfg(feature = "audio")]
fn play_sound(path: &Path) -> Result<(), AudioError> {
    use rodio::{Decoder, OutputStream, Sink};
    use std::fs::File;
    use std::io::BufReader;

    let (_stream, stream_handle) = OutputStream::try_default()
        .map_err(|e| AudioError::DeviceError(e.to_string()))?;

    let sink = Sink::try_new(&stream_handle)
        .map_err(|e| AudioError::PlayError(e.to_string()))?;

    let file = File::open(path).map_err(|e| AudioError::FileError(e.to_string()))?;
    let reader = BufReader::new(file);
    let source = Decoder::new(reader).map_err(|e| AudioError::DecodeError(e.to_string()))?;

    sink.append(source);
    // Blocks the thread only for the actual audio duration (no hardcoded sleep).
    // _stream must remain alive for the duration of playback.
    sink.sleep_until_end();

    Ok(())
}

#[cfg(not(feature = "audio"))]
fn play_sound(_path: &Path) -> Result<(), AudioError> {
    tracing::debug!("Audio disabled (compile without 'audio' feature)");
    Ok(())
}

#[derive(Debug)]
pub enum AudioError {
    DeviceError(String),
    FileError(String),
    DecodeError(String),
    PlayError(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeviceError(e) => write!(f, "audio device error: {e}"),
            Self::FileError(e) => write!(f, "file error: {e}"),
            Self::DecodeError(e) => write!(f, "decode error: {e}"),
            Self::PlayError(e) => write!(f, "playback error: {e}"),
        }
    }
}

impl std::error::Error for AudioError {}

// ── State-transition wiring ───────────────────────────────────────────────────

use crate::state_detector::DetectedState;

/// Tracks per-pane state history and fires audio cues on transitions.
///
/// Wrap an `AudioManager` in this helper and call
/// [`StateTransitionAudio::on_state_change`] after every poll cycle.
#[allow(dead_code)]
pub struct StateTransitionAudio {
    pub manager: AudioManager,
    prev_states: std::collections::HashMap<String, DetectedState>,
}

#[allow(dead_code)]
impl StateTransitionAudio {
    /// Create a new helper backed by an `AudioManager` pointing at
    /// `assets_dir` (typically `assets/sounds/`).
    pub fn new(assets_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            manager: AudioManager::new(assets_dir),
            prev_states: std::collections::HashMap::new(),
        }
    }

    /// Notify the helper of a new state for `pane_id`.
    ///
    /// Returns the `SoundEvent` that was played (if any). On the first call
    /// for a given pane the previous state is unknown so no sound is played.
    pub fn on_state_change(
        &mut self,
        pane_id: &str,
        new_state: DetectedState,
    ) -> Option<SoundEvent> {
        let event = if let Some(prev) = self.prev_states.get(pane_id) {
            match (prev, new_state) {
                // Any state → Running: agent just started working.
                (_, DetectedState::Running) => Some(SoundEvent::AgentStarted),

                // Running or Thinking → Complete: task finished.
                (DetectedState::Running | DetectedState::Thinking, DetectedState::Complete) => {
                    Some(SoundEvent::AgentComplete)
                }

                // Any → Error.
                (_, DetectedState::Error) => Some(SoundEvent::AgentError),

                // No interesting transition.
                _ => None,
            }
        } else {
            // First time we see this pane — record but don't play anything.
            None
        };

        self.prev_states.insert(pane_id.to_owned(), new_state);

        if let Some(ev) = event {
            self.manager.play(ev);
        }

        event
    }
}
