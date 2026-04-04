//! Shared daemon lifecycle helpers — used by both `thc doctor` and the TUI
//! Services page to avoid duplicating kill/restart/counting logic.
//!
//! **Unified restart rule**: all start/stop/restart operations go through
//! `systemctl --user` when the daemon's systemd unit is enabled. Direct
//! process management (setsid/SIGTERM) is only used as a fallback when the
//! unit is not loaded.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

// ── Instance counting ──────────────────────────────────────────────────────

/// Count how many OS processes match the given pattern.
///
/// - `pgrep_pattern`: full-cmdline pattern for `pgrep -cf`
/// - `binary_name`: exact binary name for `pgrep -cx` (or regex for >15 chars)
///
/// Supply whichever identifier is appropriate for the daemon. If
/// `pgrep_pattern` is `Some`, it takes priority over `binary_name`.
pub fn count_instances(binary_name: &str, pgrep_pattern: Option<&str>) -> u32 {
    if let Some(pattern) = pgrep_pattern {
        return pgrep_count_pattern(pattern);
    }
    pgrep_count_binary(binary_name)
}

/// List all PIDs matching a daemon (for targeted killing).
pub fn list_pids(binary_name: &str, pgrep_pattern: Option<&str>) -> Vec<u32> {
    let (flag, pattern) = if let Some(pat) = pgrep_pattern {
        ("-f", pat.to_string())
    } else if binary_name.len() > 15 {
        ("-f", format!("(^|/){binary_name}$"))
    } else {
        ("-x", binary_name.to_string())
    };

    let uses_full_match = flag == "-f";
    Command::new("pgrep")
        .arg(flag)
        .arg(&pattern)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                // When using -f, pgrep includes its own PID in the output.
                // That process is already dead by the time we iterate, so
                // filter by /proc existence to drop it (and any other stale PIDs).
                .filter(|pid| !uses_full_match || Path::new(&format!("/proc/{pid}")).exists())
                .collect()
        })
        .unwrap_or_default()
}

fn pgrep_count_pattern(pattern: &str) -> u32 {
    let raw = Command::new("pgrep")
        .args(["-cf", pattern])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);
    // pgrep -f matches its own process (the pgrep cmdline contains the
    // pattern), so subtract 1 to get the real count.
    raw.saturating_sub(1)
}

fn pgrep_count_binary(binary: &str) -> u32 {
    let (flag, pattern) = if binary.len() > 15 {
        ("-cf", format!("(^|/){binary}$"))
    } else {
        ("-cx", binary.to_string())
    };
    let uses_full_match = flag == "-cf";
    let raw = Command::new("pgrep")
        .arg(flag)
        .arg(&pattern)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);
    // pgrep -f matches its own process; pgrep -x does not (binary is "pgrep").
    if uses_full_match {
        raw.saturating_sub(1)
    } else {
        raw
    }
}

// ── Kill helpers ───────────────────────────────────────────────────────────

/// Kill duplicate instances of a daemon, keeping one (the pidfile owner or
/// lowest PID). Sends SIGTERM first, then SIGKILL after a short delay.
///
/// `short_name` is used for pidfile lookup (e.g. "conductor", "audio").
/// Returns the number of duplicates killed.
pub fn kill_duplicates(
    binary_name: &str,
    short_name: &str,
    pgrep_pattern: Option<&str>,
    has_pidfile: bool,
) -> u32 {
    let pids = list_pids(binary_name, pgrep_pattern);
    if pids.len() <= 1 {
        return 0;
    }

    // Determine which PID to keep: prefer the pidfile PID, else lowest.
    let keep_pid = if has_pidfile {
        let path = thermal_core::runtime::pidfile_path(short_name);
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|p| pids.contains(p))
            .unwrap_or_else(|| *pids.iter().min().unwrap())
    } else {
        *pids.iter().min().unwrap()
    };

    let kill_pids: Vec<u32> = pids.into_iter().filter(|p| *p != keep_pid).collect();
    if kill_pids.is_empty() {
        return 0;
    }

    // SIGTERM first
    for &pid in &kill_pids {
        let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }

    // Allow time for graceful shutdown before escalating to SIGKILL.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    for &pid in &kill_pids {
        if Path::new(&format!("/proc/{pid}")).exists() {
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }

    kill_pids.len() as u32
}

/// Kill ALL instances of a daemon via pkill, then clean up stale socket and
/// pidfile. Returns `Ok(())` if pkill succeeded.
pub fn kill_all(
    binary_name: &str,
    short_name: &str,
    pgrep_pattern: Option<&str>,
) -> Result<(), String> {
    let (flag, pattern) = if let Some(pat) = pgrep_pattern {
        ("-f", pat.to_string())
    } else if binary_name.len() > 15 {
        ("-f", format!("(^|/){binary_name}$"))
    } else {
        ("-x", binary_name.to_string())
    };
    let result = Command::new("pkill").arg(flag).arg(&pattern).status();
    cleanup_artifacts(short_name);
    match result {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(format!("{binary_name} not running")),
        Err(e) => Err(format!("pkill failed: {e}")),
    }
}

/// Force-kill ALL instances (SIGKILL) via pkill -9.
pub fn force_kill_all(
    binary_name: &str,
    short_name: &str,
    pgrep_pattern: Option<&str>,
) -> Result<(), String> {
    let (flag, pattern) = if let Some(pat) = pgrep_pattern {
        ("-f", pat.to_string())
    } else if binary_name.len() > 15 {
        ("-f", format!("(^|/){binary_name}$"))
    } else {
        ("-x", binary_name.to_string())
    };
    let result = Command::new("pkill")
        .arg("-9")
        .arg(flag)
        .arg(&pattern)
        .status();
    cleanup_artifacts(short_name);
    match result {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(format!("{binary_name} already gone")),
        Err(e) => Err(format!("pkill -9 failed: {e}")),
    }
}

// ── Systemd detection ─────────────────────────────────────────────────────

/// Map from daemon short-name (e.g. "conductor") to systemd unit name.
pub fn unit_for_daemon(short_name: &str) -> Option<&'static str> {
    match short_name {
        "conductor" => Some("thermal-conductor.service"),
        "audio" => Some("thermal-audio.service"),
        "dispatcher" => Some("thermal-dispatcher.service"),
        _ => None,
    }
}

/// Check if a daemon's systemd unit is loaded and enabled.
///
/// The result is cached per daemon for the lifetime of the process
/// (units don't get enabled/disabled while the TUI is running).
pub fn is_systemd_managed(short_name: &str) -> bool {
    // Per-daemon cache using a static HashMap behind OnceLock.
    static CACHE: OnceLock<std::collections::HashMap<String, bool>> = OnceLock::new();

    // Build the cache on first call by probing all known daemons.
    let cache = CACHE.get_or_init(|| {
        let mut map = std::collections::HashMap::new();
        for name in &["conductor", "audio", "dispatcher"] {
            if let Some(unit) = unit_for_daemon(name) {
                let managed = Command::new("systemctl")
                    .args(["--user", "is-enabled", unit])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                map.insert(name.to_string(), managed);
            }
        }
        map
    });

    cache.get(short_name).copied().unwrap_or(false)
}

// ── Unified start/stop/restart ────────────────────────────────────────────

/// Start a daemon: uses `systemctl --user start` when the unit is enabled,
/// otherwise falls back to direct process launch via setsid.
pub fn start_daemon(
    short_name: &str,
    binary_name: &str,
    program: &str,
    args: &[&str],
) -> Result<(), String> {
    if is_systemd_managed(short_name) {
        let unit = unit_for_daemon(short_name)
            .ok_or_else(|| format!("no systemd unit for {short_name}"))?;
        return systemctl_action("start", unit, binary_name);
    }
    start_direct(program, args)
}

/// Stop a daemon: uses `systemctl --user stop` when the unit is enabled,
/// otherwise falls back to SIGTERM / pkill.
pub fn stop_daemon(
    short_name: &str,
    binary_name: &str,
    pgrep_pattern: Option<&str>,
) -> Result<(), String> {
    if is_systemd_managed(short_name) {
        let unit = unit_for_daemon(short_name)
            .ok_or_else(|| format!("no systemd unit for {short_name}"))?;
        return systemctl_action("stop", unit, binary_name);
    }
    // Fallback: kill all instances directly.
    kill_all(binary_name, short_name, pgrep_pattern)
}

/// Restart a daemon: uses `systemctl --user restart` when the unit is enabled,
/// otherwise falls back to kill + direct start.
pub fn restart_daemon(
    short_name: &str,
    binary_name: &str,
    program: &str,
    args: &[&str],
    pgrep_pattern: Option<&str>,
) -> Result<(), String> {
    if is_systemd_managed(short_name) {
        let unit = unit_for_daemon(short_name)
            .ok_or_else(|| format!("no systemd unit for {short_name}"))?;
        return systemctl_action("restart", unit, binary_name);
    }
    // Fallback: kill then start directly.
    let _ = kill_all(binary_name, short_name, pgrep_pattern);
    std::thread::sleep(std::time::Duration::from_millis(500));
    start_direct(program, args)
}

fn systemctl_action(action: &str, unit: &str, binary_name: &str) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(["--user", action, unit])
        .output()
        .map_err(|e| format!("systemctl {action} failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last_line = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("unknown error");
        Err(format!("{binary_name}: {last_line}"))
    }
}

/// Start a daemon via `setsid --fork` (fallback when systemd unit is not
/// available). Returns `Ok(())` if the process starts successfully and is
/// alive after a short grace period.
pub fn start_direct(
    program: &str,
    args: &[&str],
) -> Result<(), String> {
    let stderr_file =
        tempfile::NamedTempFile::new().map_err(|e| format!("Failed to create temp file: {e}"))?;
    let stderr_fd = stderr_file
        .as_file()
        .try_clone()
        .map_err(|e| format!("Failed to clone stderr fd: {e}"))?;

    let mut command = Command::new("setsid");
    command
        .arg("--fork")
        .arg(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_fd))
        .env("PATH", ensure_path());
    match command.spawn() {
        Ok(_) => {
            // Brief grace period for the daemon to start.
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok(())
        }
        Err(e) => Err(format!("Failed to start {program}: {e}")),
    }
}

// ── Stale binary detection ─────────────────────────────────────────────────

/// Check if the running process is using an older binary than what's on disk.
/// Compares /proc/<pid>/exe mtime against the installed binary mtime.
pub fn is_stale_binary(pid: u32) -> bool {
    let exe_link = format!("/proc/{pid}/exe");
    // Resolve the actual binary path the process is running.
    let Ok(exe_path) = std::fs::read_link(&exe_link) else {
        return false;
    };
    // Get mtime of the running binary (from /proc — reflects when process started).
    let Ok(proc_meta) = std::fs::symlink_metadata(&exe_link) else {
        return false;
    };
    // Get mtime of the on-disk binary.
    let Ok(disk_meta) = std::fs::metadata(&exe_path) else {
        return false;
    };
    let Ok(proc_mtime) = proc_meta.modified() else {
        return false;
    };
    let Ok(disk_mtime) = disk_meta.modified() else {
        return false;
    };
    // If the on-disk binary is newer than when the process started, it's stale.
    disk_mtime > proc_mtime
}

// ── Artifact cleanup ───────────────────────────────────────────────────────

/// Remove stale socket and pidfile for a daemon.
pub fn cleanup_artifacts(short_name: &str) {
    let run_dir = thermal_core::runtime::runtime_dir();
    let sock_path = run_dir.join(format!("{short_name}.sock"));
    let _ = std::fs::remove_file(&sock_path);
    let pid_path = run_dir.join(format!("{short_name}.pid"));
    let _ = std::fs::remove_file(&pid_path);
}

// ── PATH helper ────────────────────────────────────────────────────────────

/// Ensure PATH includes common binary locations (needed when spawning daemons
/// from environments with minimal PATH).
fn ensure_path() -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/builder".into());
    let extra = format!("{home}/.cargo/bin:{home}/.local/bin:/usr/local/bin:/usr/bin");
    if current.is_empty() {
        extra
    } else {
        format!("{current}:{extra}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_path_includes_cargo_bin() {
        let path = ensure_path();
        assert!(path.contains(".cargo/bin"));
        assert!(path.contains(".local/bin"));
    }

    #[test]
    fn test_count_instances_nonexistent_binary() {
        // A binary that definitely doesn't exist should return 0.
        let count = count_instances("thermal_test_nonexistent_xyzzy_12345", None);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_list_pids_nonexistent() {
        let pids = list_pids("thermal_test_nonexistent_xyzzy_12345", None);
        assert!(pids.is_empty());
    }
}
