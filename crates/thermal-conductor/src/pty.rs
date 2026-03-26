//! Direct PTY session management.
//!
//! Re-exports [`PtySession`] from the shared `thermal-terminal` crate.
//! The core fork/exec/reader-thread logic lives there; this module provides
//! the same public API that the rest of thermal-conductor already uses.

pub use thermal_terminal::pty::*;
