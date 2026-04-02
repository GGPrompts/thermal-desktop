//! Neural Voice Activity Detection (VAD) using Silero VAD v5.
//!
//! Runs the Silero ONNX model directly via `ort` for high-accuracy speech/silence
//! classification.  The bundled model (~2.3 MB) is embedded at compile time so no
//! external files are needed at runtime.
//!
//! The `VadEvent` interface is preserved for callers in `main.rs`.

use ndarray::{Array1, Array2, ArrayD, IxDyn, s};
use ort::{execution_providers::CPUExecutionProvider, session::Session, value::Tensor};
use tracing::{debug, error};

/// The bundled Silero VAD v5 ONNX model (opset 16, supports 8 kHz and 16 kHz).
static SILERO_VAD_ONNX: &[u8] = include_bytes!("../data/silero_vad.onnx");

/// Window size the model expects at 16 kHz.
const SILERO_WINDOW: usize = 512;
/// Context size at 16 kHz (prepended to each window).
const SILERO_CONTEXT: usize = 64;
/// Internal sample rate for Silero inference.
const SILERO_RATE: u32 = 16_000;

/// Events emitted by the VAD detector as audio is processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    /// No speech detected — model probability is below the threshold.
    Silence,
    /// Speech has just started (after enough consecutive speech frames).
    SpeechStart,
    /// Speech is continuing (already in speech state, probability still above threshold).
    SpeechContinue,
    /// Speech has ended (enough consecutive silence frames after speech).
    SpeechEnd,
}

/// Silero VAD-based speech detector.
///
/// Feeds audio through the Silero ONNX model in streaming mode.  Internally
/// resamples from `native_rate` to 16 kHz and buffers samples into the
/// 512-sample windows the model expects.
pub struct VadDetector {
    /// ONNX Runtime session holding the loaded model.
    session: Session,
    /// Recurrent hidden state (shape [2, 1, 128]).
    state: ArrayD<f32>,
    /// Rolling context from the tail of the previous window (64 samples).
    context: Array2<f32>,
    /// Speech probability threshold (0.0–1.0).
    threshold: f32,
    /// Whether we are currently in a speech segment.
    in_speech: bool,
    /// Native sample rate of the audio input (e.g. 48 000).
    native_rate: u32,
    /// Accumulation buffer for resampled 16 kHz samples, fed to Silero in
    /// 512-sample windows.
    resample_buf: Vec<f32>,
    /// Number of consecutive silence frames required to end speech.
    /// At 512 samples / 16 kHz ≈ 32 ms per frame, 19 frames ≈ 600 ms.
    silence_frames_required: u32,
    /// Counter for consecutive frames with probability below threshold.
    silence_count: u32,
}

impl VadDetector {
    /// Create a new Silero VAD detector.
    ///
    /// `threshold` is the speech probability threshold (0.0–1.0).  A value of
    /// 0.5 is a reasonable default; raise to 0.6–0.7 to reduce false positives.
    ///
    /// `native_rate` is the sample rate of audio passed to `process_chunk()`
    /// (e.g. 48 000).  Samples are internally resampled to 16 kHz.
    ///
    /// Returns `None` if the ONNX model fails to load.
    pub fn new(threshold: f32, native_rate: u32) -> Option<Self> {
        let session = match (|| -> Result<Session, ort::Error> {
            let mut builder = Session::builder()?
                .with_intra_threads(1)?
                .with_inter_threads(1)?
                .with_execution_providers([CPUExecutionProvider::default().build()])?;
            builder.commit_from_memory(SILERO_VAD_ONNX)
        })() {
            Ok(s) => s,
            Err(e) => {
                error!("failed to load Silero VAD ONNX model: {e}");
                return None;
            }
        };

        Some(Self {
            session,
            state: ArrayD::<f32>::zeros(IxDyn(&[2, 1, 128])),
            context: Array2::<f32>::zeros((1, SILERO_CONTEXT)),
            threshold,
            in_speech: false,
            native_rate,
            resample_buf: Vec::with_capacity(SILERO_WINDOW * 2),
            // ~600 ms of silence to end speech (19 × 32 ms frames)
            silence_frames_required: 19,
            silence_count: 0,
        })
    }

    /// Run one 512-sample window through the model and return speech probability.
    fn infer(&mut self, window: &[f32]) -> Option<f32> {
        debug_assert_eq!(window.len(), SILERO_WINDOW);

        // Build concatenated input: [context(64) | window(512)] → shape (1, 576)
        let mut input_data = Vec::with_capacity(SILERO_CONTEXT + SILERO_WINDOW);
        input_data.extend_from_slice(self.context.as_slice().unwrap_or(&[0.0; SILERO_CONTEXT]));
        input_data.extend_from_slice(window);

        let input = Array2::from_shape_vec((1, SILERO_CONTEXT + SILERO_WINDOW), input_data).ok()?;
        let sr_array = Array1::<i64>::from_elem(1, SILERO_RATE as i64);

        let input_tensor = Tensor::from_array(input.clone()).ok()?;
        let state_tensor = Tensor::from_array(self.state.clone()).ok()?;
        let sr_tensor = Tensor::from_array(sr_array).ok()?;

        let outputs = match self
            .session
            .run(ort::inputs![input_tensor, state_tensor, sr_tensor])
        {
            Ok(o) => o,
            Err(e) => {
                debug!("silero inference error: {e}");
                return None;
            }
        };

        // Update recurrent state
        let state_key = if outputs.contains_key("stateN") {
            "stateN"
        } else {
            "state"
        };
        if let Ok((shape, data)) = outputs[state_key].try_extract_tensor::<f32>() {
            if let Ok(arr) = ArrayD::<f32>::from_shape_vec(shape.to_ixdyn(), data.to_vec()) {
                self.state = arr;
            }
        }

        // Update rolling context from the tail of this window
        let new_ctx = input.slice(s![.., (SILERO_WINDOW)..]).to_owned();
        self.context = new_ctx;

        // Extract speech probability
        let output_key = if outputs.contains_key("output") {
            "output"
        } else {
            return None;
        };
        let (_, data) = outputs[output_key].try_extract_tensor::<f32>().ok()?;
        Some(data[0])
    }

    /// Process an audio chunk at the native sample rate and return a VAD event.
    ///
    /// Internally resamples to 16 kHz and feeds 512-sample windows to the
    /// Silero model.  Multiple windows may be consumed per call; the *last*
    /// significant event wins (Start > End > Continue > Silence).
    pub fn process_chunk(&mut self, samples: &[f32]) -> VadEvent {
        // Resample from native rate to 16 kHz
        let resampled = resample_linear(samples, self.native_rate, SILERO_RATE);
        self.resample_buf.extend_from_slice(&resampled);

        let mut result = if self.in_speech {
            VadEvent::SpeechContinue
        } else {
            VadEvent::Silence
        };

        // Feed complete 512-sample windows to Silero
        while self.resample_buf.len() >= SILERO_WINDOW {
            let window: Vec<f32> = self.resample_buf.drain(..SILERO_WINDOW).collect();
            let prob = match self.infer(&window) {
                Some(p) => p,
                None => continue,
            };

            let is_speech = prob >= self.threshold;

            if is_speech {
                self.silence_count = 0;
                if !self.in_speech {
                    self.in_speech = true;
                    result = VadEvent::SpeechStart;
                    debug!("silero: speech start (prob={prob:.3})");
                } else {
                    result = VadEvent::SpeechContinue;
                }
            } else {
                self.silence_count = self.silence_count.saturating_add(1);
                if self.in_speech {
                    if self.silence_count >= self.silence_frames_required {
                        self.in_speech = false;
                        result = VadEvent::SpeechEnd;
                        debug!("silero: speech end (prob={prob:.3})");
                    } else {
                        // Tolerating brief silence during speech
                        result = VadEvent::SpeechContinue;
                    }
                }
                // else: already Silence
            }
        }

        result
    }

    /// Reset the detector state, clearing all counters and model hidden state.
    pub fn reset(&mut self) {
        self.state = ArrayD::<f32>::zeros(IxDyn(&[2, 1, 128]));
        self.context = Array2::<f32>::zeros((1, SILERO_CONTEXT));
        self.in_speech = false;
        self.silence_count = 0;
        self.resample_buf.clear();
    }

    /// Returns true if the detector is currently in a speech segment.
    pub fn is_in_speech(&self) -> bool {
        self.in_speech
    }
}

/// Compute the Root Mean Square energy of an audio buffer.
///
/// Kept for the bar-level meter in main.rs (not used for VAD decisions).
pub fn rms_energy(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// Simple linear interpolation resampler.
fn resample_linear(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    if src_rate == dst_rate {
        return samples.to_vec();
    }
    let ratio = dst_rate as f64 / src_rate as f64;
    let new_len = (samples.len() as f64 * ratio) as usize;
    let mut out = Vec::with_capacity(new_len);
    for i in 0..new_len {
        let src_idx = i as f64 / ratio;
        let idx = src_idx as usize;
        let frac = (src_idx - idx as f64) as f32;
        let s0 = samples[idx];
        let s1 = if idx + 1 < samples.len() {
            samples[idx + 1]
        } else {
            s0
        };
        out.push(s0 + frac * (s1 - s0));
    }
    out
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
        let signal = vec![0.5f32; 100];
        let rms = rms_energy(&signal);
        assert!((rms - 0.5).abs() < 0.001);
    }

    #[test]
    fn resample_identity() {
        let signal = vec![0.1, 0.2, 0.3, 0.4];
        let out = resample_linear(&signal, 16000, 16000);
        assert_eq!(out, signal);
    }

    #[test]
    fn resample_downsample() {
        // 48kHz -> 16kHz should produce ~1/3 the samples
        let signal: Vec<f32> = (0..480).map(|i| (i as f32) / 480.0).collect();
        let out = resample_linear(&signal, 48000, 16000);
        assert_eq!(out.len(), 160);
    }

    #[test]
    fn silero_model_loads() {
        let det = VadDetector::new(0.5, 16000);
        assert!(det.is_some(), "Silero VAD model should load successfully");
    }

    #[test]
    fn silence_stays_silent() {
        let mut vad = VadDetector::new(0.5, 16000).expect("model should load");
        let silence = vec![0.0f32; 512];
        for _ in 0..20 {
            let event = vad.process_chunk(&silence);
            assert!(
                matches!(event, VadEvent::Silence),
                "silence should not trigger speech, got {event:?}"
            );
        }
        assert!(!vad.is_in_speech());
    }

    #[test]
    fn reset_clears_state() {
        let mut vad = VadDetector::new(0.5, 16000).expect("model should load");
        let noise = vec![0.3f32; 512];
        vad.process_chunk(&noise);
        vad.reset();
        assert!(!vad.is_in_speech());
    }
}
