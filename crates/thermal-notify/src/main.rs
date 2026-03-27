mod audio;
mod dbus;
mod renderer;
mod stack;
mod surface;
mod timer;
mod urgency;

pub use urgency::Urgency;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use clap::Parser;

use audio::AudioPlayer;
use dbus::{NotificationQueue, NotificationServer};
use renderer::NotificationRenderer;
use stack::NotificationStack;
use surface::NotifySurface;

/// Notification popup dimensions (pixels).
const NOTIF_WIDTH: u32 = 380;
const NOTIF_HEIGHT: u32 = 100;

#[derive(Parser)]
#[command(about = "Thermal notification daemon")]
struct Cli {
    /// Notification sound volume (0-100)
    #[arg(long, default_value = "100")]
    volume: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();

    tracing::info!("thermal-notify v{} starting (volume={})", env!("CARGO_PKG_VERSION"), cli.volume);

    // Try to initialise audio; failure is non-fatal
    let audio: Option<Arc<AudioPlayer>> = match AudioPlayer::new(cli.volume) {
        Ok(a) => {
            tracing::info!("Audio player initialised");
            Some(Arc::new(a))
        }
        Err(e) => {
            tracing::warn!("Audio unavailable: {e}");
            None
        }
    };

    // Shared notification queue (std::sync::Mutex so spawn_blocking can lock it without .await)
    let queue: NotificationQueue = Arc::new(Mutex::new(VecDeque::new()));

    // Build the D-Bus server
    let server = NotificationServer::new(Arc::clone(&queue), audio);

    // Connect to the session bus, request the well-known name, and serve
    let _conn = zbus::connection::Builder::session()?
        .name("org.freedesktop.Notifications")?
        .serve_at("/org/freedesktop/Notifications", server)?
        .build()
        .await?;

    tracing::info!("D-Bus service registered — waiting for notifications");

    // Run the Wayland render loop in a blocking task so it doesn't block tokio.
    let queue_clone = Arc::clone(&queue);
    let _render_task = tokio::task::spawn_blocking(move || {
        tracing::info!("Wayland render loop starting");

        // Set up Wayland surface + wgpu
        let mut notify_surface = match NotifySurface::new(NOTIF_WIDTH, NOTIF_HEIGHT) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to create Wayland surface: {e}");
                return;
            }
        };

        // Clone the Arc<Device> and Arc<Queue> from the surface so the renderer
        // can share them without unsafe ptr reads.
        let device = Arc::clone(&notify_surface.device);
        let gpu_queue = Arc::clone(&notify_surface.queue);
        let format = notify_surface.surface_config.format;

        let mut renderer =
            NotificationRenderer::new(device, gpu_queue, format, NOTIF_WIDTH, NOTIF_HEIGHT);

        let mut stack = NotificationStack::new();
        let mut last_tick = std::time::Instant::now();

        loop {
            // Dispatch pending Wayland events (non-blocking)
            if let Err(e) = notify_surface.dispatch() {
                tracing::warn!("Wayland dispatch error: {e}");
            }

            // Drain the incoming notification queue into the stack
            {
                let mut q = queue_clone.lock().unwrap_or_else(|e| e.into_inner());
                while let Some(notif) = q.pop_front() {
                    tracing::debug!(
                        id = notif.id,
                        "Pushing notification to stack: {}",
                        notif.summary
                    );
                    stack.push(notif);
                    notify_surface.visible = true;
                }
            }

            // Advance animation/timer state
            let now = std::time::Instant::now();
            let dt = now.duration_since(last_tick).as_secs_f32();
            last_tick = now;
            stack.tick(dt);

            // Render visible notifications, or clear to transparent when idle
            if let Some(active) = stack.iter_visible().next() {
                renderer.set_alpha(active.alpha());
                let urgency_color = active.notif.urgency.to_color();

                match notify_surface.get_current_texture() {
                    Ok(output) => {
                        let view = output
                            .texture
                            .create_view(&wgpu::TextureViewDescriptor::default());
                        renderer.render(
                            &view,
                            &active.notif,
                            urgency_color,
                            NOTIF_WIDTH,
                            NOTIF_HEIGHT,
                        );
                        // Request next frame callback before presenting so the
                        // compositor continues scheduling redraws when occluded.
                        notify_surface.request_frame();
                        output.present();
                    }
                    Err(e) => {
                        tracing::warn!("Failed to acquire surface texture: {e}");
                    }
                }
            } else if notify_surface.visible {
                notify_surface.clear_transparent();
                notify_surface.visible = false;
            }

            // ~60 fps tick rate
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    });

    // Keep alive until Ctrl-C
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down thermal-notify");

    Ok(())
}
