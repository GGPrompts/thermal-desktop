use std::time::{Duration, Instant};

pub struct DismissTimer {
    pub id: u32,
    deadline: Instant,
    fade_duration: Duration,
    pub dismissed: bool,
}

impl DismissTimer {
    /// Create a timer for notification `id` that expires after `timeout_ms` ms.
    ///
    /// Returns `None` for persistent notifications (timeout_ms == 0).
    pub fn new(id: u32, timeout_ms: i32) -> Option<Self> {
        if timeout_ms == 0 {
            return None; // persistent (e.g. critical urgency)
        }
        let ms = if timeout_ms < 0 {
            // -1 means "use server default" per spec; treat as 5 s
            5000u64
        } else {
            timeout_ms as u64
        };
        Some(Self {
            id,
            deadline: Instant::now() + Duration::from_millis(ms),
            fade_duration: Duration::from_millis(300),
            dismissed: false,
        })
    }

    /// Alpha [0.0, 1.0]: 1.0 before deadline, linear fade during fade window,
    /// 0.0 after.
    pub fn alpha(&self) -> f32 {
        let now = Instant::now();
        if now < self.deadline {
            1.0
        } else {
            let elapsed = now.duration_since(self.deadline);
            if elapsed >= self.fade_duration {
                0.0
            } else {
                1.0 - elapsed.as_secs_f32() / self.fade_duration.as_secs_f32()
            }
        }
    }

    /// True when the fade has fully completed.
    pub fn is_expired(&self) -> bool {
        self.alpha() == 0.0
    }
}
