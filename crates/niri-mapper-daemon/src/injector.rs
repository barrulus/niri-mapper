//! Virtual device injection via uinput
//!
//! This module provides a virtual keyboard device for injecting remapped key events.
//! The [`SharedVirtualDevice`] type alias provides a thread-safe, shareable wrapper
//! around [`VirtualDevice`] for use across multiple input device handlers.

use std::sync::Arc;

use anyhow::Result;
use evdev::{uinput::VirtualDeviceBuilder, AttributeSet, Key, InputEvent};
use tokio::sync::Mutex;

/// A shared virtual device that can be used across multiple async tasks.
///
/// This is the primary interface for injecting remapped events from multiple
/// grabbed input devices through a single virtual output device.
pub type SharedVirtualDevice = Arc<Mutex<VirtualDevice>>;

/// Create a shared virtual keyboard device for output injection.
///
/// This creates a single virtual keyboard device wrapped in `Arc<Mutex<>>` that can
/// be cloned and shared across multiple device event handlers. All grabbed input
/// devices inject their remapped events through this single output device.
///
/// # Arguments
///
/// * `name` - The name for the virtual device (e.g., "niri-mapper")
///
/// # Returns
///
/// A [`SharedVirtualDevice`] that can be cloned and used from multiple async tasks.
///
/// # Errors
///
/// Returns an error if the virtual device cannot be created (e.g., insufficient
/// permissions to access /dev/uinput).
///
/// # Example
///
/// ```no_run
/// use niri_mapper_daemon::injector::create_shared_virtual_device;
///
/// # async fn example() -> anyhow::Result<()> {
/// let virtual_device = create_shared_virtual_device("niri-mapper")?;
///
/// // Clone for use in multiple tasks
/// let vd_clone = virtual_device.clone();
///
/// // Use in an async context
/// {
///     let mut vd = virtual_device.lock().await;
///     vd.press_key(evdev::Key::KEY_A)?;
/// }
/// # Ok(())
/// # }
/// ```
pub fn create_shared_virtual_device(name: &str) -> Result<SharedVirtualDevice> {
    let device = VirtualDevice::new_keyboard(name)?;
    Ok(Arc::new(Mutex::new(device)))
}

/// A virtual input device for injecting events
pub struct VirtualDevice {
    device: evdev::uinput::VirtualDevice,
}

impl VirtualDevice {
    /// Create a new virtual keyboard device
    pub fn new_keyboard(name: &str) -> Result<Self> {
        let mut keys = AttributeSet::<Key>::new();

        // Add all standard keys
        for code in 0..256u16 {
            keys.insert(Key::new(code));
        }

        let device = VirtualDeviceBuilder::new()?
            .name(name)
            .with_keys(&keys)?
            .build()?;

        Ok(Self { device })
    }

    /// Emit an input event
    pub fn emit(&mut self, events: &[InputEvent]) -> Result<()> {
        self.device.emit(events)?;
        Ok(())
    }

    /// Send a key press event
    pub fn press_key(&mut self, key: Key) -> Result<()> {
        let press = InputEvent::new(evdev::EventType::KEY, key.code(), 1);
        let syn = InputEvent::new(evdev::EventType::SYNCHRONIZATION, 0, 0);
        self.emit(&[press, syn])?;
        Ok(())
    }

    /// Send a key release event
    pub fn release_key(&mut self, key: Key) -> Result<()> {
        let release = InputEvent::new(evdev::EventType::KEY, key.code(), 0);
        let syn = InputEvent::new(evdev::EventType::SYNCHRONIZATION, 0, 0);
        self.emit(&[release, syn])?;
        Ok(())
    }

    /// Send a key tap (press + release)
    pub fn tap_key(&mut self, key: Key) -> Result<()> {
        self.press_key(key)?;
        self.release_key(key)?;
        Ok(())
    }
}
