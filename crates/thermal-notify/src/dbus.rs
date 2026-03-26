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
    #[allow(clippy::too_many_arguments)]
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

        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(notif);

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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Notification struct construction ──────────────────────────────────────

    #[test]
    fn notification_stores_all_fields() {
        let n = Notification {
            id: 42,
            app_name: "test-app".to_string(),
            summary: "Test Summary".to_string(),
            body: "Test body text".to_string(),
            urgency: Urgency::Normal,
            timeout: 5000,
        };
        assert_eq!(n.id, 42);
        assert_eq!(n.app_name, "test-app");
        assert_eq!(n.summary, "Test Summary");
        assert_eq!(n.body, "Test body text");
        assert_eq!(n.urgency, Urgency::Normal);
        assert_eq!(n.timeout, 5000);
    }

    #[test]
    fn notification_empty_strings_are_valid() {
        let n = Notification {
            id: 1,
            app_name: String::new(),
            summary: String::new(),
            body: String::new(),
            urgency: Urgency::Low,
            timeout: -1,
        };
        assert_eq!(n.app_name, "");
        assert_eq!(n.summary, "");
        assert_eq!(n.body, "");
    }

    #[test]
    fn notification_clone_produces_independent_copy() {
        let n = Notification {
            id: 10,
            app_name: "cloned-app".to_string(),
            summary: "Clone test".to_string(),
            body: "body".to_string(),
            urgency: Urgency::Critical,
            timeout: 0,
        };
        let cloned = n.clone();
        assert_eq!(cloned.id, n.id);
        assert_eq!(cloned.app_name, n.app_name);
        assert_eq!(cloned.summary, n.summary);
        assert_eq!(cloned.body, n.body);
        assert_eq!(cloned.urgency, n.urgency);
        assert_eq!(cloned.timeout, n.timeout);
    }

    #[test]
    fn notification_urgency_low() {
        let n = Notification {
            id: 1,
            app_name: "a".to_string(),
            summary: "s".to_string(),
            body: "b".to_string(),
            urgency: Urgency::Low,
            timeout: 5000,
        };
        assert_eq!(n.urgency, Urgency::Low);
    }

    #[test]
    fn notification_urgency_critical_with_zero_timeout() {
        let n = Notification {
            id: 2,
            app_name: "a".to_string(),
            summary: "s".to_string(),
            body: "b".to_string(),
            urgency: Urgency::Critical,
            timeout: 0,
        };
        assert_eq!(n.urgency, Urgency::Critical);
        assert_eq!(n.timeout, 0);
    }

    #[test]
    fn notification_negative_one_timeout_is_server_default() {
        let n = Notification {
            id: 3,
            app_name: "a".to_string(),
            summary: "s".to_string(),
            body: "b".to_string(),
            urgency: Urgency::Normal,
            timeout: -1,
        };
        assert_eq!(n.timeout, -1);
    }

    // ── NotificationServer construction ───────────────────────────────────────

    #[test]
    fn server_new_initialises_without_audio() {
        let queue: NotificationQueue = Arc::new(Mutex::new(VecDeque::new()));
        let _server = NotificationServer::new(Arc::clone(&queue), None);
        // Merely confirm construction succeeds and queue is empty
        assert!(queue.lock().unwrap().is_empty());
    }

    #[test]
    fn server_counter_starts_at_one() {
        let queue: NotificationQueue = Arc::new(Mutex::new(VecDeque::new()));
        let server = NotificationServer::new(Arc::clone(&queue), None);
        // The atomic counter starts at 1 (as initialised in new())
        assert_eq!(server.counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ── get_capabilities / get_server_information (sync helpers) ─────────────

    #[test]
    fn get_capabilities_includes_body() {
        let queue: NotificationQueue = Arc::new(Mutex::new(VecDeque::new()));
        let server = NotificationServer::new(Arc::clone(&queue), None);
        let caps = server.get_capabilities();
        assert!(caps.contains(&"body".to_string()));
    }

    #[test]
    fn get_capabilities_includes_persistence() {
        let queue: NotificationQueue = Arc::new(Mutex::new(VecDeque::new()));
        let server = NotificationServer::new(Arc::clone(&queue), None);
        let caps = server.get_capabilities();
        assert!(caps.contains(&"persistence".to_string()));
    }

    #[test]
    fn get_server_information_returns_correct_name() {
        let queue: NotificationQueue = Arc::new(Mutex::new(VecDeque::new()));
        let server = NotificationServer::new(Arc::clone(&queue), None);
        let (name, vendor, version, spec) = server.get_server_information();
        assert_eq!(name, "thermal-notify");
        assert_eq!(vendor, "thermal");
        assert_eq!(version, "1.0");
        assert_eq!(spec, "1.2");
    }
}
