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

    tracing::info!("thermal-bar v{}", env!("CARGO_PKG_VERSION"));

    wayland::run().await?;

    Ok(())
}
