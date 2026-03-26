//! Shared terminal primitives for the Thermal ecosystem.
//!
//! This crate extracts the platform-agnostic terminal building blocks that are
//! shared between `thermal-conductor` (desktop/Wayland) and `thermal-term`
//! (Android/thermobile):
//!
//! - **`osc633`** — OSC 633 shell-integration parser and command tracker.
//! - **`input`**  — Platform-agnostic key encoding (KeyCode + Modifiers -> PTY bytes).
//! - **`pty`**    — PtySession: fork/exec + blocking reader thread.
//! - **`terminal`** — TerminalSize (Dimensions impl for alacritty_terminal).

pub mod input;
pub mod osc633;
pub mod pty;
pub mod terminal;
