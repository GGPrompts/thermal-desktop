use serde::{Deserialize, Serialize};

// Layout is now ggl-generated. See ggl_types.rs for the type alias and Copy impl.
pub use crate::ggl_types::Layout;

/// Top-level configuration for the thermal-conductor component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConductorConfig {
    /// Name of the tmux session to attach to or create.
    pub tmux_session: String,

    /// Maximum number of panes the conductor will track simultaneously.
    pub max_panes: usize,

    /// Target frame-rate for pane capture polling (frames per second).
    pub capture_fps: u32,

    /// Initial pane layout strategy.
    pub layout: Layout,

    /// Enable audio feedback (bell events, state-change tones).
    pub audio_enabled: bool,

    /// Enable D-Bus IPC (`org.thermal.Conductor` interface).
    pub dbus_enabled: bool,
}

impl Default for ConductorConfig {
    fn default() -> Self {
        Self {
            tmux_session: "thermal-conductor".to_string(),
            max_panes: 16,
            capture_fps: 30,
            layout: Layout::Grid,
            audio_enabled: true,
            dbus_enabled: true,
        }
    }
}
