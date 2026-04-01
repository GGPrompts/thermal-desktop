pub mod daemon_subscriber;
pub mod renderer;
pub mod voice;
pub mod wayland;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("thermal_hud=debug".parse().unwrap()),
        )
        .init();

    let pidfile = thermal_core::runtime::pidfile_path("hud");
    thermal_core::runtime::enforce_single_instance_at("thermal-hud", &pidfile);
    let _ = thermal_core::runtime::write_pidfile("thermal-hud", &pidfile);

    tracing::info!("thermal-hud v{}", env!("CARGO_PKG_VERSION"));

    let result = wayland::run().await;

    thermal_core::runtime::remove_pidfile("thermal-hud", &pidfile);

    result
}
