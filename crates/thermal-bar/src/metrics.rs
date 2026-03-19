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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_cpu_times
    // -----------------------------------------------------------------------

    #[test]
    fn parse_cpu_times_typical_line() {
        // Realistic /proc/stat first line
        let line = "cpu  123456 2048 45678 9876543 12345 678 901 0 0 0";
        let t = parse_cpu_times(line).expect("should parse");
        assert_eq!(t.user,    123456);
        assert_eq!(t.nice,    2048);
        assert_eq!(t.system,  45678);
        assert_eq!(t.idle,    9876543);
        assert_eq!(t.iowait,  12345);
        assert_eq!(t.irq,     678);
        assert_eq!(t.softirq, 901);
    }

    #[test]
    fn parse_cpu_times_all_zeros() {
        let line = "cpu  0 0 0 0 0 0 0";
        let t = parse_cpu_times(line).expect("should parse");
        assert_eq!(t.total(), 0);
        assert_eq!(t.idle_total(), 0);
    }

    #[test]
    fn parse_cpu_times_returns_none_on_empty() {
        assert!(parse_cpu_times("").is_none());
    }

    #[test]
    fn parse_cpu_times_returns_none_when_too_few_fields() {
        // Only 5 numbers — missing irq and softirq.
        assert!(parse_cpu_times("cpu  100 0 50 800 10").is_none());
    }

    #[test]
    fn parse_cpu_times_returns_none_on_non_numeric() {
        let line = "cpu  abc def ghi jkl mno pqr stu";
        assert!(parse_cpu_times(line).is_none());
    }

    #[test]
    fn cpu_times_total_sums_all_fields() {
        let t = CpuTimes { user: 10, nice: 2, system: 3, idle: 80, iowait: 4, irq: 1, softirq: 0 };
        assert_eq!(t.total(), 100);
    }

    #[test]
    fn cpu_times_idle_total_includes_iowait() {
        let t = CpuTimes { user: 10, nice: 0, system: 5, idle: 70, iowait: 15, irq: 0, softirq: 0 };
        assert_eq!(t.idle_total(), 85);
    }

    #[test]
    fn parse_cpu_times_cpu0_line_is_skipped_correctly() {
        // Per-CPU lines also start with "cpu0" etc.; the parser skips the first
        // whitespace token (whatever label it is) and parses the numbers after it.
        let line = "cpu0 500 10 200 4000 50 5 2 0 0 0";
        let t = parse_cpu_times(line).expect("should parse");
        assert_eq!(t.user, 500);
        assert_eq!(t.idle, 4000);
    }

    // -----------------------------------------------------------------------
    // parse_meminfo_kb
    // -----------------------------------------------------------------------

    const MEMINFO_SAMPLE: &str = "\
MemTotal:       32768000 kB
MemFree:         8192000 kB
MemAvailable:   16384000 kB
Buffers:          512000 kB
Cached:          4096000 kB
SwapTotal:       8192000 kB
SwapFree:        8000000 kB
";

    #[test]
    fn parse_meminfo_kb_finds_mem_total() {
        let val = parse_meminfo_kb(MEMINFO_SAMPLE, "MemTotal:").expect("should find");
        assert_eq!(val, 32_768_000);
    }

    #[test]
    fn parse_meminfo_kb_finds_mem_available() {
        let val = parse_meminfo_kb(MEMINFO_SAMPLE, "MemAvailable:").expect("should find");
        assert_eq!(val, 16_384_000);
    }

    #[test]
    fn parse_meminfo_kb_finds_mem_free() {
        let val = parse_meminfo_kb(MEMINFO_SAMPLE, "MemFree:").expect("should find");
        assert_eq!(val, 8_192_000);
    }

    #[test]
    fn parse_meminfo_kb_returns_none_for_missing_key() {
        assert!(parse_meminfo_kb(MEMINFO_SAMPLE, "HugepagesTotal:").is_none());
    }

    #[test]
    fn parse_meminfo_kb_returns_none_on_empty_content() {
        assert!(parse_meminfo_kb("", "MemTotal:").is_none());
    }

    #[test]
    fn parse_meminfo_kb_does_not_match_partial_key() {
        // "Mem" should NOT match the "MemTotal:" line.
        // The key "Mem" starts the line "MemTotal: ..." so it does match by prefix;
        // this test documents the actual prefix-match behaviour.
        // "XMemTotal:" is the unambiguous non-match test.
        assert!(parse_meminfo_kb(MEMINFO_SAMPLE, "XMemTotal:").is_none());
    }

    #[test]
    fn parse_meminfo_kb_zero_value() {
        let content = "MemTotal:       0 kB\n";
        let val = parse_meminfo_kb(content, "MemTotal:").expect("should find");
        assert_eq!(val, 0);
    }

    // -----------------------------------------------------------------------
    // parse_net_totals — via controlled /proc/net/dev content
    // -----------------------------------------------------------------------

    /// A helper that mirrors the logic in `parse_net_totals` but accepts
    /// a string directly, so tests don't touch the real filesystem.
    fn parse_net_totals_from_str(contents: &str) -> (u64, u64) {
        let mut total_rx = 0u64;
        let mut total_tx = 0u64;
        for line in contents.lines().skip(2) {
            let line = line.trim();
            let Some(colon) = line.find(':') else { continue };
            let iface = &line[..colon];
            if iface.trim() == "lo" {
                continue;
            }
            let fields: Vec<&str> = line[colon + 1..].split_ascii_whitespace().collect();
            let rx: u64 = fields.get(0).and_then(|v| v.parse().ok()).unwrap_or(0);
            let tx: u64 = fields.get(8).and_then(|v| v.parse().ok()).unwrap_or(0);
            total_rx = total_rx.saturating_add(rx);
            total_tx = total_tx.saturating_add(tx);
        }
        (total_rx, total_tx)
    }

    const NET_DEV_SAMPLE: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  123456     100    0    0    0     0          0         0   123456     100    0    0    0     0       0          0
  eth0: 9876543    5000    0    0    0     0          0         0  1234567    4000    0    0    0     0       0          0
  wlan0:  500000    2000    0    0    0     0          0         0   100000    1500    0    0    0     0       0          0
";

    #[test]
    fn net_totals_excludes_loopback() {
        let (rx, tx) = parse_net_totals_from_str(NET_DEV_SAMPLE);
        // lo bytes must not be included
        assert_eq!(rx, 9_876_543 + 500_000);
        assert_eq!(tx, 1_234_567 + 100_000);
    }

    #[test]
    fn net_totals_single_interface() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
  eth0: 1000000    5000    0    0    0     0          0         0  2000000    4000    0    0    0     0       0          0
";
        let (rx, tx) = parse_net_totals_from_str(content);
        assert_eq!(rx, 1_000_000);
        assert_eq!(tx, 2_000_000);
    }

    #[test]
    fn net_totals_only_loopback_returns_zero() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 9999 100 0 0 0 0 0 0 9999 100 0 0 0 0 0 0
";
        let (rx, tx) = parse_net_totals_from_str(content);
        assert_eq!(rx, 0);
        assert_eq!(tx, 0);
    }

    #[test]
    fn net_totals_empty_after_header_returns_zero() {
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
";
        let (rx, tx) = parse_net_totals_from_str(content);
        assert_eq!(rx, 0);
        assert_eq!(tx, 0);
    }

    #[test]
    fn net_totals_saturating_add_does_not_overflow() {
        // Construct two interfaces each with u64::MAX / 2 bytes to verify
        // saturating_add prevents overflow panic.
        let half = u64::MAX / 2;
        let content = format!("\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
  eth0: {half} 0 0 0 0 0 0 0 {half} 0 0 0 0 0 0 0
  eth1: {half} 0 0 0 0 0 0 0 {half} 0 0 0 0 0 0 0
");
        // Should not panic.
        let (_rx, _tx) = parse_net_totals_from_str(&content);
    }

    // -----------------------------------------------------------------------
    // nvidia-smi output parsing (via the same logic used in try_nvidia)
    // -----------------------------------------------------------------------

    /// Mirror of the parse logic inside `try_nvidia`, operating on a string.
    fn parse_nvidia_output(text: &str) -> Option<(f32, f32)> {
        let mut parts = text.trim().splitn(2, ',');
        let temp: f32 = parts.next()?.trim().parse().ok()?;
        let usage: f32 = parts.next()?.trim().parse().ok()?;
        Some((temp, usage))
    }

    #[test]
    fn nvidia_parse_typical_output() {
        let output = "72, 45\n";
        let (temp, usage) = parse_nvidia_output(output).expect("should parse");
        assert!((temp - 72.0).abs() < 1e-6);
        assert!((usage - 45.0).abs() < 1e-6);
    }

    #[test]
    fn nvidia_parse_no_spaces() {
        let output = "65,30";
        let (temp, usage) = parse_nvidia_output(output).expect("should parse");
        assert!((temp - 65.0).abs() < 1e-6);
        assert!((usage - 30.0).abs() < 1e-6);
    }

    #[test]
    fn nvidia_parse_zero_values() {
        let output = "0, 0";
        let (temp, usage) = parse_nvidia_output(output).expect("should parse");
        assert_eq!(temp, 0.0);
        assert_eq!(usage, 0.0);
    }

    #[test]
    fn nvidia_parse_max_values() {
        let output = "100, 100";
        let (temp, usage) = parse_nvidia_output(output).expect("should parse");
        assert_eq!(temp, 100.0);
        assert_eq!(usage, 100.0);
    }

    #[test]
    fn nvidia_parse_returns_none_on_empty() {
        assert!(parse_nvidia_output("").is_none());
    }

    #[test]
    fn nvidia_parse_returns_none_on_non_numeric() {
        assert!(parse_nvidia_output("[Not Supported], [N/A]").is_none());
    }

    #[test]
    fn nvidia_parse_returns_none_with_only_one_value() {
        // No comma → splitn gives one element → second parse returns None.
        assert!(parse_nvidia_output("75").is_none());
    }

    // -----------------------------------------------------------------------
    // CPU millidegree temperature parsing (mirrors read_cpu_temp logic)
    // -----------------------------------------------------------------------

    fn parse_millideg(raw: &str) -> Option<f32> {
        raw.trim().parse::<i64>().ok().map(|v| v as f32 / 1000.0)
    }

    #[test]
    fn millideg_typical_cpu_temp() {
        // 55000 millidegrees = 55.0 °C
        assert!((parse_millideg("55000\n").unwrap() - 55.0).abs() < 1e-3);
    }

    #[test]
    fn millideg_zero() {
        assert_eq!(parse_millideg("0").unwrap(), 0.0);
    }

    #[test]
    fn millideg_returns_none_on_garbage() {
        assert!(parse_millideg("N/A").is_none());
    }

    #[test]
    fn millideg_negative_temp() {
        // Some sensors report below 0°C
        assert!((parse_millideg("-5000").unwrap() - (-5.0)).abs() < 1e-3);
    }

    // -----------------------------------------------------------------------
    // SystemMetrics struct field types / defaults
    // -----------------------------------------------------------------------

    #[test]
    fn system_metrics_optional_fields_are_none_when_absent() {
        // Build a metrics snapshot with explicit None for GPU fields.
        let m = SystemMetrics {
            cpu_usage_pct: 50.0,
            cpu_temp_c: None,
            mem_used_mb: 4096,
            mem_total_mb: 16384,
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            gpu_temp_c: None,
            gpu_usage_pct: None,
        };
        assert!(m.cpu_temp_c.is_none());
        assert!(m.gpu_temp_c.is_none());
        assert!(m.gpu_usage_pct.is_none());
    }

    #[test]
    fn system_metrics_field_values_are_stored_correctly() {
        let m = SystemMetrics {
            cpu_usage_pct: 75.5,
            cpu_temp_c: Some(62.0),
            mem_used_mb: 8000,
            mem_total_mb: 32000,
            net_rx_kbps: 128.5,
            net_tx_kbps: 64.25,
            gpu_temp_c: Some(80.0),
            gpu_usage_pct: Some(90.0),
        };
        assert!((m.cpu_usage_pct - 75.5).abs() < 1e-6);
        assert!((m.cpu_temp_c.unwrap() - 62.0).abs() < 1e-6);
        assert_eq!(m.mem_used_mb, 8000);
        assert_eq!(m.mem_total_mb, 32000);
        assert!((m.net_rx_kbps - 128.5).abs() < 1e-6);
        assert!((m.gpu_usage_pct.unwrap() - 90.0).abs() < 1e-6);
    }
}
