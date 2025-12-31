//! niri-mapper daemon
//!
//! Grabs input devices and remaps keys according to configuration.

mod device;
mod injector;
mod remapper;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "niri-mapperd")]
#[command(about = "Input remapping daemon for niri")]
struct Args {
    /// Path to configuration file
    #[arg(short, long, default_value = "~/.config/niri-mapper/config.kdl")]
    config: String,

    /// Run in foreground (don't daemonize)
    #[arg(short, long)]
    foreground: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Expand tilde in config path
    let config_path: PathBuf = shellexpand::tilde(&args.config).into_owned().into();

    tracing::info!("Loading configuration from {}", config_path.display());

    // Parse configuration
    let config = niri_mapper_config::parse_config(&config_path)?;

    tracing::info!(
        "Loaded configuration with {} device(s)",
        config.devices.len()
    );

    // Generate niri keybinds
    niri_mapper_config::generate_niri_keybinds(&config);
    tracing::info!(
        "Generated niri keybinds at {}",
        config.global.niri_keybinds_path.display()
    );

    // TODO: Enumerate and grab devices
    // TODO: Start event loop
    // TODO: Handle signals for graceful shutdown

    tracing::info!("niri-mapper daemon starting...");

    // For now, just run until interrupted
    tokio::signal::ctrl_c().await?;

    tracing::info!("Shutting down...");

    Ok(())
}
