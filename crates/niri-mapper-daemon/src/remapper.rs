//! Key remapping logic

use std::collections::HashMap;

use evdev::{InputEvent, Key};
use niri_mapper_config::Profile;

/// Remapper handles translating input events according to a profile
pub struct Remapper {
    /// Simple key remaps (from -> to)
    remap: HashMap<Key, Key>,
    /// Keys that should be passed through unmodified
    passthrough: Vec<Key>,
}

impl Remapper {
    /// Create a new remapper from a profile
    pub fn from_profile(profile: &Profile) -> Self {
        let mut remap = HashMap::new();

        for (from, to) in &profile.remap {
            if let (Some(from_key), Some(to_key)) = (parse_key(from), parse_key(to)) {
                remap.insert(from_key, to_key);
            }
        }

        // TODO: Parse passthrough keys from niri_passthrough

        Self {
            remap,
            passthrough: Vec::new(),
        }
    }

    /// Process an input event, returning the remapped event(s)
    pub fn process(&self, event: InputEvent) -> Vec<InputEvent> {
        // Only process key events
        if event.event_type() != evdev::EventType::KEY {
            return vec![event];
        }

        let key = Key::new(event.code());

        if let Some(&remapped_key) = self.remap.get(&key) {
            // Remap the key
            return vec![InputEvent::new(
                evdev::EventType::KEY,
                remapped_key.code(),
                event.value(),
            )];
        }

        // Pass through unmodified
        vec![event]
    }
}

/// Parse a key name string to an evdev Key
fn parse_key(name: &str) -> Option<Key> {
    // Common key mappings
    match name.to_uppercase().as_str() {
        "CAPSLOCK" | "CAPS_LOCK" | "CAPS" => Some(Key::KEY_CAPSLOCK),
        "ESCAPE" | "ESC" => Some(Key::KEY_ESC),
        "ENTER" | "RETURN" => Some(Key::KEY_ENTER),
        "TAB" => Some(Key::KEY_TAB),
        "SPACE" => Some(Key::KEY_SPACE),
        "BACKSPACE" => Some(Key::KEY_BACKSPACE),

        // Letters
        "A" => Some(Key::KEY_A),
        "B" => Some(Key::KEY_B),
        "C" => Some(Key::KEY_C),
        "D" => Some(Key::KEY_D),
        "E" => Some(Key::KEY_E),
        "F" => Some(Key::KEY_F),
        "G" => Some(Key::KEY_G),
        "H" => Some(Key::KEY_H),
        "I" => Some(Key::KEY_I),
        "J" => Some(Key::KEY_J),
        "K" => Some(Key::KEY_K),
        "L" => Some(Key::KEY_L),
        "M" => Some(Key::KEY_M),
        "N" => Some(Key::KEY_N),
        "O" => Some(Key::KEY_O),
        "P" => Some(Key::KEY_P),
        "Q" => Some(Key::KEY_Q),
        "R" => Some(Key::KEY_R),
        "S" => Some(Key::KEY_S),
        "T" => Some(Key::KEY_T),
        "U" => Some(Key::KEY_U),
        "V" => Some(Key::KEY_V),
        "W" => Some(Key::KEY_W),
        "X" => Some(Key::KEY_X),
        "Y" => Some(Key::KEY_Y),
        "Z" => Some(Key::KEY_Z),

        // Modifiers
        "LEFTCTRL" | "LCTRL" | "CTRL" => Some(Key::KEY_LEFTCTRL),
        "RIGHTCTRL" | "RCTRL" => Some(Key::KEY_RIGHTCTRL),
        "LEFTSHIFT" | "LSHIFT" | "SHIFT" => Some(Key::KEY_LEFTSHIFT),
        "RIGHTSHIFT" | "RSHIFT" => Some(Key::KEY_RIGHTSHIFT),
        "LEFTALT" | "LALT" | "ALT" => Some(Key::KEY_LEFTALT),
        "RIGHTALT" | "RALT" => Some(Key::KEY_RIGHTALT),
        "LEFTMETA" | "LMETA" | "SUPER" | "META" => Some(Key::KEY_LEFTMETA),
        "RIGHTMETA" | "RMETA" => Some(Key::KEY_RIGHTMETA),

        // Function keys
        "F1" => Some(Key::KEY_F1),
        "F2" => Some(Key::KEY_F2),
        "F3" => Some(Key::KEY_F3),
        "F4" => Some(Key::KEY_F4),
        "F5" => Some(Key::KEY_F5),
        "F6" => Some(Key::KEY_F6),
        "F7" => Some(Key::KEY_F7),
        "F8" => Some(Key::KEY_F8),
        "F9" => Some(Key::KEY_F9),
        "F10" => Some(Key::KEY_F10),
        "F11" => Some(Key::KEY_F11),
        "F12" => Some(Key::KEY_F12),

        // Media keys
        "XF86BACK" => Some(Key::KEY_BACK),
        "XF86FORWARD" => Some(Key::KEY_FORWARD),

        _ => {
            tracing::warn!("Unknown key: {}", name);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key() {
        assert_eq!(parse_key("CapsLock"), Some(Key::KEY_CAPSLOCK));
        assert_eq!(parse_key("Escape"), Some(Key::KEY_ESC));
        assert_eq!(parse_key("A"), Some(Key::KEY_A));
    }
}
