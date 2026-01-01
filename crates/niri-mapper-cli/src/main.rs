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

    /// Manage device profiles
    ///
    /// Switch between named profiles for a device or list available profiles.
    ///
    /// Examples:
    ///   niri-mapper profile "Keychron K3 Pro" gaming
    ///   niri-mapper profile --list "Keychron K3 Pro"
    Profile {
        /// List available profiles instead of switching
        #[arg(long, short)]
        list: bool,

        /// Name of the device (as configured in config.kdl)
        device_name: String,

        /// Name of the profile to switch to (required unless --list is specified)
        profile_name: Option<String>,
    },

    /// Switch a device to a specific profile
    ///
    /// This command sends a profile switch request to the running daemon via
    /// the control socket. The daemon must be running for this command to work.
    ///
    /// Examples:
    ///   niri-mapper switch-profile "Keychron K3 Pro" gaming
    ///   niri-mapper switch-profile "My Keyboard" default
    #[command(name = "switch-profile")]
    SwitchProfile {
        /// Name of the device (as configured in config.kdl)
        device: String,

        /// Name of the profile to switch to
        profile: String,
    },

    /// Query niri compositor state
    ///
    /// Connects to the niri IPC socket and queries the current focused window
    /// and workspace information. Requires niri to be running.
    ///
    /// Examples:
    ///   niri-mapper niri-status
    ///   niri-mapper niri-status --json
    #[command(name = "niri-status")]
    NiriStatus {
        /// Output as JSON instead of formatted text
        #[arg(long)]
        json: bool,
    },
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
        Commands::Profile {
            list,
            device_name,
            profile_name,
        } => {
            if list {
                cmd_profile_list(&device_name)
            } else {
                match profile_name {
                    Some(profile) => cmd_profile_switch(&device_name, &profile),
                    None => {
                        eprintln!("Error: profile name required when not using --list");
                        eprintln!("Usage: niri-mapper profile <device-name> <profile-name>");
                        eprintln!("       niri-mapper profile --list <device-name>");
                        std::process::exit(1);
                    }
                }
            }
        }
        Commands::SwitchProfile { device, profile } => cmd_switch_profile(&device, &profile),
        Commands::NiriStatus { json } => cmd_niri_status(json),
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

/// Switch the active profile for a device.
///
/// Sends a profile switch request to the daemon via IPC (Unix socket).
///
/// # Arguments
/// * `device_name` - Name of the device as configured in config.kdl
/// * `profile_name` - Name of the profile to switch to
fn cmd_profile_switch(device_name: &str, profile_name: &str) -> miette::Result<()> {
    use serde::{Deserialize, Serialize};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    // IPC request message format (matches daemon's IpcRequest)
    #[derive(Serialize)]
    struct ProfileSwitchRequest<'a> {
        #[serde(rename = "type")]
        msg_type: &'static str,
        device: &'a str,
        profile: &'a str,
    }

    // IPC response message format (matches daemon's IpcResponse)
    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum IpcResponse {
        Success {
            #[serde(default)]
            message: Option<String>,
        },
        Error {
            message: String,
        },
        #[serde(other)]
        Unknown,
    }

    // Determine socket path
    let socket_path = if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        std::path::PathBuf::from(runtime_dir).join("niri-mapper.sock")
    } else {
        let uid = unsafe { nix::libc::getuid() };
        std::path::PathBuf::from(format!("/tmp/niri-mapper-{}.sock", uid))
    };

    // Connect to the daemon
    let mut stream = UnixStream::connect(&socket_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound
            || e.kind() == std::io::ErrorKind::ConnectionRefused
        {
            miette::miette!(
                "Cannot connect to niri-mapper daemon.\n\
                 Is the daemon running? Check with: niri-mapper status"
            )
        } else {
            miette::miette!("Failed to connect to daemon: {}", e)
        }
    })?;

    // Build and send the request
    let request = ProfileSwitchRequest {
        msg_type: "profile_switch",
        device: device_name,
        profile: profile_name,
    };

    let request_json =
        serde_json::to_string(&request).map_err(|e| miette::miette!("Failed to serialize request: {}", e))?;

    writeln!(stream, "{}", request_json)
        .map_err(|e| miette::miette!("Failed to send request to daemon: {}", e))?;

    stream
        .flush()
        .map_err(|e| miette::miette!("Failed to flush request: {}", e))?;

    // Read the response
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .map_err(|e| miette::miette!("Failed to read response from daemon: {}", e))?;

    // Parse and display the response
    let response: IpcResponse = serde_json::from_str(response_line.trim())
        .map_err(|e| miette::miette!("Failed to parse daemon response: {}", e))?;

    match response {
        IpcResponse::Success { message } => {
            println!(
                "Switched device '{}' to profile '{}'.",
                device_name, profile_name
            );
            if let Some(msg) = message {
                println!("{}", msg);
            }
            Ok(())
        }
        IpcResponse::Error { message } => {
            Err(miette::miette!("Profile switch failed: {}", message))
        }
        IpcResponse::Unknown => {
            Err(miette::miette!("Unexpected response from daemon"))
        }
    }
}

/// List available profiles for a device.
///
/// Queries the daemon via IPC to get the list of profiles and which one is active.
///
/// # Arguments
/// * `device_name` - Name of the device as configured in config.kdl
fn cmd_profile_list(device_name: &str) -> miette::Result<()> {
    use serde::{Deserialize, Serialize};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    // IPC request message format (matches daemon's IpcRequest)
    #[derive(Serialize)]
    struct ProfileListRequest<'a> {
        #[serde(rename = "type")]
        msg_type: &'static str,
        device: &'a str,
    }

    // IPC response message format (matches daemon's IpcResponse)
    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum IpcResponse {
        ProfileList {
            profiles: Vec<String>,
            active: String,
        },
        Error {
            message: String,
        },
        #[serde(other)]
        Unknown,
    }

    // Determine socket path
    let socket_path = if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        std::path::PathBuf::from(runtime_dir).join("niri-mapper.sock")
    } else {
        let uid = unsafe { nix::libc::getuid() };
        std::path::PathBuf::from(format!("/tmp/niri-mapper-{}.sock", uid))
    };

    // Connect to the daemon
    let mut stream = UnixStream::connect(&socket_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound
            || e.kind() == std::io::ErrorKind::ConnectionRefused
        {
            miette::miette!(
                "Cannot connect to niri-mapper daemon.\n\
                 Is the daemon running? Check with: niri-mapper status"
            )
        } else {
            miette::miette!("Failed to connect to daemon: {}", e)
        }
    })?;

    // Build and send the request
    let request = ProfileListRequest {
        msg_type: "profile_list",
        device: device_name,
    };

    let request_json =
        serde_json::to_string(&request).map_err(|e| miette::miette!("Failed to serialize request: {}", e))?;

    writeln!(stream, "{}", request_json)
        .map_err(|e| miette::miette!("Failed to send request to daemon: {}", e))?;

    stream
        .flush()
        .map_err(|e| miette::miette!("Failed to flush request: {}", e))?;

    // Read the response
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .map_err(|e| miette::miette!("Failed to read response from daemon: {}", e))?;

    // Parse and display the response
    let response: IpcResponse = serde_json::from_str(response_line.trim())
        .map_err(|e| miette::miette!("Failed to parse daemon response: {}", e))?;

    match response {
        IpcResponse::ProfileList { profiles, active } => {
            println!("Profiles for device '{}':", device_name);
            for profile in &profiles {
                if profile == &active {
                    println!("  * {} [active]", profile);
                } else {
                    println!("    {}", profile);
                }
            }
            Ok(())
        }
        IpcResponse::Error { message } => {
            Err(miette::miette!("Failed to list profiles: {}", message))
        }
        IpcResponse::Unknown => {
            Err(miette::miette!("Unexpected response from daemon"))
        }
    }
}

/// Switch a device to a specific profile using the control socket.
///
/// This function uses the control socket format from `control.rs`:
/// - Command: `{"switch_profile": {"device": "...", "profile": "..."}}`
/// - Response: `{"success": {...}}` or `{"error": {"message": "..."}}`
///
/// The socket path is determined by `get_socket_path()` logic:
/// - `$XDG_RUNTIME_DIR/niri-mapper.sock` if XDG_RUNTIME_DIR is set
/// - `/tmp/niri-mapper-$UID.sock` otherwise
///
/// # Arguments
/// * `device` - Name of the device as configured in config.kdl
/// * `profile` - Name of the profile to switch to
fn cmd_switch_profile(device: &str, profile: &str) -> miette::Result<()> {
    use serde::{Deserialize, Serialize};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    // ControlCommand format from control.rs: {"switch_profile": {"device": "...", "profile": "..."}}
    #[derive(Serialize)]
    struct SwitchProfileArgs<'a> {
        device: &'a str,
        profile: &'a str,
    }

    #[derive(Serialize)]
    struct ControlCommand<'a> {
        switch_profile: SwitchProfileArgs<'a>,
    }

    // ControlResponse format from control.rs
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum ControlResponse {
        Success {
            #[serde(default)]
            message: Option<String>,
        },
        Error {
            message: String,
        },
        #[serde(other)]
        Unknown,
    }

    // Get socket path (mirrors get_socket_path() from control.rs)
    let socket_path = if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        std::path::PathBuf::from(runtime_dir).join("niri-mapper.sock")
    } else {
        let uid = unsafe { nix::libc::getuid() };
        std::path::PathBuf::from(format!("/tmp/niri-mapper-{}.sock", uid))
    };

    // Connect to the daemon control socket
    let mut stream = UnixStream::connect(&socket_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound
            || e.kind() == std::io::ErrorKind::ConnectionRefused
        {
            miette::miette!(
                "Cannot connect to niri-mapper daemon at {}.\n\
                 Is the daemon running? Check with: niri-mapper status",
                socket_path.display()
            )
        } else {
            miette::miette!("Failed to connect to daemon: {}", e)
        }
    })?;

    // Build and send the control command
    let command = ControlCommand {
        switch_profile: SwitchProfileArgs { device, profile },
    };

    let command_json = serde_json::to_string(&command)
        .map_err(|e| miette::miette!("Failed to serialize command: {}", e))?;

    writeln!(stream, "{}", command_json)
        .map_err(|e| miette::miette!("Failed to send command to daemon: {}", e))?;

    stream
        .flush()
        .map_err(|e| miette::miette!("Failed to flush command: {}", e))?;

    // Read the response
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .map_err(|e| miette::miette!("Failed to read response from daemon: {}", e))?;

    // Parse and display the response
    let response: ControlResponse = serde_json::from_str(response_line.trim())
        .map_err(|e| miette::miette!("Failed to parse daemon response: {}", e))?;

    match response {
        ControlResponse::Success { message } => {
            println!(
                "Switched device '{}' to profile '{}'.",
                device, profile
            );
            if let Some(msg) = message {
                println!("{}", msg);
            }
            Ok(())
        }
        ControlResponse::Error { message } => {
            Err(miette::miette!("Profile switch failed: {}", message))
        }
        ControlResponse::Unknown => {
            Err(miette::miette!("Unexpected response from daemon"))
        }
    }
}

/// Query niri compositor state (focused window and workspaces).
///
/// Connects to the niri IPC socket and queries current state.
/// Requires niri to be running.
///
/// # Arguments
/// * `json_output` - If true, output as JSON; otherwise, formatted text
fn cmd_niri_status(json_output: bool) -> miette::Result<()> {
    use serde::Serialize;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;

    // Environment variable name for the niri socket path
    const NIRI_SOCKET_ENV: &str = "NIRI_SOCKET";

    // Discover the niri IPC socket path from the environment
    let socket_path_str = std::env::var(NIRI_SOCKET_ENV).map_err(|_| {
        miette::miette!(
            "NIRI_SOCKET environment variable not set.\n\
             Is niri running? niri-mapper needs to be run from within a niri session."
        )
    })?;

    let socket_path = PathBuf::from(&socket_path_str);

    // Validate the path exists
    if !socket_path.exists() {
        return Err(miette::miette!(
            "Niri socket not found at {}.\n\
             Is niri running?",
            socket_path.display()
        ));
    }

    // Connect to niri IPC socket
    let mut stream = UnixStream::connect(&socket_path).map_err(|e| {
        miette::miette!(
            "Failed to connect to niri socket at {}: {}\n\
             Is niri running?",
            socket_path.display(),
            e
        )
    })?;

    // Helper function to send a request and read the response
    fn send_request(
        stream: &mut UnixStream,
        request: &niri_ipc::Request,
    ) -> miette::Result<niri_ipc::Response> {
        // Serialize the request to JSON
        let request_json = serde_json::to_string(request)
            .map_err(|e| miette::miette!("Failed to serialize request: {}", e))?;

        // Write the request to the socket with a newline
        writeln!(stream, "{}", request_json)
            .map_err(|e| miette::miette!("Failed to send request to niri: {}", e))?;

        stream
            .flush()
            .map_err(|e| miette::miette!("Failed to flush request: {}", e))?;

        // Read the response line
        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader
            .read_line(&mut response_line)
            .map_err(|e| miette::miette!("Failed to read response from niri: {}", e))?;

        if response_line.is_empty() {
            return Err(miette::miette!("Connection to niri closed unexpectedly"));
        }

        // Deserialize the reply (Result<Response, String>)
        let reply: niri_ipc::Reply = serde_json::from_str(&response_line)
            .map_err(|e| miette::miette!("Failed to parse niri response: {}", e))?;

        // Extract the response or convert the error
        reply.map_err(|message| miette::miette!("Niri returned error: {}", message))
    }

    // Query focused window
    let focused_window_response = send_request(&mut stream, &niri_ipc::Request::FocusedWindow)?;
    let focused_window = match focused_window_response {
        niri_ipc::Response::FocusedWindow(window) => window,
        _ => {
            return Err(miette::miette!(
                "Unexpected response to FocusedWindow request"
            ))
        }
    };

    // We need a new connection for each request since niri expects one request per connection
    let mut stream2 = UnixStream::connect(&socket_path).map_err(|e| {
        miette::miette!(
            "Failed to connect to niri socket for workspaces query: {}",
            e
        )
    })?;

    // Query workspaces
    let workspaces_response = send_request(&mut stream2, &niri_ipc::Request::Workspaces)?;
    let workspaces = match workspaces_response {
        niri_ipc::Response::Workspaces(ws) => ws,
        _ => return Err(miette::miette!("Unexpected response to Workspaces request")),
    };

    // Build output structure
    #[derive(Serialize)]
    struct NiriStatusOutput {
        focused_window: Option<FocusedWindowInfo>,
        workspaces: Vec<WorkspaceInfo>,
    }

    #[derive(Serialize)]
    struct FocusedWindowInfo {
        id: u64,
        app_id: Option<String>,
        title: Option<String>,
    }

    #[derive(Serialize)]
    struct WorkspaceInfo {
        id: u64,
        idx: u8,
        name: Option<String>,
        output: Option<String>,
        is_active: bool,
        is_focused: bool,
    }

    let output = NiriStatusOutput {
        focused_window: focused_window.map(|w| FocusedWindowInfo {
            id: w.id,
            app_id: w.app_id,
            title: w.title,
        }),
        workspaces: workspaces
            .into_iter()
            .map(|ws| WorkspaceInfo {
                id: ws.id,
                idx: ws.idx,
                name: ws.name,
                output: ws.output,
                is_active: ws.is_active,
                is_focused: ws.is_focused,
            })
            .collect(),
    };

    if json_output {
        // Output as JSON
        let json = serde_json::to_string_pretty(&output)
            .map_err(|e| miette::miette!("Failed to serialize output: {}", e))?;
        println!("{}", json);
    } else {
        // Output as formatted text
        println!("Niri Status");
        println!("===========");
        println!();

        // Focused window
        println!("Focused Window:");
        match &output.focused_window {
            Some(window) => {
                println!(
                    "  App ID: {}",
                    window.app_id.as_deref().unwrap_or("<none>")
                );
                println!("  Title:  {}", window.title.as_deref().unwrap_or("<none>"));
                println!("  ID:     {}", window.id);
            }
            None => {
                println!("  <no window focused>");
            }
        }

        println!();

        // Workspaces
        println!("Workspaces:");
        for ws in &output.workspaces {
            let name_str = ws
                .name
                .as_ref()
                .map(|n| format!(" \"{}\"", n))
                .unwrap_or_default();
            let output_str = ws
                .output
                .as_ref()
                .map(|o| format!(" on {}", o))
                .unwrap_or_default();
            let status = if ws.is_focused {
                " [focused]"
            } else if ws.is_active {
                " [active]"
            } else {
                ""
            };
            println!(
                "  #{}{}{}{} (id: {})",
                ws.idx, name_str, output_str, status, ws.id
            );
        }
    }

    Ok(())
}

