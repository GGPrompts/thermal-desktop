//! Thermal Conductor — Native GPU-rendered agent dashboard.
//!
//! A wall of terminal panes, each running a Claude agent session.
//! Thermal state indicators, PipeWire audio cues, git diff awareness.

mod ansi;
mod audio;
mod state_detector;
mod tmux;

use thermal_core::ThermalPalette;

fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("◉ THERMAL CONDUCTOR — Initializing...");

    println!("thermal-conductor v{}", env!("CARGO_PKG_VERSION"));
    println!("Palette BG: {:?}", ThermalPalette::BG);

    // ── tmux integration smoke-test ──────────────────────────────────────────
    match tmux::TmuxSession::new("thermal-test") {
        Ok(session) => {
            println!("Session created: {}", session.session_name);
            println!("Panes: {:?}", session.pane_ids);

            if let Some(pane_id) = session.pane_ids.first() {
                match session.capture_pane(pane_id, None) {
                    Ok(content) => println!("Captured {} chars", content.len()),
                    Err(e) => println!("Capture error: {e}"),
                }

                match session.list_panes() {
                    Ok(panes) => {
                        for p in &panes {
                            println!(
                                "  Pane {} — {}×{} cmd={} active={}",
                                p.id, p.width, p.height, p.command, p.active
                            );
                        }
                    }
                    Err(e) => println!("list_panes error: {e}"),
                }
            }

            // Clean up test session — ignore errors (already gone is fine).
            let _ = session.kill_session();
        }
        Err(e) => println!("Error: {e}"),
    }
}
