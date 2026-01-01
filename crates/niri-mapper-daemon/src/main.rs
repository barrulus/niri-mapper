//! niri-mapper daemon
//!
//! Grabs input devices and remaps keys according to configuration.

mod control;
mod device;
mod device_manager;
mod hotplug;
mod injector;
mod ipc;
mod macro_executor;
mod niri_ipc;
mod remapper;

pub use device_manager::DeviceManager;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use evdev::Device;
use futures::stream::{SelectAll, StreamExt};
use niri_mapper_config::Config;
use tokio::signal::unix::{signal, SignalKind};
use tracing_subscriber::EnvFilter;

use device::DeviceInfo;
use hotplug::{HotplugEvent, HotplugMonitor};
use injector::{create_shared_virtual_device, SharedVirtualDevice};
use ipc::{handle_ipc_connection, DeviceStatus, IpcRequest, IpcResponse, IpcServer};
use macro_executor::MacroExecutor;
use niri_ipc::{NiriEventDispatcher, NiriEventReceiver, DEFAULT_CHANNEL_BUFFER};
use remapper::{RemapResult, Remapper};

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
    /// The name of the currently active profile for this device (Task 040-4.3)
    ///
    /// Initialized to "default" when the device is grabbed. This tracks which
    /// profile is currently active for profile switching functionality.
    active_profile: String,
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
            active_profile: "default".to_string(),
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
/// 5. Accepts IPC connections for CLI/external tool communication
/// 6. Monitors for device hotplug events (connect/disconnect)
/// 7. Dynamically adds/removes device streams on hotplug events
///
/// The loop runs indefinitely until an error occurs or a shutdown signal
/// (SIGTERM or SIGINT) is received.
///
/// # Arguments
///
/// * `grabbed_devices` - Vector of devices that have been grabbed for exclusive access
/// * `virtual_device` - Shared virtual device for injecting remapped events
/// * `macro_executor` - Executor for running macro action sequences
/// * `config_path` - Path to the configuration file (for reload on SIGHUP)
/// * `config` - The parsed configuration for device matching on hotplug
/// * `ipc_server` - Optional IPC server for CLI communication
/// * `hotplug_monitor` - Monitor for device connect/disconnect events
/// * `niri_event_receiver` - Optional receiver for niri compositor events (focus changes, etc.)
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
    macro_executor: MacroExecutor,
    config_path: &Path,
    config: Arc<Config>,
    ipc_server: Option<IpcServer>,
    mut hotplug_monitor: HotplugMonitor,
    mut niri_event_receiver: Option<NiriEventReceiver>,
) -> Result<()> {
    // Set up signal handlers for graceful shutdown and reload
    let mut sigterm = signal(SignalKind::terminate())
        .context("Failed to set up SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt())
        .context("Failed to set up SIGINT handler")?;
    let mut sighup = signal(SignalKind::hangup())
        .context("Failed to set up SIGHUP handler")?;

    // Create DeviceManager for handling hotplug events
    // The DeviceManager will be used to grab new devices on hotplug Add events
    let mut device_manager = DeviceManager::new(config.clone(), virtual_device.clone());

    // Use path-based device tracking for dynamic hotplug support
    // Maps device path -> (Remapper, DeviceInfo) for event processing
    let mut remappers: HashMap<PathBuf, Remapper> = HashMap::new();
    let mut device_infos: HashMap<PathBuf, DeviceInfo> = HashMap::new();
    // Track active profile per device path (Task 040-4.3)
    // Initialized to "default" for each grabbed device
    let mut active_profiles: HashMap<PathBuf, String> = HashMap::new();

    // Track current focused app for future per-app profile switching (Task 040-4.8)
    // Updated on focus change events from niri IPC. Currently just tracks state;
    // automatic profile switching based on focused app is a future backlog item.
    let mut current_focused_app_id: Option<String> = None;

    // Stream type: yields (device_path, event_result) for path-based device lookup
    let mut streams: SelectAll<futures::stream::BoxStream<'static, (PathBuf, std::io::Result<evdev::InputEvent>)>> = SelectAll::new();

    // Initialize streams from initially grabbed devices
    for grabbed_device in grabbed_devices {
        let GrabbedDevice { device, remapper, info, active_profile } = grabbed_device;
        let path = info.path.clone();
        let device_name = info.name.clone();

        remappers.insert(path.clone(), remapper);
        device_infos.insert(path.clone(), info);
        active_profiles.insert(path.clone(), active_profile);

        // Convert the device to an async event stream
        let event_stream = device.into_event_stream().with_context(|| {
            format!(
                "Failed to create event stream for device '{}'",
                device_name
            )
        })?;

        // Wrap the stream to include the device path with each event
        let path_for_stream = path.clone();
        let indexed_stream = event_stream.map(move |event| (path_for_stream.clone(), event));
        streams.push(Box::pin(indexed_stream));
    }

    tracing::info!("Event loop starting with {} device stream(s)", remappers.len());

    // Main event loop
    loop {
        tokio::select! {
            Some((device_path, event_result)) = streams.next() => {
                match event_result {
                    Ok(event) => {
                        // Look up device by path
                        let device_name = match device_infos.get(&device_path) {
                            Some(info) => info.name.clone(),
                            None => {
                                // Device was removed but stream hasn't ended yet
                                tracing::debug!(
                                    "Received event for unknown device at {}, ignoring",
                                    device_path.display()
                                );
                                continue;
                            }
                        };

                        let remapper = match remappers.get_mut(&device_path) {
                            Some(r) => r,
                            None => {
                                // Device was removed
                                continue;
                            }
                        };

                        // Process the event through the remapper
                        match remapper.process(event) {
                            RemapResult::Events(remapped_events) => {
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
                            RemapResult::Macro(actions) => {
                                // Spawn macro execution as an async task
                                // This allows the event loop to continue without blocking during delays
                                // Concurrent macro execution is allowed (no queuing for v0.3.0)
                                let executor = macro_executor.clone();
                                let device_name_owned = device_name.clone();
                                let action_count = actions.len();

                                tracing::debug!(
                                    "Macro triggered on device '{}' with {} actions, spawning execution",
                                    device_name,
                                    action_count
                                );

                                tokio::spawn(async move {
                                    if let Err(e) = executor.execute_macro(&actions).await {
                                        tracing::error!(
                                            "Macro execution failed on device '{}': {}",
                                            device_name_owned,
                                            e
                                        );
                                    }
                                });
                            }
                            RemapResult::ProfileSwitch(profile_name) => {
                                // Profile switching is handled by DeviceRemapper, not Remapper
                                // This case shouldn't occur when using Remapper directly
                                tracing::warn!(
                                    "Unexpected ProfileSwitch({}) from Remapper on device '{}'",
                                    profile_name,
                                    device_name
                                );
                            }
                        }
                    }
                    Err(e) => {
                        // Device error - likely disconnected
                        // Look up device info before removing
                        let device_name = device_infos
                            .get(&device_path)
                            .map(|info| info.name.clone())
                            .unwrap_or_else(|| device_path.display().to_string());

                        // Check if this is a "No such device" error indicating disconnect
                        let is_disconnect = e.raw_os_error() == Some(nix::libc::ENODEV);

                        if is_disconnect {
                            tracing::info!(
                                "Device '{}' disconnected (stream error: {})",
                                device_name,
                                e
                            );
                        } else {
                            tracing::error!(
                                "Error reading event from device '{}': {}",
                                device_name,
                                e
                            );
                        }

                        // Clean up the device from our tracking maps
                        // The stream will naturally end/be removed from SelectAll
                        remappers.remove(&device_path);
                        device_infos.remove(&device_path);
                        active_profiles.remove(&device_path);

                        tracing::info!(
                            "Removed device '{}' from event loop ({} device(s) remaining)",
                            device_name,
                            remappers.len()
                        );
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

                        for (path, device_info) in device_infos.iter() {
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
                                            if let Some(remapper) = remappers.get_mut(path) {
                                                *remapper = new_remapper;
                                                tracing::info!(
                                                    "Updated remapper for device '{}' with new configuration",
                                                    device_info.name
                                                );
                                                updated_count += 1;
                                            }
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
            // Handle device hotplug events (connect/disconnect)
            //
            // When a device is connected, we attempt to grab it if it matches our
            // configuration, then add its event stream to SelectAll for processing.
            // When a device is disconnected, the stream will error naturally and
            // cleanup is handled in the device event processing branch above.
            hotplug_event = hotplug_monitor.next_event() => {
                match hotplug_event {
                    Some(event) => {
                        match event {
                            HotplugEvent::Add { devnode } => {
                                // Skip if we already have this device
                                if device_infos.contains_key(&devnode) {
                                    tracing::debug!(
                                        "Device at {} is already grabbed, skipping",
                                        devnode.display()
                                    );
                                    continue;
                                }

                                // Try to get the device name before grabbing for better logging
                                let device_name_preview = evdev::Device::open(&devnode)
                                    .ok()
                                    .and_then(|d| d.name().map(|s| s.to_string()));

                                // Use DeviceManager to try grabbing the device
                                match device_manager.try_grab_device(&devnode) {
                                    Ok(true) => {
                                        // Device was grabbed, now get its stream and add to SelectAll
                                        // We need to extract the device from DeviceManager
                                        match device_manager.get_event_streams() {
                                            Ok(mut streams_data) => {
                                                for (path, remapper, info, active_profile, event_stream) in streams_data.drain(..) {
                                                    let device_name = info.name.clone();

                                                    tracing::info!(
                                                        "Device connected: '{}' at {} - grabbing (profile: {})",
                                                        device_name,
                                                        path.display(),
                                                        active_profile
                                                    );

                                                    // Add to our tracking maps (Task 040-4.3: include active_profile)
                                                    remappers.insert(path.clone(), remapper);
                                                    device_infos.insert(path.clone(), info);
                                                    active_profiles.insert(path.clone(), active_profile);

                                                    // Wrap stream with path for identification
                                                    let path_for_stream = path.clone();
                                                    let indexed_stream = event_stream.map(move |ev| {
                                                        (path_for_stream.clone(), ev)
                                                    });
                                                    streams.push(Box::pin(indexed_stream));

                                                    tracing::debug!(
                                                        "Device '{}' added to event loop ({} device(s) total)",
                                                        device_name,
                                                        remappers.len()
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!(
                                                    "Failed to get event stream for device at {}: {}",
                                                    devnode.display(),
                                                    e
                                                );
                                            }
                                        }
                                    }
                                    Ok(false) => {
                                        // Device doesn't match configuration - log at INFO level with device name
                                        let name = device_name_preview.unwrap_or_else(|| "Unknown Device".to_string());
                                        tracing::info!(
                                            "Device connected: '{}' at {} - not configured, ignoring",
                                            name,
                                            devnode.display()
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Device connected at {} - failed to grab: {}",
                                            devnode.display(),
                                            e
                                        );
                                    }
                                }
                            }
                            HotplugEvent::Remove { devnode } => {
                                // Look up device name for logging before removing
                                let device_name = device_infos
                                    .get(&devnode)
                                    .map(|info| info.name.clone());

                                if let Some(name) = device_name {
                                    tracing::info!(
                                        "Device disconnected: '{}' at {} - released",
                                        name,
                                        devnode.display()
                                    );
                                } else {
                                    tracing::debug!(
                                        "Untracked device disconnected at {}",
                                        devnode.display()
                                    );
                                }

                                // Clean up from our tracking maps
                                // Note: The stream will error and be removed from SelectAll
                                // automatically, but we proactively clean up our maps here
                                remappers.remove(&devnode);
                                device_infos.remove(&devnode);
                                active_profiles.remove(&devnode);

                                // Also release from DeviceManager (no-op if not tracked there)
                                device_manager.release_device(&devnode);
                            }
                        }
                    }
                    None => {
                        // Hotplug monitor stream ended unexpectedly
                        tracing::warn!("Hotplug monitor stream ended, device detection disabled");
                    }
                }
            }
            // Handle IPC connections (if server is available)
            //
            // Accepts incoming connections from CLI/external tools and processes
            // IPC requests (profile_switch, profile_list, status).
            result = async {
                match &ipc_server {
                    Some(server) => server.accept().await,
                    None => std::future::pending().await,
                }
            } => {
                match result {
                    Ok(stream) => {
                        // Build device status information from current state (Task 040-4.3)
                        // Uses the active_profiles HashMap to get the real active profile
                        let device_statuses: Vec<DeviceStatus> = device_infos
                            .iter()
                            .map(|(path, info)| {
                                let active_profile = active_profiles
                                    .get(path)
                                    .cloned()
                                    .unwrap_or_else(|| "default".to_string());
                                DeviceStatus {
                                    name: info.name.clone(),
                                    path: info.path.clone(),
                                    active_profile,
                                    // TODO: Get actual profile list from config when
                                    // DeviceRemapper is integrated into the event loop
                                    available_profiles: vec!["default".to_string()],
                                }
                            })
                            .collect();

                        // Clone data needed for the handler closure
                        let device_names: Vec<String> = device_infos
                            .values()
                            .map(|info| info.name.clone())
                            .collect();

                        // Handle the IPC connection with a request handler
                        let handler = |request: IpcRequest| -> IpcResponse {
                            match request {
                                IpcRequest::ProfileSwitch { device, profile } => {
                                    // Check if device exists
                                    if !device_names.iter().any(|name| name == &device) {
                                        return IpcResponse::Error {
                                            message: format!(
                                                "Device '{}' not found. Available devices: {}",
                                                device,
                                                device_names.join(", ")
                                            ),
                                        };
                                    }

                                    // TODO: Implement actual profile switching when
                                    // DeviceRemapper is integrated into the event loop.
                                    // For now, we can only validate the request.
                                    if profile != "default" {
                                        return IpcResponse::Error {
                                            message: format!(
                                                "Profile '{}' not found for device '{}'. \
                                                 Note: Profile switching via IPC requires \
                                                 DeviceRemapper integration (not yet implemented). \
                                                 Available profiles: default",
                                                profile, device
                                            ),
                                        };
                                    }

                                    tracing::info!(
                                        "IPC: Profile switch request for device '{}' to profile '{}'",
                                        device,
                                        profile
                                    );

                                    IpcResponse::Success {
                                        message: Some(format!(
                                            "Switched device '{}' to profile '{}'",
                                            device, profile
                                        )),
                                    }
                                }

                                IpcRequest::ProfileList { device } => {
                                    // Find the device in our status list
                                    match device_statuses.iter().find(|s| s.name == device) {
                                        Some(status) => IpcResponse::ProfileList {
                                            profiles: status.available_profiles.clone(),
                                            active: status.active_profile.clone(),
                                        },
                                        None => IpcResponse::Error {
                                            message: format!(
                                                "Device '{}' not found. Available devices: {}",
                                                device,
                                                device_names.join(", ")
                                            ),
                                        },
                                    }
                                }

                                IpcRequest::Status => {
                                    IpcResponse::Status {
                                        devices: device_statuses.clone(),
                                    }
                                }
                            }
                        };

                        // Process the IPC request
                        if let Err(e) = handle_ipc_connection(stream, handler).await {
                            tracing::error!("Error handling IPC connection: {}", e);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to accept IPC connection: {}", e);
                    }
                }
            }
            // Handle niri IPC events (focus changes, workspace changes)
            //
            // Events are received from the NiriEventDispatcher which runs in a
            // background task. These events are used to track compositor state
            // for future per-application profile switching.
            result = async {
                match &mut niri_event_receiver {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match result {
                    Some(event) => {
                        // Log focus change events at debug level (task 040-3.7)
                        // and update focused app state (task 040-4.8)
                        match &event {
                            niri_ipc::NiriEvent::FocusChanged(focus_event) => {
                                let new_app_id = focus_event.window.as_ref().map(|w| w.app_id.clone());

                                // Log and update state only if the app_id actually changed
                                if new_app_id != current_focused_app_id {
                                    match &focus_event.window {
                                        Some(window) => {
                                            tracing::debug!(
                                                app_id = %window.app_id,
                                                title = %window.title,
                                                prev_app_id = ?current_focused_app_id,
                                                "Focused app changed"
                                            );
                                        }
                                        None => {
                                            tracing::debug!(
                                                prev_app_id = ?current_focused_app_id,
                                                "Focused app changed: no window focused"
                                            );
                                        }
                                    }
                                    current_focused_app_id = new_app_id;
                                }
                            }
                            niri_ipc::NiriEvent::WorkspaceActivated(ws_event) => {
                                tracing::debug!(
                                    workspace_id = %ws_event.workspace_id,
                                    is_focused = %ws_event.is_focused,
                                    "Workspace activated"
                                );
                            }
                        }
                        // TODO(future): Implement automatic per-app profile switching
                    }
                    None => {
                        // Channel closed - event reader task has ended
                        // This could be due to niri disconnection or task failure
                        tracing::warn!(
                            "Niri event channel closed. \
                             Niri IPC features will be unavailable until daemon restart."
                        );
                        // Set receiver to None to prevent polling a closed channel
                        niri_event_receiver = None;
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
    // and destroys the virtual device.
    // IpcServer's Drop impl will automatically remove the socket file.
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
    let config = Arc::new(config);

    // Generate niri keybinds
    niri_mapper_config::write_niri_keybinds(&config, &config_path)
        .context("Failed to generate niri keybinds")?;

    // Grab all configured devices
    let grabbed_devices = grab_configured_devices(&config)?;

    // Create the virtual device for output injection
    let virtual_device = create_shared_virtual_device("niri-mapper")
        .context("Failed to create virtual keyboard device")?;

    // Create the macro executor with shared access to the virtual device
    let macro_executor = MacroExecutor::new(virtual_device.clone());

    // Initialize IPC server for CLI communication
    // Failure to create IPC server is non-fatal - daemon can still function without it
    let ipc_server = match IpcServer::new() {
        Ok(server) => {
            tracing::info!("IPC server listening on {}", server.socket_path().display());
            Some(server)
        }
        Err(e) => {
            tracing::warn!(
                "Failed to initialize IPC server, CLI commands will not work: {}",
                e
            );
            None
        }
    };

    // Initialize hotplug monitor for device connect/disconnect events
    // Fail hard if this doesn't work - hotplug support is critical for v0.3.0
    let hotplug_monitor = HotplugMonitor::new()
        .context("Failed to create hotplug monitor")?;
    tracing::info!("Hotplug monitor initialized");

    // Initialize niri IPC event receiver based on configuration (Task 040-3.6)
    //
    // If niri IPC is enabled, we:
    // 1. Create a NiriEventDispatcher with an mpsc channel
    // 2. Spawn a background task to read events from niri
    // 3. Pass the receiver to the event loop for processing
    //
    // Connection failure is non-fatal - daemon continues without niri IPC features
    let niri_event_receiver: Option<NiriEventReceiver> = if config.global.niri_ipc_enabled {
        tracing::debug!(
            "Niri IPC enabled, attempting event stream connection (max retries: {})",
            config.global.niri_ipc_retry_count
        );

        // Create the event dispatcher and spawn the reader task
        let (dispatcher, receiver) = NiriEventDispatcher::new(DEFAULT_CHANNEL_BUFFER);

        // Use an empty window list for now.
        // The window list is used to enrich focus events with window details.
        // Task 040-3.8 will enhance this to maintain an up-to-date window list
        // by tracking WindowOpenedOrChanged and WindowClosed events.
        //
        // For v0.4.0, focus events will have limited context (window ID only)
        // when the window isn't in the initial list. This is acceptable as the
        // main use case (per-app profile switching) can be implemented later.
        //
        // Note: We use the unit type `()` as a WindowProvider which always
        // returns an empty window list. This is the simplest approach for now.
        let window_provider: () = ();

        match dispatcher.spawn_reader(window_provider).await {
            Ok(handle) => {
                tracing::info!("Niri event stream connected and reader task spawned");

                // Spawn a task to monitor the reader handle for errors
                // This allows us to log if the reader task fails unexpectedly
                tokio::spawn(async move {
                    match handle.await {
                        Ok(Ok(())) => {
                            tracing::debug!("Niri event reader task completed normally");
                        }
                        Ok(Err(e)) => {
                            // The reader encountered an error (e.g., connection closed)
                            // This is logged here; the main loop will see the channel close
                            tracing::warn!("Niri event reader task ended with error: {}", e);
                        }
                        Err(e) => {
                            // The task itself panicked
                            tracing::error!("Niri event reader task panicked: {}", e);
                        }
                    }
                });

                Some(receiver)
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to connect to niri event stream: {}. \
                     Daemon will continue without niri event features.",
                    e
                );
                None
            }
        }
    } else {
        tracing::info!("Niri IPC disabled by configuration");
        None
    };

    // Log niri IPC status for user visibility
    if niri_event_receiver.is_some() {
        tracing::info!("Niri IPC: event stream connected");
    } else {
        tracing::info!("Niri IPC: not connected (features requiring niri will be unavailable)");
    }

    tracing::info!("niri-mapper daemon starting...");

    // Run the event loop (handles SIGTERM/SIGINT for graceful shutdown, SIGHUP for config reload)
    run_event_loop(grabbed_devices, virtual_device, macro_executor, &config_path, config, ipc_server, hotplug_monitor, niri_event_receiver).await?;

    tracing::info!("Shutting down...");

    Ok(())
}
