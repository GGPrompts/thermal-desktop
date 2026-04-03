//! Thermal Core — shared library for the thermal desktop suite.
//! Provides the color palette, agent state types, pane metadata, and
//! configuration types used across all thermal components.
//!
//! Protocol types and runtime helpers are provided by `thermal-protocol` and
//! `thermal-runtime` respectively. This crate re-exports them for backward
//! compatibility — existing consumers can continue to `use thermal_core::*`.

pub mod claude_state;
pub mod geometry;
pub mod ggl_types;
pub mod palette;
pub mod session;
pub mod text;
pub mod wgpu_ctx;

// Thin re-export modules that delegate to thermal-protocol.
pub mod config {
    pub use thermal_protocol::config::*;
}
pub mod message {
    pub use thermal_protocol::message::*;
}
pub mod pane {
    pub use thermal_protocol::pane::*;
}
pub mod state {
    pub use thermal_protocol::state::*;
}

// Re-export thermal-runtime as the `runtime` module.
pub mod runtime {
    pub use thermal_runtime::runtime::*;
}

pub use claude_state::{
    ClaudeSessionState, ClaudeStatePoller, ClaudeStatus, SessionStateExt, is_known_model,
    model_display_name,
};
pub use ggl_types::{ToolArgs, ToolDetails};
pub use message::{AgentId, Message, MessageType, ParseAgentIdError, TaskState};
// Generalized aliases — prefer these in new code.
pub use claude_state::{AgentSessionState, AgentStatePoller, AgentStatus};
pub use config::{ConductorConfig, Layout};
pub use geometry::{Point, Rect, Size};
pub use palette::{
    Color, ThermalPalette, heat_label, thermal_gradient, thermal_gradient_f32, thermal_gradient_lut,
};
pub use pane::PaneInfo;
pub use session::{TerminalManager, TerminalSession};
pub use state::AgentState;
pub use text::ThermalTextRenderer;
pub use wgpu_ctx::WgpuContext;
