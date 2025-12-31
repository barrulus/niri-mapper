//! niri-mapper CLI
//!
//! Control and configuration tool for niri-mapper.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use miette::IntoDiagnostic;

#[derive(Parser, Debug)]
#[command(name = "niri-mapper")]
#[command(about = "Input remapping tool for niri")]
#[command(version)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "~/.config/niri-mapper/config.kdl")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Validate the configuration file
    Validate,

    /// List available input devices
    Devices,

    /// Generate niri keybinds KDL file
    Generate {
        /// Output path (overrides config setting)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Show current daemon status
    Status,

    /// Reload daemon configuration
    Reload,
}

fn main() -> miette::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    // Expand tilde in config path
    let config_path: PathBuf = shellexpand::tilde(&cli.config).into_owned().into();

    match cli.command {
        Commands::Validate => cmd_validate(&config_path),
        Commands::Devices => cmd_devices(),
        Commands::Generate { output } => cmd_generate(&config_path, output),
        Commands::Status => cmd_status(),
        Commands::Reload => cmd_reload(),
    }
}

fn cmd_validate(config_path: &PathBuf) -> miette::Result<()> {
    println!("Validating configuration: {}", config_path.display());

    match niri_mapper_config::parse_config(config_path) {
        Ok(config) => {
            println!("Configuration is valid!");
            println!("  Devices: {}", config.devices.len());
            for device in &config.devices {
                println!(
                    "    - {} ({} profile(s))",
                    device.name.as_deref().unwrap_or("<unnamed>"),
                    device.profiles.len()
                );
            }
            Ok(())
        }
        Err(e) => Err(miette::miette!("{}", e)),
    }
}

fn cmd_devices() -> miette::Result<()> {
    println!("Available input devices:\n");

    for entry in std::fs::read_dir("/dev/input").into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();

        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("event"))
            .unwrap_or(false)
        {
            continue;
        }

        match evdev::Device::open(&path) {
            Ok(device) => {
                let name = device.name().unwrap_or("Unknown");
                let id = device.input_id();
                let vendor_product = format!("{:04x}:{:04x}", id.vendor(), id.product());

                // Check if it's a keyboard
                let is_keyboard = device.supported_events().contains(evdev::EventType::KEY)
                    && device
                        .supported_keys()
                        .map(|keys| keys.contains(evdev::Key::KEY_A))
                        .unwrap_or(false);

                let device_type = if is_keyboard { "keyboard" } else { "other" };

                println!("  {} [{}]", name, device_type);
                println!("    Path: {}", path.display());
                println!("    ID: {}", vendor_product);
                println!();
            }
            Err(_) => {
                // Skip devices we can't open
            }
        }
    }

    Ok(())
}

fn cmd_generate(config_path: &PathBuf, output: Option<PathBuf>) -> miette::Result<()> {
    let mut config =
        niri_mapper_config::parse_config(config_path).map_err(|e| miette::miette!("{}", e))?;

    if let Some(output_path) = output {
        config.global.niri_keybinds_path = output_path;
    }

    let content = niri_mapper_config::generate_niri_keybinds(&config);

    // Ensure parent directory exists
    if let Some(parent) = config.global.niri_keybinds_path.parent() {
        std::fs::create_dir_all(parent).into_diagnostic()?;
    }

    std::fs::write(&config.global.niri_keybinds_path, &content).into_diagnostic()?;

    println!(
        "Generated niri keybinds: {}",
        config.global.niri_keybinds_path.display()
    );
    println!("\nAdd this to your niri config.kdl:");
    println!(
        "  include \"{}\"",
        config.global.niri_keybinds_path.display()
    );

    Ok(())
}

fn cmd_status() -> miette::Result<()> {
    // TODO: Communicate with daemon via IPC
    println!("Status: not implemented yet");
    println!("(daemon IPC not yet implemented)");
    Ok(())
}

fn cmd_reload() -> miette::Result<()> {
    // TODO: Send SIGHUP to daemon
    println!("Reload: not implemented yet");
    println!("(daemon IPC not yet implemented)");
    Ok(())
}
