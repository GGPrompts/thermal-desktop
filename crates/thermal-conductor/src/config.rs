//! `thc config` — show all effective settings with their sources (env, toml, default).

use anyhow::Result;

/// ANSI helpers for config output.
mod config_colors {
    pub const GREEN: &str = "\x1b[32m"; // env override
    pub const YELLOW: &str = "\x1b[33m"; // toml value
    pub const DIM: &str = "\x1b[2m"; // default
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";
}

/// Where a setting's effective value came from.
#[derive(Clone, Copy)]
enum ConfigSource {
    Env,
    Toml,
    Default,
}

impl ConfigSource {
    fn label(self) -> &'static str {
        match self {
            ConfigSource::Env => "env",
            ConfigSource::Toml => "toml",
            ConfigSource::Default => "default",
        }
    }

    fn color(self) -> &'static str {
        match self {
            ConfigSource::Env => config_colors::GREEN,
            ConfigSource::Toml => config_colors::YELLOW,
            ConfigSource::Default => config_colors::DIM,
        }
    }
}

/// Print a single config line with colored source annotation.
fn print_setting(key: &str, value: &str, source: ConfigSource) {
    use config_colors::*;
    let color = source.color();
    println!(
        "  {key:<30} = {color}{value:<30}{RESET} {DIM}[source: {}]{RESET}",
        source.label()
    );
}

/// Print a section header.
fn print_section(title: &str) {
    use config_colors::*;
    println!("\n{BOLD}── {title} ──{RESET}");
}

/// Resolve a setting: check env var, then TOML section/key, then default.
fn resolve(
    env_var: &str,
    toml_section: Option<&str>,
    toml_key: Option<&str>,
    toml_table: &toml::Table,
    default: &str,
) -> (String, ConfigSource) {
    // 1. Environment variable wins.
    if let Ok(val) = std::env::var(env_var) {
        return (val, ConfigSource::Env);
    }

    // 2. TOML file value.
    if let (Some(section), Some(key)) = (toml_section, toml_key) {
        if let Some(toml::Value::Table(inner)) = toml_table.get(section) {
            if let Some(v) = inner.get(key) {
                let display = match v {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Integer(i) => i.to_string(),
                    toml::Value::Float(f) => format!("{f:.2}"),
                    toml::Value::Boolean(b) => b.to_string(),
                    other => other.to_string(),
                };
                return (display, ConfigSource::Toml);
            }
        }
    }

    // 3. Default.
    (default.to_string(), ConfigSource::Default)
}

pub(crate) async fn cmd_config() -> Result<()> {
    use crate::tui::settings::{ensure_settings_file, settings_path};

    // Load TOML once.
    let toml_path = settings_path();
    let _ = ensure_settings_file();
    let toml_table: toml::Table = std::fs::read_to_string(&toml_path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    println!(
        "{}Thermal Desktop — effective configuration{}\n",
        config_colors::BOLD,
        config_colors::RESET
    );
    println!(
        "  {}Settings file:{} {}",
        config_colors::DIM,
        config_colors::RESET,
        toml_path.display()
    );

    // ── Font & Display ─────────────────────────────────────────────────────
    print_section("Font & Display");

    let (val, src) = resolve(
        "THERMAL_FONT_FAMILY",
        None,
        None,
        &toml_table,
        "JetBrainsMono Nerd Font Mono",
    );
    print_setting("font_family", &val, src);

    let (val, src) = resolve("THERMAL_FONT_SIZE", None, None, &toml_table, "17.0");
    print_setting("font_size", &val, src);

    let (val, src) = resolve(
        "THERMAL_FONT_FALLBACK",
        None,
        None,
        &toml_table,
        "Noto Color Emoji",
    );
    print_setting("font_fallback", &val, src);

    let (val, src) = resolve("THERMAL_SCROLLBACK", None, None, &toml_table, "50000");
    print_setting("scrollback_lines", &val, src);

    let (val, src) = resolve("THERMAL_BELL", None, None, &toml_table, "visual");
    print_setting("bell_mode", &val, src);

    // ── Audio & Voice ──────────────────────────────────────────────────────
    print_section("Audio & Voice");

    let (val, src) = resolve(
        "THERMAL_AUDIO_VOICE",
        Some("audio"),
        Some("voice"),
        &toml_table,
        "en-US-GuyNeural",
    );
    print_setting("audio.voice", &val, src);

    let (val, src) = resolve(
        "THERMAL_AUDIO_SPEED",
        Some("audio"),
        Some("speed"),
        &toml_table,
        "1.0",
    );
    print_setting("audio.speed", &val, src);

    let (val, src) = resolve(
        "THERMAL_AUDIO_VOLUME",
        Some("audio"),
        Some("volume"),
        &toml_table,
        "1.0",
    );
    print_setting("audio.volume", &val, src);

    let (val, src) = resolve(
        "THERMAL_VOICE_MODE",
        Some("voice"),
        Some("mode"),
        &toml_table,
        "vad",
    );
    print_setting("voice.mode", &val, src);

    let (val, src) = resolve(
        "THERMAL_VOICE_SENSITIVITY",
        Some("voice"),
        Some("sensitivity"),
        &toml_table,
        "0.6",
    );
    print_setting("voice.sensitivity", &val, src);

    let (val, src) = resolve(
        "THERMAL_VOICE_STT_MODEL",
        Some("voice"),
        Some("stt_model"),
        &toml_table,
        "base.en",
    );
    print_setting("voice.stt_model", &val, src);

    // ── Dispatcher ─────────────────────────────────────────────────────────
    print_section("Dispatcher");

    let (val, src) = resolve(
        "THERMAL_DISPATCHER_BACKEND",
        Some("dispatcher"),
        Some("backend"),
        &toml_table,
        "ollama",
    );
    print_setting("dispatcher.backend", &val, src);

    let (val, src) = resolve(
        "THERMAL_DISPATCHER_MODEL",
        Some("dispatcher"),
        Some("model"),
        &toml_table,
        "qwen3:8b",
    );
    print_setting("dispatcher.model", &val, src);

    // ── Runtime Paths ──────────────────────────────────────────────────────
    print_section("Runtime Paths");

    let runtime = thermal_core::runtime::runtime_dir();
    let xdg_runtime = std::env::var("XDG_RUNTIME_DIR").ok();
    let xdg_config = std::env::var("XDG_CONFIG_HOME").ok();

    if let Some(ref val) = xdg_config {
        print_setting("XDG_CONFIG_HOME", val, ConfigSource::Env);
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| "~".into());
        print_setting(
            "XDG_CONFIG_HOME",
            &format!("{home}/.config"),
            ConfigSource::Default,
        );
    }

    if let Some(ref val) = xdg_runtime {
        print_setting("XDG_RUNTIME_DIR", val, ConfigSource::Env);
    } else {
        print_setting(
            "XDG_RUNTIME_DIR",
            "(unset — using /run/user/<uid>)",
            ConfigSource::Default,
        );
    }

    print_setting(
        "runtime_dir",
        &runtime.display().to_string(),
        if xdg_runtime.is_some() {
            ConfigSource::Env
        } else {
            ConfigSource::Default
        },
    );
    print_setting(
        "settings_file",
        &toml_path.display().to_string(),
        if xdg_config.is_some() {
            ConfigSource::Env
        } else {
            ConfigSource::Default
        },
    );

    let socket_names = ["conductor", "audio", "dispatcher"];
    for name in socket_names {
        let sock = thermal_core::runtime::socket_path(name);
        let exists = sock.exists();
        let status = if exists { "exists" } else { "absent" };
        println!(
            "  {:<30} = {}{:<30}{} {}[{}]{}",
            format!("{name}.sock"),
            if exists {
                config_colors::GREEN
            } else {
                config_colors::DIM
            },
            sock.display(),
            config_colors::RESET,
            config_colors::DIM,
            status,
            config_colors::RESET,
        );
    }

    println!();
    Ok(())
}
