//! Device enumeration and management

use std::path::PathBuf;

use anyhow::{bail, Result};
use evdev::Device;
use niri_mapper_config::Config;

/// Information about an input device
#[derive(Debug)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: String,
    pub vendor: u16,
    pub product: u16,
}

impl DeviceInfo {
    /// Get vendor:product string (e.g., "3434:0361")
    pub fn vendor_product(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor, self.product)
    }
}

/// Enumerate all input devices
pub fn enumerate_devices() -> Result<Vec<DeviceInfo>> {
    let mut devices = Vec::new();

    for entry in std::fs::read_dir("/dev/input")? {
        let entry = entry?;
        let path = entry.path();

        // Only look at event* devices
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("event"))
            .unwrap_or(false)
        {
            continue;
        }

        match Device::open(&path) {
            Ok(device) => {
                let name = device.name().unwrap_or("Unknown").to_string();
                let id = device.input_id();

                devices.push(DeviceInfo {
                    path,
                    name,
                    vendor: id.vendor(),
                    product: id.product(),
                });
            }
            Err(e) => {
                tracing::debug!("Could not open {}: {}", path.display(), e);
            }
        }
    }

    Ok(devices)
}

/// Check if a device is a keyboard
pub fn is_keyboard(device: &Device) -> bool {
    device
        .supported_events()
        .contains(evdev::EventType::KEY)
        && device
            .supported_keys()
            .map(|keys| keys.contains(evdev::Key::KEY_A))
            .unwrap_or(false)
}

/// Grab a device for exclusive access
pub fn grab_device(device: &mut Device) -> Result<()> {
    device.grab()?;
    Ok(())
}

/// Release a grabbed device
pub fn ungrab_device(device: &mut Device) -> Result<()> {
    device.ungrab()?;
    Ok(())
}

/// Find devices that match the configuration
///
/// For each device in the config, finds the corresponding physical device.
/// Returns pairs of (DeviceInfo, &DeviceConfig) for each match.
///
/// # Errors
///
/// Returns an error if:
/// - Device enumeration fails
/// - A configured device name is not found among physical devices
/// - No devices match the configuration (at least one match is required)
pub fn find_matching_devices(config: &Config) -> Result<Vec<(DeviceInfo, usize)>> {
    let all_devices = enumerate_devices()?;

    let mut matched: Vec<(DeviceInfo, usize)> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();

    for (config_idx, device_config) in config.devices.iter().enumerate() {
        // Match by name if specified
        if let Some(ref config_name) = device_config.name {
            let found = all_devices
                .iter()
                .position(|d| &d.name == config_name);

            match found {
                Some(device_idx) => {
                    // Clone the device info since we're iterating
                    let device_info = &all_devices[device_idx];
                    tracing::info!(
                        "Matched device '{}' at {}",
                        device_info.name,
                        device_info.path.display()
                    );
                    matched.push((
                        DeviceInfo {
                            path: device_info.path.clone(),
                            name: device_info.name.clone(),
                            vendor: device_info.vendor,
                            product: device_info.product,
                        },
                        config_idx,
                    ));
                }
                None => {
                    tracing::warn!("Configured device '{}' not found in system", config_name);
                    not_found.push(config_name.clone());
                }
            }
        }
        // TODO: Match by vendor_product when implemented
    }

    if !not_found.is_empty() {
        bail!(
            "Configured device(s) not found: {}. Available devices: {}",
            not_found.join(", "),
            all_devices
                .iter()
                .map(|d| format!("\"{}\"", d.name))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Validate that at least one device matched
    if matched.is_empty() {
        let configured_names: Vec<String> = config
            .devices
            .iter()
            .filter_map(|d| d.name.clone())
            .collect();

        let available_names: String = all_devices
            .iter()
            .map(|d| format!("\"{}\"", d.name))
            .collect::<Vec<_>>()
            .join(", ");

        if configured_names.is_empty() {
            bail!(
                "No devices configured with a name. Please specify at least one device with a 'name' field. Available devices: {}",
                available_names
            );
        } else {
            bail!(
                "No devices matched the configuration. Configured device names: {}. Available devices: {}",
                configured_names.iter().map(|n| format!("\"{}\"", n)).collect::<Vec<_>>().join(", "),
                available_names
            );
        }
    }

    Ok(matched)
}
