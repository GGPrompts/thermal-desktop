//! Font configuration for the GPU terminal window.
//!
//! Reads `THERMAL_FONT_FAMILY` and `THERMAL_FONT_SIZE` environment variables
//! at startup. Provides runtime font size adjustment (Ctrl+Plus/Minus/0).

/// Default font size when `THERMAL_FONT_SIZE` is not set.
const DEFAULT_FONT_SIZE: f32 = 14.0;

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

/// Font configuration read from environment variables.
#[derive(Clone, Debug)]
pub struct FontConfig {
    /// Font family name (e.g. "JetBrains Mono", "Fira Code").
    pub family: String,
    /// Current font size in points.
    pub font_size: f32,
    /// Line height in points (derived from font_size * LINE_HEIGHT_RATIO).
    pub line_height: f32,
    /// The original font size at startup, used for Ctrl+0 reset.
    default_font_size: f32,
}

impl FontConfig {
    /// Read font configuration from environment variables.
    ///
    /// - `THERMAL_FONT_FAMILY`: font family name (default: "JetBrainsMono Nerd Font Mono")
    /// - `THERMAL_FONT_SIZE`: font size in points (default: 14.0)
    pub fn from_env() -> Self {
        let family = std::env::var("THERMAL_FONT_FAMILY")
            .unwrap_or_else(|_| DEFAULT_FONT_FAMILY.to_string());

        let font_size = std::env::var("THERMAL_FONT_SIZE")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(DEFAULT_FONT_SIZE)
            .clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);

        let line_height = font_size * LINE_HEIGHT_RATIO;

        tracing::info!(
            font_family = %family,
            font_size,
            line_height,
            "Font configuration loaded"
        );

        Self {
            family,
            font_size,
            line_height,
            default_font_size: font_size,
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
