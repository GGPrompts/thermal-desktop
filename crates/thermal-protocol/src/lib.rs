//! Thermal Protocol — lightweight wire protocol types for the thermal desktop suite.
//!
//! This crate contains ggl-generated types, message bus types, and configuration
//! schemas. It has no GPU, async, or system dependencies — making it suitable
//! for lightweight consumers like `thermal-terminal`.

pub mod config;
pub mod ggl_types;
pub mod message;
pub mod pane;
pub mod state;

pub use config::{ConductorConfig, Layout};
pub use ggl_types::{
    AgentId, AgentState, ClaudeStatus, PaneInfo, ParseAgentIdError, SessionState, TaskState,
    ToolArgs, ToolDetails,
};
pub use message::{Message, MessageType};
