use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rodio::{OutputStream, Source};

use crate::urgency::Urgency;

/// Handle to the audio worker thread. The thread owns the OutputStream.
pub struct AudioPlayer {
    tx: Sender<Urgency>,
    /// Global volume 0-100, shared with the audio thread.
    #[allow(dead_code)]
    volume_pct: Arc<AtomicU8>,
}

impl AudioPlayer {
    /// Spawn a dedicated audio thread and return a handle to it.
    pub fn new(volume: u8) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Urgency>();
        let volume_pct = Arc::new(AtomicU8::new(volume.min(100)));
        let vol = Arc::clone(&volume_pct);

        thread::Builder::new()
            .name("thermal-notify-audio".into())
            .spawn(move || {
                // OutputStream must live on this thread (it's !Send)
                let Ok((_stream, handle)) = OutputStream::try_default() else {
                    tracing::warn!("AudioPlayer: no default audio output — audio disabled");
                    return;
                };

                for urgency in rx {
                    let v = vol.load(Ordering::Relaxed) as f32 / 100.0;
                    play(&handle, urgency, v);
                }
            })?;

        Ok(Self { tx, volume_pct })
    }

    /// Send a play request to the audio thread (fire-and-forget).
    pub fn play_for_urgency(&self, urgency: Urgency) {
        if let Err(e) = self.tx.send(urgency) {
            tracing::warn!("audio channel send failed: {e}");
        }
    }

    /// Set global volume (0-100).
    #[allow(dead_code)]
    pub fn set_volume(&self, pct: u8) {
        self.volume_pct.store(pct.min(100), Ordering::Relaxed);
    }

    /// Get current volume (0-100).
    #[allow(dead_code)]
    pub fn volume(&self) -> u8 {
        self.volume_pct.load(Ordering::Relaxed)
    }
}

fn play(handle: &rodio::OutputStreamHandle, urgency: Urgency, vol: f32) {
    match urgency {
        Urgency::Low => {
            let source = rodio::source::SineWave::new(440.0)
                .amplify(0.3 * vol)
                .take_duration(Duration::from_millis(150));
            if let Err(e) = handle.play_raw(source.convert_samples()) {
                tracing::warn!("audio play error (low): {e}");
            }
        }
        Urgency::Normal => {
            let tone1 = rodio::source::SineWave::new(660.0)
                .amplify(0.5 * vol)
                .take_duration(Duration::from_millis(200));
            if let Err(e) = handle.play_raw(tone1.convert_samples()) {
                tracing::warn!("audio play error (normal tone1): {e}");
            }
            thread::sleep(Duration::from_millis(200));
            let tone2 = rodio::source::SineWave::new(880.0)
                .amplify(0.5 * vol)
                .take_duration(Duration::from_millis(100));
            if let Err(e) = handle.play_raw(tone2.convert_samples()) {
                tracing::warn!("audio play error (normal tone2): {e}");
            }
        }
        Urgency::Critical => {
            for i in 0u32..3 {
                let beep = rodio::source::SineWave::new(880.0)
                    .amplify(0.8 * vol)
                    .take_duration(Duration::from_millis(100));
                if let Err(e) = handle.play_raw(beep.convert_samples()) {
                    tracing::warn!("audio play error (critical beep {i}): {e}");
                }
                if i < 2 {
                    thread::sleep(Duration::from_millis(150));
                }
            }
        }
    }
}
