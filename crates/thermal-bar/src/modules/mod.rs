pub mod agent_module;
pub mod clock;
pub mod metrics_module;
pub mod voice;
pub mod workspace_map;

// Backward-compatible alias so existing `use crate::modules::claude_module::*` still works.
pub use agent_module as claude_module;
