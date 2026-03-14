use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use zbus::interface;
use zbus::zvariant::Value;

use crate::audio::AudioPlayer;
use crate::urgency::Urgency;

// ── Notification struct ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    pub timeout: i32,
}

// ── Type alias for the shared queue ─────────────────────────────────────────

/// Shared notification queue. Uses std::sync::Mutex so it can be accessed
/// from both async D-Bus handlers (non-blocking lock) and the sync render loop
/// running in spawn_blocking.
pub type NotificationQueue = Arc<Mutex<VecDeque<Notification>>>;

// ── D-Bus server implementation ──────────────────────────────────────────────

pub struct NotificationServer {
    queue: NotificationQueue,
    counter: Arc<AtomicU32>,
    /// None when audio failed to initialise.
    audio: Option<Arc<AudioPlayer>>,
}

impl NotificationServer {
    pub fn new(queue: NotificationQueue, audio: Option<Arc<AudioPlayer>>) -> Self {
        Self {
            queue,
            counter: Arc::new(AtomicU32::new(1)),
            audio,
        }
    }
}

#[interface(name = "org.freedesktop.Notifications")]
impl NotificationServer {
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        hints: HashMap<String, Value<'_>>,
        expire_timeout: i32,
    ) -> u32 {
        // Parse urgency from hints
        let urgency = Urgency::from_hints(&hints);

        // Determine ID
        let id = if replaces_id > 0 {
            replaces_id
        } else {
            self.counter.fetch_add(1, Ordering::SeqCst)
        };

        let notif = Notification {
            id,
            app_name,
            summary,
            body,
            urgency,
            timeout: expire_timeout,
        };

        tracing::info!(
            id = notif.id,
            app = %notif.app_name,
            urgency = ?notif.urgency,
            summary = %notif.summary,
            "New notification"
        );

        self.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(notif);

        // Play audio cue (fire-and-forget via channel)
        if let Some(audio) = &self.audio {
            audio.play_for_urgency(urgency);
        }

        id
    }

    async fn close_notification(&self, id: u32) {
        tracing::debug!(id, "CloseNotification requested");
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.retain(|n| n.id != id);
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".to_string(), "persistence".to_string()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "thermal-notify".to_string(),
            "thermal".to_string(),
            "1.0".to_string(),
            "1.2".to_string(),
        )
    }
}
