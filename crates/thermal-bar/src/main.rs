pub mod dbus;
pub mod layout;
pub mod metrics;
pub mod modules;
pub mod renderer;
pub mod sparkline;
pub mod wayland;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("thermal_bar=debug".parse().unwrap()),
        )
        .init();

    let pidfile = thermal_core::runtime::pidfile_path("bar");
    thermal_core::runtime::enforce_single_instance_at("thermal-bar", &pidfile);
    let _ = thermal_core::runtime::write_pidfile("thermal-bar", &pidfile);

    tracing::info!("thermal-bar v{}", env!("CARGO_PKG_VERSION"));

    let result = wayland::run().await;

    thermal_core::runtime::remove_pidfile("thermal-bar", &pidfile);

    result
}
