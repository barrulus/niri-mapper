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
    Validate {
        /// Also enumerate devices and check which configured devices exist (read-only)
        #[arg(long)]
        dry_run: bool,
    },

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

    /// Start the daemon via systemctl
    Start,

    /// Stop the daemon via systemctl
    Stop,

    /// Reload daemon configuration (sends SIGHUP)
    ///
    /// Triggers the daemon to re-read and apply configuration changes.
    ///
    /// ## Can be reloaded (SIGHUP):
    /// - Remap rules (1:1 key remappings)
    /// - Combo rules (multi-key sequences)
    /// - niri-passthrough keybinds
    /// - Profile settings within existing devices
    ///
    /// ## Requires restart (stop + start):
    /// - Adding/removing devices from config
    /// - Changing device names
    /// - Adding new profiles (device structure changes)
    ///
    /// If configuration parsing fails, the daemon keeps running with the
    /// previous configuration. Fix the config and reload again.
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
        Commands::Validate { dry_run } => cmd_validate(&config_path, dry_run),
        Commands::Devices => cmd_devices(),
        Commands::Generate { output } => cmd_generate(&config_path, output),
        Commands::Status => cmd_status(),
        Commands::Start => cmd_start(),
        Commands::Stop => cmd_stop(),
        Commands::Reload => cmd_reload(),
    }
}

fn cmd_validate(config_path: &PathBuf, dry_run: bool) -> miette::Result<()> {
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

            if dry_run {
                println!("\nDry run: checking device availability...");
                check_device_availability(&config)?;
            }

            Ok(())
        }
        Err(e) => Err(miette::miette!("{}", e)),
    }
}

/// Information about a system device for matching
struct SystemDevice {
    name: String,
    vendor: u16,
    product: u16,
}

impl SystemDevice {
    fn vendor_product(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor, self.product)
    }
}

/// Enumerate all system input devices (read-only, no grabbing)
fn enumerate_system_devices() -> miette::Result<Vec<SystemDevice>> {
    let mut devices = Vec::new();

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
                let name = device.name().unwrap_or("Unknown").to_string();
                let id = device.input_id();

                devices.push(SystemDevice {
                    name,
                    vendor: id.vendor(),
                    product: id.product(),
                });
            }
            Err(_) => {
                // Skip devices we can't open (permission issues, etc.)
            }
        }
    }

    // Remove duplicates by name
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    devices.dedup_by(|a, b| a.name == b.name);

    Ok(devices)
}

/// Check which configured devices exist in the system
fn check_device_availability(config: &niri_mapper_config::Config) -> miette::Result<()> {
    let system_devices = enumerate_system_devices()?;

    let mut found_count = 0;
    let mut missing_count = 0;

    for device_config in &config.devices {
        let config_name = device_config.name.as_deref().unwrap_or("<unnamed>");

        // Match by name if specified
        if let Some(ref name) = device_config.name {
            let found = system_devices.iter().find(|d| &d.name == name);

            match found {
                Some(sys_dev) => {
                    println!(
                        "  [FOUND] \"{}\" (vendor:product {})",
                        sys_dev.name,
                        sys_dev.vendor_product()
                    );
                    found_count += 1;
                }
                None => {
                    println!("  [MISSING] \"{}\"", config_name);
                    missing_count += 1;
                }
            }
        } else {
            println!("  [SKIPPED] Device without name configured");
        }
    }

    println!();
    println!(
        "Summary: {} found, {} missing",
        found_count, missing_count
    );

    if missing_count > 0 {
        println!("\nAvailable devices:");
        for sys_dev in &system_devices {
            println!("  - \"{}\" ({})", sys_dev.name, sys_dev.vendor_product());
        }
    }

    Ok(())
}

/// Represents a detected input device with its properties
struct DetectedDevice {
    name: String,
    device_type: DeviceType,
}

/// Type of input device
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum DeviceType {
    Keyboard,
    Mouse,
    Other,
}

impl DeviceType {
    fn as_tag(&self) -> Option<&'static str> {
        match self {
            DeviceType::Keyboard => Some("[keyboard]"),
            DeviceType::Mouse => Some("[mouse]"),
            DeviceType::Other => None,
        }
    }
}

fn cmd_devices() -> miette::Result<()> {
    let mut devices: Vec<DetectedDevice> = Vec::new();

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
                let name = device.name().unwrap_or("Unknown").to_string();

                // Check if it's a keyboard (has KEY events and supports letter keys)
                let is_keyboard = device.supported_events().contains(evdev::EventType::KEY)
                    && device
                        .supported_keys()
                        .map(|keys| keys.contains(evdev::Key::KEY_A))
                        .unwrap_or(false);

                // Check if it's a mouse (has relative axes for movement)
                let is_mouse = device
                    .supported_events()
                    .contains(evdev::EventType::RELATIVE)
                    && device
                        .supported_relative_axes()
                        .map(|axes| {
                            axes.contains(evdev::RelativeAxisType::REL_X)
                                && axes.contains(evdev::RelativeAxisType::REL_Y)
                        })
                        .unwrap_or(false);

                let device_type = if is_keyboard {
                    DeviceType::Keyboard
                } else if is_mouse {
                    DeviceType::Mouse
                } else {
                    DeviceType::Other
                };

                devices.push(DetectedDevice { name, device_type });
            }
            Err(_) => {
                // Skip devices we can't open
            }
        }
    }

    // Sort devices alphabetically by name (case-insensitive)
    devices.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    // Remove duplicates (same device name)
    devices.dedup_by(|a, b| a.name == b.name);

    println!("Available input devices:");
    for device in &devices {
        match device.device_type.as_tag() {
            Some(tag) => println!("  \"{}\" {}", device.name, tag),
            None => println!("  \"{}\"", device.name),
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

    let content = niri_mapper_config::generate_niri_keybinds(&config, config_path);

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
    use std::process::Command;

    let output = Command::new("systemctl")
        .args(["--user", "status", "niri-mapper"])
        .output()
        .into_diagnostic()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Check if service unit is not found
    if stderr.contains("could not be found")
        || stdout.contains("could not be found")
        || stderr.contains("Unit niri-mapper.service could not be found")
    {
        println!("Status: not installed");
        println!("The niri-mapper systemd user service is not installed.");
        println!("\nTo install, run:");
        println!("  niri-mapper install");
        return Ok(());
    }

    // Parse the Active line to determine status
    // Format: "Active: active (running) since ..."
    //     or: "Active: inactive (dead)"
    //     or: "Active: failed (Result: ...)"
    let mut status = "unknown";
    let mut pid: Option<u32> = None;

    for line in stdout.lines() {
        let line = line.trim();

        if line.starts_with("Active:") {
            if line.contains("active (running)") {
                status = "running";
            } else if line.contains("inactive") || line.contains("dead") {
                status = "stopped";
            } else if line.contains("failed") {
                status = "failed";
            } else if line.contains("activating") {
                status = "starting";
            } else if line.contains("deactivating") {
                status = "stopping";
            }
        }

        // Parse Main PID line
        // Format: "Main PID: 12345 (niri-mapper)"
        if line.starts_with("Main PID:") {
            if let Some(pid_str) = line
                .strip_prefix("Main PID:")
                .and_then(|s| s.split_whitespace().next())
            {
                pid = pid_str.parse().ok();
            }
        }
    }

    // Display status
    match status {
        "running" => {
            println!("Status: running");
            if let Some(p) = pid {
                println!("PID: {}", p);
            }
        }
        "stopped" => {
            println!("Status: stopped");
        }
        "failed" => {
            println!("Status: failed");
            println!("\nCheck logs with:");
            println!("  journalctl --user -u niri-mapper -e");
        }
        "starting" => {
            println!("Status: starting...");
        }
        "stopping" => {
            println!("Status: stopping...");
        }
        _ => {
            println!("Status: {}", status);
        }
    }

    Ok(())
}

fn cmd_start() -> miette::Result<()> {
    use std::process::Command;

    let output = Command::new("systemctl")
        .args(["--user", "start", "niri-mapper"])
        .output()
        .into_diagnostic()?;

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Check if service unit is not found
    if stderr.contains("could not be found")
        || stderr.contains("Unit niri-mapper.service could not be found")
    {
        println!("Error: niri-mapper service is not installed.");
        println!("\nTo install, run:");
        println!("  niri-mapper install");
        return Ok(());
    }

    if output.status.success() {
        println!("Started niri-mapper service.");
    } else {
        println!("Failed to start niri-mapper service.");
        if !stderr.is_empty() {
            println!("Error: {}", stderr.trim());
        }
        println!("\nCheck logs with:");
        println!("  journalctl --user -u niri-mapper -e");
    }

    Ok(())
}

fn cmd_stop() -> miette::Result<()> {
    use std::process::Command;

    let output = Command::new("systemctl")
        .args(["--user", "stop", "niri-mapper"])
        .output()
        .into_diagnostic()?;

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Check if service unit is not found
    if stderr.contains("could not be found")
        || stderr.contains("Unit niri-mapper.service could not be found")
    {
        println!("Error: niri-mapper service is not installed.");
        println!("\nTo install, run:");
        println!("  niri-mapper install");
        return Ok(());
    }

    if output.status.success() {
        println!("Stopped niri-mapper service.");
    } else {
        println!("Failed to stop niri-mapper service.");
        if !stderr.is_empty() {
            println!("Error: {}", stderr.trim());
        }
        println!("\nCheck logs with:");
        println!("  journalctl --user -u niri-mapper -e");
    }

    Ok(())
}

/// Send SIGHUP to the daemon to trigger a configuration reload.
///
/// This sends a reload signal via systemctl, which causes the daemon to:
/// 1. Re-parse the configuration file
/// 2. Rebuild remappers for all currently grabbed devices
/// 3. Regenerate the niri keybinds file
///
/// The daemon keeps the old configuration if parsing fails, so this is safe
/// to run even with a potentially broken config - just fix and reload again.
///
/// Note: This only reloads rules and settings. Device changes (adding/removing
/// devices, changing names) require a full restart via `stop` + `start`.
fn cmd_reload() -> miette::Result<()> {
    use std::process::Command;

    // Try user service first
    let user_output = Command::new("systemctl")
        .args(["--user", "reload", "niri-mapper"])
        .output()
        .into_diagnostic()?;

    let user_stderr = String::from_utf8_lossy(&user_output.stderr);

    // Check if user service exists and reload succeeded
    if user_output.status.success() {
        println!("Reload signal sent to niri-mapper daemon.");
        return Ok(());
    }

    // Check if user service is not found - try system service
    if user_stderr.contains("could not be found")
        || user_stderr.contains("Unit niri-mapper.service could not be found")
    {
        // Try system service
        let sys_output = Command::new("systemctl")
            .args(["reload", "niri-mapper"])
            .output()
            .into_diagnostic()?;

        let sys_stderr = String::from_utf8_lossy(&sys_output.stderr);

        if sys_output.status.success() {
            println!("Reload signal sent to niri-mapper daemon.");
            return Ok(());
        }

        // Check if system service is also not found
        if sys_stderr.contains("could not be found")
            || sys_stderr.contains("Unit niri-mapper.service could not be found")
        {
            println!("Error: niri-mapper service is not installed.");
            println!("\nTo install, run:");
            println!("  niri-mapper install");
            return Ok(());
        }

        // System service exists but reload failed
        println!("Failed to reload niri-mapper service.");
        if !sys_stderr.is_empty() {
            println!("Error: {}", sys_stderr.trim());
        }
        println!("\nCheck logs with:");
        println!("  journalctl -u niri-mapper -e");
        return Ok(());
    }

    // User service exists but reload failed (maybe not running)
    println!("Failed to reload niri-mapper service.");
    if !user_stderr.is_empty() {
        println!("Error: {}", user_stderr.trim());
    }
    println!("\nIs the service running? Check with:");
    println!("  niri-mapper status");

    Ok(())
}

