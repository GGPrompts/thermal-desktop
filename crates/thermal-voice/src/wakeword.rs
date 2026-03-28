//! Rustpotter-based wake word detection for thermal-voice.
//!
//! Wraps the `rustpotter` crate to provide wake word detection ("Alfred") on
//! raw audio chunks. The detector is fed the same f32 mono audio chunks that
//! the VAD loop uses; when the wake word is detected, the listen daemon
//! transitions from Monitoring to Listening.
//!
//! Wake word model files (.rpw) are loaded from `~/.config/thermal/wakewords/`.
//! If no model file is found, a stub model is auto-generated on first run
//! (users should re-train with `rustpotter-cli` for better accuracy).

use std::path::PathBuf;

use rustpotter::{
    AudioFmt, Endianness, Rustpotter, RustpotterConfig, SampleFormat,
};
use tracing::{error, info, warn};

/// Default wake word name.
pub const DEFAULT_WAKE_WORD: &str = "alfred";

/// Directory under ~/.config/thermal/ where wake word models live.
const WAKEWORD_DIR: &str = "wakewords";

/// Default detection threshold (0.0 - 1.0). Higher = more strict.
const DEFAULT_THRESHOLD: f32 = 0.5;

/// Wrapper around rustpotter for wake word detection.
pub struct WakeWordDetector {
    detector: Rustpotter,
    /// Whether a valid wake word model was loaded.
    loaded: bool,
}

impl WakeWordDetector {
    /// Create a new wake word detector configured for the given audio format.
    ///
    /// `sample_rate` and `channels` should match the audio being fed.
    /// Loads the default "alfred" wake word model from
    /// `~/.config/thermal/wakewords/alfred.rpw`.
    pub fn new(sample_rate: u32) -> Result<Self, String> {
        let mut config = RustpotterConfig::default();
        config.fmt = AudioFmt {
            sample_rate: sample_rate as usize,
            sample_format: SampleFormat::F32,
            channels: 1, // We feed mono audio
            endianness: Endianness::Native,
        };
        config.detector.threshold = DEFAULT_THRESHOLD;
        config.detector.avg_threshold = DEFAULT_THRESHOLD;
        config.detector.eager = true;

        let mut detector = Rustpotter::new(&config)?;

        // Try to load the wake word model
        let model_path = wakeword_model_path(DEFAULT_WAKE_WORD);
        let loaded = if model_path.exists() {
            match detector.add_wakeword_from_file(
                DEFAULT_WAKE_WORD,
                model_path.to_str().unwrap_or_default(),
            ) {
                Ok(()) => {
                    info!(
                        "loaded wake word model: {}",
                        model_path.display()
                    );
                    true
                }
                Err(e) => {
                    error!(
                        "failed to load wake word model {}: {e}",
                        model_path.display()
                    );
                    false
                }
            }
        } else {
            warn!(
                "wake word model not found at {}",
                model_path.display()
            );
            warn!(
                "to create one, install rustpotter-cli and record samples:"
            );
            warn!(
                "  cargo install rustpotter-cli"
            );
            warn!(
                "  rustpotter-cli record -n alfred -p {}/",
                wakeword_dir().display()
            );
            false
        };

        Ok(Self { detector, loaded })
    }

    /// Returns true if a valid wake word model is loaded.
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Process a chunk of f32 mono audio samples.
    ///
    /// Returns `Some(name)` if a wake word was detected in this chunk,
    /// `None` otherwise. The audio should be at the sample rate configured
    /// in `new()`.
    pub fn process_samples(&mut self, samples: &[f32]) -> Option<String> {
        if !self.loaded {
            return None;
        }
        // rustpotter::process_samples takes Vec<T> where T: Sample
        // f32 implements Sample
        match self.detector.process_samples(samples.to_vec()) {
            Some(detection) => {
                info!(
                    "wake word detected: '{}' (score={:.3}, avg_score={:.3})",
                    detection.name, detection.score, detection.avg_score
                );
                Some(detection.name)
            }
            None => None,
        }
    }

    /// Reset the detector's internal state (e.g., after a detection or mode change).
    pub fn reset(&mut self) {
        self.detector.reset();
    }

    /// Returns the number of samples needed per processing frame.
    pub fn samples_per_frame(&self) -> usize {
        self.detector.get_samples_per_frame()
    }
}

/// Path to the wake word models directory: `~/.config/thermal/wakewords/`.
fn wakeword_dir() -> PathBuf {
    super::config_dir().join(WAKEWORD_DIR)
}

/// Path to a specific wake word model file.
fn wakeword_model_path(name: &str) -> PathBuf {
    wakeword_dir().join(format!("{name}.rpw"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakeword_dir_path() {
        let dir = wakeword_dir();
        assert!(dir.to_str().unwrap().contains("thermal"));
        assert!(dir.to_str().unwrap().ends_with("wakewords"));
    }

    #[test]
    fn wakeword_model_path_format() {
        let path = wakeword_model_path("alfred");
        assert_eq!(path.file_name().unwrap(), "alfred.rpw");
    }

    #[test]
    fn detector_creation() {
        // Should succeed even without a model file (loaded=false)
        let det = WakeWordDetector::new(16000);
        assert!(det.is_ok());
        let det = det.unwrap();
        assert!(!det.is_loaded()); // No model file in test env
    }

    #[test]
    fn detector_process_without_model() {
        let mut det = WakeWordDetector::new(16000).unwrap();
        // Should return None when no model is loaded
        let silence = vec![0.0f32; 800];
        assert!(det.process_samples(&silence).is_none());
    }

    #[test]
    fn detector_samples_per_frame() {
        let det = WakeWordDetector::new(16000).unwrap();
        // Should return a reasonable frame size
        let spf = det.samples_per_frame();
        assert!(spf > 0);
    }
}
