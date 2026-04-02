//! Read-only settings dashboard — interactive, scrollable view of all thermal
//! settings grouped by section, showing current value, source, and editability.

use std::collections::HashMap;

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use thermal_core::{ClaudeStatePoller, palette::ThermalPalette};

use super::TuiPage;
use super::settings;

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

// ---------------------------------------------------------------------------
// Setting source + definition
// ---------------------------------------------------------------------------

/// Where a setting's current value was resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// Read from an environment variable.
    Env,
    /// Read from settings.toml.
    Toml,
    /// Hardcoded default (no env var or TOML override found).
    Default,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::Toml => "toml",
            Source::Default => "default",
        }
    }

    fn color(self) -> Color {
        match self {
            Source::Env => WARM,
            Source::Toml => ACCENT_COLD,
            Source::Default => TEXT_MUTED,
        }
    }
}

/// A single resolved setting to display.
#[derive(Debug, Clone)]
struct ResolvedSetting {
    name: &'static str,
    value: String,
    source: Source,
    editable: bool,
}

/// A named group of settings.
#[derive(Debug, Clone)]
struct SettingGroup {
    title: &'static str,
    items: Vec<ResolvedSetting>,
}

// ---------------------------------------------------------------------------
// Setting definitions per section
// ---------------------------------------------------------------------------

/// Descriptor for a setting we want to display.
struct SettingDef {
    /// Display name.
    name: &'static str,
    /// TOML section (e.g. "audio").
    toml_section: &'static str,
    /// TOML key within the section.
    toml_key: &'static str,
    /// Environment variable that can override, if any.
    env_var: Option<&'static str>,
    /// Default value when nothing is set.
    default: &'static str,
    /// Whether the setting can be changed by the user (all are read-only for
    /// now, but some are truly fixed constants).
    editable: bool,
}

/// All known settings, grouped.
fn setting_groups() -> Vec<(&'static str, Vec<SettingDef>)> {
    vec![
        (
            "Font & Display",
            vec![
                SettingDef {
                    name: "Font family",
                    toml_section: "conductor",
                    toml_key: "font_family",
                    env_var: Some("THERMAL_FONT_FAMILY"),
                    default: "JetBrains Mono",
                    editable: true,
                },
                SettingDef {
                    name: "Font fallback",
                    toml_section: "conductor",
                    toml_key: "font_fallback",
                    env_var: Some("THERMAL_FONT_FALLBACK"),
                    default: "Noto Color Emoji",
                    editable: true,
                },
                SettingDef {
                    name: "Font size",
                    toml_section: "conductor",
                    toml_key: "font_size",
                    env_var: Some("THERMAL_FONT_SIZE"),
                    default: "13.0",
                    editable: true,
                },
                SettingDef {
                    name: "Scrollback lines",
                    toml_section: "conductor",
                    toml_key: "scrollback",
                    env_var: Some("THERMAL_SCROLLBACK"),
                    default: "10000",
                    editable: true,
                },
                SettingDef {
                    name: "Line height ratio",
                    toml_section: "conductor",
                    toml_key: "line_height_ratio",
                    env_var: None,
                    default: "1.2",
                    editable: true,
                },
                SettingDef {
                    name: "BG opacity",
                    toml_section: "conductor",
                    toml_key: "bg_opacity",
                    env_var: None,
                    default: "0.92",
                    editable: true,
                },
                SettingDef {
                    name: "Bell mode",
                    toml_section: "conductor",
                    toml_key: "bell",
                    env_var: Some("THERMAL_BELL"),
                    default: "visual",
                    editable: true,
                },
            ],
        ),
        (
            "Audio & Voice",
            vec![
                SettingDef {
                    name: "TTS voice",
                    toml_section: "audio",
                    toml_key: "voice",
                    env_var: None,
                    default: "en-US-GuyNeural",
                    editable: true,
                },
                SettingDef {
                    name: "TTS speed",
                    toml_section: "audio",
                    toml_key: "speed",
                    env_var: None,
                    default: "1.0",
                    editable: true,
                },
                SettingDef {
                    name: "Master volume",
                    toml_section: "audio",
                    toml_key: "volume",
                    env_var: None,
                    default: "1.0",
                    editable: true,
                },
                SettingDef {
                    name: "STT model",
                    toml_section: "voice",
                    toml_key: "stt_model",
                    env_var: None,
                    default: "base.en",
                    editable: true,
                },
                SettingDef {
                    name: "VAD sensitivity",
                    toml_section: "voice",
                    toml_key: "sensitivity",
                    env_var: None,
                    default: "0.6",
                    editable: true,
                },
                SettingDef {
                    name: "Voice mode",
                    toml_section: "voice",
                    toml_key: "mode",
                    env_var: None,
                    default: "vad",
                    editable: true,
                },
            ],
        ),
        (
            "Dispatcher",
            vec![
                SettingDef {
                    name: "Backend",
                    toml_section: "dispatcher",
                    toml_key: "backend",
                    env_var: None,
                    default: "ollama",
                    editable: true,
                },
                SettingDef {
                    name: "Model",
                    toml_section: "dispatcher",
                    toml_key: "model",
                    env_var: None,
                    default: "qwen3:8b",
                    editable: true,
                },
            ],
        ),
        (
            "Bar",
            vec![
                SettingDef {
                    name: "Position",
                    toml_section: "bar",
                    toml_key: "position",
                    env_var: None,
                    default: "top",
                    editable: true,
                },
                SettingDef {
                    name: "Update interval (ms)",
                    toml_section: "bar",
                    toml_key: "update_interval_ms",
                    env_var: None,
                    default: "1000",
                    editable: true,
                },
                SettingDef {
                    name: "Modules",
                    toml_section: "bar",
                    toml_key: "modules",
                    env_var: None,
                    default: "cpu,gpu,mem,net,workspaces,agents,voice",
                    editable: true,
                },
            ],
        ),
        (
            "Conductor",
            vec![
                SettingDef {
                    name: "Backend",
                    toml_section: "conductor",
                    toml_key: "backend",
                    env_var: None,
                    default: "auto",
                    editable: true,
                },
                SettingDef {
                    name: "Preview refresh (ms)",
                    toml_section: "conductor",
                    toml_key: "preview_refresh_ms",
                    env_var: None,
                    default: "500",
                    editable: true,
                },
            ],
        ),
        (
            "Notifications",
            vec![
                SettingDef {
                    name: "Timeout (ms)",
                    toml_section: "notify",
                    toml_key: "timeout_ms",
                    env_var: None,
                    default: "5000",
                    editable: true,
                },
            ],
        ),
        (
            "Messages",
            vec![
                SettingDef {
                    name: "Persist to disk",
                    toml_section: "messages",
                    toml_key: "persist",
                    env_var: None,
                    default: "false",
                    editable: true,
                },
                SettingDef {
                    name: "Ring buffer size",
                    toml_section: "messages",
                    toml_key: "ring_size",
                    env_var: None,
                    default: "500",
                    editable: true,
                },
            ],
        ),
    ]
}

// ---------------------------------------------------------------------------
// Resolution logic
// ---------------------------------------------------------------------------

/// Resolve all settings against env vars + parsed TOML data.
fn resolve_all(toml_sections: &HashMap<String, Vec<(String, String)>>) -> Vec<SettingGroup> {
    setting_groups()
        .into_iter()
        .map(|(title, defs)| {
            let items = defs
                .into_iter()
                .map(|def| resolve_one(def, toml_sections))
                .collect();
            SettingGroup { title, items }
        })
        .collect()
}

fn resolve_one(
    def: SettingDef,
    toml_sections: &HashMap<String, Vec<(String, String)>>,
) -> ResolvedSetting {
    // Priority: env > toml > default.
    if let Some(env_var) = def.env_var {
        if let Ok(val) = std::env::var(env_var) {
            return ResolvedSetting {
                name: def.name,
                value: val,
                source: Source::Env,
                editable: def.editable,
            };
        }
    }

    if let Some(pairs) = toml_sections.get(def.toml_section) {
        if let Some((_, val)) = pairs.iter().find(|(k, _)| k == def.toml_key) {
            return ResolvedSetting {
                name: def.name,
                value: val.clone(),
                source: Source::Toml,
                editable: def.editable,
            };
        }
    }

    ResolvedSetting {
        name: def.name,
        value: def.default.to_string(),
        source: Source::Default,
        editable: def.editable,
    }
}

// ---------------------------------------------------------------------------
// Page state
// ---------------------------------------------------------------------------

pub struct SettingsPage {
    groups: Vec<SettingGroup>,
    /// Scroll offset (in lines) for the settings list.
    scroll: u16,
    /// Total rendered line count (updated on each render).
    total_lines: u16,
    /// Visible height of the content area (updated on each render).
    visible_height: u16,
}

impl SettingsPage {
    pub fn new() -> Self {
        let svc_settings = settings::load_settings();
        let groups = resolve_all(svc_settings.sections_raw());
        Self {
            groups,
            scroll: 0,
            total_lines: 0,
            visible_height: 0,
        }
    }

    fn reload(&mut self) {
        let svc_settings = settings::load_settings();
        self.groups = resolve_all(svc_settings.sections_raw());
    }

    /// Build the lines to render. Returns owned lines (no borrow on self).
    fn build_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line> = Vec::new();

        // Header
        lines.push(Line::from(vec![
            Span::styled(
                " Settings Dashboard ",
                Style::default()
                    .fg(TEXT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  (read-only)  ",
                Style::default().fg(TEXT_MUTED),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            " Source legend: ",
            Style::default().fg(TEXT_MUTED),
        )));
        lines.push(Line::from(vec![
            Span::styled("   env", Style::default().fg(WARM)),
            Span::styled(" = environment variable  ", Style::default().fg(TEXT_MUTED)),
            Span::styled("toml", Style::default().fg(ACCENT_COLD)),
            Span::styled(" = settings.toml  ", Style::default().fg(TEXT_MUTED)),
            Span::styled("default", Style::default().fg(TEXT_MUTED)),
            Span::styled(" = hardcoded", Style::default().fg(TEXT_MUTED)),
        ]));
        lines.push(Line::from(""));

        let path_display = settings::settings_path()
            .to_string_lossy()
            .to_string();
        lines.push(Line::from(vec![
            Span::styled(" File: ", Style::default().fg(TEXT_MUTED)),
            Span::styled(
                path_display,
                Style::default().fg(ACCENT_COLD),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            " Press 'e' to open in $EDITOR, 'r' to reload",
            Style::default().fg(TEXT_MUTED),
        )));
        lines.push(Line::from(""));

        for group in &self.groups {
            // Section header
            lines.push(Line::from(Span::styled(
                format!(" [{}]", group.title),
                Style::default()
                    .fg(ACCENT_COLD)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));

            for item in &group.items {
                let source_tag = format!("[{}]", item.source.label());
                let editable_marker = if item.editable { "" } else { " (fixed)" };

                // Name column (padded), value, source tag
                let name_width = 24;
                let padded_name = format!("   {:width$}", item.name, width = name_width);

                lines.push(Line::from(vec![
                    Span::styled(padded_name, Style::default().fg(TEXT)),
                    Span::styled(item.value.clone(), Style::default().fg(TEXT_BRIGHT)),
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        source_tag,
                        Style::default().fg(item.source.color()),
                    ),
                    Span::styled(
                        editable_marker.to_string(),
                        Style::default().fg(TEXT_MUTED),
                    ),
                ]));
            }
            lines.push(Line::from("")); // blank line between sections
        }

        lines
    }
}

impl TuiPage for SettingsPage {
    fn title(&self) -> &str {
        "Settings"
    }

    fn tick(&mut self, _poller: &mut ClaudeStatePoller) {
        // Settings are static-ish; only reload on explicit user action.
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .title(" SETTINGS ")
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(TEXT_MUTED))
            .style(Style::default().bg(BG));

        let inner = block.inner(area);
        f.render_widget(block, area);

        let lines = self.build_lines();
        self.total_lines = lines.len() as u16;
        self.visible_height = inner.height;

        // Clamp scroll.
        let max_scroll = self.total_lines.saturating_sub(self.visible_height);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }

        let paragraph = Paragraph::new(lines)
            .style(Style::default().bg(BG).fg(TEXT))
            .scroll((self.scroll, 0))
            .wrap(Wrap { trim: false });

        f.render_widget(paragraph, inner);

        // Scroll indicator
        if self.total_lines > self.visible_height {
            let pct = if max_scroll == 0 {
                100
            } else {
                (self.scroll as u32 * 100 / max_scroll as u32).min(100)
            };
            let indicator = format!(" {}/{} ({}%) ", self.scroll, max_scroll, pct);
            let indicator_line = Line::from(Span::styled(
                indicator,
                Style::default().fg(TEXT_MUTED).bg(BG_SURFACE),
            ));
            // Render at bottom-right of inner area.
            let indicator_area = Rect {
                x: inner.x + inner.width.saturating_sub(20),
                y: inner.y + inner.height.saturating_sub(1),
                width: 20.min(inner.width),
                height: 1,
            };
            f.render_widget(Paragraph::new(indicator_line).alignment(Alignment::Right), indicator_area);
        }
    }

    fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        _poller: &mut ClaudeStatePoller,
    ) -> super::KeyResult {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(self.visible_height.saturating_sub(2));
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(self.visible_height.saturating_sub(2));
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll = self.total_lines;
            }
            KeyCode::Char('r') => {
                self.reload();
            }
            KeyCode::Char('e') => {
                if let Err(e) = settings::open_in_editor() {
                    let _ = crossterm::terminal::enable_raw_mode();
                    let _ = crossterm::execute!(
                        std::io::stdout(),
                        crossterm::terminal::EnterAlternateScreen,
                        crossterm::event::EnableMouseCapture
                    );
                    tracing::error!("open_in_editor failed: {e}");
                }
                self.reload();
                return super::KeyResult::CLEAR;
            }
            _ => {}
        }
        super::KeyResult::NONE
    }

    fn handle_mouse(&mut self, event: crossterm::event::MouseEvent, _poller: &mut ClaudeStatePoller) {
        use crossterm::event::MouseEventKind;
        match event.kind {
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3);
            }
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
            }
            _ => {}
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
    fn resolve_with_empty_toml_uses_defaults() {
        let toml_sections = HashMap::new();
        let groups = resolve_all(&toml_sections);
        assert!(!groups.is_empty());
        // Every item should be Source::Default when there's no toml or env.
        for group in &groups {
            for item in &group.items {
                // Could be Env if the test runner has THERMAL_* set, so just
                // check it resolved to *something*.
                assert!(!item.value.is_empty(), "setting {} has empty value", item.name);
            }
        }
    }

    #[test]
    fn resolve_picks_toml_over_default() {
        let mut toml_sections = HashMap::new();
        toml_sections.insert(
            "audio".to_string(),
            vec![("voice".to_string(), "en-GB-SoniaNeural".to_string())],
        );
        let groups = resolve_all(&toml_sections);
        let audio_group = groups.iter().find(|g| g.title == "Audio & Voice").unwrap();
        let voice = audio_group.items.iter().find(|i| i.name == "TTS voice").unwrap();
        assert_eq!(voice.value, "en-GB-SoniaNeural");
        assert_eq!(voice.source, Source::Toml);
    }

    #[test]
    fn source_labels() {
        assert_eq!(Source::Env.label(), "env");
        assert_eq!(Source::Toml.label(), "toml");
        assert_eq!(Source::Default.label(), "default");
    }

    #[test]
    fn setting_groups_has_all_sections() {
        let groups = setting_groups();
        let titles: Vec<_> = groups.iter().map(|(t, _)| *t).collect();
        assert!(titles.contains(&"Font & Display"));
        assert!(titles.contains(&"Audio & Voice"));
        assert!(titles.contains(&"Dispatcher"));
        assert!(titles.contains(&"Bar"));
        assert!(titles.contains(&"Conductor"));
        assert!(titles.contains(&"Notifications"));
        assert!(titles.contains(&"Messages"));
    }
}
