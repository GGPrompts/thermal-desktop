use crate::dbus::Notification;
use crate::timer::DismissTimer;

pub struct ActiveNotif {
    pub notif: Notification,
    pub timer: Option<DismissTimer>,
    pub y_offset: f32,
    pub target_y: f32,
}

impl ActiveNotif {
    pub fn alpha(&self) -> f32 {
        self.timer.as_ref().map(|t| t.alpha()).unwrap_or(1.0)
    }
}

pub struct NotificationStack {
    slots: Vec<ActiveNotif>,
    card_height: f32,
    gap: f32,
}

impl NotificationStack {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            card_height: 100.0,
            gap: 8.0,
        }
    }

    /// Push a new notification at the top of the stack.
    pub fn push(&mut self, notif: Notification) {
        let timeout_ms = notif.timeout;
        let id = notif.id;

        // Shift existing entries down
        let step = self.card_height + self.gap;
        for entry in &mut self.slots {
            entry.target_y += step;
        }

        let timer = DismissTimer::with_urgency(id, timeout_ms, notif.urgency);
        self.slots.insert(
            0,
            ActiveNotif {
                notif,
                timer,
                y_offset: 0.0,
                target_y: 0.0,
            },
        );
    }

    /// Remove a notification by ID and shift remaining entries up.
    pub fn remove_id(&mut self, id: u32) {
        let step = self.card_height + self.gap;
        let mut removed = false;
        self.slots.retain(|e| {
            if e.notif.id == id {
                removed = true;
                false
            } else {
                true
            }
        });
        if removed {
            // Re-target positions
            for (i, entry) in self.slots.iter_mut().enumerate() {
                entry.target_y = i as f32 * step;
            }
        }
    }

    /// Advance animation state and remove expired entries.
    ///
    /// `dt` — seconds since last tick.
    pub fn tick(&mut self, dt: f32) {
        let speed = 12.0;
        for entry in &mut self.slots {
            let delta = entry.target_y - entry.y_offset;
            entry.y_offset += delta * speed * dt;
        }

        // Remove expired entries and compact targets
        let step = self.card_height + self.gap;
        self.slots
            .retain(|e| e.timer.as_ref().map(|t| !t.is_expired()).unwrap_or(true));
        for (i, entry) in self.slots.iter_mut().enumerate() {
            entry.target_y = i as f32 * step;
        }
    }

    /// Iterate over visible notifications (alpha > 0).
    pub fn iter_visible(&self) -> impl Iterator<Item = &ActiveNotif> {
        self.slots.iter().filter(|e| e.alpha() > 0.0)
    }

    /// How many entries are currently tracked.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn card_height(&self) -> f32 {
        self.card_height
    }

    pub fn gap(&self) -> f32 {
        self.gap
    }

    /// Dismiss the front-most (top) notification by triggering its fade-out.
    /// If it has no timer (persistent/critical), force-remove it immediately.
    pub fn dismiss_front(&mut self) {
        if let Some(entry) = self.slots.first_mut() {
            if let Some(timer) = &mut entry.timer {
                timer.dismiss();
            } else {
                // Persistent notification — force remove
                let id = entry.notif.id;
                self.remove_id(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::Notification;
    use crate::urgency::Urgency;

    /// Build a minimal `Notification` for testing without needing D-Bus.
    fn make_notif(id: u32, urgency: Urgency, timeout_ms: i32) -> Notification {
        Notification {
            id,
            app_name: format!("app-{id}"),
            summary: format!("Summary {id}"),
            body: format!("Body {id}"),
            urgency,
            timeout: timeout_ms,
        }
    }

    // ── NotificationStack::new ───────────────────────────────────────────────

    #[test]
    fn new_stack_is_empty() {
        let stack = NotificationStack::new();
        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn new_stack_has_expected_card_dimensions() {
        let stack = NotificationStack::new();
        assert_eq!(stack.card_height(), 100.0);
        assert_eq!(stack.gap(), 8.0);
    }

    // ── NotificationStack::push ──────────────────────────────────────────────

    #[test]
    fn push_single_notification_increases_len() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        assert_eq!(stack.len(), 1);
        assert!(!stack.is_empty());
    }

    #[test]
    fn push_inserts_newest_at_front() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        stack.push(make_notif(2, Urgency::Normal, 5000));
        // The most-recently pushed notification should be at slot index 0
        let ids: Vec<u32> = stack.slots.iter().map(|e| e.notif.id).collect();
        assert_eq!(ids[0], 2, "newest notification should be first");
        assert_eq!(ids[1], 1);
    }

    #[test]
    fn push_multiple_updates_len() {
        let mut stack = NotificationStack::new();
        for i in 1..=5 {
            stack.push(make_notif(i, Urgency::Low, 5000));
        }
        assert_eq!(stack.len(), 5);
    }

    #[test]
    fn push_shifts_existing_target_y_down() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        let initial_target = stack.slots[0].target_y;

        stack.push(make_notif(2, Urgency::Normal, 5000));
        // The original notification (now at index 1) should have target_y shifted
        let step = stack.card_height() + stack.gap();
        assert!(
            (stack.slots[1].target_y - (initial_target + step)).abs() < 1e-3,
            "existing entry should have moved down by one step"
        );
    }

    #[test]
    fn push_new_entry_starts_at_y_zero() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        assert!((stack.slots[0].y_offset).abs() < 1e-6);
        assert!((stack.slots[0].target_y).abs() < 1e-6);
    }

    #[test]
    fn push_critical_zero_timeout_creates_persistent_timer_none() {
        // timeout 0 → DismissTimer::new returns None → persistent notification
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Critical, 0));
        assert!(
            stack.slots[0].timer.is_none(),
            "critical/persistent should have no timer"
        );
    }

    // ── NotificationStack::remove_id ─────────────────────────────────────────

    #[test]
    fn remove_id_removes_correct_notification() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        stack.push(make_notif(2, Urgency::Normal, 5000));
        stack.push(make_notif(3, Urgency::Normal, 5000));
        stack.remove_id(2);
        let ids: Vec<u32> = stack.slots.iter().map(|e| e.notif.id).collect();
        assert!(!ids.contains(&2), "id 2 should be gone");
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn remove_id_on_missing_id_is_noop() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        stack.remove_id(999);
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn remove_id_updates_target_positions() {
        let mut stack = NotificationStack::new();
        // Push three notifications (newest first: 3, 2, 1)
        stack.push(make_notif(1, Urgency::Normal, 5000));
        stack.push(make_notif(2, Urgency::Normal, 5000));
        stack.push(make_notif(3, Urgency::Normal, 5000));
        // Remove the middle one
        stack.remove_id(2);
        // Remaining: 3 at index 0, 1 at index 1
        let step = stack.card_height() + stack.gap();
        assert!(
            (stack.slots[0].target_y - 0.0).abs() < 1e-3,
            "slot 0 target_y should be 0"
        );
        assert!(
            (stack.slots[1].target_y - step).abs() < 1e-3,
            "slot 1 target_y should be one step"
        );
    }

    #[test]
    fn remove_id_empties_single_element_stack() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(42, Urgency::Low, 5000));
        stack.remove_id(42);
        assert!(stack.is_empty());
    }

    // ── NotificationStack::iter_visible ──────────────────────────────────────

    #[test]
    fn iter_visible_returns_all_live_notifications() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        stack.push(make_notif(2, Urgency::Normal, 5000));
        // Both should be visible (alpha == 1.0)
        assert_eq!(stack.iter_visible().count(), 2);
    }

    #[test]
    fn iter_visible_empty_stack_returns_none() {
        let stack = NotificationStack::new();
        assert_eq!(stack.iter_visible().count(), 0);
    }

    // ── NotificationStack::tick ───────────────────────────────────────────────

    #[test]
    fn tick_zero_dt_preserves_len() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        stack.tick(0.0);
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn tick_removes_expired_notifications() {
        let mut stack = NotificationStack::new();
        // Push a notification with a 1 ms timeout so the timer expires quickly
        stack.push(make_notif(1, Urgency::Normal, 1));

        // Wait until the timer has fully expired (1 ms deadline + 300 ms fade)
        let limit = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            stack.tick(0.016); // simulate ~60 fps tick
            if stack.is_empty() {
                break;
            }
            if std::time::Instant::now() > limit {
                panic!("notification never expired from stack");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(stack.is_empty());
    }

    #[test]
    fn tick_moves_y_offset_toward_target() {
        let mut stack = NotificationStack::new();
        // Push two notifications so the older one has target_y > 0
        stack.push(make_notif(1, Urgency::Normal, 5000));
        stack.push(make_notif(2, Urgency::Normal, 5000));
        // Manually disturb y_offset on the second slot away from target
        let target = stack.slots[1].target_y;
        stack.slots[1].y_offset = 0.0; // force it away from target
        let before_delta = (target - stack.slots[1].y_offset).abs();

        stack.tick(0.016);
        let after_delta = (target - stack.slots[1].y_offset).abs();
        assert!(
            after_delta < before_delta,
            "y_offset should move closer to target_y after tick"
        );
    }

    #[test]
    fn tick_persistent_notification_is_not_removed() {
        let mut stack = NotificationStack::new();
        // timeout 0 → no timer → persistent
        stack.push(make_notif(1, Urgency::Critical, 0));
        for _ in 0..10 {
            stack.tick(0.016);
        }
        assert_eq!(
            stack.len(),
            1,
            "persistent notification should not be removed by tick"
        );
    }

    // ── NotificationStack::dismiss_front ─────────────────────────────────────

    #[test]
    fn dismiss_front_on_empty_stack_is_noop() {
        let mut stack = NotificationStack::new();
        stack.dismiss_front(); // must not panic
        assert!(stack.is_empty());
    }

    #[test]
    fn dismiss_front_marks_timer_as_dismissed() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        stack.dismiss_front();
        let timer = stack.slots[0].timer.as_ref().expect("should have timer");
        assert!(timer.dismissed);
    }

    #[test]
    fn dismiss_front_persistent_removes_immediately() {
        let mut stack = NotificationStack::new();
        // timeout 0 → None timer → force-remove on dismiss
        stack.push(make_notif(1, Urgency::Critical, 0));
        stack.dismiss_front();
        assert!(
            stack.is_empty(),
            "persistent notification should be removed immediately on dismiss"
        );
    }

    #[test]
    fn dismiss_front_only_affects_first_entry() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        stack.push(make_notif(2, Urgency::Normal, 5000));
        stack.dismiss_front();
        // Slot 0 (id=2) should be dismissed; slot 1 (id=1) should not
        assert!(stack.slots[0].timer.as_ref().unwrap().dismissed);
        assert!(!stack.slots[1].timer.as_ref().unwrap().dismissed);
    }

    // ── ActiveNotif::alpha ────────────────────────────────────────────────────

    #[test]
    fn active_notif_alpha_with_timer_returns_timer_alpha() {
        let mut stack = NotificationStack::new();
        stack.push(make_notif(1, Urgency::Normal, 5000));
        // Timer is fresh → alpha should be 1.0
        assert!((stack.slots[0].alpha() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn active_notif_alpha_without_timer_returns_one() {
        let mut stack = NotificationStack::new();
        // timeout 0 → no timer → ActiveNotif::alpha() returns 1.0
        stack.push(make_notif(1, Urgency::Critical, 0));
        assert!((stack.slots[0].alpha() - 1.0).abs() < 1e-6);
    }

    // ── Notification struct construction ──────────────────────────────────────

    #[test]
    fn notification_fields_are_stored_correctly() {
        let n = make_notif(7, Urgency::Critical, 3000);
        assert_eq!(n.id, 7);
        assert_eq!(n.urgency, Urgency::Critical);
        assert_eq!(n.timeout, 3000);
        assert_eq!(n.app_name, "app-7");
        assert_eq!(n.summary, "Summary 7");
        assert_eq!(n.body, "Body 7");
    }

    #[test]
    fn notification_urgency_low_field() {
        let n = make_notif(1, Urgency::Low, 5000);
        assert_eq!(n.urgency, Urgency::Low);
    }

    #[test]
    fn notification_urgency_normal_field() {
        let n = make_notif(2, Urgency::Normal, 8000);
        assert_eq!(n.urgency, Urgency::Normal);
    }

    #[test]
    fn notification_urgency_critical_field() {
        let n = make_notif(3, Urgency::Critical, 0);
        assert_eq!(n.urgency, Urgency::Critical);
        assert_eq!(n.timeout, 0);
    }

    #[test]
    fn notification_default_negative_timeout_stored_as_given() {
        // The spec says -1 means "server default"; the struct stores it verbatim
        let n = make_notif(4, Urgency::Normal, -1);
        assert_eq!(n.timeout, -1);
    }
}
