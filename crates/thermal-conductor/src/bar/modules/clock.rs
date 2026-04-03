/// Clock/date module for the bar's right zone.
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use thermal_core::ThermalPalette;

use crate::bar::layout::{ModuleOutput, Zone};

// ---------------------------------------------------------------------------
// Cached clock output
// ---------------------------------------------------------------------------

struct ClockCache {
    time_str: String,
    date_str: String,
    last_updated: Instant,
}

static CLOCK_CACHE: Mutex<Option<ClockCache>> = Mutex::new(None);

fn refresh_cache(guard: &mut MutexGuard<'_, Option<ClockCache>>) {
    let needs_refresh = match guard.as_ref() {
        None => true,
        Some(c) => c.last_updated.elapsed() > Duration::from_millis(500),
    };

    if !needs_refresh {
        return;
    }

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
        .env("TZ", detect_tz())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "--:--:--".to_owned())
}

fn detect_tz() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        if !tz.is_empty() {
            return tz;
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let s = target.to_string_lossy();
        if let Some(pos) = s.find("zoneinfo/") {
            return s[pos + "zoneinfo/".len()..].to_string();
        }
    }
    "UTC".to_string()
}

// ---------------------------------------------------------------------------
// ClockModule
// ---------------------------------------------------------------------------

pub struct ClockModule;

impl ClockModule {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self) -> Vec<ModuleOutput> {
        let mut guard = CLOCK_CACHE.lock().unwrap();
        refresh_cache(&mut guard);

        let cache = guard.as_ref().unwrap();
        vec![
            ModuleOutput::new(Zone::Right, &cache.time_str, ThermalPalette::WARM),
            ModuleOutput::new(Zone::Right, &cache.date_str, ThermalPalette::TEXT),
        ]
    }
}

impl Default for ClockModule {
    fn default() -> Self {
        Self::new()
    }
}
