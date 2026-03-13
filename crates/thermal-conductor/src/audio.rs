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
    use rodio::{Decoder, OutputStream, Source};
    use std::fs::File;
    use std::io::BufReader;

    let (_stream, stream_handle) = OutputStream::try_default()
        .map_err(|e| AudioError::DeviceError(e.to_string()))?;

    let file = File::open(path).map_err(|e| AudioError::FileError(e.to_string()))?;
    let reader = BufReader::new(file);

    let source = Decoder::new(reader).map_err(|e| AudioError::DecodeError(e.to_string()))?;

    stream_handle
        .play_raw(source.convert_samples())
        .map_err(|e| AudioError::PlayError(e.to_string()))?;

    // Wait for playback (simple approach — rodio drops the stream when done)
    std::thread::sleep(std::time::Duration::from_secs(2));

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
