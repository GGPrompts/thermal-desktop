use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use rodio::{OutputStream, Source};

use crate::urgency::Urgency;

/// Handle to the audio worker thread. The thread owns the OutputStream.
pub struct AudioPlayer {
    tx: Sender<Urgency>,
}

impl AudioPlayer {
    /// Spawn a dedicated audio thread and return a handle to it.
    pub fn new() -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Urgency>();

        thread::Builder::new()
            .name("thermal-notify-audio".into())
            .spawn(move || {
                // OutputStream must live on this thread (it's !Send)
                let Ok((_stream, handle)) = OutputStream::try_default() else {
                    tracing::warn!("AudioPlayer: no default audio output — audio disabled");
                    return;
                };

                for urgency in rx {
                    play(&handle, urgency);
                }
            })?;

        Ok(Self { tx })
    }

    /// Send a play request to the audio thread (fire-and-forget).
    pub fn play_for_urgency(&self, urgency: Urgency) {
        if let Err(e) = self.tx.send(urgency) {
            tracing::warn!("audio channel send failed: {e}");
        }
    }
}

fn play(handle: &rodio::OutputStreamHandle, urgency: Urgency) {
    match urgency {
        Urgency::Low => {
            // 440 Hz, 150 ms, low amplitude
            let source = rodio::source::SineWave::new(440.0)
                .amplify(0.3)
                .take_duration(Duration::from_millis(150));
            if let Err(e) = handle.play_raw(source.convert_samples()) {
                tracing::warn!("audio play error (low): {e}");
            }
        }
        Urgency::Normal => {
            // Two-tone: play 660 Hz then 880 Hz sequentially
            let tone1 = rodio::source::SineWave::new(660.0)
                .amplify(0.5)
                .take_duration(Duration::from_millis(200));
            if let Err(e) = handle.play_raw(tone1.convert_samples()) {
                tracing::warn!("audio play error (normal tone1): {e}");
            }
            // Small sleep to let tone1 finish before tone2
            thread::sleep(Duration::from_millis(200));
            let tone2 = rodio::source::SineWave::new(880.0)
                .amplify(0.5)
                .take_duration(Duration::from_millis(100));
            if let Err(e) = handle.play_raw(tone2.convert_samples()) {
                tracing::warn!("audio play error (normal tone2): {e}");
            }
        }
        Urgency::Critical => {
            // 880 Hz × 3 with 50 ms gaps
            for i in 0u32..3 {
                let beep = rodio::source::SineWave::new(880.0)
                    .amplify(0.8)
                    .take_duration(Duration::from_millis(100));
                if let Err(e) = handle.play_raw(beep.convert_samples()) {
                    tracing::warn!("audio play error (critical beep {i}): {e}");
                }
                if i < 2 {
                    thread::sleep(Duration::from_millis(150)); // 100ms tone + 50ms gap
                }
            }
        }
    }
}
