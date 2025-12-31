//! Virtual device injection via uinput

use anyhow::Result;
use evdev::{uinput::VirtualDeviceBuilder, AttributeSet, Key, InputEvent};

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
