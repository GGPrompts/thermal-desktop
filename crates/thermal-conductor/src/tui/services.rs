//! Services page — manage thermal daemon lifecycle from the TUI.
//!
//! Shows the 3 core daemons (audio, dispatcher, conductor) with live status,
//! start/stop toggling, and restart support. Uses `systemctl --user` for
//! systemd-managed services to avoid conflicts with Restart=on-failure.

use std::fs;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use nix::sys::signal;
use nix::unistd::Pid;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap},
};

use thermal_core::{ClaudeStatePoller, palette::ThermalPalette};

use super::TuiPage;
use super::settings::{self, ServiceSettings};

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

const fn pal(c: [f32; 4]) -> Color {
    Color::Rgb(
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8,
    )
}

const BG: Color = pal(ThermalPalette::BG);
const BG_SURFACE: Color = pal(ThermalPalette::BG_SURFACE);
const TEXT: Color = pal(ThermalPalette::TEXT);
const TEXT_BRIGHT: Color = pal(ThermalPalette::TEXT_BRIGHT);
const TEXT_MUTED: Color = pal(ThermalPalette::TEXT_MUTED);
const ACCENT_COLD: Color = pal(ThermalPalette::ACCENT_COLD);
const WARM: Color = pal(ThermalPalette::WARM);
const SEARING: Color = pal(ThermalPalette::SEARING);
const STATUS_OK: Color = pal(ThermalPalette::STATUS_OK);
const STATUS_WARN: Color = pal(ThermalPalette::STATUS_WARN);
const STATUS_ERROR: Color = pal(ThermalPalette::STATUS_ERROR);

// ---------------------------------------------------------------------------
// Service definitions
// ---------------------------------------------------------------------------

/// How to detect whether a service is running.
#[derive(Debug, Clone)]
enum PidSource {
    /// Read PID from a file under `/run/user/<uid>/thermal/`.
    Pidfile(&'static str),
    /// Fall back to `pgrep -x <binary>`.
    Pgrep,
}

#[derive(Debug, Clone)]
struct ServiceDef {
    /// Display / process name.
    binary: &'static str,
    /// Human-readable description.
    description: &'static str,
    /// How to find the PID.
    pid_source: PidSource,
    /// Optional explicit command path for services that are launched via script.
    command: Option<&'static str>,
    /// Extra args passed to the command.
    args: &'static [&'static str],
    /// Systemd user unit name (e.g. "thermal-audio.service").
    /// When set, start/stop/restart use `systemctl --user` instead of
    /// direct process management (setsid/SIGTERM).
    systemd_unit: Option<&'static str>,
    /// Embedded daemon documentation (from docs/daemons/*.md), if available.
    doc_content: Option<&'static str>,
    /// Optional `pgrep -f` pattern for instance counting and kill operations.
    /// When set, overrides the default binary-name-based counting. Needed when
    /// multiple subcommands share the same binary (e.g. `thc daemon` vs `thc tui`).
    count_pattern: Option<&'static str>,
}

/// Runtime status of a service.
#[derive(Debug, Clone)]
struct ServiceStatus {
    running: bool,
    pid: Option<u32>,
    /// Binary on disk is newer than the running process (needs restart).
    stale_binary: bool,
    /// More than one instance of this service is running.
    duplicate_count: u32,
}

const SERVICES: &[ServiceDef] = &[
    ServiceDef {
        binary: "thermal-audio",
        description: "TTS playback + voice capture",
        pid_source: PidSource::Pidfile("audio.pid"),
        command: None,
        args: &[],
        systemd_unit: Some("thermal-audio.service"),
        doc_content: Some(include_str!("../../../../docs/daemons/thermal-audio.md")),
        count_pattern: None,
    },
    ServiceDef {
        binary: "thermal-dispatcher",
        description: "LLM API routing + trust tiers",
        pid_source: PidSource::Pgrep,
        command: None,
        args: &[],
        systemd_unit: Some("thermal-dispatcher.service"),
        doc_content: Some(include_str!(
            "../../../../docs/daemons/thermal-dispatcher.md"
        )),
        count_pattern: None,
    },
    ServiceDef {
        binary: "thermal-conductor",
        description: "Session daemon + bar + HUD",
        pid_source: PidSource::Pidfile("conductor.pid"),
        command: Some("thc"),
        args: &["daemon"],
        systemd_unit: Some("thermal-conductor.service"),
        doc_content: Some(include_str!(
            "../../../../docs/daemons/thermal-conductor.md"
        )),
        // `thc` is a symlink to `thermal-conductor`, so pgrep -x thc matches
        // thc tui, thc window, AND thc daemon. Use a cmdline pattern to count
        // only the daemon process.
        count_pattern: Some("thc daemon"),
    },
];

// ---------------------------------------------------------------------------
// Status detection helpers
// ---------------------------------------------------------------------------

fn runtime_dir() -> PathBuf {
    thermal_core::runtime::runtime_dir()
}

fn read_pid_from_file(filename: &str) -> Option<u32> {
    let path = runtime_dir().join(filename);
    let mut contents = String::new();
    fs::File::open(&path)
        .ok()?
        .read_to_string(&mut contents)
        .ok()?;
    let pid: u32 = contents.trim().parse().ok()?;
    // Verify the process is actually alive.
    if is_pid_alive(pid) { Some(pid) } else { None }
}

fn pgrep_pid(binary: &str) -> Option<u32> {
    // pgrep -x truncates at 15 chars on Linux. Use -f (full command line)
    // with an anchored pattern for long binary names.
    let (flag, pattern) = if binary.len() > 15 {
        ("-f", format!("(^|/){binary}$"))
    } else {
        ("-x", binary.to_string())
    };
    let output = Command::new("pgrep")
        .arg(flag)
        .arg(&pattern)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // pgrep may return multiple PIDs; take the first.
    stdout.lines().next()?.trim().parse().ok()
}

fn is_pid_alive(pid: u32) -> bool {
    // Sending signal 0 checks if process exists without actually signaling.
    signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

/// Read process start time from /proc and return a human-friendly uptime string.
fn process_uptime(pid: u32) -> Option<String> {
    // /proc/<pid>/stat field 22 is starttime in clock ticks since boot.
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields are space-separated, but field 2 (comm) may contain spaces inside
    // parens. Skip past the closing paren to parse reliably.
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After comm, field indices are offset by 2: field 22 (starttime) is at index 19.
    let starttime_ticks: u64 = fields.get(19)?.parse().ok()?;
    let ticks_per_sec = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()? as u64;

    // Read system uptime to convert boot-relative ticks to wall clock duration.
    let uptime_str = fs::read_to_string("/proc/uptime").ok()?;
    let system_uptime_secs: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;

    let process_start_secs = starttime_ticks / ticks_per_sec;
    let elapsed = system_uptime_secs as u64 - process_start_secs;

    Some(format_uptime(elapsed))
}

/// Format seconds into a compact human-readable string.
fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

fn get_service_status(def: &ServiceDef) -> ServiceStatus {
    let pid = match &def.pid_source {
        PidSource::Pidfile(filename) => read_pid_from_file(filename),
        PidSource::Pgrep => pgrep_pid(def.binary).or_else(|| {
            def.command
                .filter(|cmd| *cmd != def.binary)
                .and_then(|cmd| pgrep_pid(cmd))
        }),
    };

    let stale_binary = pid.map_or(false, |p| is_stale_binary(p, def));
    let duplicate_count = count_instances(def);

    ServiceStatus {
        running: pid.is_some(),
        pid,
        stale_binary,
        duplicate_count,
    }
}

/// Check if the running process is using an older binary than what's on disk.
/// Delegates to the shared `daemon_lifecycle` module.
fn is_stale_binary(pid: u32, _def: &ServiceDef) -> bool {
    crate::daemon_lifecycle::is_stale_binary(pid)
}

/// Count how many instances of this service are running.
/// Delegates to the shared `daemon_lifecycle` module.
fn count_instances(def: &ServiceDef) -> u32 {
    // Prefer the explicit count_pattern (avoids matching sibling subcommands).
    let pgrep_pattern = def.count_pattern;
    let count = crate::daemon_lifecycle::count_instances(def.binary, pgrep_pattern);
    if count > 0 || pgrep_pattern.is_some() {
        return count;
    }
    // Fallback: try the command name if different from binary.
    def.command
        .filter(|cmd| *cmd != def.binary)
        .map(|cmd| crate::daemon_lifecycle::count_instances(cmd, None))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Service actions
// ---------------------------------------------------------------------------

fn start_service(def: &ServiceDef) -> Result<(), String> {
    let short_name = binary_to_short_name(def.binary);
    let program = def.command.unwrap_or(def.binary);

    // Unified path: systemctl when managed, setsid fallback otherwise.
    crate::daemon_lifecycle::start_daemon(short_name, def.binary, program, def.args)
}

fn stop_service(def: &ServiceDef, _status: &ServiceStatus) -> Result<(), String> {
    let short_name = binary_to_short_name(def.binary);
    let pgrep_pattern = def.count_pattern;

    // Unified path: systemctl when managed, direct kill otherwise.
    let result = crate::daemon_lifecycle::stop_daemon(short_name, def.binary, pgrep_pattern);
    if result.is_ok() {
        cleanup_stale_socket(def);
    }
    result
}

/// Remove stale Unix socket and pidfile after stopping a service.
/// Delegates to the shared `daemon_lifecycle` module.
fn cleanup_stale_socket(def: &ServiceDef) {
    let short_name = binary_to_short_name(def.binary);
    crate::daemon_lifecycle::cleanup_artifacts(short_name);
}

/// Map a binary name (e.g. "thermal-audio") to its short runtime name
/// (e.g. "audio") for pidfile/socket lookup.
fn binary_to_short_name(binary: &str) -> &str {
    match binary {
        "thermal-audio" => "audio",
        "thermal-dispatcher" => "dispatcher",
        "thermal-conductor" => "conductor",
        _ => binary,
    }
}

// ---------------------------------------------------------------------------
// ServicesPage
// ---------------------------------------------------------------------------

/// Cached state from thermal-audio's control API.
#[derive(Debug, Clone, Default)]
struct AudioControlState {
    muted: bool,
    volume: f32,
    last_fetched: Option<Instant>,
}

pub struct ServicesPage {
    statuses: Vec<ServiceStatus>,
    selected: usize,
    status_msg: Option<(String, bool, Instant)>,
    /// Pending restart: index of service to start after it stops.
    pending_restart: Option<(usize, Instant)>,
    /// Last time statuses were refreshed (throttle pgrep calls).
    last_refresh: Instant,
    /// Cached thermal-audio mute/volume state.
    audio_state: AudioControlState,
    /// Parsed settings.toml data for inline config summaries.
    settings: ServiceSettings,
    /// Whether the help overlay is visible for the selected daemon.
    show_help: bool,
    /// Scroll offset for the help overlay content.
    help_scroll: u16,
}

impl ServicesPage {
    pub fn new() -> Self {
        let statuses = SERVICES.iter().map(get_service_status).collect();
        let settings = settings::load_settings();
        Self {
            statuses,
            selected: 0,
            status_msg: None,
            pending_restart: None,
            last_refresh: Instant::now(),
            audio_state: AudioControlState::default(),
            settings,
            show_help: false,
            help_scroll: 0,
        }
    }

    fn refresh_statuses(&mut self) {
        for (i, def) in SERVICES.iter().enumerate() {
            self.statuses[i] = get_service_status(def);
        }
    }

    fn toggle_selected(&mut self) {
        let def = &SERVICES[self.selected];
        let status = &self.statuses[self.selected];

        if status.running {
            match stop_service(def, status) {
                Ok(()) => {
                    self.status_msg =
                        Some((format!("Stopping {}...", def.binary), false, Instant::now()));
                }
                Err(e) => {
                    self.status_msg = Some((e, true, Instant::now()));
                }
            }
        } else {
            // If another service shares the same binary, stop it first
            // (e.g. switching between voice PTT and voice VAD modes).
            for (i, other) in SERVICES.iter().enumerate() {
                if i != self.selected && other.binary == def.binary && self.statuses[i].running {
                    let _ = stop_service(other, &self.statuses[i]);
                    // Brief pause for the process to exit and release the pidfile/socket.
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            }
            match start_service(def) {
                Ok(()) => {
                    self.status_msg =
                        Some((format!("Starting {}...", def.binary), false, Instant::now()));
                }
                Err(e) => {
                    self.status_msg = Some((e, true, Instant::now()));
                }
            }
        }
        // Force immediate refresh on next tick.
        self.last_refresh = Instant::now() - std::time::Duration::from_secs(10);
    }

    /// Send a JSON command to the thermal-audio socket and parse the response.
    fn send_audio_command(&mut self, json: &str) {
        let sock_path = runtime_dir().join("audio.sock");
        match std::os::unix::net::UnixStream::connect(&sock_path) {
            Ok(mut stream) => {
                use std::io::{Read as _, Write as _};
                let msg = format!("{json}\n");
                if let Err(e) = stream.write_all(msg.as_bytes()) {
                    self.status_msg =
                        Some((format!("audio send failed: {e}"), true, Instant::now()));
                    return;
                }
                let _ = stream.shutdown(std::net::Shutdown::Write);
                let mut resp = String::new();
                let _ = stream.read_to_string(&mut resp);
                // Parse response for muted/volume fields.
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp) {
                    if let Some(m) = v.get("muted").and_then(|v| v.as_bool()) {
                        self.audio_state.muted = m;
                    }
                    if let Some(vol) = v.get("volume").and_then(|v| v.as_f64()) {
                        self.audio_state.volume = vol as f32;
                    }
                    self.audio_state.last_fetched = Some(Instant::now());
                }
            }
            Err(_) => {
                self.status_msg = Some(("thermal-audio not running".into(), true, Instant::now()));
            }
        }
    }

    fn toggle_audio_mute(&mut self) {
        self.send_audio_command(r#"{"action":"toggle_mute"}"#);
        let state = if self.audio_state.muted {
            "muted"
        } else {
            "unmuted"
        };
        self.status_msg = Some((format!("Audio {state}"), false, Instant::now()));
    }

    fn adjust_audio_volume(&mut self, delta: f32) {
        let new_vol = (self.audio_state.volume + delta).clamp(0.0, 1.0);
        let cmd = format!(r#"{{"action":"set_volume","value":{:.2}}}"#, new_vol);
        self.send_audio_command(&cmd);
        let pct = (self.audio_state.volume * 100.0).round() as u32;
        self.status_msg = Some((format!("Volume: {pct}%"), false, Instant::now()));
    }

    fn refresh_audio_state(&mut self) {
        let sock_path = runtime_dir().join("audio.sock");
        if !sock_path.exists() {
            return;
        }
        // Only refresh every 2s.
        if let Some(last) = self.audio_state.last_fetched
            && last.elapsed().as_secs() < 2
        {
            return;
        }
        self.send_audio_command(r#"{"action":"get_status"}"#);
    }

    /// Check if the selected service is thermal-audio.
    fn selected_is_audio(&self) -> bool {
        SERVICES[self.selected].binary == "thermal-audio"
    }

    fn restart_selected(&mut self) {
        let def = &SERVICES[self.selected];
        let short_name = binary_to_short_name(def.binary);
        let program = def.command.unwrap_or(def.binary);
        let pgrep_pattern = def.count_pattern;

        // Unified path: systemctl when managed (atomic restart), setsid fallback.
        match crate::daemon_lifecycle::restart_daemon(
            short_name, def.binary, program, def.args, pgrep_pattern,
        ) {
            Ok(()) => {
                self.status_msg = Some((
                    format!("Restarting {}...", def.binary),
                    false,
                    Instant::now(),
                ));
            }
            Err(e) => {
                self.status_msg = Some((e, true, Instant::now()));
            }
        }
        self.last_refresh = Instant::now() - std::time::Duration::from_secs(10);
    }

    /// Open settings.toml in $EDITOR and reload on return.
    fn open_settings_editor(&mut self) {
        match settings::open_in_editor() {
            Ok(true) => {
                self.settings = settings::load_settings();
                self.status_msg = Some(("Settings reloaded".into(), false, Instant::now()));
            }
            Ok(false) => {
                self.settings = settings::load_settings();
                self.status_msg = Some(("Editor exited with error".into(), true, Instant::now()));
            }
            Err(e) => {
                self.status_msg = Some((e, true, Instant::now()));
            }
        }
    }

    /// Force-kill ALL instances of the selected service (SIGKILL).
    fn force_kill_selected(&mut self) {
        let def = &SERVICES[self.selected];
        let status = &self.statuses[self.selected];

        if !status.running {
            self.status_msg = Some((format!("{} not running", def.binary), true, Instant::now()));
            return;
        }

        // For systemd-managed services, try `systemctl --user kill` first.
        // If the unit isn't active (process started outside systemd), fall through to pkill.
        if let Some(unit) = def.systemd_unit {
            let result = Command::new("systemctl")
                .args(["--user", "kill", "--signal=KILL", unit])
                .status();
            if matches!(result, Ok(s) if s.success()) {
                let _ = Command::new("systemctl")
                    .args(["--user", "stop", unit])
                    .status();
                cleanup_stale_socket(def);
                self.status_msg = Some((
                    format!("Force-killed {}", def.binary),
                    false,
                    Instant::now(),
                ));
                self.last_refresh = Instant::now() - std::time::Duration::from_secs(10);
                return;
            }
            // systemctl kill failed — unit not active, fall through to pkill.
        }

        let pgrep_pattern = def.count_pattern;
        let short_name = binary_to_short_name(def.binary);

        match crate::daemon_lifecycle::force_kill_all(def.binary, short_name, pgrep_pattern) {
            Ok(()) => {
                let count = status.duplicate_count;
                let msg = if count > 1 {
                    format!("Force-killed {} ({count} instances)", def.binary)
                } else {
                    format!("Force-killed {}", def.binary)
                };
                self.status_msg = Some((msg, false, Instant::now()));
            }
            Err(e) => {
                self.status_msg = Some((e, true, Instant::now()));
            }
        }
        self.last_refresh = Instant::now() - std::time::Duration::from_secs(10);
    }
}

impl TuiPage for ServicesPage {
    fn title(&self) -> &str {
        "Services"
    }

    fn tick(&mut self, _poller: &mut ClaudeStatePoller) {
        // Throttle status refresh to every 2s — pgrep spawns subprocesses.
        let now = Instant::now();
        let refresh_interval = if self.pending_restart.is_some() {
            std::time::Duration::from_millis(500) // faster during restart
        } else {
            std::time::Duration::from_secs(2)
        };
        if now.duration_since(self.last_refresh) >= refresh_interval {
            self.refresh_statuses();
            self.last_refresh = now;
        }

        // Handle pending restart: once the service is stopped, start it.
        if let Some((idx, started)) = self.pending_restart {
            if !self.statuses[idx].running {
                let def = &SERVICES[idx];
                match start_service(def) {
                    Ok(()) => {
                        self.status_msg =
                            Some((format!("Restarted {}", def.binary), false, Instant::now()));
                    }
                    Err(e) => {
                        self.status_msg = Some((e, true, Instant::now()));
                    }
                }
                self.pending_restart = None;
            } else if started.elapsed().as_secs() > 5 {
                // Timeout — give up waiting for stop.
                self.status_msg = Some((
                    format!("Restart timeout for {}", SERVICES[idx].binary),
                    true,
                    Instant::now(),
                ));
                self.pending_restart = None;
            }
        }

        // Refresh audio control state periodically.
        self.refresh_audio_state();

        // Clear status message after 4 seconds.
        if let Some((_, _, when)) = &self.status_msg
            && when.elapsed().as_secs() >= 4
        {
            self.status_msg = None;
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // title
                Constraint::Min(5),    // service table
                Constraint::Length(2), // hints
                Constraint::Length(1), // status message
            ])
            .margin(1)
            .split(area);

        // Title
        let running_count = self.statuses.iter().filter(|s| s.running).count();
        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                "Thermal Services",
                Style::default()
                    .fg(TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({}/{} running)", running_count, SERVICES.len()),
                Style::default().fg(TEXT_MUTED),
            ),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(title, chunks[0]);

        // Service table
        let header = Row::new(vec![
            "",
            "Service",
            "Description",
            "Status",
            "PID",
            "Uptime",
        ])
        .style(
            Style::default()
                .fg(ACCENT_COLD)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

        let rows: Vec<Row> = SERVICES
            .iter()
            .zip(self.statuses.iter())
            .enumerate()
            .map(|(i, (def, status))| {
                let selected = i == self.selected;
                let pointer = if selected { "\u{25b8}" } else { " " };
                let (status_text, status_color) = if status.running {
                    if status.stale_binary {
                        ("stale".to_string(), WARM)
                    } else if status.duplicate_count > 1 {
                        (format!("{}x dup", status.duplicate_count), SEARING)
                    } else if def.binary == "thermal-audio"
                        && self.audio_state.last_fetched.is_some()
                    {
                        if self.audio_state.muted {
                            ("muted".to_string(), STATUS_WARN)
                        } else {
                            let pct = (self.audio_state.volume * 100.0).round() as u32;
                            (format!("vol {pct}%"), STATUS_OK)
                        }
                    } else {
                        ("running".to_string(), STATUS_OK)
                    }
                } else {
                    ("stopped".to_string(), STATUS_ERROR)
                };
                let pid_text = status
                    .pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".to_string());

                let uptime_text = status
                    .pid
                    .and_then(|p| process_uptime(p))
                    .unwrap_or_else(|| "-".to_string());

                let row_style = if selected {
                    Style::default().bg(BG_SURFACE).fg(TEXT_BRIGHT)
                } else {
                    Style::default().fg(TEXT)
                };

                Row::new(vec![
                    Span::styled(pointer, Style::default().fg(ACCENT_COLD)),
                    Span::styled(
                        def.binary,
                        Style::default().fg(if selected { TEXT_BRIGHT } else { TEXT }),
                    ),
                    Span::styled(def.description, Style::default().fg(TEXT_MUTED)),
                    Span::styled(
                        status_text.clone(),
                        Style::default()
                            .fg(status_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(pid_text, Style::default().fg(TEXT_MUTED)),
                    Span::styled(uptime_text, Style::default().fg(TEXT_MUTED)),
                ])
                .style(row_style)
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(2),  // pointer
                Constraint::Length(18), // service name
                Constraint::Length(22), // description
                Constraint::Length(9),  // status
                Constraint::Length(8),  // PID
                Constraint::Min(10),    // uptime
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(TEXT_MUTED))
                .style(Style::default().bg(BG)),
        );

        f.render_widget(table, chunks[1]);

        // Hints
        let mut hints = vec![
            Span::styled(
                "Enter/Space",
                Style::default().fg(WARM).add_modifier(Modifier::BOLD),
            ),
            Span::styled(": toggle  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "r",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": restart  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "e",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": edit config  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "K",
                Style::default().fg(SEARING).add_modifier(Modifier::BOLD),
            ),
            Span::styled(": force kill  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "j/k",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": navigate  ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                "?",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": help", Style::default().fg(TEXT_MUTED)),
        ];
        if self.selected_is_audio() {
            hints.push(Span::styled("  ", Style::default().fg(TEXT_MUTED)));
            hints.push(Span::styled(
                "m",
                Style::default().fg(WARM).add_modifier(Modifier::BOLD),
            ));
            hints.push(Span::styled(": mute  ", Style::default().fg(TEXT_MUTED)));
            hints.push(Span::styled(
                "+/-",
                Style::default().fg(WARM).add_modifier(Modifier::BOLD),
            ));
            hints.push(Span::styled(": volume", Style::default().fg(TEXT_MUTED)));
        }
        let hint = Paragraph::new(Line::from(hints)).alignment(Alignment::Center);
        f.render_widget(hint, chunks[2]);

        // Status message
        if let Some((ref msg, is_error, _)) = self.status_msg {
            let color = if is_error { SEARING } else { WARM };
            let status = Paragraph::new(msg.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(color));
            f.render_widget(status, chunks[3]);
        }

        // Help overlay (rendered last so it draws on top).
        if self.show_help {
            if let Some(doc) = SERVICES[self.selected].doc_content {
                let popup = centered_rect(80, 80, area);
                f.render_widget(Clear, popup);

                let title = format!(" {} ", SERVICES[self.selected].binary);
                let lines: Vec<Line> = doc.lines().map(|l| Line::from(l.to_string())).collect();
                let total_lines = lines.len() as u16;
                // Clamp scroll so we don't scroll past the content.
                let visible_height = popup.height.saturating_sub(2); // borders
                if total_lines > visible_height {
                    self.help_scroll = self.help_scroll.min(total_lines - visible_height);
                } else {
                    self.help_scroll = 0;
                }
                let help = Paragraph::new(lines)
                    .block(
                        Block::default()
                            .title(title)
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT_COLD))
                            .style(Style::default().bg(BG_SURFACE)),
                    )
                    .style(Style::default().fg(TEXT))
                    .wrap(Wrap { trim: false })
                    .scroll((self.help_scroll, 0));
                f.render_widget(help, popup);
            }
        }
    }

    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        _poller: &mut ClaudeStatePoller,
    ) -> super::KeyResult {
        use crossterm::event::KeyCode;

        // When the help overlay is visible, only allow dismiss and scroll.
        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                    self.show_help = false;
                    self.help_scroll = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    self.help_scroll = self.help_scroll.saturating_add(10);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(10);
                }
                _ => {}
            }
            return super::KeyResult::NONE;
        }

        match key.code {
            KeyCode::Char('?') => {
                if SERVICES[self.selected].doc_content.is_some() {
                    self.show_help = true;
                    self.help_scroll = 0;
                } else {
                    self.status_msg = Some((
                        format!("No docs available for {}", SERVICES[self.selected].binary),
                        true,
                        Instant::now(),
                    ));
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < SERVICES.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle_selected();
            }
            KeyCode::Char('r') => {
                self.restart_selected();
            }
            KeyCode::Char('e') => {
                self.open_settings_editor();
                return super::KeyResult::CLEAR;
            }
            KeyCode::Char('K') => {
                self.force_kill_selected();
            }
            KeyCode::Char('m') => {
                if self.selected_is_audio() {
                    self.toggle_audio_mute();
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if self.selected_is_audio() {
                    self.adjust_audio_volume(0.1);
                }
            }
            KeyCode::Char('-') => {
                if self.selected_is_audio() {
                    self.adjust_audio_volume(-0.1);
                }
            }
            KeyCode::Esc => {
                self.status_msg = None;
            }
            _ => {}
        }
        super::KeyResult::NONE
    }

    fn handle_mouse(
        &mut self,
        event: crossterm::event::MouseEvent,
        _poller: &mut ClaudeStatePoller,
    ) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match event.kind {
            MouseEventKind::ScrollDown => {
                if self.selected + 1 < SERVICES.len() {
                    self.selected += 1;
                }
            }
            MouseEventKind::ScrollUp => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Page area starts at absolute row 3 (tab bar).
                // Layout: margin(1), then:
                //   [0] title:   Length(2) — rows 4..6
                //   [1] table:   Min(5)   — starts at row 6
                // Table has Borders::ALL (+1 top) and a header row (+1) with
                // bottom_margin(1), so data rows start at row 6+1+1+1 = 9.
                let page_top = 3u16;
                let margin = 1u16;
                let title_height = 2u16;
                let table_top = page_top + margin + title_height;
                // Table: border(1) + header(1) + bottom_margin(1) = 3 rows before data
                let data_start = table_top + 1 + 1 + 1;
                if event.row >= data_start {
                    let clicked_row = (event.row - data_start) as usize;
                    if clicked_row < SERVICES.len() {
                        self.selected = clicked_row;
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------

/// Return a centered rectangle that occupies `percent_x` x `percent_y` of `r`.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_count_matches_expected() {
        assert_eq!(SERVICES.len(), 3);
    }

    #[test]
    fn all_services_have_nonempty_binary_and_description() {
        for def in SERVICES {
            assert!(!def.binary.is_empty(), "service has empty binary name");
            assert!(
                !def.description.is_empty(),
                "service {} has empty description",
                def.binary
            );
        }
    }

    #[test]
    fn all_binaries_start_with_thermal() {
        for def in SERVICES {
            assert!(
                def.binary.starts_with("thermal-") || def.binary == "codex-state-adapter",
                "binary {:?} should start with thermal- or be the Codex adapter",
                def.binary
            );
        }
    }

    #[test]
    fn services_page_new_creates_correct_status_count() {
        let page = ServicesPage::new();
        assert_eq!(page.statuses.len(), SERVICES.len());
    }

    #[test]
    fn services_page_default_selection_is_zero() {
        let page = ServicesPage::new();
        assert_eq!(page.selected, 0);
    }

    #[test]
    fn services_page_title() {
        let page = ServicesPage::new();
        assert_eq!(page.title(), "Services");
    }

    #[test]
    fn pidfile_services_have_correct_filenames() {
        let audio = &SERVICES[0];
        assert_eq!(audio.binary, "thermal-audio");
        assert!(matches!(audio.pid_source, PidSource::Pidfile("audio.pid")));
    }

    #[test]
    fn pgrep_services_use_pgrep_source() {
        let dispatcher = &SERVICES[1];
        assert_eq!(dispatcher.binary, "thermal-dispatcher");
        assert!(
            matches!(dispatcher.pid_source, PidSource::Pgrep),
            "{} should use Pgrep source",
            dispatcher.binary
        );
    }

    #[test]
    fn conductor_uses_pidfile_and_count_pattern() {
        let conductor = &SERVICES[2];
        assert_eq!(conductor.binary, "thermal-conductor");
        assert!(matches!(
            conductor.pid_source,
            PidSource::Pidfile("conductor.pid")
        ));
        assert_eq!(conductor.command, Some("thc"));
        assert_eq!(conductor.args, ["daemon"]);
        // count_pattern must target "thc daemon" specifically — without it,
        // pgrep -cx thc matches thc tui and thc window too (false duplicates).
        assert_eq!(conductor.count_pattern, Some("thc daemon"));
    }

    #[test]
    fn runtime_dir_is_under_thermal() {
        let dir = runtime_dir();
        assert!(
            dir.to_str().unwrap().ends_with("/thermal"),
            "runtime dir should end with /thermal, got {:?}",
            dir
        );
    }

    #[test]
    fn service_status_default_is_not_running() {
        let status = ServiceStatus {
            running: false,
            pid: None,
            stale_binary: false,
            duplicate_count: 0,
        };
        assert!(!status.running);
        assert!(status.pid.is_none());
        assert!(!status.stale_binary);
        assert_eq!(status.duplicate_count, 0);
    }
}

