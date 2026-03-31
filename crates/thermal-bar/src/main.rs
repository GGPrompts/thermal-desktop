pub mod dbus;
pub mod layout;
pub mod metrics;
pub mod modules;
pub mod renderer;
pub mod sparkline;
pub mod wayland;

fn pidfile_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()))
        .join("thermal")
        .join("bar.pid")
}

fn enforce_single_instance() {
    let pidfile = pidfile_path();
    if pidfile.exists() {
        if let Ok(contents) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = contents.trim().parse::<u32>()
            && std::path::Path::new(&format!("/proc/{pid}")).exists()
        {
            eprintln!("thermal-bar already running (pid {pid}). Exiting.");
            std::process::exit(0);
        }
        // Stale pidfile
        let _ = std::fs::remove_file(&pidfile);
    }
}

fn write_pidfile() {
    let pidfile = pidfile_path();
    if let Some(parent) = pidfile.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&pidfile, std::process::id().to_string());
}

fn cleanup_pidfile() {
    let _ = std::fs::remove_file(pidfile_path());
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("thermal_bar=debug".parse().unwrap()),
        )
        .init();

    enforce_single_instance();
    write_pidfile();

    tracing::info!("thermal-bar v{}", env!("CARGO_PKG_VERSION"));

    let result = wayland::run().await;

    cleanup_pidfile();

    result
}
