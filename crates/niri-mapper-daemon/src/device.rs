//! Device enumeration and management

use std::path::PathBuf;

use anyhow::Result;
use evdev::Device;

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
