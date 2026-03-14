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
        self.timer
            .as_ref()
            .map(|t| t.alpha())
            .unwrap_or(1.0)
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
        self.slots.retain(|e| {
            e.timer.as_ref().map(|t| !t.is_expired()).unwrap_or(true)
        });
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
