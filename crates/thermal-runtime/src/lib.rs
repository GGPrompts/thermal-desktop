//! Thermal Runtime — runtime path logic and stale-artifact cleanup for thermal daemons.
//!
//! Provides socket path helpers, pidfile management, single-instance guards,
//! and stale socket cleanup. No GPU or async dependencies.

pub mod runtime;

pub use runtime::*;
