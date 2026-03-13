//! Thermal Core — shared library for the thermal desktop suite.
//! Provides the color palette, agent state types, pane metadata, and
//! configuration types used across all thermal components.

pub mod config;
pub mod palette;
pub mod pane;
pub mod state;

pub use config::{ConductorConfig, Layout};
pub use palette::{thermal_gradient, Color, ThermalPalette};
pub use pane::PaneInfo;
pub use state::AgentState;
