use clap::Parser;
use tracing_subscriber::EnvFilter;
use vc_server::{Cli, config, net};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let mut cli = Cli::parse();
    if cli.config.is_none() {
        cli.config = config::find_default_config(config::default_config_dirs());
    }
    match &cli.config {
        Some(path) => tracing::info!("config file: {}", path.display()),
        None => tracing::info!("no config file, using defaults"),
    }
    let cfg = config::load(cli)?;
    let name = cfg.name.clone();
    let channels = cfg.channels.join(", ");
    let handle = net::run(cfg).await?;
    tracing::info!("server {name:?} listening on {}", handle.addr);
    tracing::info!("channels: {channels}");
    tracing::info!(
        "certificate fingerprint (SHA-256): {}",
        handle.fingerprint_hex()
    );
    tokio::signal::ctrl_c().await?;
    tracing::info!("Ctrl-C received, shutting down");
    handle.shutdown().await;
    tracing::info!("shutdown complete");
    Ok(())
}
