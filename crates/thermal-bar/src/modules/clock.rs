/// Clock/date module for thermal-bar's right zone.
///
/// Reads the current time by invoking the `date` binary, which is always
/// available on Linux and avoids pulling in a time crate.
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use thermal_core::ThermalPalette;

use crate::layout::{ModuleOutput, Zone};

// ---------------------------------------------------------------------------
// Cached clock output
// ---------------------------------------------------------------------------

struct ClockCache {
    time_str: String,
    date_str: String,
    last_updated: Instant,
}

static CLOCK_CACHE: Mutex<Option<ClockCache>> = Mutex::new(None);

/// Refresh the cached time strings if more than 500ms have passed.
fn refresh_cache(guard: &mut MutexGuard<'_, Option<ClockCache>>) {
    let needs_refresh = match guard.as_ref() {
        None => true,
        Some(c) => c.last_updated.elapsed() > Duration::from_millis(500),
    };

    if !needs_refresh {
        return;
    }

    // Call `date` for time and date separately.
    let time_str = run_date("+%H:%M:%S");
    let date_str = run_date("+%Y-%m-%d");

    **guard = Some(ClockCache {
        time_str,
        date_str,
        last_updated: Instant::now(),
    });
}

fn run_date(fmt: &str) -> String {
    Command::new("date")
        .arg(fmt)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "--:--:--".to_owned())
}

// ---------------------------------------------------------------------------
// ClockModule
// ---------------------------------------------------------------------------

/// Renders a digital clock + date in the right zone.
pub struct ClockModule;

impl ClockModule {
    pub fn new() -> Self {
        Self
    }

    /// Return module outputs for the current time and date.
    ///
    /// Results are cached and refreshed at most every 500ms.
    pub fn render(&self) -> Vec<ModuleOutput> {
        let mut guard = CLOCK_CACHE.lock().unwrap();
        refresh_cache(&mut guard);

        let cache = guard.as_ref().unwrap();
        vec![
            // Time in warm green (like a digital readout)
            ModuleOutput::new(Zone::Right, &cache.time_str, ThermalPalette::WARM),
            // Date in muted text
            ModuleOutput::new(Zone::Right, &cache.date_str, ThermalPalette::TEXT_MUTED),
        ]
    }
}

impl Default for ClockModule {
    fn default() -> Self {
        Self::new()
    }
}
