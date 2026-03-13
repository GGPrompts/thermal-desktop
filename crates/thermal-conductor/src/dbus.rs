//! D-Bus interface for thermal-conductor.
//!
//! Implements the `org.thermal.Conductor` interface via `zbus`.
//! Methods are stubbed — real wiring to `Conductor` happens in a later pass
//! once the async event loop is in place.

use std::sync::{Arc, Mutex};

use zbus::interface;

/// The D-Bus object that exposes the thermal-conductor control interface.
///
/// Method implementations are stubs that return placeholder values.
/// Real dispatch to `Conductor` will be added when the zbus connection
/// is integrated into the winit event loop.
#[allow(dead_code)]
pub struct ConductorInterface {
    /// List of known pane IDs.
    pub panes: Arc<Mutex<Vec<String>>>,
    /// Currently focused pane ID.
    pub active_pane: Arc<Mutex<String>>,
    /// Current layout name: `"grid"`, `"sidebar"`, or `"stack"`.
    pub layout: Arc<Mutex<String>>,
}

#[interface(name = "org.thermal.Conductor")]
impl ConductorInterface {
    // ── Methods ───────────────────────────────────────────────────────────────

    /// Create a new pane (stub — returns a placeholder pane ID).
    fn create_pane(&self, _command: &str) -> String {
        let mut panes = self.panes.lock().unwrap();
        let id = format!("pane-{}", panes.len());
        panes.push(id.clone());
        id
    }

    /// Send keys to a pane (stub — no-op until conductor is wired).
    fn send_keys(&self, _pane_id: &str, _keys: &str) {}

    /// Return the content of a pane as a plain string (stub).
    fn get_pane_content(&self, pane_id: &str, _lines: i32) -> String {
        format!("[stub] content of {pane_id}")
    }

    /// Focus a pane (stub).
    fn focus_pane(&self, pane_id: &str) {
        let mut active = self.active_pane.lock().unwrap();
        *active = pane_id.to_owned();
    }

    /// Set the layout (stub — updates the stored layout name).
    fn set_layout(&self, layout: &str) {
        let mut l = self.layout.lock().unwrap();
        *l = layout.to_owned();
    }

    /// Return the agent state for a pane (stub).
    fn get_agent_state(&self, _pane_id: &str) -> String {
        "idle".to_owned()
    }

    // ── Properties ────────────────────────────────────────────────────────────

    /// All known pane IDs.
    #[zbus(property)]
    fn panes(&self) -> Vec<String> {
        self.panes.lock().unwrap().clone()
    }

    /// Currently focused pane ID.
    #[zbus(property)]
    fn active_pane(&self) -> String {
        self.active_pane.lock().unwrap().clone()
    }

    /// Current layout name.
    #[zbus(property)]
    fn layout(&self) -> String {
        self.layout.lock().unwrap().clone()
    }
}
