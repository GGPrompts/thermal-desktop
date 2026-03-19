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

    tracing::info!("thermal-hud v{}", env!("CARGO_PKG_VERSION"));

    wayland::run().await?;

    Ok(())
}
