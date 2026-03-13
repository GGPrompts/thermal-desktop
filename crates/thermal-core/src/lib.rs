//! Thermal Core — shared library for the thermal desktop suite.
//! Provides the color palette, agent state types, pane metadata, and
//! configuration types used across all thermal components.

pub mod config;
pub mod geometry;
pub mod palette;
pub mod pane;
pub mod state;
pub mod text;
pub mod wgpu_ctx;

pub use config::{ConductorConfig, Layout};
pub use geometry::{Point, Rect, Size};
pub use palette::{heat_label, thermal_gradient, thermal_gradient_f32, thermal_gradient_lut, Color, ThermalPalette};
pub use pane::PaneInfo;
pub use state::AgentState;
pub use text::ThermalTextRenderer;
pub use wgpu_ctx::WgpuContext;
