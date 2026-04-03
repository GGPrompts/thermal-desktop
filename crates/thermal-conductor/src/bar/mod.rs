//! Status bar layer-shell surface — managed within conductor.
//!
//! Shows system metrics (CPU, GPU, memory, network), workspace map,
//! voice status, agent session overview, and clock. Renders via wgpu
//! on a Wayland layer-shell surface anchored to the top of the screen
//! (32px exclusive zone).
//!
//! Unlike the former standalone `thermal-bar` binary, this module reads
//! agent session state directly from the conductor's `SemanticEventBus`
//! — no D-Bus queries needed.

pub(crate) mod layout;
pub(crate) mod metrics;
pub(crate) mod modules;
pub(crate) mod renderer;
pub(crate) mod sparkline;
pub(crate) mod wayland;

use std::sync::Arc;
use std::thread;

use tracing::{info, warn};

use crate::semantic_state::SemanticEventBus;

/// Spawn the bar layer-shell surface on a dedicated thread.
///
/// The bar runs a blocking Wayland event loop, so it cannot share a
/// tokio task. It reads session state directly from the provided
/// `SemanticEventBus` (zero-copy, in-process).
///
/// Returns a `JoinHandle` that can be used to detect if the bar thread
/// exits unexpectedly.
pub(crate) fn spawn(event_bus: Arc<SemanticEventBus>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("bar-surface".into())
        .spawn(move || {
            info!("Bar surface thread starting");
            match wayland::run(event_bus) {
                Ok(()) => info!("Bar surface thread exited cleanly"),
                Err(e) => warn!("Bar surface thread error: {e}"),
            }
        })
        .expect("failed to spawn bar thread")
}
