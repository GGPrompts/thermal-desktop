//! Energy-based Voice Activity Detection (VAD).
//!
//! Computes RMS energy on audio chunks and uses hysteresis to distinguish
//! between speech and silence, avoiding false triggers from transient noise.
//!
//! // TODO: Replace with silero-vad-rust for better accuracy

/// Events emitted by the VAD detector as audio is processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    /// No speech detected — audio is below the energy threshold.
    Silence,
    /// Speech has just started (after enough consecutive speech frames).
    SpeechStart,
    /// Speech is continuing (already in speech state, energy still above threshold).
    SpeechContinue,
    /// Speech has ended (enough consecutive silence frames after speech).
    SpeechEnd,
}

/// Energy-based Voice Activity Detector with hysteresis.
///
/// Uses RMS (root mean square) energy of audio chunks to detect speech.
/// Requires multiple consecutive frames above/below threshold to trigger
/// state transitions, preventing false triggers from transient noise.
pub struct VadDetector {
    /// RMS energy threshold for speech detection (normalized 0.0–1.0 for f32 audio).
    threshold: f32,
    /// Number of consecutive speech frames required to trigger SpeechStart.
    speech_frames_required: u32,
    /// Number of consecutive silence frames required to trigger SpeechEnd.
    silence_frames_required: u32,
    /// Counter for consecutive frames above threshold.
    speech_count: u32,
    /// Counter for consecutive frames below threshold.
    silence_count: u32,
    /// Whether we are currently in a speech segment.
    in_speech: bool,
}

impl VadDetector {
    /// Create a new VAD detector with the given energy threshold.
    ///
    /// The threshold is compared against RMS energy of f32 audio samples
    /// (normalized to -1.0..1.0 range). A value of 0.015 works well for
    /// typical desktop microphones in a quiet room.
    ///
    /// Hysteresis defaults:
    /// - 3 consecutive speech frames (~150ms at 50ms chunks) to start
    /// - 15 consecutive silence frames (~750ms at 50ms chunks) to stop
    pub fn new(threshold: f32) -> Self {
        Self {
            threshold,
            speech_frames_required: 3,
            silence_frames_required: 15,
            speech_count: 0,
            silence_count: 0,
            in_speech: false,
        }
    }

    /// Process an audio chunk and return the resulting VAD event.
    ///
    /// `samples` should be f32 audio normalized to -1.0..1.0. The chunk
    /// size determines the time resolution of detection (e.g., 800 samples
    /// at 16kHz = 50ms per chunk).
    pub fn process_chunk(&mut self, samples: &[f32]) -> VadEvent {
        let energy = rms_energy(samples);
        let is_speech = energy >= self.threshold;

        if is_speech {
            self.silence_count = 0;
            self.speech_count = self.speech_count.saturating_add(1);

            if self.in_speech {
                VadEvent::SpeechContinue
            } else if self.speech_count >= self.speech_frames_required {
                self.in_speech = true;
                VadEvent::SpeechStart
            } else {
                // Building up speech frames, not yet triggered
                VadEvent::Silence
            }
        } else {
            self.speech_count = 0;
            self.silence_count = self.silence_count.saturating_add(1);

            if self.in_speech {
                if self.silence_count >= self.silence_frames_required {
                    self.in_speech = false;
                    VadEvent::SpeechEnd
                } else {
                    // Still in speech, tolerating a short silence gap
                    VadEvent::SpeechContinue
                }
            } else {
                VadEvent::Silence
            }
        }
    }

    /// Reset the detector state, clearing all counters.
    pub fn reset(&mut self) {
        self.speech_count = 0;
        self.silence_count = 0;
        self.in_speech = false;
    }

    /// Returns true if the detector is currently in a speech segment.
    pub fn is_in_speech(&self) -> bool {
        self.in_speech
    }
}

/// Compute the Root Mean Square energy of an audio buffer.
pub fn rms_energy(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero() {
        let silence = vec![0.0f32; 800];
        assert_eq!(rms_energy(&silence), 0.0);
    }

    #[test]
    fn rms_of_empty_is_zero() {
        assert_eq!(rms_energy(&[]), 0.0);
    }

    #[test]
    fn rms_of_constant_signal() {
        // RMS of a constant 0.5 signal should be 0.5
        let signal = vec![0.5f32; 100];
        let rms = rms_energy(&signal);
        assert!((rms - 0.5).abs() < 0.001);
    }

    #[test]
    fn silence_stays_silent() {
        let mut vad = VadDetector::new(0.015);
        let silence = vec![0.0f32; 800];
        for _ in 0..20 {
            assert_eq!(vad.process_chunk(&silence), VadEvent::Silence);
        }
        assert!(!vad.is_in_speech());
    }

    #[test]
    fn speech_requires_hysteresis() {
        let mut vad = VadDetector::new(0.015);
        let speech = vec![0.1f32; 800]; // Well above threshold

        // First two frames: not enough to trigger yet
        assert_eq!(vad.process_chunk(&speech), VadEvent::Silence);
        assert_eq!(vad.process_chunk(&speech), VadEvent::Silence);
        assert!(!vad.is_in_speech());

        // Third frame: triggers SpeechStart
        assert_eq!(vad.process_chunk(&speech), VadEvent::SpeechStart);
        assert!(vad.is_in_speech());

        // Subsequent frames: SpeechContinue
        assert_eq!(vad.process_chunk(&speech), VadEvent::SpeechContinue);
    }

    #[test]
    fn silence_after_speech_requires_hysteresis() {
        let mut vad = VadDetector::new(0.015);
        let speech = vec![0.1f32; 800];
        let silence = vec![0.0f32; 800];

        // Enter speech state
        for _ in 0..3 {
            vad.process_chunk(&speech);
        }
        assert!(vad.is_in_speech());

        // 14 silence frames should not end speech (need 15)
        for _ in 0..14 {
            let event = vad.process_chunk(&silence);
            assert_eq!(event, VadEvent::SpeechContinue);
        }
        assert!(vad.is_in_speech());

        // 15th frame triggers SpeechEnd
        assert_eq!(vad.process_chunk(&silence), VadEvent::SpeechEnd);
        assert!(!vad.is_in_speech());
    }

    #[test]
    fn brief_silence_during_speech_does_not_end() {
        let mut vad = VadDetector::new(0.015);
        let speech = vec![0.1f32; 800];
        let silence = vec![0.0f32; 800];

        // Enter speech
        for _ in 0..3 {
            vad.process_chunk(&speech);
        }
        assert!(vad.is_in_speech());

        // Short silence gap (5 frames = ~250ms) then speech resumes
        for _ in 0..5 {
            vad.process_chunk(&silence);
        }
        assert!(vad.is_in_speech()); // Still in speech

        // Speech resumes — silence counter resets
        assert_eq!(vad.process_chunk(&speech), VadEvent::SpeechContinue);
    }

    #[test]
    fn reset_clears_state() {
        let mut vad = VadDetector::new(0.015);
        let speech = vec![0.1f32; 800];

        // Enter speech
        for _ in 0..3 {
            vad.process_chunk(&speech);
        }
        assert!(vad.is_in_speech());

        vad.reset();
        assert!(!vad.is_in_speech());

        // After reset, need full hysteresis again
        assert_eq!(vad.process_chunk(&speech), VadEvent::Silence);
    }

    #[test]
    fn threshold_boundary() {
        let mut vad = VadDetector::new(0.1);
        // Signal exactly at threshold
        let at_threshold = vec![0.1f32; 800];
        // Signal just below
        let below = vec![0.099f32; 800];

        // At threshold should count as speech
        for _ in 0..3 {
            vad.process_chunk(&at_threshold);
        }
        assert!(vad.is_in_speech());

        vad.reset();

        // Below threshold should not trigger
        for _ in 0..10 {
            assert_eq!(vad.process_chunk(&below), VadEvent::Silence);
        }
        assert!(!vad.is_in_speech());
    }
}
