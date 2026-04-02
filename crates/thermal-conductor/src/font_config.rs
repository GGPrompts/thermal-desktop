//! Font configuration for the GPU terminal window.
//!
//! Reads `THERMAL_FONT_FAMILY` and `THERMAL_FONT_SIZE` environment variables
//! at startup. Provides runtime font size adjustment (Ctrl+Plus/Minus/0).

/// Default font size when `THERMAL_FONT_SIZE` is not set.
const DEFAULT_FONT_SIZE: f32 = 17.0;

/// Line height multiplier relative to font size.
const LINE_HEIGHT_RATIO: f32 = 1.375;

/// Default font family when `THERMAL_FONT_FAMILY` is not set.
/// Falls back to "monospace" if the preferred family is not available.
const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

/// Minimum font size (points) for runtime adjustment.
const MIN_FONT_SIZE: f32 = 6.0;

/// Maximum font size (points) for runtime adjustment.
const MAX_FONT_SIZE: f32 = 72.0;

/// Step size for Ctrl+Plus/Minus font size adjustment.
const FONT_SIZE_STEP: f32 = 1.0;

/// Default scrollback history size (lines).
const DEFAULT_SCROLLBACK: usize = 50_000;

/// Font configuration read from environment variables.
#[derive(Clone, Debug)]
pub struct FontConfig {
    /// Font family name (e.g. "JetBrains Mono", "Fira Code").
    pub family: String,
    /// Fallback font families for glyphs not found in the primary font.
    /// Parsed from `THERMAL_FONT_FALLBACK` (comma-separated).
    pub fallback_families: Vec<String>,
    /// Current font size in points.
    pub font_size: f32,
    /// Line height in points (derived from font_size * LINE_HEIGHT_RATIO).
    pub line_height: f32,
    /// The original font size at startup, used for Ctrl+0 reset.
    default_font_size: f32,
    /// Scrollback history size (lines). Read from `THERMAL_SCROLLBACK`.
    pub scrollback_lines: usize,
}

impl FontConfig {
    /// Read font configuration from environment variables.
    ///
    /// - `THERMAL_FONT_FAMILY`: font family name (default: "JetBrainsMono Nerd Font Mono")
    /// - `THERMAL_FONT_SIZE`: font size in points (default: 14.0)
    pub fn from_env() -> Self {
        let family = std::env::var("THERMAL_FONT_FAMILY")
            .unwrap_or_else(|_| DEFAULT_FONT_FAMILY.to_string());

        let fallback_families: Vec<String> = std::env::var("THERMAL_FONT_FALLBACK")
            .unwrap_or_else(|_| "Noto Color Emoji".to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let font_size = std::env::var("THERMAL_FONT_SIZE")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(DEFAULT_FONT_SIZE)
            .clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);

        let line_height = font_size * LINE_HEIGHT_RATIO;

        let scrollback_lines = std::env::var("THERMAL_SCROLLBACK")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_SCROLLBACK);

        tracing::info!(
            font_family = %family,
            ?fallback_families,
            font_size,
            line_height,
            scrollback_lines,
            "Font configuration loaded"
        );

        Self {
            family,
            fallback_families,
            font_size,
            line_height,
            default_font_size: font_size,
            scrollback_lines,
        }
    }

    /// Increase font size by one step. Returns true if the size changed.
    pub fn increase(&mut self) -> bool {
        let new_size = (self.font_size + FONT_SIZE_STEP).min(MAX_FONT_SIZE);
        if (new_size - self.font_size).abs() > f32::EPSILON {
            self.font_size = new_size;
            self.line_height = new_size * LINE_HEIGHT_RATIO;
            true
        } else {
            false
        }
    }

    /// Decrease font size by one step. Returns true if the size changed.
    pub fn decrease(&mut self) -> bool {
        let new_size = (self.font_size - FONT_SIZE_STEP).max(MIN_FONT_SIZE);
        if (new_size - self.font_size).abs() > f32::EPSILON {
            self.font_size = new_size;
            self.line_height = new_size * LINE_HEIGHT_RATIO;
            true
        } else {
            false
        }
    }

    /// Reset font size to the startup default. Returns true if the size changed.
    pub fn reset(&mut self) -> bool {
        if (self.font_size - self.default_font_size).abs() > f32::EPSILON {
            self.font_size = self.default_font_size;
            self.line_height = self.default_font_size * LINE_HEIGHT_RATIO;
            true
        } else {
            false
        }
    }
}
