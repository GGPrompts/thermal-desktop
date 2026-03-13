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
use tokio::sync::Mutex;

use audio::AudioPlayer;
use dbus::{NotificationQueue, NotificationServer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("thermal-notify v{} starting", env!("CARGO_PKG_VERSION"));

    // Try to initialise audio; failure is non-fatal
    let audio: Option<Arc<AudioPlayer>> = match AudioPlayer::new() {
        Ok(a) => {
            tracing::info!("Audio player initialised");
            Some(Arc::new(a))
        }
        Err(e) => {
            tracing::warn!("Audio unavailable: {e}");
            None
        }
    };

    // Shared notification queue
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

    // Run the Wayland event loop in a blocking task so it doesn't block tokio
    let _wayland_task = tokio::task::spawn_blocking(move || {
        tracing::debug!("Wayland event loop thread started (stub)");
    });

    // Keep alive until Ctrl-C
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down thermal-notify");

    Ok(())
}
