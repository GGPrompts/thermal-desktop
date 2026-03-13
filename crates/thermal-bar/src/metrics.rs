/// System metrics polling — reads from /proc and /sys, no external crates.
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// CPU state (stored between polls for delta computation)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct CpuTimes {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
}

impl CpuTimes {
    fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq
    }

    fn idle_total(&self) -> u64 {
        self.idle + self.iowait
    }
}

static PREV_CPU: Mutex<Option<CpuTimes>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Net state (stored between polls)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct NetSample {
    rx_bytes: u64,
    tx_bytes: u64,
    /// Monotonic seconds when this sample was taken.
    when_secs: u64,
}

static PREV_NET: Mutex<Option<NetSample>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A snapshot of key system metrics.
pub struct SystemMetrics {
    pub cpu_usage_pct: f32,
    pub cpu_temp_c: Option<f32>,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub net_rx_kbps: f32,
    pub net_tx_kbps: f32,
    pub gpu_temp_c: Option<f32>,
    pub gpu_usage_pct: Option<f32>,
}

impl SystemMetrics {
    /// Poll current system metrics synchronously.
    pub fn poll() -> Self {
        Self {
            cpu_usage_pct: read_cpu_usage(),
            cpu_temp_c: read_cpu_temp(),
            mem_used_mb: read_mem_used(),
            mem_total_mb: read_mem_total(),
            net_rx_kbps: 0.0,   // populated by read_net() below
            net_tx_kbps: 0.0,
            gpu_temp_c: read_gpu_temp(),
            gpu_usage_pct: read_gpu_usage(),
        }
    }

    /// Poll all metrics including network throughput.
    pub fn poll_full() -> Self {
        let (rx, tx) = read_net();
        let mut m = Self::poll();
        m.net_rx_kbps = rx;
        m.net_tx_kbps = tx;
        m
    }
}

// ---------------------------------------------------------------------------
// CPU usage
// ---------------------------------------------------------------------------

fn parse_cpu_times(line: &str) -> Option<CpuTimes> {
    // Format: "cpu  user nice system idle iowait irq softirq ..."
    let mut parts = line.split_ascii_whitespace();
    parts.next(); // skip "cpu"
    let user    = parts.next()?.parse().ok()?;
    let nice    = parts.next()?.parse().ok()?;
    let system  = parts.next()?.parse().ok()?;
    let idle    = parts.next()?.parse().ok()?;
    let iowait  = parts.next()?.parse().ok()?;
    let irq     = parts.next()?.parse().ok()?;
    let softirq = parts.next()?.parse().ok()?;
    Some(CpuTimes { user, nice, system, idle, iowait, irq, softirq })
}

fn read_cpu_usage() -> f32 {
    let Ok(contents) = std::fs::read_to_string("/proc/stat") else {
        return 0.0;
    };
    let first_line = contents.lines().next().unwrap_or("");
    let Some(current) = parse_cpu_times(first_line) else {
        return 0.0;
    };

    let mut guard = PREV_CPU.lock().unwrap();
    let usage = match *guard {
        None => 0.0,
        Some(prev) => {
            let delta_total = current.total().saturating_sub(prev.total());
            let delta_idle  = current.idle_total().saturating_sub(prev.idle_total());
            if delta_total == 0 {
                0.0
            } else {
                (delta_total - delta_idle) as f32 / delta_total as f32 * 100.0
            }
        }
    };
    *guard = Some(current);
    usage
}

// ---------------------------------------------------------------------------
// CPU temperature
// ---------------------------------------------------------------------------

fn read_cpu_temp() -> Option<f32> {
    // Walk /sys/class/thermal/thermal_zone*/type to find x86_pkg_temp or cpu.
    for i in 0..=16u32 {
        let zone = format!("/sys/class/thermal/thermal_zone{i}");
        let type_path = format!("{zone}/type");
        let Ok(zone_type) = std::fs::read_to_string(&type_path) else {
            continue;
        };
        let zone_type_lower = zone_type.trim().to_ascii_lowercase();
        if zone_type_lower.contains("x86_pkg_temp") || zone_type_lower.contains("cpu") {
            let temp_path = format!("{zone}/temp");
            if let Ok(raw) = std::fs::read_to_string(&temp_path) {
                if let Ok(millideg) = raw.trim().parse::<i64>() {
                    return Some(millideg as f32 / 1000.0);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

fn parse_meminfo_kb(contents: &str, key: &str) -> Option<u64> {
    for line in contents.lines() {
        if line.starts_with(key) {
            let val: u64 = line
                .split_ascii_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            return Some(val);
        }
    }
    None
}

fn read_meminfo() -> Option<(u64, u64)> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    let total     = parse_meminfo_kb(&contents, "MemTotal:")?;
    let available = parse_meminfo_kb(&contents, "MemAvailable:")?;
    Some((total, available))
}

fn read_mem_total() -> u64 {
    read_meminfo().map(|(t, _)| t / 1024).unwrap_or(0)
}

fn read_mem_used() -> u64 {
    read_meminfo().map(|(t, a)| (t.saturating_sub(a)) / 1024).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Network throughput
// ---------------------------------------------------------------------------

fn monotonic_secs() -> u64 {
    // Use CLOCK_MONOTONIC via std::time::Instant isn't directly available as
    // wall-clock seconds, so read /proc/uptime instead.
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_ascii_whitespace().next().and_then(|v| v.parse::<f64>().ok()))
        .map(|secs| secs as u64)
        .unwrap_or(0)
}

fn parse_net_totals() -> (u64, u64) {
    let Ok(contents) = std::fs::read_to_string("/proc/net/dev") else {
        return (0, 0);
    };
    let mut total_rx = 0u64;
    let mut total_tx = 0u64;
    for line in contents.lines().skip(2) {
        // Each line: "  eth0:  rx_bytes packets ... tx_bytes ..."
        let line = line.trim();
        let Some(colon) = line.find(':') else { continue };
        let iface = &line[..colon];
        // skip loopback
        if iface.trim() == "lo" {
            continue;
        }
        let fields: Vec<&str> = line[colon + 1..].split_ascii_whitespace().collect();
        // col 0 = rx_bytes, col 8 = tx_bytes
        let rx: u64 = fields.get(0).and_then(|v| v.parse().ok()).unwrap_or(0);
        let tx: u64 = fields.get(8).and_then(|v| v.parse().ok()).unwrap_or(0);
        total_rx = total_rx.saturating_add(rx);
        total_tx = total_tx.saturating_add(tx);
    }
    (total_rx, total_tx)
}

fn read_net() -> (f32, f32) {
    let (rx, tx) = parse_net_totals();
    let now = monotonic_secs();
    let mut guard = PREV_NET.lock().unwrap();
    let (rx_kbps, tx_kbps) = match *guard {
        None => (0.0, 0.0),
        Some(prev) => {
            let elapsed = now.saturating_sub(prev.when_secs);
            if elapsed == 0 {
                (0.0, 0.0)
            } else {
                let rx_delta = rx.saturating_sub(prev.rx_bytes) as f32;
                let tx_delta = tx.saturating_sub(prev.tx_bytes) as f32;
                let elapsed_f = elapsed as f32;
                (rx_delta / elapsed_f / 1024.0, tx_delta / elapsed_f / 1024.0)
            }
        }
    };
    *guard = Some(NetSample { rx_bytes: rx, tx_bytes: tx, when_secs: now });
    (rx_kbps, tx_kbps)
}

// ---------------------------------------------------------------------------
// GPU metrics
// ---------------------------------------------------------------------------

fn try_amd_temp() -> Option<f32> {
    // Try /sys/class/drm/card0/device/hwmon/hwmon*/temp1_input
    for entry in std::fs::read_dir("/sys/class/drm/card0/device/hwmon").ok()? {
        let entry = entry.ok()?;
        let path = entry.path().join("temp1_input");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(millideg) = raw.trim().parse::<i64>() {
                return Some(millideg as f32 / 1000.0);
            }
        }
    }
    // Try /sys/class/hwmon/hwmon*/name == "amdgpu"
    for i in 0..=8u32 {
        let base = format!("/sys/class/hwmon/hwmon{i}");
        let name_path = format!("{base}/name");
        if let Ok(name) = std::fs::read_to_string(&name_path) {
            if name.trim() == "amdgpu" {
                let temp_path = format!("{base}/temp1_input");
                if let Ok(raw) = std::fs::read_to_string(&temp_path) {
                    if let Ok(millideg) = raw.trim().parse::<i64>() {
                        return Some(millideg as f32 / 1000.0);
                    }
                }
            }
        }
    }
    None
}

fn try_amd_usage() -> Option<f32> {
    let raw = std::fs::read_to_string("/sys/class/drm/card0/device/gpu_busy_percent").ok()?;
    raw.trim().parse::<f32>().ok()
}

fn try_nvidia() -> Option<(f32, f32)> {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=temperature.gpu,utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let mut parts = text.trim().splitn(2, ',');
    let temp: f32 = parts.next()?.trim().parse().ok()?;
    let usage: f32 = parts.next()?.trim().parse().ok()?;
    Some((temp, usage))
}

fn read_gpu_temp() -> Option<f32> {
    try_amd_temp().or_else(|| try_nvidia().map(|(t, _)| t))
}

fn read_gpu_usage() -> Option<f32> {
    try_amd_usage().or_else(|| try_nvidia().map(|(_, u)| u))
}
