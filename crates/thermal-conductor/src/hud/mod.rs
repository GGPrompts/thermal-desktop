//! HUD layer-shell overlay — managed surface within conductor.
//!
//! Shows agent session tabs with display names, status labels, and voice
//! assistant state. Renders via wgpu on a Wayland layer-shell surface
//! anchored to the top of the screen (48px exclusive zone).
//!
//! Unlike the former standalone `thermal-hud` binary, this module reads
//! session state directly from the conductor's `SemanticEventBus` — no
//! socket subscription needed.

mod renderer;
mod voice;
pub(crate) mod wayland;

use std::sync::Arc;
use std::thread;

use tracing::{info, warn};

use crate::semantic_state::SemanticEventBus;

/// Spawn the HUD layer-shell surface on a dedicated thread.
///
/// The HUD runs a blocking Wayland event loop, so it cannot share a
/// tokio task. It reads session state directly from the provided
/// `SemanticEventBus` (zero-copy, in-process).
///
/// Returns a `JoinHandle` that can be used to detect if the HUD thread
/// exits unexpectedly.
pub(crate) fn spawn(event_bus: Arc<SemanticEventBus>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("hud-surface".into())
        .spawn(move || {
            info!("HUD surface thread starting");
            match wayland::run(event_bus) {
                Ok(()) => info!("HUD surface thread exited cleanly"),
                Err(e) => warn!("HUD surface thread error: {e}"),
            }
        })
        .expect("failed to spawn HUD thread")
}
