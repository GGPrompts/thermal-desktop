/// D-Bus client for querying thermal-conductor agent status.
///
/// Connects to org.thermal.Conductor on the session bus. Resilient: if the
/// conductor is not running or the bus is unavailable, returns empty results
/// rather than panicking.
use thermal_core::AgentState;
use zbus::Connection;

// ---------------------------------------------------------------------------
// Proxy definition
// ---------------------------------------------------------------------------

/// zbus auto-generated proxy for the thermal-conductor D-Bus interface.
#[zbus::proxy(
    interface = "org.thermal.Conductor",
    default_service = "org.thermal.Conductor",
    default_path = "/org/thermal/conductor"
)]
trait Conductor {
    /// Returns the list of active pane IDs.
    #[zbus(property)]
    fn panes(&self) -> zbus::Result<Vec<String>>;

    /// Returns the agent state string for a given pane (e.g. "idle", "running").
    async fn get_agent_state(&self, pane_id: &str) -> zbus::Result<String>;
}

// ---------------------------------------------------------------------------
// Public client wrapper
// ---------------------------------------------------------------------------

/// A resilient D-Bus client for thermal-conductor.
pub struct ConductorClient {
    conn: Option<Connection>,
}

impl ConductorClient {
    /// Connect to the session bus.  On failure, the client silently degrades to
    /// returning empty results.
    pub async fn new() -> Self {
        let conn = match Connection::session().await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::debug!("D-Bus session bus unavailable: {e}");
                None
            }
        };
        Self { conn }
    }

    /// Query all panes and their agent states from thermal-conductor.
    ///
    /// Returns an empty `Vec` if the conductor is not running or the bus is
    /// not available.
    pub async fn get_all_states(&self) -> Vec<(String, AgentState)> {
        let Some(conn) = &self.conn else {
            return Vec::new();
        };

        let proxy = match ConductorProxy::new(conn).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("Failed to create ConductorProxy: {e}");
                return Vec::new();
            }
        };

        let panes = match proxy.panes().await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("Failed to read panes property: {e}");
                return Vec::new();
            }
        };

        let mut results = Vec::with_capacity(panes.len());
        for pane_id in panes {
            let state = match proxy.get_agent_state(&pane_id).await {
                Ok(s) => parse_agent_state(&s),
                Err(e) => {
                    tracing::debug!("get_agent_state({pane_id}) failed: {e}");
                    AgentState::Idle
                }
            };
            results.push((pane_id, state));
        }
        results
    }
}

// ---------------------------------------------------------------------------
// State string parser
// ---------------------------------------------------------------------------

fn parse_agent_state(s: &str) -> AgentState {
    match s.trim().to_ascii_lowercase().as_str() {
        "running" => AgentState::Running,
        "thinking" => AgentState::Thinking,
        "warning" => AgentState::Warning,
        "error" => AgentState::Error,
        "complete" => AgentState::Complete,
        _ => AgentState::Idle,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_agent_state
    // -----------------------------------------------------------------------

    #[test]
    fn parse_agent_state_running() {
        assert_eq!(parse_agent_state("running"), AgentState::Running);
    }

    #[test]
    fn parse_agent_state_thinking() {
        assert_eq!(parse_agent_state("thinking"), AgentState::Thinking);
    }

    #[test]
    fn parse_agent_state_warning() {
        assert_eq!(parse_agent_state("warning"), AgentState::Warning);
    }

    #[test]
    fn parse_agent_state_error() {
        assert_eq!(parse_agent_state("error"), AgentState::Error);
    }

    #[test]
    fn parse_agent_state_complete() {
        assert_eq!(parse_agent_state("complete"), AgentState::Complete);
    }

    #[test]
    fn parse_agent_state_idle_for_unknown() {
        assert_eq!(parse_agent_state("unknown_state"), AgentState::Idle);
    }

    #[test]
    fn parse_agent_state_empty_string_returns_idle() {
        assert_eq!(parse_agent_state(""), AgentState::Idle);
    }

    #[test]
    fn parse_agent_state_uppercase_normalized() {
        assert_eq!(parse_agent_state("RUNNING"), AgentState::Running);
        assert_eq!(parse_agent_state("Running"), AgentState::Running);
        assert_eq!(parse_agent_state("ERROR"), AgentState::Error);
    }

    #[test]
    fn parse_agent_state_trims_whitespace() {
        assert_eq!(parse_agent_state("  running  "), AgentState::Running);
        assert_eq!(parse_agent_state("\tcomplete\n"), AgentState::Complete);
    }

    #[test]
    fn parse_agent_state_gibberish_returns_idle() {
        assert_eq!(parse_agent_state("xyzzy"), AgentState::Idle);
        assert_eq!(parse_agent_state("123"), AgentState::Idle);
    }
}
