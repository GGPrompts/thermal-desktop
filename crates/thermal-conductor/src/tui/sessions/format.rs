//! Palette constants and formatting helpers for the sessions page.

use ratatui::style::Color;
use thermal_core::palette::ThermalPalette;
use thermal_core::{ClaudeSessionState, ClaudeStatus};

use crate::agent_timeline::ToolCategory;

// ---------------------------------------------------------------------------
// Palette helpers (same as thermal-monitor)
// ---------------------------------------------------------------------------

pub(super) const fn pal(c: [f32; 4]) -> Color {
    Color::Rgb(
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8,
    )
}

pub(super) const BG: Color = pal(ThermalPalette::BG);
pub(super) const BG_SURFACE: Color = pal(ThermalPalette::BG_SURFACE);
pub(super) const TEXT: Color = pal(ThermalPalette::TEXT);
pub(super) const TEXT_BRIGHT: Color = pal(ThermalPalette::TEXT_BRIGHT);
pub(super) const TEXT_MUTED: Color = pal(ThermalPalette::TEXT_MUTED);
pub(super) const ACCENT_COLD: Color = pal(ThermalPalette::ACCENT_COLD);

/// Map a ToolCategory to a ratatui Color using thermal palette colors.
pub(super) fn tool_category_color(cat: ToolCategory) -> Color {
    match cat {
        ToolCategory::Read => pal(ThermalPalette::COLD),
        ToolCategory::Write => pal(ThermalPalette::HOT),
        ToolCategory::Execute => pal(ThermalPalette::HOTTER),
        ToolCategory::Thinking => pal(ThermalPalette::MILD),
        ToolCategory::Idle => pal(ThermalPalette::TEXT_MUTED),
    }
}

pub(super) fn status_color(status: &ClaudeStatus) -> Color {
    match status {
        ClaudeStatus::Idle => pal(ThermalPalette::TEXT_MUTED),
        ClaudeStatus::Processing => pal(ThermalPalette::WARM),
        ClaudeStatus::ToolUse => pal(ThermalPalette::HOT),
        ClaudeStatus::AwaitingInput => pal(ThermalPalette::SEARING),
    }
}

/// Emoji badge and color for the agent type column.
/// Subagent count renders next to the emoji: `\u{1F916}x3`.
pub(super) fn agent_type_badge(session: &ClaudeSessionState) -> (String, Color) {
    let (emoji, color) = match session.agent_type.as_deref() {
        Some("copilot") => ("\u{1F916}", pal(ThermalPalette::ACCENT_HOT)),
        Some("codex") => ("\u{1F916}", pal(ThermalPalette::ACCENT_COOL)),
        _ => ("\u{1F916}", pal(ThermalPalette::ACCENT_WARM)),
    };

    let label = if let Some(n) = session.subagent_count
        && n > 0
    {
        format!("{}x{}", emoji, n)
    } else {
        emoji.to_string()
    };

    (label, color)
}

pub(super) fn status_label(status: &ClaudeStatus) -> &'static str {
    match status {
        ClaudeStatus::Idle => "IDLE",
        ClaudeStatus::Processing => "RUNNING",
        ClaudeStatus::ToolUse => "TOOL USE",
        ClaudeStatus::AwaitingInput => "AWAITING",
    }
}

/// Color for context percentage thresholds.
pub(super) fn ctx_color(pct: f32) -> Color {
    if pct < 50.0 {
        Color::Green
    } else if pct < 75.0 {
        Color::Yellow
    } else if pct < 90.0 {
        Color::Rgb(249, 115, 22) // orange
    } else {
        Color::Red
    }
}

// ---------------------------------------------------------------------------
// Activity formatting
// ---------------------------------------------------------------------------

/// Extract just the filename from a path.
pub(super) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Build the activity string from session state.
pub(super) fn format_activity(s: &ClaudeSessionState) -> String {
    if s.status == ClaudeStatus::Idle || s.status == ClaudeStatus::AwaitingInput {
        return "\u{2705} Ready".into();
    }

    let tool_name = s.current_tool.as_deref().unwrap_or("");
    if tool_name.is_empty() {
        return "\u{26A1} Processing".into();
    }

    let trunc = |s: &str, n: usize| -> String {
        if s.chars().count() > n {
            format!("{}...", s.chars().take(n).collect::<String>())
        } else {
            s.to_string()
        }
    };
    let detail = s
        .details
        .as_ref()
        .and_then(|d| d.args.as_ref())
        .map(|a| {
            if let Some(fp) = &a.file_path {
                basename(fp).to_string()
            } else if let Some(cmd) = &a.command {
                trunc(cmd, 20)
            } else if let Some(pat) = &a.pattern {
                pat.clone()
            } else if let Some(desc) = &a.description {
                trunc(desc, 20)
            } else {
                String::new()
            }
        })
        .unwrap_or_default();

    let (emoji, label) = match tool_name {
        "Read" => ("\u{1F4D6}", "Read"),
        "Write" => ("\u{1F4DD}", "Write"),
        "Edit" => ("\u{270F}\u{FE0F}", "Edit"),
        "Bash" => ("\u{1F53A}", "Bash"),
        "Glob" => ("\u{1F50D}", "Glob"),
        "Grep" => ("\u{1F50E}", "Grep"),
        "Task" | "Agent" => ("\u{1F916}", "Task"),
        "WebFetch" => ("\u{1F310}", "Fetch"),
        "WebSearch" => ("\u{1F50D}", "Search"),
        other => ("", other),
    };

    if emoji.is_empty() {
        label.to_string()
    } else if detail.is_empty() {
        format!("{} {}", emoji, label)
    } else {
        format!("{} {}: {}", emoji, label, detail)
    }
}

// ---------------------------------------------------------------------------
// Relative timestamps
// ---------------------------------------------------------------------------

pub(super) fn format_duration_ms(ms: i64) -> String {
    let secs = (ms / 1000).max(0);
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{}m", m)
        } else {
            format!("{}m{}s", m, s)
        }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{}h", h)
        } else {
            format!("{}h{}m", h, m)
        }
    }
}

pub(super) fn relative_time(iso: &str) -> String {
    parse_secs_ago(iso)
        .map(|s| {
            let s = s.max(0);
            if s < 60 {
                format!("{}s", s)
            } else if s < 3600 {
                format!("{}m", s / 60)
            } else {
                format!("{}h", s / 3600)
            }
        })
        .unwrap_or_else(|| "-".into())
}

pub(super) fn parse_secs_ago(iso: &str) -> Option<i64> {
    let s = iso.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let time = time.split('.').next()?;
    let time = time.split('+').next()?;
    let mut t = time.split(':');
    let (h, mi, sc): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next().and_then(|s| s.parse().ok()).unwrap_or(0),
    );
    let (mut yr, mut mn) = (y, mo);
    if mn <= 2 {
        yr -= 1;
        mn += 12;
    }
    let days = 365 * yr + yr / 4 - yr / 100 + yr / 400 + (153 * (mn - 3) + 2) / 5 + day - 719469;
    let ts = days * 86400 + h * 3600 + mi * 60 + sc;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(now - ts)
}
