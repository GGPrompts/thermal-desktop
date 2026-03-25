//! Services page — manage thermal daemon lifecycle from the TUI.
//!
//! Shows thermal daemons (audio, bar, hud, notify, voice) with live status,
//! start/stop toggling, and restart support.

use std::any::Any;
use std::fs;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use nix::sys::signal::{self, Signal};
use nix::unistd::{Pid, getuid};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table},
};

use thermal_core::{ClaudeStatePoller, palette::ThermalPalette};

use super::TuiPage;

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
const COLD: Color = pal(ThermalPalette::COLD);
const ACCENT_COLD: Color = pal(ThermalPalette::ACCENT_COLD);
const WARM: Color = pal(ThermalPalette::WARM);
const SEARING: Color = pal(ThermalPalette::SEARING);

// Green for running
const RUNNING_COLOR: Color = Color::Rgb(0, 200, 80);
// Red for stopped
const STOPPED_COLOR: Color = Color::Rgb(200, 50, 50);

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
    /// Binary name (also used for pgrep and Command::new).
    binary: &'static str,
    /// Human-readable description.
    description: &'static str,
    /// How to find the PID.
    pid_source: PidSource,
}

/// Runtime status of a service.
#[derive(Debug, Clone)]
struct ServiceStatus {
    running: bool,
    pid: Option<u32>,
}

const SERVICES: &[ServiceDef] = &[
    ServiceDef {
        binary: "thermal-audio",
        description: "TTS announcements",
        pid_source: PidSource::Pidfile("audio.pid"),
    },
    ServiceDef {
        binary: "thermal-bar",
        description: "Status bar",
        pid_source: PidSource::Pgrep,
    },
    ServiceDef {
        binary: "thermal-hud",
        description: "Overlay HUD",
        pid_source: PidSource::Pgrep,
    },
    ServiceDef {
        binary: "thermal-notify",
        description: "Notification daemon",
        pid_source: PidSource::Pgrep,
    },
    ServiceDef {
        binary: "thermal-voice",
        description: "Voice input",
        pid_source: PidSource::Pidfile("voice.pid"),
    },
];

// ---------------------------------------------------------------------------
// Status detection helpers
// ---------------------------------------------------------------------------

fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("thermal")
    } else {
        PathBuf::from(format!("/run/user/{}/thermal", getuid()))
    }
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
    let output = Command::new("pgrep").arg("-x").arg(binary).output().ok()?;
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

fn get_service_status(def: &ServiceDef) -> ServiceStatus {
    let pid = match &def.pid_source {
        PidSource::Pidfile(filename) => read_pid_from_file(filename),
        PidSource::Pgrep => pgrep_pid(def.binary),
    };
    ServiceStatus {
        running: pid.is_some(),
        pid,
    }
}

// ---------------------------------------------------------------------------
// Service actions
// ---------------------------------------------------------------------------

fn start_service(def: &ServiceDef) -> Result<(), String> {
    // Spawn detached — setsid so it outlives the TUI.
    let result = Command::new("setsid")
        .arg("--fork")
        .arg(def.binary)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Failed to start {}: {}", def.binary, e)),
    }
}

fn stop_service(def: &ServiceDef, status: &ServiceStatus) -> Result<(), String> {
    if let Some(pid) = status.pid {
        // Send SIGTERM.
        match signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
            Ok(()) => return Ok(()),
            Err(e) => return Err(format!("Failed to kill PID {}: {}", pid, e)),
        }
    } else {
        // No known PID — try pkill as fallback.
        let result = Command::new("pkill").arg("-x").arg(def.binary).status();
        match result {
            Ok(s) if s.success() => Ok(()),
            Ok(_) => Err(format!("{} not running", def.binary)),
            Err(e) => Err(format!("pkill failed: {}", e)),
        }
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
}

impl ServicesPage {
    pub fn new() -> Self {
        let statuses = SERVICES.iter().map(get_service_status).collect();
        Self {
            statuses,
            selected: 0,
            status_msg: None,
            pending_restart: None,
            last_refresh: Instant::now(),
            audio_state: AudioControlState::default(),
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
        if let Some(last) = self.audio_state.last_fetched {
            if last.elapsed().as_secs() < 2 {
                return;
            }
        }
        self.send_audio_command(r#"{"action":"get_status"}"#);
    }

    /// Check if the selected service is thermal-audio.
    fn selected_is_audio(&self) -> bool {
        SERVICES[self.selected].binary == "thermal-audio"
    }

    fn restart_selected(&mut self) {
        let def = &SERVICES[self.selected];
        let status = &self.statuses[self.selected];

        if status.running {
            match stop_service(def, status) {
                Ok(()) => {
                    self.pending_restart = Some((self.selected, Instant::now()));
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
        } else {
            // Not running — just start it.
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
        if let Some((_, _, when)) = &self.status_msg {
            if when.elapsed().as_secs() >= 4 {
                self.status_msg = None;
            }
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
        let header = Row::new(vec!["", "Service", "Description", "Status", "PID"])
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
                    if def.binary == "thermal-audio" && self.audio_state.last_fetched.is_some() {
                        if self.audio_state.muted {
                            ("muted".to_string(), Color::Rgb(200, 150, 50))
                        } else {
                            let pct = (self.audio_state.volume * 100.0).round() as u32;
                            (format!("vol {pct}%"), RUNNING_COLOR)
                        }
                    } else {
                        ("running".to_string(), RUNNING_COLOR)
                    }
                } else {
                    ("stopped".to_string(), STOPPED_COLOR)
                };
                let pid_text = status
                    .pid
                    .map(|p| p.to_string())
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
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(COLD))
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
                "j/k",
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(": navigate", Style::default().fg(TEXT_MUTED)),
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
    }

    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        _poller: &mut ClaudeStatePoller,
    ) -> bool {
        use crossterm::event::KeyCode;

        match key.code {
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
        false
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

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_count_matches_expected() {
        assert_eq!(SERVICES.len(), 5);
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
                def.binary.starts_with("thermal-"),
                "binary {:?} should start with thermal-",
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

        let voice = &SERVICES[4];
        assert_eq!(voice.binary, "thermal-voice");
        assert!(matches!(voice.pid_source, PidSource::Pidfile("voice.pid")));
    }

    #[test]
    fn pgrep_services_use_pgrep_source() {
        for def in &SERVICES[1..4] {
            assert!(
                matches!(def.pid_source, PidSource::Pgrep),
                "{} should use Pgrep source",
                def.binary
            );
        }
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
        };
        assert!(!status.running);
        assert!(status.pid.is_none());
    }
}
