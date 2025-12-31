//! niri-mapper daemon
//!
//! Grabs input devices and remaps keys according to configuration.

mod device;
mod injector;
mod remapper;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use evdev::Device;
use futures::stream::{SelectAll, StreamExt};
use niri_mapper_config::Config;
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::EnvFilter;

use device::DeviceInfo;
use injector::{create_shared_virtual_device, SharedVirtualDevice};
use remapper::Remapper;

/// Load and parse configuration from the given path
///
/// This function handles:
/// 1. Parsing the KDL configuration file
/// 2. Logging the number of devices configured
///
/// # Arguments
///
/// * `path` - Path to the configuration file
///
/// # Errors
///
/// Returns an error if:
/// - The configuration file cannot be read
/// - The configuration file is invalid KDL
/// - The configuration fails validation
fn load_config(path: &Path) -> Result<Config> {
    tracing::info!("Loading configuration from {}", path.display());

    let config = niri_mapper_config::parse_config(path)?;

    tracing::info!(
        "Loaded configuration with {} device(s)",
        config.devices.len()
    );

    Ok(config)
}

/// A grabbed input device with its remapper and metadata
///
/// This struct encapsulates all the state needed to process events
/// from a single grabbed device in the event loop.
pub struct GrabbedDevice {
    /// The evdev device (grabbed for exclusive access)
    device: Device,
    /// The remapper that processes events according to the profile
    remapper: Remapper,
    /// Metadata about the device (path, name, vendor/product IDs)
    info: DeviceInfo,
}

/// Grab all devices that match the configuration
///
/// For each device configured in `config.devices`, this function:
/// 1. Finds the matching physical device using `device::find_matching_devices()`
/// 2. Opens and grabs the device for exclusive access
/// 3. Creates a Remapper for the "default" profile
/// 4. Wraps everything in a GrabbedDevice struct
///
/// # Errors
///
/// Returns an error if:
/// - No devices match the configuration
/// - A device cannot be opened (permissions, not found, etc.)
/// - A device cannot be grabbed (already in use, etc.)
/// - The "default" profile is missing for a device
fn grab_configured_devices(config: &Config) -> Result<Vec<GrabbedDevice>> {
    let matched_devices = device::find_matching_devices(config)?;

    let mut grabbed_devices = Vec::new();

    for (device_info, config_idx) in matched_devices {
        let device_config = &config.devices[config_idx];

        tracing::info!(
            "Grabbing device: {} ({})",
            device_info.name,
            device_info.vendor_product()
        );

        // Open the device
        let mut device = Device::open(&device_info.path).with_context(|| {
            format!(
                "Failed to open device '{}' at {}",
                device_info.name,
                device_info.path.display()
            )
        })?;

        // Grab for exclusive access - fail hard if this doesn't work
        device.grab().with_context(|| {
            format!(
                "Failed to grab device '{}' for exclusive access. \
                 Is another application using this device?",
                device_info.name
            )
        })?;

        tracing::debug!("Successfully grabbed device: {}", device_info.name);

        // Get the default profile
        let default_profile = device_config.profiles.get("default").with_context(|| {
            format!(
                "Device '{}' has no 'default' profile defined. \
                 Please add a 'profile \"default\" {{ ... }}' block to the device configuration.",
                device_info.name
            )
        })?;

        // Create the remapper from the default profile
        let remapper = Remapper::from_profile(default_profile);

        grabbed_devices.push(GrabbedDevice {
            device,
            remapper,
            info: device_info,
        });
    }

    tracing::info!(
        "Successfully grabbed {} device(s)",
        grabbed_devices.len()
    );

    Ok(grabbed_devices)
}

/// Run the main event loop, processing events from all grabbed devices
///
/// This function:
/// 1. Converts each grabbed device into an async event stream
/// 2. Merges all streams using `SelectAll`
/// 3. Processes each event through the device's `Remapper`
/// 4. Injects remapped events via the shared `VirtualDevice`
///
/// The loop runs indefinitely until an error occurs or a shutdown signal
/// (SIGTERM or SIGINT) is received.
///
/// # Arguments
///
/// * `grabbed_devices` - Vector of devices that have been grabbed for exclusive access
/// * `virtual_device` - Shared virtual device for injecting remapped events
/// * `config_path` - Path to the configuration file (for reload on SIGHUP)
///
/// # Errors
///
/// Returns an error if:
/// - Converting a device to an event stream fails
/// - Reading events from a device fails
/// - Injecting events via the virtual device fails
/// - Setting up signal handlers fails
async fn run_event_loop(
    grabbed_devices: Vec<GrabbedDevice>,
    virtual_device: SharedVirtualDevice,
    config_path: &Path,
) -> Result<()> {
    // Set up signal handlers for graceful shutdown and reload
    let mut sigterm = signal(SignalKind::terminate())
        .context("Failed to set up SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt())
        .context("Failed to set up SIGINT handler")?;
    let mut sighup = signal(SignalKind::hangup())
        .context("Failed to set up SIGHUP handler")?;

    // Build a list of (Remapper, DeviceInfo) indexed by device position
    // We need to store these separately since we consume the Device to create the stream
    let mut remappers: Vec<Remapper> = Vec::with_capacity(grabbed_devices.len());
    let mut device_infos: Vec<DeviceInfo> = Vec::with_capacity(grabbed_devices.len());
    let mut streams: SelectAll<futures::stream::BoxStream<'static, (usize, std::io::Result<evdev::InputEvent>)>> = SelectAll::new();

    for (idx, grabbed_device) in grabbed_devices.into_iter().enumerate() {
        let GrabbedDevice { device, remapper, info } = grabbed_device;

        remappers.push(remapper);
        device_infos.push(info);

        // Convert the device to an async event stream
        let event_stream = device.into_event_stream().with_context(|| {
            format!(
                "Failed to create event stream for device '{}'",
                device_infos[idx].name
            )
        })?;

        // Wrap the stream to include the device index with each event
        let indexed_stream = event_stream.map(move |event| (idx, event));
        streams.push(Box::pin(indexed_stream));
    }

    tracing::info!("Event loop starting with {} device stream(s)", remappers.len());

    // Main event loop
    loop {
        tokio::select! {
            Some((device_idx, event_result)) = streams.next() => {
                match event_result {
                    Ok(event) => {
                        let device_name = &device_infos[device_idx].name;
                        let remapper = &mut remappers[device_idx];

                        // Process the event through the remapper
                        let remapped_events = remapper.process(event);

                        // Inject remapped events via the virtual device
                        if !remapped_events.is_empty() {
                            let mut vd = virtual_device.lock().await;
                            if let Err(e) = vd.emit(&remapped_events) {
                                tracing::error!(
                                    "Failed to inject events for device '{}': {}",
                                    device_name,
                                    e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        let device_name = &device_infos[device_idx].name;
                        tracing::error!(
                            "Error reading event from device '{}': {}",
                            device_name,
                            e
                        );
                        // Continue processing other devices even if one has an error
                        // A more robust implementation might attempt to re-grab the device
                    }
                }
            }
            // Handle SIGTERM for graceful shutdown (e.g., from systemctl stop)
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, initiating graceful shutdown...");
                break;
            }
            // Handle SIGINT for graceful shutdown (e.g., Ctrl+C)
            _ = sigint.recv() => {
                tracing::info!("Received SIGINT, initiating graceful shutdown...");
                break;
            }
            // Handle SIGHUP for configuration reload
            //
            // ## Configuration Reload Behavior (SIGHUP)
            //
            // When SIGHUP is received, the daemon performs a "hot reload" of the
            // configuration. This updates remapper rules without restarting or
            // re-grabbing devices.
            //
            // ### Can be reloaded (SIGHUP):
            // - Remap rules (1:1 key remappings)
            // - Combo rules (multi-key sequences)
            // - niri-passthrough keybinds
            // - Profile settings within existing devices (rules are rebuilt from config)
            //
            // ### Requires restart:
            // - Adding new devices to config (daemon only grabs devices at startup)
            // - Removing devices from config (already-grabbed devices keep running)
            // - Changing device names (matching is done at startup)
            // - Adding new profiles (device structure changes require restart)
            // - Changing which profile is active (future: profile switching will
            //   allow dynamic profile changes)
            //
            // ### Error handling:
            // If configuration parsing fails, the daemon logs the error and
            // continues running with the previous (working) configuration.
            // This ensures a typo in the config file doesn't crash the daemon.
            // The user should fix the config and send SIGHUP again.
            //
            _ = sighup.recv() => {
                tracing::info!("SIGHUP received, reloading configuration...");

                match load_config(config_path) {
                    Ok(new_config) => {
                        tracing::info!(
                            "Configuration reloaded successfully with {} device(s)",
                            new_config.devices.len()
                        );

                        // Hot-swap remappers for existing grabbed devices
                        let mut updated_count = 0;
                        let mut not_found_count = 0;

                        for (idx, device_info) in device_infos.iter().enumerate() {
                            // Find the matching device config in the new config by name
                            let matching_device_config = new_config.devices.iter().find(|dc| {
                                dc.name.as_ref() == Some(&device_info.name)
                            });

                            match matching_device_config {
                                Some(device_config) => {
                                    // Get the default profile from the new config
                                    match device_config.profiles.get("default") {
                                        Some(default_profile) => {
                                            // Create a new remapper and replace the old one
                                            let new_remapper = Remapper::from_profile(default_profile);
                                            remappers[idx] = new_remapper;
                                            tracing::info!(
                                                "Updated remapper for device '{}' with new configuration",
                                                device_info.name
                                            );
                                            updated_count += 1;
                                        }
                                        None => {
                                            tracing::warn!(
                                                "Device '{}' in new config has no 'default' profile, keeping old remapper",
                                                device_info.name
                                            );
                                        }
                                    }
                                }
                                None => {
                                    tracing::warn!(
                                        "Device '{}' not found in new config, keeping old remapper",
                                        device_info.name
                                    );
                                    not_found_count += 1;
                                }
                            }
                        }

                        tracing::info!(
                            "Remapper hot-swap complete: {} updated, {} kept (not in new config)",
                            updated_count,
                            not_found_count
                        );

                        // Regenerate niri keybinds after successful config reload
                        match niri_mapper_config::write_niri_keybinds(&new_config, &config_path) {
                            Ok(()) => {
                                tracing::info!(
                                    "Regenerated niri keybinds at {}",
                                    new_config.global.niri_keybinds_path.display()
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to regenerate niri keybinds: {}",
                                    e
                                );
                                // Continue running - keybind generation failure shouldn't
                                // stop the daemon, but the user should be aware
                            }
                        }
                    }
                    Err(e) => {
                        // Log the error with full details
                        tracing::error!("Failed to reload configuration: {}", e);

                        // Log the full error chain for debugging
                        // Using Debug format to capture all error context
                        tracing::debug!("Configuration reload error details: {:?}", e);

                        // Log the error chain (anyhow captures the full cause chain)
                        for (i, cause) in e.chain().skip(1).enumerate() {
                            tracing::error!("  Caused by [{}]: {}", i + 1, cause);
                        }

                        // Explicitly inform that the old configuration remains active
                        tracing::warn!(
                            "Configuration reload failed - continuing with previous configuration. \
                             Fix the configuration file and send SIGHUP again to retry."
                        );
                    }
                }
            }
            // All device streams have ended (shouldn't happen under normal operation)
            else => {
                tracing::warn!("All device streams have ended unexpectedly");
                break;
            }
        }
    }

    // Clean shutdown: streams and virtual_device will be dropped here,
    // which releases the grabbed devices (evdev ungrab happens on Drop)
    // and destroys the virtual device
    tracing::info!(
        "Releasing {} device(s) and cleaning up...",
        device_infos.len()
    );

    Ok(())
}

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

    // Load configuration using the reusable function
    let config = load_config(&config_path)?;

    // Generate niri keybinds
    niri_mapper_config::write_niri_keybinds(&config, &config_path)
        .context("Failed to generate niri keybinds")?;

    // Grab all configured devices
    let grabbed_devices = grab_configured_devices(&config)?;

    // Create the virtual device for output injection
    let virtual_device = create_shared_virtual_device("niri-mapper")
        .context("Failed to create virtual keyboard device")?;

    tracing::info!("niri-mapper daemon starting...");

    // Run the event loop (handles SIGTERM/SIGINT for graceful shutdown, SIGHUP for config reload)
    run_event_loop(grabbed_devices, virtual_device, &config_path).await?;

    tracing::info!("Shutting down...");

    Ok(())
}
