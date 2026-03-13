//! SessionManager — wires ConductorConfig to TmuxSession.
//!
//! Builds on the low-level TmuxSession to create and manage a named tmux
//! session with the right number of panes as configured by ConductorConfig.

use thermal_core::ConductorConfig;

use crate::tmux::{TmuxError, TmuxSession};

pub struct SessionManager {
    pub session: TmuxSession,
    pub config: ConductorConfig,
}

#[allow(dead_code)]
impl SessionManager {
    /// Create (or attach to) the tmux session named in `config.tmux_session`,
    /// then ensure at least `min(config.max_panes, 4)` panes exist.
    pub fn start(config: ConductorConfig) -> Result<Self, TmuxError> {
        let mut session = TmuxSession::new(&config.tmux_session)?;

        let target_panes = config.max_panes.min(4);

        // Spawn additional panes until we reach the target count.
        while session.pane_ids.len() < target_panes {
            session.create_pane(None)?;
        }

        Ok(Self { session, config })
    }

    /// Returns the IDs of all managed panes.
    pub fn pane_ids(&self) -> &[String] {
        &self.session.pane_ids
    }

    /// Shut down the session manager.
    ///
    /// If `kill` is true the underlying tmux session is destroyed. Otherwise
    /// the tmux session is left running (useful for attaching with `tmux a`).
    pub fn shutdown(self, kill: bool) -> Result<(), TmuxError> {
        if kill {
            self.session.kill_session()?;
        }
        Ok(())
    }
}
