//! Thermal Conductor — Native GPU-rendered agent dashboard.
//!
//! A wall of terminal panes, each running a Claude agent session.
//! Thermal state indicators, PipeWire audio cues, git diff awareness.

mod ansi;
mod audio;
mod capture;
mod conductor;
mod dbus;
mod git_watcher;
mod hud;
mod input;
mod layout;
mod renderer;
mod session;
mod state_detector;
mod tmux;

use thermal_core::{ConductorConfig, ThermalPalette};

fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("◉ THERMAL CONDUCTOR — Initializing...");

    println!("thermal-conductor v{}", env!("CARGO_PKG_VERSION"));
    println!("Palette BG: {:?}", ThermalPalette::BG);

    // ── Session manager smoke-test ───────────────────────────────────────────
    let config = ConductorConfig::default();
    match session::SessionManager::start(config) {
        Ok(mgr) => {
            println!(
                "Session '{}' ready — {} pane(s)",
                mgr.session.session_name,
                mgr.pane_ids().len()
            );
            // Leave the session alive so the user can `tmux a` into it.
            if let Err(e) = mgr.shutdown(false) {
                println!("Shutdown error: {e}");
            }
        }
        Err(e) => println!("SessionManager error: {e}"),
    }

    // ── Renderer note ────────────────────────────────────────────────────────
    // WgpuState::new() requires an active Wayland/X11 display and an async
    // runtime (it is async). The winit EventLoop cannot run without a
    // compositor. In Docker/CI we just verify the module compiles.
    //
    // On bare-metal with a Wayland compositor, wire up like this:
    //
    //   let event_loop = winit::event_loop::EventLoop::new().unwrap();
    //   let window = Arc::new(winit::window::WindowBuilder::new()
    //       .with_title("THERMAL CONDUCTOR")
    //       .build(&event_loop).unwrap());
    //   let mut renderer = pollster::block_on(WgpuState::new(Arc::clone(&window))).unwrap();
    //   event_loop.run(move |event, target| {
    //       match event {
    //           winit::event::Event::WindowEvent { event, .. } => match event {
    //               winit::event::WindowEvent::RedrawRequested => {
    //                   let _ = renderer.render();
    //               }
    //               winit::event::WindowEvent::Resized(size) => {
    //                   renderer.resize(size);
    //               }
    //               winit::event::WindowEvent::CloseRequested => target.exit(),
    //               _ => {}
    //           },
    //           winit::event::Event::AboutToWait => {
    //               window.request_redraw();
    //           }
    //           _ => {}
    //       }
    //   }).unwrap();
    tracing::info!("◉ THERMAL CONDUCTOR — Ready (no display — running headless)");
}
