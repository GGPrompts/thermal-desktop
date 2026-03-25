//! Backend detection and routing layer.
//!
//! Provides a unified `Backend` enum that abstracts over two session backends:
//!
//! - **Kitty**: Uses `kitty @` remote control to manage terminal windows.
//!   Available when kitty is running with `allow_remote_control` enabled.
//!
//! - **Daemon**: Connects to the thermal-conductor session daemon over a Unix
//!   socket. Available when `thc daemon` is running.
//!
//! The `detect_backend()` function probes for available backends based on a
//! `BackendPreference` (auto, kitty-only, daemon-only) and returns the first
//! match.

use anyhow::{Result, bail};

use crate::client::DaemonClient;
use crate::kitty::KittyController;

// ── Preference enum (parsed from CLI flag) ──────────────────────────────────

/// User preference for which backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendPreference {
    /// Try kitty first, then daemon.
    Auto,
    /// Force kitty backend (error if unavailable).
    Kitty,
    /// Force daemon backend (error if unavailable).
    Daemon,
}

impl std::fmt::Display for BackendPreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendPreference::Auto => write!(f, "auto"),
            BackendPreference::Kitty => write!(f, "kitty"),
            BackendPreference::Daemon => write!(f, "daemon"),
        }
    }
}

impl std::str::FromStr for BackendPreference {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(BackendPreference::Auto),
            "kitty" => Ok(BackendPreference::Kitty),
            "daemon" => Ok(BackendPreference::Daemon),
            other => Err(format!(
                "unknown backend '{}': expected auto, kitty, or daemon",
                other
            )),
        }
    }
}

// ── Backend enum ────────────────────────────────────────────────────────────

/// A detected and connected session backend.
pub enum Backend {
    Kitty(KittyController),
    Daemon(DaemonClient),
}

impl Backend {
    /// Human-readable name for status messages.
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Kitty(_) => "kitty",
            Backend::Daemon(_) => "daemon",
        }
    }
}

// ── Detection ───────────────────────────────────────────────────────────────

/// Detect and connect to a backend based on the user's preference.
///
/// - `Auto`: tries kitty first (fast `kitty @ ls` probe), then daemon socket.
/// - `Kitty`: requires kitty remote control; errors if unavailable.
/// - `Daemon`: requires a running daemon; errors if socket is missing/stale.
pub async fn detect_backend(preference: BackendPreference) -> Result<Backend> {
    match preference {
        BackendPreference::Auto => {
            // Try kitty first.
            let kitty = KittyController::new();
            if kitty.is_available().await {
                return Ok(Backend::Kitty(kitty));
            }

            // Fall back to daemon.
            match DaemonClient::connect().await? {
                Some(client) => Ok(Backend::Daemon(client)),
                None => bail!(
                    "No backend available. Either start kitty with allow_remote_control \
                     or run `thc daemon`."
                ),
            }
        }

        BackendPreference::Kitty => {
            let kitty = KittyController::new();
            if !kitty.is_available().await {
                bail!(
                    "Kitty remote control not available. Is kitty running with \
                     allow_remote_control enabled?"
                );
            }
            Ok(Backend::Kitty(kitty))
        }

        BackendPreference::Daemon => match DaemonClient::connect().await? {
            Some(client) => Ok(Backend::Daemon(client)),
            None => bail!("Daemon not available. Start it with `thc daemon`."),
        },
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_backend_preference_auto() {
        assert_eq!(
            "auto".parse::<BackendPreference>().unwrap(),
            BackendPreference::Auto
        );
        assert_eq!(
            "Auto".parse::<BackendPreference>().unwrap(),
            BackendPreference::Auto
        );
        assert_eq!(
            "AUTO".parse::<BackendPreference>().unwrap(),
            BackendPreference::Auto
        );
    }

    #[test]
    fn parse_backend_preference_kitty() {
        assert_eq!(
            "kitty".parse::<BackendPreference>().unwrap(),
            BackendPreference::Kitty
        );
        assert_eq!(
            "Kitty".parse::<BackendPreference>().unwrap(),
            BackendPreference::Kitty
        );
    }

    #[test]
    fn parse_backend_preference_daemon() {
        assert_eq!(
            "daemon".parse::<BackendPreference>().unwrap(),
            BackendPreference::Daemon
        );
        assert_eq!(
            "Daemon".parse::<BackendPreference>().unwrap(),
            BackendPreference::Daemon
        );
    }

    #[test]
    fn parse_backend_preference_invalid() {
        assert!("tmux".parse::<BackendPreference>().is_err());
        assert!("".parse::<BackendPreference>().is_err());
    }

    #[test]
    fn backend_preference_display() {
        assert_eq!(BackendPreference::Auto.to_string(), "auto");
        assert_eq!(BackendPreference::Kitty.to_string(), "kitty");
        assert_eq!(BackendPreference::Daemon.to_string(), "daemon");
    }
}
