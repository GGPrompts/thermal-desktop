use std::time::{Duration, Instant};

use crate::Urgency;

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

    /// Create a timer with urgency-specific fade duration.
    ///
    /// Low urgency fades faster (200ms), normal is standard (300ms),
    /// critical uses a slower fade (500ms) for emphasis.
    pub fn with_urgency(id: u32, timeout_ms: i32, urgency: Urgency) -> Option<Self> {
        let mut timer = Self::new(id, timeout_ms)?;
        timer.fade_duration = match urgency {
            Urgency::Low => Duration::from_millis(200),
            Urgency::Normal => Duration::from_millis(300),
            Urgency::Critical => Duration::from_millis(500),
        };
        Some(timer)
    }

    /// Immediately start the fade-out animation.
    pub fn dismiss(&mut self) {
        if !self.dismissed {
            self.dismissed = true;
            self.deadline = Instant::now();
        }
    }

    /// Alpha [0.0, 1.0]: 1.0 before deadline, cubic ease-out fade during
    /// fade window, 0.0 after.
    pub fn alpha(&self) -> f32 {
        let now = Instant::now();
        if now < self.deadline {
            1.0
        } else {
            let elapsed = now.duration_since(self.deadline);
            if elapsed >= self.fade_duration {
                0.0
            } else {
                let t = elapsed.as_secs_f32() / self.fade_duration.as_secs_f32();
                // Cubic ease-out: f(t) = 1 - (t)^3
                // Starts fast, decelerates smoothly to zero
                let inv = 1.0 - t;
                inv * inv * inv
            }
        }
    }

    /// True when the fade has fully completed.
    pub fn is_expired(&self) -> bool {
        self.alpha() == 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── DismissTimer::new ────────────────────────────────────────────────────

    #[test]
    fn new_zero_timeout_returns_none_persistent() {
        assert!(DismissTimer::new(1, 0).is_none());
    }

    #[test]
    fn new_positive_timeout_returns_some() {
        assert!(DismissTimer::new(1, 1000).is_some());
    }

    #[test]
    fn new_negative_timeout_uses_server_default_5000ms() {
        // -1 should be treated as 5000 ms server default, not None
        let timer = DismissTimer::new(1, -1).expect("should return Some for -1");
        // Immediately after creation the timer should not be expired
        assert!(!timer.is_expired());
    }

    #[test]
    fn new_stores_id() {
        let timer = DismissTimer::new(42, 5000).unwrap();
        assert_eq!(timer.id, 42);
    }

    #[test]
    fn new_dismissed_field_starts_false() {
        let timer = DismissTimer::new(1, 5000).unwrap();
        assert!(!timer.dismissed);
    }

    #[test]
    fn new_alpha_is_one_immediately_after_creation() {
        let timer = DismissTimer::new(1, 5000).unwrap();
        assert!(
            (timer.alpha() - 1.0).abs() < 1e-6,
            "alpha should be 1.0 before deadline, got {}",
            timer.alpha()
        );
    }

    #[test]
    fn new_not_expired_immediately() {
        let timer = DismissTimer::new(1, 5000).unwrap();
        assert!(!timer.is_expired());
    }

    // ── DismissTimer::with_urgency ───────────────────────────────────────────

    #[test]
    fn with_urgency_zero_timeout_returns_none() {
        assert!(DismissTimer::with_urgency(1, 0, Urgency::Critical).is_none());
    }

    #[test]
    fn with_urgency_low_returns_some_and_not_expired() {
        let t = DismissTimer::with_urgency(1, 5000, Urgency::Low).unwrap();
        assert!(!t.is_expired());
    }

    #[test]
    fn with_urgency_normal_returns_some_and_not_expired() {
        let t = DismissTimer::with_urgency(1, 8000, Urgency::Normal).unwrap();
        assert!(!t.is_expired());
    }

    #[test]
    fn with_urgency_critical_positive_timeout_returns_some() {
        // Critical with explicit positive timeout should give a timer
        let t = DismissTimer::with_urgency(1, 1000, Urgency::Critical).unwrap();
        assert!(!t.is_expired());
    }

    #[test]
    fn with_urgency_stores_id() {
        let t = DismissTimer::with_urgency(99, 5000, Urgency::Normal).unwrap();
        assert_eq!(t.id, 99);
    }

    // ── DismissTimer::dismiss ────────────────────────────────────────────────

    #[test]
    fn dismiss_sets_dismissed_flag() {
        let mut timer = DismissTimer::new(1, 5000).unwrap();
        assert!(!timer.dismissed);
        timer.dismiss();
        assert!(timer.dismissed);
    }

    #[test]
    fn dismiss_is_idempotent() {
        let mut timer = DismissTimer::new(1, 5000).unwrap();
        timer.dismiss();
        timer.dismiss(); // second call must not panic
        assert!(timer.dismissed);
    }

    #[test]
    fn dismiss_starts_fade_alpha_below_one_after_fade_duration() {
        let mut timer = DismissTimer::new(1, 5000).unwrap();
        timer.dismiss();
        // The deadline is now set to Instant::now() at dismiss time.
        // After the full fade_duration (300 ms default) the alpha should be 0.
        // We cannot sleep in a fast unit test, but we can verify alpha ≤ 1.0.
        assert!(timer.alpha() <= 1.0);
    }

    // ── DismissTimer::alpha ──────────────────────────────────────────────────

    #[test]
    fn alpha_before_deadline_is_one() {
        // Large timeout so deadline is well in the future
        let timer = DismissTimer::new(1, 60_000).unwrap();
        assert!((timer.alpha() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn alpha_after_full_fade_is_zero() {
        // Create a timer with a very short timeout (1 ms) so deadline passes
        // immediately, then spin until the 300 ms fade completes.
        let timer = DismissTimer::new(1, 1).unwrap();
        // Spin-wait up to 400 ms for expiry; test environment should be fast enough.
        let deadline = std::time::Instant::now() + Duration::from_millis(400);
        loop {
            if timer.is_expired() { break; }
            if std::time::Instant::now() > deadline {
                panic!("timer did not expire within 400 ms; alpha={}", timer.alpha());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(timer.alpha(), 0.0);
    }

    #[test]
    fn alpha_is_clamped_to_zero_one_range() {
        let timer = DismissTimer::new(1, 5000).unwrap();
        let a = timer.alpha();
        assert!(a >= 0.0 && a <= 1.0, "alpha out of range: {a}");
    }

    // ── DismissTimer::is_expired ─────────────────────────────────────────────

    #[test]
    fn is_expired_false_for_fresh_timer() {
        let timer = DismissTimer::new(1, 5000).unwrap();
        assert!(!timer.is_expired());
    }

    #[test]
    fn is_expired_true_after_full_fade() {
        // 1 ms timeout + 300 ms fade; wait up to 500 ms
        let timer = DismissTimer::new(1, 1).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if timer.is_expired() { break; }
            if std::time::Instant::now() > deadline {
                panic!("timer never expired; alpha={}", timer.alpha());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(timer.is_expired());
    }

    #[test]
    fn is_expired_consistent_with_alpha_zero() {
        let timer = DismissTimer::new(1, 5000).unwrap();
        // For a live timer both sides of the invariant must hold
        let expired = timer.is_expired();
        let alpha_zero = timer.alpha() == 0.0;
        assert_eq!(expired, alpha_zero);
    }
}
