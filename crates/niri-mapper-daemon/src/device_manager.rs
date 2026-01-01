//! Device management for hot-plug support
//!
//! This module provides the `DeviceManager` struct which encapsulates all device
//! management functionality. It is the foundational struct for hot-plug support,
//! managing the lifecycle of grabbed devices.
//!
//! # Overview
//!
//! The `DeviceManager` is responsible for:
//! - Tracking grabbed devices by their path
//! - Holding references to shared configuration and virtual device
//! - Providing a central point for device grab/release operations (future)
//!
//! # Example
//!
//! ```ignore
//! use std::sync::Arc;
//! use niri_mapper_config::Config;
//! use crate::device_manager::DeviceManager;
//! use crate::injector::create_shared_virtual_device;
//!
//! let config = Arc::new(config);
//! let virtual_device = create_shared_virtual_device("niri-mapper")?;
//! let device_manager = DeviceManager::new(config, virtual_device);
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use evdev::Device;
use niri_mapper_config::Config;
use tokio::sync::Mutex;

use crate::device::DeviceInfo;
use crate::hotplug::HotplugEvent;
use crate::injector::VirtualDevice;
use crate::remapper::Remapper;
use crate::GrabbedDevice;

/// Manages grabbed input devices for the daemon.
///
/// This struct is the central coordinator for device lifecycle management,
/// holding references to all grabbed devices and the shared resources they need.
///
/// # Fields
///
/// - `config`: The parsed configuration, wrapped in `Arc` for sharing
/// - `grabbed_devices`: Map from device path to grabbed device state
/// - `virtual_device`: Shared virtual device for output injection
///
/// # Thread Safety
///
/// The `DeviceManager` itself is not thread-safe. It should be owned by a single
/// task (the main event loop). The `virtual_device` is wrapped in `Arc<Mutex<>>`
/// and can be shared with macro execution tasks.
pub struct DeviceManager {
    /// The parsed configuration
    config: Arc<Config>,
    /// Map from device path to grabbed device
    grabbed_devices: HashMap<PathBuf, GrabbedDevice>,
    /// Shared virtual device for injecting remapped events
    virtual_device: Arc<Mutex<VirtualDevice>>,
}

impl DeviceManager {
    /// Create a new `DeviceManager` with the given configuration and virtual device.
    ///
    /// The device manager starts with no grabbed devices. Use `try_grab_device()`
    /// (to be implemented in task 030-2.2.2) to grab devices.
    ///
    /// # Arguments
    ///
    /// * `config` - The parsed configuration, wrapped in `Arc` for sharing
    /// * `virtual_device` - The shared virtual device for output injection
    ///
    /// # Example
    ///
    /// ```ignore
    /// let config = Arc::new(config);
    /// let virtual_device = create_shared_virtual_device("niri-mapper")?;
    /// let device_manager = DeviceManager::new(config, virtual_device);
    /// ```
    pub fn new(config: Arc<Config>, virtual_device: Arc<Mutex<VirtualDevice>>) -> Self {
        Self {
            config,
            grabbed_devices: HashMap::new(),
            virtual_device,
        }
    }

    /// Returns a reference to the configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Returns the number of currently grabbed devices.
    pub fn grabbed_count(&self) -> usize {
        self.grabbed_devices.len()
    }

    /// Returns a reference to the grabbed devices map.
    pub fn grabbed_devices(&self) -> &HashMap<PathBuf, GrabbedDevice> {
        &self.grabbed_devices
    }

    /// Returns a mutable reference to the grabbed devices map.
    ///
    /// This is needed for the event loop to modify device state.
    pub fn grabbed_devices_mut(&mut self) -> &mut HashMap<PathBuf, GrabbedDevice> {
        &mut self.grabbed_devices
    }

    /// Returns a reference to the shared virtual device.
    pub fn virtual_device(&self) -> &Arc<Mutex<VirtualDevice>> {
        &self.virtual_device
    }

    /// Try to grab a device at the given path.
    ///
    /// Opens the evdev device, checks if it matches any configured device by name,
    /// and if so, grabs it for exclusive access and creates a remapper for it.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the evdev device (e.g., `/dev/input/event3`)
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - Device was matched, grabbed, and stored
    /// * `Ok(false)` - Device does not match any configured device (not an error)
    /// * `Err(_)` - Device could not be opened or grabbed (failure)
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::path::Path;
    ///
    /// let grabbed = device_manager.try_grab_device(Path::new("/dev/input/event3"))?;
    /// if grabbed {
    ///     println!("Device grabbed successfully");
    /// } else {
    ///     println!("Device not in configuration, skipped");
    /// }
    /// ```
    pub fn try_grab_device(&mut self, path: &Path) -> Result<bool> {
        // Open the evdev device
        let mut device = Device::open(path).with_context(|| {
            format!("Failed to open device at {}", path.display())
        })?;

        // Get the device name
        let device_name = device.name().unwrap_or("Unknown").to_string();

        tracing::debug!(
            "Checking device '{}' at {} for configuration match",
            device_name,
            path.display()
        );

        // Find matching device config by name
        let matching_config = self.config.devices.iter().find(|device_config| {
            device_config
                .name
                .as_ref()
                .map(|name| name == &device_name)
                .unwrap_or(false)
        });

        let device_config = match matching_config {
            Some(config) => config,
            None => {
                tracing::debug!(
                    "Device '{}' does not match any configured device, skipping",
                    device_name
                );
                return Ok(false);
            }
        };

        tracing::info!(
            "Device '{}' matches configuration, grabbing...",
            device_name
        );

        // Grab the device for exclusive access - fail hard if this doesn't work
        device.grab().with_context(|| {
            format!(
                "Failed to grab device '{}' for exclusive access. \
                 Is another application using this device?",
                device_name
            )
        })?;

        tracing::debug!("Successfully grabbed device: {}", device_name);

        // Get the default profile
        let default_profile = device_config.profiles.get("default").with_context(|| {
            format!(
                "Device '{}' has no 'default' profile defined. \
                 Please add a 'profile \"default\" {{ ... }}' block to the device configuration.",
                device_name
            )
        })?;

        // Create the remapper from the default profile
        let remapper = Remapper::from_profile(default_profile);

        // Get device info for metadata
        let id = device.input_id();
        let info = DeviceInfo {
            path: path.to_path_buf(),
            name: device_name.clone(),
            vendor: id.vendor(),
            product: id.product(),
        };

        // Store the grabbed device
        self.grabbed_devices.insert(
            path.to_path_buf(),
            GrabbedDevice {
                device,
                remapper,
                info,
                active_profile: "default".to_string(),
            },
        );

        tracing::info!(
            "Device '{}' grabbed and configured (path: {})",
            device_name,
            path.display()
        );

        Ok(true)
    }

    /// Release a grabbed device at the given path.
    ///
    /// If the device is currently grabbed, it will be ungrabbed and removed from
    /// the internal tracking map. The ungrab happens automatically when the
    /// `GrabbedDevice` is dropped.
    ///
    /// If the device is not in the grabbed devices map, this is a no-op and
    /// a debug message is logged.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the evdev device (e.g., `/dev/input/event3`)
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::path::Path;
    ///
    /// // Release a device that was previously grabbed
    /// device_manager.release_device(Path::new("/dev/input/event3"));
    /// ```
    pub fn release_device(&mut self, path: &Path) {
        match self.grabbed_devices.remove(path) {
            Some(grabbed_device) => {
                tracing::info!(
                    "Released device '{}' at {}",
                    grabbed_device.info.name,
                    path.display()
                );
                // GrabbedDevice is dropped here, which automatically ungrabs the device
            }
            None => {
                tracing::debug!(
                    "Device at {} was not grabbed, nothing to release",
                    path.display()
                );
            }
        }
    }

    /// Handle a hotplug event by grabbing or releasing the device.
    ///
    /// This method is the bridge between hotplug events and device management.
    /// For `Add` events, it attempts to grab the device (matching it against
    /// the configuration). For `Remove` events, it releases the device.
    ///
    /// # Arguments
    ///
    /// * `event` - The hotplug event to handle
    ///
    /// # Example
    ///
    /// ```ignore
    /// use crate::hotplug::HotplugEvent;
    ///
    /// let event = HotplugEvent::Add { devnode: PathBuf::from("/dev/input/event5") };
    /// device_manager.handle_hotplug(event);
    /// ```
    pub fn handle_hotplug(&mut self, event: HotplugEvent) {
        match event {
            HotplugEvent::Add { devnode } => {
                tracing::info!("Device connected at {}", devnode.display());
                match self.try_grab_device(&devnode) {
                    Ok(true) => {
                        tracing::info!("Successfully grabbed device at {}", devnode.display());
                    }
                    Ok(false) => {
                        tracing::debug!(
                            "Device at {} does not match configuration, ignored",
                            devnode.display()
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to grab device at {}: {:#}",
                            devnode.display(),
                            e
                        );
                    }
                }
            }
            HotplugEvent::Remove { devnode } => {
                tracing::info!("Device disconnected at {}", devnode.display());
                self.release_device(&devnode);
            }
        }
    }

    /// Convert all grabbed devices into async event streams.
    ///
    /// This method consumes the grabbed devices from the manager and returns them
    /// as event streams along with their associated metadata. The event streams are
    /// used by the event loop to `select!` on multiple device inputs simultaneously.
    ///
    /// **Note**: This method takes `&mut self` and drains the `grabbed_devices` map.
    /// After calling this method, the `DeviceManager` will have no grabbed devices.
    /// This is by design - the event streams consume the underlying `Device` handles,
    /// so they cannot remain in the manager.
    ///
    /// # Returns
    ///
    /// A `Result` containing a vector of tuples, where each tuple contains:
    /// - `PathBuf`: The device path (e.g., `/dev/input/event3`)
    /// - `Remapper`: The remapper instance for processing events
    /// - `DeviceInfo`: Metadata about the device
    /// - `String`: The active profile name (Task 040-4.3)
    /// - `evdev::EventStream`: The async stream of input events
    ///
    /// # Errors
    ///
    /// Returns an error if any device fails to be converted to an event stream.
    /// The error message includes the device name and path for debugging.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let streams = device_manager.get_event_streams()?;
    /// for (path, remapper, info, active_profile, stream) in streams {
    ///     println!("Stream ready for device '{}' at {} (profile: {})", info.name, path.display(), active_profile);
    /// }
    /// ```
    pub fn get_event_streams(
        &mut self,
    ) -> Result<Vec<(PathBuf, Remapper, DeviceInfo, String, evdev::EventStream)>> {
        use std::mem;

        // Take ownership of all grabbed devices
        let grabbed_devices = mem::take(&mut self.grabbed_devices);

        let mut result = Vec::with_capacity(grabbed_devices.len());

        for (path, grabbed_device) in grabbed_devices {
            let GrabbedDevice {
                device,
                remapper,
                info,
                active_profile,
            } = grabbed_device;

            // Convert the device to an async event stream
            let event_stream = device.into_event_stream().with_context(|| {
                format!(
                    "Failed to create event stream for device '{}' at {}",
                    info.name,
                    path.display()
                )
            })?;

            result.push((path, remapper, info, active_profile, event_stream));
        }

        tracing::debug!(
            "Created {} event stream(s) from grabbed devices",
            result.len()
        );

        Ok(result)
    }
}
