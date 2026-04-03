//! Re-exports ggl-generated types from `thermal-protocol` and adds
//! rendering-related helpers that depend on `palette::Color`.

pub use thermal_protocol::ggl_types::*;

use crate::palette::Color;

/// Returns the thermal-palette `Color` that represents the given `AgentState`.
///
/// This lives in thermal-core (not thermal-protocol) because it depends on
/// `palette::Color`. Use this free function instead of a method when you need
/// the color mapping.
pub fn agent_state_color(state: AgentState) -> Color {
    match state {
        AgentState::Idle => Color::ACCENT_COOL,
        AgentState::Running => Color::WARM,
        AgentState::Thinking => Color::HOT,
        AgentState::Warning => Color::HOTTER,
        AgentState::Error => Color::SEARING,
        AgentState::Complete => Color::WHITE_HOT,
    }
}
