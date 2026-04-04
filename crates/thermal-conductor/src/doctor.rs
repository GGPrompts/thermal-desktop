//! `thc doctor` — daemon health checker, diagnostic report, and `thc smoke` pipeline.

use anyhow::Result;

use crate::daemon_lifecycle;

// ---------------------------------------------------------------------------
// Daemon specifications
// ---------------------------------------------------------------------------

pub(crate) struct DaemonSpec {
    pub name: &'static str,
    /// Short name used with runtime::pidfile_path() / runtime::socket_path().
    pub short_name: &'static str,
    pub has_pidfile: bool,
    pub has_socket: bool,
    pub restart_cmd: Option<&'static [&'static str]>,
    /// Custom pgrep -f pattern for counting instances. Required when the
    /// binary name is ambiguous (e.g. `thc` runs as tui/daemon/window).
    pub pgrep_pattern: Option<&'static str>,
}

pub(crate) static DAEMONS: &[DaemonSpec] = &[
    DaemonSpec {
        name: "thermal-conductor",
        short_name: "conductor",
        has_pidfile: true,
        has_socket: true,
        restart_cmd: Some(&["thc", "daemon"]),
        // `thc` is ambiguous (tui/daemon/window) — match the daemon subcommand.
        // Must use "thc daemon" (not "thermal-conductor daemon") because the
        // process cmdline is "/path/to/thc daemon", not "thermal-conductor daemon".
        pgrep_pattern: Some("thc daemon"),
    },
    DaemonSpec {
        name: "thermal-audio",
        short_name: "audio",
        has_pidfile: true,
        has_socket: true,
        restart_cmd: Some(&["thermal-audio"]),
        pgrep_pattern: None,
    },
    DaemonSpec {
        name: "thermal-dispatcher",
        short_name: "dispatcher",
        has_pidfile: true,
        has_socket: false,
        restart_cmd: Some(&["thermal-dispatcher"]),
        pgrep_pattern: None,
    },
];

/// Stale artifacts from removed daemons that should be cleaned up on `--fix`.
/// Format: (short_name, has_pidfile, has_socket)
static REMOVED_DAEMON_ARTIFACTS: &[(&str, bool, bool)] = &[
    ("bar", true, false),
    ("hud", false, false),
    ("messages", true, true),
    ("voice", true, true),
    ("monitor", false, false),
    ("dispatch-cli", false, false),
];

// ---------------------------------------------------------------------------
// Health check types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DaemonHealth {
    Running,
    Dead,
    NotRunning,
}

/// Result of checking a single daemon.
#[derive(Debug, Clone)]
pub(crate) struct DaemonCheckResult {
    pub name: String,
    pub health: DaemonHealth,
    pub pid: Option<u32>,
    pub pid_status: Option<PidStatus>,
    pub sock_status: Option<SocketStatus>,
    /// How many OS processes match this daemon (0 = not running, >1 = duplicates).
    pub instance_count: u32,
    /// Binary on disk is newer than the running process (needs restart).
    pub stale_binary: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PidStatus {
    Alive,
    Stale,
    Missing,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SocketStatus {
    Connectable,
    Stale,
    Missing,
}

/// Status of a socket file under the runtime directory.
#[derive(Debug, Clone)]
struct SocketFileInfo {
    name: String,
    path: std::path::PathBuf,
    status: SocketStatus,
}

/// State directory info.
#[derive(Debug, Clone)]
struct StateDirectoryInfo {
    path: String,
    agent_type: &'static str,
    file_count: usize,
}

/// Full diagnostic report data.
pub(crate) struct DiagnosticReport {
    timestamp: String,
    runtime_dir: std::path::PathBuf,
    daemon_results: Vec<DaemonCheckResult>,
    socket_files: Vec<SocketFileInfo>,
    backend_mode: String,
    session_info: Option<String>,
    log_locations: Vec<(String, String)>,
    gpu_info: Option<String>,
    state_dirs: Vec<StateDirectoryInfo>,
    suggested_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorExecutionPlan {
    report_filename: Option<String>,
    should_run_fix: bool,
    should_print_fix_hint: bool,
}

impl DiagnosticReport {
    /// Format the report as plain text (no ANSI escape codes).
    fn format_plain(&self) -> String {
        let mut out = String::new();

        out.push_str(&format!("Thermal Doctor Report — {}\n", self.timestamp));
        out.push_str(&"=".repeat(60));
        out.push('\n');

        // Section 1: Daemon status
        out.push_str("\n## Daemon Status\n\n");
        for d in &self.daemon_results {
            let tag = match d.health {
                DaemonHealth::Running => "[OK]",
                DaemonHealth::Dead => "[STALE]",
                DaemonHealth::NotRunning => "[MISSING]",
            };
            let pid_info = match (&d.pid_status, d.pid) {
                (Some(PidStatus::Alive), Some(pid)) => format!("  pid {pid} alive"),
                (Some(PidStatus::Stale), Some(pid)) => format!("  pid {pid} STALE"),
                (Some(PidStatus::Alive), None) => "  alive (pid unknown)".to_string(),
                (Some(PidStatus::Stale), None) => "  STALE (pid unknown)".to_string(),
                (Some(PidStatus::Missing), _) => "  no pidfile".to_string(),
                (None, _) => String::new(),
            };
            let sock_info = match &d.sock_status {
                Some(SocketStatus::Connectable) => "  sock OK",
                Some(SocketStatus::Stale) => "  sock STALE",
                Some(SocketStatus::Missing) => "  sock MISSING",
                None => "",
            };
            let instance_info = if d.instance_count > 1 {
                format!("  ({} instances -- expected 1)", d.instance_count)
            } else {
                String::new()
            };
            let stale_info = if d.stale_binary {
                "  STALE BINARY"
            } else {
                ""
            };
            out.push_str(&format!(
                "  {:<9} {:<24}{}{}{}{}\n",
                tag, d.name, pid_info, sock_info, instance_info, stale_info
            ));
        }

        // Section 2: Socket paths
        out.push_str("\n## Socket Paths\n\n");
        out.push_str(&format!(
            "  Runtime dir: {}\n\n",
            self.runtime_dir.display()
        ));
        if self.socket_files.is_empty() {
            out.push_str("  No socket files found.\n");
        } else {
            for s in &self.socket_files {
                let tag = match s.status {
                    SocketStatus::Connectable => "[OK]",
                    SocketStatus::Stale => "[STALE]",
                    SocketStatus::Missing => "[MISSING]",
                };
                out.push_str(&format!("  {:<9} {}\n", tag, s.path.display()));
            }
        }

        // Section 3: Backend mode
        out.push_str("\n## Backend Mode\n\n");
        out.push_str(&format!("  {}\n", self.backend_mode));

        // Section 4: Session info
        out.push_str("\n## Session Info\n\n");
        if let Some(ref info) = self.session_info {
            out.push_str(&format!("  {info}\n"));
        } else {
            out.push_str("  No daemon connection — session info unavailable.\n");
        }

        // Section 5: Log locations
        out.push_str("\n## Log Locations\n\n");
        for (component, location) in &self.log_locations {
            out.push_str(&format!("  {:<24} {}\n", component, location));
        }

        // Section 6: GPU info
        out.push_str("\n## GPU / Adapter\n\n");
        if let Some(ref info) = self.gpu_info {
            out.push_str(&format!("  {info}\n"));
        } else {
            out.push_str("  GPU adapter info not available (no wgpu instance).\n");
        }

        // Section 7: Compatibility state readers
        out.push_str("\n## Compatibility State Files\n\n");
        if self.state_dirs.is_empty() {
            out.push_str("  No state directories found.\n");
        } else {
            for sd in &self.state_dirs {
                let files = if sd.file_count == 1 { "file" } else { "files" };
                out.push_str(&format!(
                    "  {:<36} {} ({} {})\n",
                    sd.path, sd.agent_type, sd.file_count, files
                ));
            }
        }

        // Suggested actions
        if !self.suggested_actions.is_empty() {
            out.push_str("\n## Suggested Actions\n\n");
            for action in &self.suggested_actions {
                out.push_str(&format!("  - {action}\n"));
            }
        }

        out.push('\n');
        out
    }

    /// Format with ANSI colors for terminal display.
    fn format_colored(&self) -> String {
        let mut out = String::new();

        out.push_str(&format!(
            "\n\x1b[1mThermal Doctor Report\x1b[0m — {}\n",
            self.timestamp
        ));
        out.push_str(&"\x1b[90m─\x1b[0m".repeat(60));
        out.push('\n');

        // Section 1: Daemon status
        out.push_str("\n\x1b[1m## Daemon Status\x1b[0m\n\n");
        for d in &self.daemon_results {
            let (icon, tag_color) = match d.health {
                DaemonHealth::Running => ("\x1b[32m✓\x1b[0m", "\x1b[32m"),
                DaemonHealth::Dead => ("\x1b[31m✗\x1b[0m", "\x1b[31m"),
                DaemonHealth::NotRunning => ("\x1b[90m-\x1b[0m", "\x1b[90m"),
            };
            let tag = match d.health {
                DaemonHealth::Running => "OK",
                DaemonHealth::Dead => "STALE",
                DaemonHealth::NotRunning => "MISSING",
            };
            let pid_info = match (&d.pid_status, d.pid) {
                (Some(PidStatus::Alive), Some(pid)) => format!("  pid {pid}"),
                (Some(PidStatus::Stale), Some(pid)) => {
                    format!("  \x1b[31mpid {pid} stale\x1b[0m")
                }
                (Some(PidStatus::Alive), None) => "  alive (pid unknown)".to_string(),
                (Some(PidStatus::Stale), None) => {
                    "  \x1b[31mstale (pid unknown)\x1b[0m".to_string()
                }
                (Some(PidStatus::Missing), _) => "  no pidfile".to_string(),
                (None, _) => String::new(),
            };
            let sock_info = match &d.sock_status {
                Some(SocketStatus::Connectable) => "  sock \x1b[32m✓\x1b[0m",
                Some(SocketStatus::Stale) => "  sock \x1b[31m✗ stale\x1b[0m",
                Some(SocketStatus::Missing) => "  sock \x1b[90mmissing\x1b[0m",
                None => "",
            };
            let instance_info = if d.instance_count > 1 {
                format!(
                    "  \x1b[33m({} instances — expected 1)\x1b[0m",
                    d.instance_count
                )
            } else {
                String::new()
            };
            let stale_info = if d.stale_binary {
                "  \x1b[33m⚠ stale binary\x1b[0m"
            } else {
                ""
            };
            out.push_str(&format!(
                "  {icon} {tag_color}[{tag}]\x1b[0m {:<24}{}{}{}{}\n",
                d.name, pid_info, sock_info, instance_info, stale_info
            ));
        }

        // Section 2: Socket paths
        out.push_str(&format!(
            "\n\x1b[1m## Socket Paths\x1b[0m  ({})\n\n",
            self.runtime_dir.display()
        ));
        if self.socket_files.is_empty() {
            out.push_str("  \x1b[90mNo socket files found.\x1b[0m\n");
        } else {
            for s in &self.socket_files {
                let (icon, label) = match s.status {
                    SocketStatus::Connectable => ("\x1b[32m✓\x1b[0m", "\x1b[32m[OK]\x1b[0m"),
                    SocketStatus::Stale => ("\x1b[31m✗\x1b[0m", "\x1b[31m[STALE]\x1b[0m"),
                    SocketStatus::Missing => ("\x1b[90m-\x1b[0m", "\x1b[90m[MISSING]\x1b[0m"),
                };
                out.push_str(&format!("  {icon} {label} {}\n", s.name));
            }
        }

        // Section 3: Backend mode
        out.push_str("\n\x1b[1m## Backend Mode\x1b[0m\n\n");
        out.push_str(&format!("  {}\n", self.backend_mode));

        // Section 4: Session info
        out.push_str("\n\x1b[1m## Session Info\x1b[0m\n\n");
        if let Some(ref info) = self.session_info {
            out.push_str(&format!("  {info}\n"));
        } else {
            out.push_str("  \x1b[90mNo daemon connection — session info unavailable.\x1b[0m\n");
        }

        // Section 5: Log locations
        out.push_str("\n\x1b[1m## Log Locations\x1b[0m\n\n");
        for (component, location) in &self.log_locations {
            out.push_str(&format!(
                "  {:<24} \x1b[90m{}\x1b[0m\n",
                component, location
            ));
        }

        // Section 6: GPU info
        out.push_str("\n\x1b[1m## GPU / Adapter\x1b[0m\n\n");
        if let Some(ref info) = self.gpu_info {
            out.push_str(&format!("  {info}\n"));
        } else {
            out.push_str("  \x1b[90mGPU adapter info not available (no wgpu instance).\x1b[0m\n");
        }

        // Section 7: Compatibility state readers
        out.push_str("\n\x1b[1m## Compatibility State Files\x1b[0m\n\n");
        if self.state_dirs.is_empty() {
            out.push_str("  \x1b[90mNo state directories found.\x1b[0m\n");
        } else {
            for sd in &self.state_dirs {
                let files = if sd.file_count == 1 { "file" } else { "files" };
                out.push_str(&format!(
                    "  {:<36} \x1b[36m{}\x1b[0m ({} {})\n",
                    sd.path, sd.agent_type, sd.file_count, files
                ));
            }
        }

        // Suggested actions
        if !self.suggested_actions.is_empty() {
            out.push_str("\n\x1b[1;33m## Suggested Actions\x1b[0m\n\n");
            for action in &self.suggested_actions {
                out.push_str(&format!("  \x1b[33m-\x1b[0m {action}\n"));
            }
        }

        out.push('\n');
        out
    }
}

// ---------------------------------------------------------------------------
// Diagnostic report builder
// ---------------------------------------------------------------------------

async fn build_diagnostic_report() -> DiagnosticReport {
    use thermal_core::runtime;

    let run_dir = runtime::runtime_dir();
    let timestamp = chrono_timestamp();

    // 1. Check each daemon
    let mut daemon_results = Vec::new();
    let mut suggested_actions = Vec::new();

    for spec in DAEMONS {
        let result = check_daemon(spec);
        if result.health == DaemonHealth::Dead {
            if spec.restart_cmd.is_some() {
                suggested_actions.push(format!(
                    "Run `thc doctor --fix` to clean stale files and restart {}",
                    spec.name
                ));
            } else {
                suggested_actions.push(format!(
                    "Restart {} manually (stale artifacts detected)",
                    spec.name
                ));
            }
        }
        if result.instance_count > 1 {
            suggested_actions.push(format!(
                "Run `thc doctor --fix` to kill {} duplicate {} instance(s)",
                result.instance_count - 1,
                spec.name
            ));
        }
        if result.stale_binary {
            suggested_actions.push(format!(
                "Restart {} — binary on disk is newer than the running process",
                spec.name
            ));
        }
        daemon_results.push(result);
    }

    // 2. Enumerate all socket files under runtime_dir
    let socket_files = enumerate_sockets(&run_dir);

    // 3. Backend mode detection
    let conductor_sock = runtime::socket_path("conductor");
    let backend_mode = if conductor_sock.exists() {
        match runtime::try_connect_read_only("conductor", &conductor_sock) {
            Ok(_stream) => "Daemon mode (conductor socket responding)".to_string(),
            Err(msg) => format!("Standalone / no daemon ({msg})"),
        }
    } else {
        "Standalone PTY (no conductor socket)".to_string()
    };

    // 4. Session info — try to get session count from daemon
    let session_info = gather_session_info().await;

    // 5. Log locations
    let log_locations = gather_log_locations(&run_dir);

    // 6. GPU adapter info (best-effort, synchronous probe)
    let gpu_info = probe_gpu_adapter();

    // 7. Compatibility state directories
    let state_dirs = scan_state_directories();

    DiagnosticReport {
        timestamp,
        runtime_dir: run_dir,
        daemon_results,
        socket_files,
        backend_mode,
        session_info,
        log_locations,
        gpu_info,
        state_dirs,
        suggested_actions,
    }
}

fn doctor_execution_plan(
    fix: bool,
    report: bool,
    diagnostic: &DiagnosticReport,
) -> DoctorExecutionPlan {
    let has_problems = diagnostic.daemon_results.iter().any(|d| {
        d.health == DaemonHealth::Dead || d.instance_count > 1
    });

    DoctorExecutionPlan {
        report_filename: report.then(|| {
            format!(
                "/tmp/thermal-doctor-{}.txt",
                diagnostic.timestamp.replace([':', ' ', '-'], "")
            )
        }),
        should_run_fix: fix,
        should_print_fix_hint: has_problems && !fix,
    }
}

// ---------------------------------------------------------------------------
// thc doctor
// ---------------------------------------------------------------------------

pub(crate) async fn cmd_doctor(fix: bool, report: bool) -> Result<()> {
    let diagnostic = build_diagnostic_report().await;
    let plan = doctor_execution_plan(fix, report, &diagnostic);

    if let Some(filename) = &plan.report_filename {
        let plain = diagnostic.format_plain();
        std::fs::write(filename, &plain)?;
    }

    print!("{}", diagnostic.format_colored());

    if let Some(filename) = &plan.report_filename {
        println!("  Report written to \x1b[1m{filename}\x1b[0m\n");
    }

    if plan.should_run_fix {
        let run_dir = thermal_core::runtime::runtime_dir();
        for (i, spec) in DAEMONS.iter().enumerate() {
            let result = &diagnostic.daemon_results[i];
            if result.health == DaemonHealth::Dead {
                fix_daemon(spec, &run_dir).await;
            } else if result.instance_count > 1 {
                // Daemon is healthy but has duplicates — kill extras only.
                kill_duplicate_instances(spec);
            }
        }
        cleanup_removed_daemon_artifacts(&run_dir);
        println!();
    } else if plan.should_print_fix_hint {
        println!(
            "  Run \x1b[1mthc doctor --fix\x1b[0m to clean stale files and restart core daemons.\n"
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// thc smoke
// ---------------------------------------------------------------------------

/// A single step result in the smoke test pipeline.
struct SmokeStepResult {
    name: &'static str,
    passed: bool,
    duration: std::time::Duration,
    detail: Option<String>,
}

pub(crate) async fn cmd_smoke(fix: bool) -> Result<()> {
    println!("\n  \x1b[1;36m▸ thc smoke\x1b[0m — headless verification\n");

    let mut results: Vec<SmokeStepResult> = Vec::new();

    // Step 1: cargo check --workspace
    {
        let start = std::time::Instant::now();
        let output = tokio::process::Command::new("cargo")
            .args(["check", "--workspace"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await;
        let duration = start.elapsed();
        let (passed, detail) = match output {
            Ok(o) if o.status.success() => (true, None),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let last_lines: String = stderr
                    .lines()
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                (false, Some(last_lines))
            }
            Err(e) => (false, Some(format!("failed to run cargo: {e}"))),
        };
        print_step_progress("cargo check", passed);
        results.push(SmokeStepResult {
            name: "cargo check --workspace",
            passed,
            duration,
            detail,
        });
    }

    // Step 2: cargo test --workspace --lib
    {
        let start = std::time::Instant::now();
        let output = tokio::process::Command::new("cargo")
            .args(["test", "--workspace", "--lib"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await;
        let duration = start.elapsed();
        let (passed, detail) = match output {
            Ok(o) if o.status.success() => (true, None),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let stdout = String::from_utf8_lossy(&o.stdout);
                let combined = format!("{stderr}\n{stdout}");
                let last_lines: String = combined
                    .lines()
                    .rev()
                    .take(8)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                (false, Some(last_lines))
            }
            Err(e) => (false, Some(format!("failed to run cargo: {e}"))),
        };
        print_step_progress("cargo test --lib", passed);
        results.push(SmokeStepResult {
            name: "cargo test --workspace --lib",
            passed,
            duration,
            detail,
        });
    }

    // Step 3: doctor health checks
    {
        let start = std::time::Instant::now();
        let diagnostic = build_diagnostic_report().await;
        let duration = start.elapsed();

        let dead_count = diagnostic
            .daemon_results
            .iter()
            .filter(|d| d.health == DaemonHealth::Dead)
            .count();
        let duplicate_count = diagnostic
            .daemon_results
            .iter()
            .filter(|d| d.instance_count > 1)
            .count();

        let passed = dead_count == 0 && duplicate_count == 0;
        let detail = if !passed {
            let mut parts = Vec::new();
            let dead_names: Vec<String> = diagnostic
                .daemon_results
                .iter()
                .filter(|d| d.health == DaemonHealth::Dead)
                .map(|d| d.name.clone())
                .collect();
            if !dead_names.is_empty() {
                parts.push(format!("dead daemons: {}", dead_names.join(", ")));
            }
            let dup_names: Vec<String> = diagnostic
                .daemon_results
                .iter()
                .filter(|d| d.instance_count > 1)
                .map(|d| format!("{} ({})", d.name, d.instance_count))
                .collect();
            if !dup_names.is_empty() {
                parts.push(format!("duplicates: {}", dup_names.join(", ")));
            }
            Some(parts.join("; "))
        } else {
            None
        };

        print_step_progress("daemon health", passed);
        results.push(SmokeStepResult {
            name: "daemon health (doctor)",
            passed,
            duration,
            detail,
        });

        // If --fix and there are problems, run fix logic
        if fix && (dead_count > 0 || duplicate_count > 0) {
            println!("    \x1b[33m→ fixing daemon issues...\x1b[0m");
            let run_dir = thermal_core::runtime::runtime_dir();
            for (i, spec) in DAEMONS.iter().enumerate() {
                let result = &diagnostic.daemon_results[i];
                if result.health == DaemonHealth::Dead {
                    fix_daemon(spec, &run_dir).await;
                } else if result.instance_count > 1 {
                    kill_duplicate_instances(spec);
                }
            }
            cleanup_removed_daemon_artifacts(&run_dir);
        }
    }

    // Summary table
    println!();
    println!(
        "  \x1b[1m{:<32} {:>6}  {:>8}\x1b[0m",
        "Step", "Result", "Duration"
    );
    println!("  {}", "─".repeat(50));

    let mut all_passed = true;
    for r in &results {
        let status = if r.passed {
            "\x1b[32m PASS \x1b[0m"
        } else {
            all_passed = false;
            "\x1b[31m FAIL \x1b[0m"
        };
        let dur = format_duration(r.duration);
        println!("  {:<32} {}  {:>8}", r.name, status, dur);
        if let Some(ref detail) = r.detail {
            for line in detail.lines().take(4) {
                println!("    \x1b[90m{line}\x1b[0m");
            }
        }
    }

    println!();
    if all_passed {
        println!("  \x1b[32;1m✓ All checks passed.\x1b[0m\n");
        Ok(())
    } else {
        println!("  \x1b[31;1m✗ Some checks failed.\x1b[0m\n");
        std::process::exit(1);
    }
}

/// Print a live progress indicator for a smoke step.
fn print_step_progress(name: &str, passed: bool) {
    let icon = if passed {
        "\x1b[32m✓\x1b[0m"
    } else {
        "\x1b[31m✗\x1b[0m"
    };
    println!("  {icon} {name}");
}

/// Format a duration as human-readable (e.g., "1.2s", "340ms").
fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

// ---------------------------------------------------------------------------
// Daemon check helpers
// ---------------------------------------------------------------------------

/// Count how many OS processes match this daemon.
///
/// Delegates to `daemon_lifecycle::count_instances()`.
fn count_daemon_instances(spec: &DaemonSpec) -> u32 {
    daemon_lifecycle::count_instances(spec.name, spec.pgrep_pattern)
}

/// List all PIDs matching a daemon spec (for targeted killing).
///
/// Delegates to `daemon_lifecycle::list_pids()`.
fn list_daemon_pids(spec: &DaemonSpec) -> Vec<u32> {
    daemon_lifecycle::list_pids(spec.name, spec.pgrep_pattern)
}

/// Kill duplicate instances of a daemon, keeping the one that owns the
/// pidfile (or the lowest PID as a fallback). Sends SIGTERM first, then
/// SIGKILL after a short delay.
fn kill_duplicate_instances(spec: &DaemonSpec) {
    let killed = daemon_lifecycle::kill_duplicates(
        spec.name,
        spec.short_name,
        spec.pgrep_pattern,
        spec.has_pidfile,
    );
    if killed > 0 {
        println!(
            "    \x1b[33m→ killed {killed} duplicate {} instance(s)\x1b[0m",
            spec.name
        );
    }
}

/// Check a single daemon using thermal_core::runtime helpers.
fn check_daemon(spec: &DaemonSpec) -> DaemonCheckResult {
    use thermal_core::runtime;

    let mut pid_status = None;
    let mut pid_val: Option<u32> = None;
    let mut sock_status = None;

    if spec.has_pidfile {
        let path = runtime::pidfile_path(spec.short_name);
        if !path.exists() {
            pid_status = Some(PidStatus::Missing);
        } else {
            // Read pid manually (validate_pidfile removes stale files, which we
            // don't want during a read-only diagnostic).
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    pid_val = Some(pid);
                    if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                        pid_status = Some(PidStatus::Alive);
                    } else {
                        pid_status = Some(PidStatus::Stale);
                    }
                } else {
                    pid_status = Some(PidStatus::Stale);
                }
            } else {
                pid_status = Some(PidStatus::Missing);
            }
        }
    }

    if spec.has_socket {
        let path = runtime::socket_path(spec.short_name);
        if !path.exists() {
            sock_status = Some(SocketStatus::Missing);
        } else {
            // Non-blocking sync check
            use std::os::unix::net::UnixStream;
            match UnixStream::connect(&path) {
                Ok(_) => sock_status = Some(SocketStatus::Connectable),
                Err(_) => sock_status = Some(SocketStatus::Stale),
            }
        }
    }

    let instance_count = count_daemon_instances(spec);

    let health = match (&pid_status, &sock_status) {
        (Some(PidStatus::Alive), _) => DaemonHealth::Running,
        (_, Some(SocketStatus::Connectable)) => DaemonHealth::Running,
        (Some(PidStatus::Stale), _) => DaemonHealth::Dead,
        (_, Some(SocketStatus::Stale)) => DaemonHealth::Dead,
        _ => DaemonHealth::NotRunning,
    };

    // Stale binary check: if daemon is running, compare /proc/<pid>/exe
    // mtime against the installed binary on disk.
    let stale_binary = pid_val
        .filter(|_| health == DaemonHealth::Running)
        .map_or(false, |pid| daemon_lifecycle::is_stale_binary(pid));

    DaemonCheckResult {
        name: spec.name.to_string(),
        health,
        pid: pid_val,
        pid_status,
        sock_status,
        instance_count,
        stale_binary,
    }
}

/// Enumerate all .sock files under the runtime directory and check their status.
fn enumerate_sockets(run_dir: &std::path::Path) -> Vec<SocketFileInfo> {
    let mut sockets = Vec::new();

    // Check expected sockets from DAEMONS
    for spec in DAEMONS {
        if spec.has_socket {
            let path = run_dir.join(format!("{}.sock", spec.short_name));
            let status = if !path.exists() {
                SocketStatus::Missing
            } else {
                use std::os::unix::net::UnixStream;
                match UnixStream::connect(&path) {
                    Ok(_) => SocketStatus::Connectable,
                    Err(_) => SocketStatus::Stale,
                }
            };
            sockets.push(SocketFileInfo {
                name: format!("{}.sock", spec.short_name),
                path: path.clone(),
                status,
            });
        }
    }

    // Also scan for any unexpected .sock files
    if let Ok(entries) = std::fs::read_dir(run_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("sock") {
                let fname = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                // Skip if already listed
                if sockets.iter().any(|s| s.name == fname) {
                    continue;
                }
                let status = {
                    use std::os::unix::net::UnixStream;
                    match UnixStream::connect(&p) {
                        Ok(_) => SocketStatus::Connectable,
                        Err(_) => SocketStatus::Stale,
                    }
                };
                sockets.push(SocketFileInfo {
                    name: fname,
                    path: p,
                    status,
                });
            }
        }
    }

    sockets
}

/// Try connecting to the daemon and getting session count.
async fn gather_session_info() -> Option<String> {
    use crate::client::DaemonClient;

    let mut client = DaemonClient::connect().await.ok()??;
    let sessions = client.list_sessions().await.ok()?;

    let total = sessions.len();
    let alive = sessions.iter().filter(|s| s.is_alive).count();

    Some(format!(
        "{total} session{} ({alive} alive)",
        if total == 1 { "" } else { "s" }
    ))
}

/// Gather well-known log file locations.
fn gather_log_locations(run_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut locs = Vec::new();

    // Conductor TUI log
    let tui_log = run_dir.join("conductor-tui.log");
    locs.push((
        "conductor (TUI)".to_string(),
        if tui_log.exists() {
            tui_log.display().to_string()
        } else {
            format!("{} (not found)", tui_log.display())
        },
    ));

    // General pattern: RUST_LOG=debug <binary> 2>file.log
    locs.push((
        "all daemons".to_string(),
        "RUST_LOG=debug <daemon> 2>/path/to/file.log".to_string(),
    ));

    locs
}

/// Best-effort wgpu adapter probe.
fn probe_gpu_adapter() -> Option<String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))?;

    let info = adapter.get_info();
    Some(format!(
        "{} ({:?}, {:?})",
        info.name, info.backend, info.device_type,
    ))
}

/// Scan /tmp/*-state/ directories for compatibility state files.
fn scan_state_directories() -> Vec<StateDirectoryInfo> {
    let dirs: &[(&str, &str)] = &[
        ("/tmp/claude-code-state", "claude-code"),
        ("/tmp/codex-state", "codex"),
        ("/tmp/copilot-state", "copilot"),
    ];

    let mut results = Vec::new();

    for (path, agent_type) in dirs {
        let p = std::path::Path::new(path);
        if p.exists() {
            let file_count = std::fs::read_dir(p)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .filter(|e| {
                            e.path().extension().and_then(|ext| ext.to_str()) == Some("json")
                        })
                        .count()
                })
                .unwrap_or(0);
            results.push(StateDirectoryInfo {
                path: path.to_string(),
                agent_type,
                file_count,
            });
        }
    }

    results
}

/// Produce a compact timestamp string.
fn chrono_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple UTC timestamp: YYYY-MM-DD HH:MM:SS
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;
    // Approximate date from epoch days (good enough for a diagnostic timestamp)
    let (year, month, day) = epoch_days_to_date(days);
    format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02} UTC")
}

/// Convert days since Unix epoch to (year, month, day).
fn epoch_days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's civil_from_days
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

async fn fix_daemon(spec: &DaemonSpec, _run_dir: &std::path::Path) {
    kill_duplicate_instances(spec);

    // Clean stale artifacts (pidfile + socket)
    daemon_lifecycle::cleanup_artifacts(spec.short_name);
    println!("    \x1b[90mcleaned stale artifacts for {}\x1b[0m", spec.short_name);

    // Restart if this is a core service daemon
    if let Some(cmd) = spec.restart_cmd {
        let program = cmd[0];
        let args = &cmd[1..];
        match tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => {
                println!("    \x1b[32m↻ restarted {}\x1b[0m", spec.name);
            }
            Err(e) => {
                eprintln!("    \x1b[31m! failed to restart {}: {e}\x1b[0m", spec.name);
            }
        }
    }
}

/// Remove stale pidfiles and sockets left behind by removed daemons.
///
/// Daemons that no longer exist (thermal-bar, thermal-hud, thermal-messages,
/// thermal-voice, thermal-notify, thermal-wallpaper) may have left runtime
/// artifacts. This function silently removes them on `--fix`.
fn cleanup_removed_daemon_artifacts(run_dir: &std::path::Path) {
    for (short_name, has_pidfile, has_socket) in REMOVED_DAEMON_ARTIFACTS {
        if *has_pidfile {
            let path = run_dir.join(format!("{short_name}.pid"));
            if path.exists() {
                match std::fs::remove_file(&path) {
                    Ok(()) => println!(
                        "    \x1b[90mcleaned stale artifact: {short_name}.pid (removed daemon)\x1b[0m"
                    ),
                    Err(e) => eprintln!(
                        "    \x1b[33m! could not remove {}: {e}\x1b[0m",
                        path.display()
                    ),
                }
            }
        }
        if *has_socket {
            let path = run_dir.join(format!("{short_name}.sock"));
            if path.exists() {
                match std::fs::remove_file(&path) {
                    Ok(()) => println!(
                        "    \x1b[90mcleaned stale artifact: {short_name}.sock (removed daemon)\x1b[0m"
                    ),
                    Err(e) => eprintln!(
                        "    \x1b[33m! could not remove {}: {e}\x1b[0m",
                        path.display()
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_epoch_days_to_date() {
        // 2024-01-01 = 19723 days since epoch
        let (y, m, d) = epoch_days_to_date(19723);
        assert_eq!((y, m, d), (2024, 1, 1));

        // 1970-01-01 = day 0
        let (y, m, d) = epoch_days_to_date(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn test_chrono_timestamp_format() {
        let ts = chrono_timestamp();
        // Should match YYYY-MM-DD HH:MM:SS UTC
        assert!(ts.ends_with(" UTC"), "timestamp should end with UTC: {ts}");
        assert!(ts.len() >= 20, "timestamp too short: {ts}");
    }

    #[test]
    fn test_report_format_plain_has_all_sections() {
        let report = DiagnosticReport {
            timestamp: "2026-04-01 00:00:00 UTC".to_string(),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/thermal"),
            daemon_results: vec![
                DaemonCheckResult {
                    name: "thermal-audio".to_string(),
                    health: DaemonHealth::Dead,
                    pid: Some(9999),
                    pid_status: Some(PidStatus::Stale),
                    sock_status: Some(SocketStatus::Stale),
                    instance_count: 1,
                    stale_binary: false,
                },
                DaemonCheckResult {
                    name: "thermal-dispatcher".to_string(),
                    health: DaemonHealth::NotRunning,
                    pid: None,
                    pid_status: Some(PidStatus::Missing),
                    sock_status: None,
                    instance_count: 0,
                    stale_binary: false,
                },
            ],
            socket_files: vec![SocketFileInfo {
                name: "conductor.sock".to_string(),
                path: std::path::PathBuf::from("/run/user/1000/thermal/conductor.sock"),
                status: SocketStatus::Connectable,
            }],
            backend_mode: "Standalone PTY (no conductor socket)".to_string(),
            session_info: None,
            log_locations: vec![("conductor (TUI)".to_string(), "/tmp/log".to_string())],
            gpu_info: Some("Test GPU (Vulkan, DiscreteGpu)".to_string()),
            state_dirs: vec![StateDirectoryInfo {
                path: "/tmp/claude-code-state".to_string(),
                agent_type: "claude-code",
                file_count: 2,
            }],
            suggested_actions: vec![
                "Run `thc doctor --fix` to clean stale files and restart thermal-audio".to_string(),
            ],
        };

        let plain = report.format_plain();

        assert!(
            plain.contains("## Daemon Status"),
            "missing Daemon Status section"
        );
        assert!(plain.contains("[OK]"), "missing [OK] tag");
        assert!(plain.contains("[STALE]"), "missing [STALE] tag");
        assert!(plain.contains("[MISSING]"), "missing [MISSING] tag");
        assert!(
            plain.contains("## Socket Paths"),
            "missing Socket Paths section"
        );
        assert!(
            plain.contains("## Backend Mode"),
            "missing Backend Mode section"
        );
        assert!(
            plain.contains("## Session Info"),
            "missing Session Info section"
        );
        assert!(
            plain.contains("## Log Locations"),
            "missing Log Locations section"
        );
        assert!(plain.contains("## GPU / Adapter"), "missing GPU section");
        assert!(
            plain.contains("## Compatibility State Files"),
            "missing state files section"
        );
        assert!(
            plain.contains("## Suggested Actions"),
            "missing suggested actions"
        );
        assert!(plain.contains("claude-code"), "missing agent type");
        assert!(plain.contains("2 files"), "missing file count");
    }

    #[test]
    fn test_report_format_colored_has_ansi() {
        let report = DiagnosticReport {
            timestamp: "2026-04-01 00:00:00 UTC".to_string(),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/thermal"),
            daemon_results: vec![DaemonCheckResult {
                name: "thermal-test".to_string(),
                health: DaemonHealth::Running,
                pid: Some(42),
                pid_status: Some(PidStatus::Alive),
                sock_status: None,
                instance_count: 1,
                stale_binary: false,
            }],
            socket_files: vec![],
            backend_mode: "test".to_string(),
            session_info: None,
            log_locations: vec![],
            gpu_info: None,
            state_dirs: vec![],
            suggested_actions: vec![],
        };

        let colored = report.format_colored();
        assert!(
            colored.contains("\x1b["),
            "colored output should have ANSI codes"
        );
        assert!(
            colored.contains("\x1b[32m"),
            "should have green for running daemon"
        );
    }

    #[test]
    fn test_report_no_suggested_actions_when_all_healthy() {
        let report = DiagnosticReport {
            timestamp: "2026-04-01 00:00:00 UTC".to_string(),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/thermal"),
            daemon_results: vec![DaemonCheckResult {
                name: "thermal-test".to_string(),
                health: DaemonHealth::Running,
                pid: Some(42),
                pid_status: Some(PidStatus::Alive),
                sock_status: None,
                instance_count: 1,
                stale_binary: false,
            }],
            socket_files: vec![],
            backend_mode: "test".to_string(),
            session_info: Some("1 session (1 alive)".to_string()),
            log_locations: vec![],
            gpu_info: None,
            state_dirs: vec![],
            suggested_actions: vec![],
        };

        let plain = report.format_plain();
        assert!(
            !plain.contains("## Suggested Actions"),
            "should not have suggestions when all healthy"
        );
    }

    #[test]
    fn test_scan_state_directories_returns_vec() {
        // Just ensure it doesn't panic — the actual directories may or may not exist
        let dirs = scan_state_directories();
        // All entries should have a valid agent type
        for d in &dirs {
            assert!(
                ["claude-code", "codex", "copilot"].contains(&d.agent_type),
                "unexpected agent type: {}",
                d.agent_type
            );
        }
    }

    #[test]
    fn test_doctor_execution_plan_allows_fix_and_report_together() {
        let report = DiagnosticReport {
            timestamp: "2026-04-01 00:00:00 UTC".to_string(),
            runtime_dir: std::path::PathBuf::from("/run/user/1000/thermal"),
            daemon_results: vec![DaemonCheckResult {
                name: "thermal-audio".to_string(),
                health: DaemonHealth::Dead,
                pid: Some(9999),
                pid_status: Some(PidStatus::Stale),
                sock_status: Some(SocketStatus::Stale),
                instance_count: 1,
                stale_binary: false,
            }],
            socket_files: vec![],
            backend_mode: "test".to_string(),
            session_info: None,
            log_locations: vec![],
            gpu_info: None,
            state_dirs: vec![],
            suggested_actions: vec![],
        };

        let plan = doctor_execution_plan(true, true, &report);
        assert!(plan.should_run_fix, "fix should still run when report=true");
        assert!(
            plan.report_filename.is_some(),
            "report path should still be generated when fix=true"
        );
        assert!(
            !plan.should_print_fix_hint,
            "fix hint should not print when fix=true"
        );
    }
}
