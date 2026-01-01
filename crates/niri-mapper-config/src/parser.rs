//! KDL configuration parser
//!
//! This module parses niri-mapper configuration files written in KDL format.
//!
//! # Per-Application Profile Parsing (v0.4.0)
//!
//! The parser supports the `app-id-hint` field within profile blocks:
//!
//! ```kdl
//! profile "firefox" {
//!     app-id-hint "org.mozilla.firefox"
//!     remap {
//!         CapsLock "LeftCtrl"
//!     }
//! }
//! ```
//!
//! ## Current Behavior (v0.4.0)
//!
//! - The `app-id-hint` field is parsed and stored in [`Profile::app_id_hint`]
//! - The value is validated to be a string
//! - **No automatic behavior** is triggered by this field; it is stored for
//!   future use
//!
//! ## Future Behavior (Backlog)
//!
//! In a future release, the daemon will:
//! 1. Monitor niri focus change events
//! 2. Match focused window's `app_id` against profiles' `app_id_hint` values
//! 3. Automatically switch to the matching profile
//!
//! ## Manual Profile Switching (v0.4.0)
//!
//! Until automatic switching is implemented, users can switch profiles via:
//!
//! - **Keybinds**: Configure `profile-switch` block in device config
//! - **CLI**: `niri-mapper switch-profile <device> <profile>`
//! - **Control socket**: Send JSON command to daemon socket
//!
//! See [`crate::model`] module documentation for more details.

use std::path::Path;
use crate::error::{ConfigError, InvalidKeyInfo, KeyPosition, SourceLocation};
use crate::model::*;

/// Extract source location from a KDL node's name span
fn get_node_location(node: &kdl::KdlNode, source: &str) -> SourceLocation {
    let span = node.name().span();
    let offset = span.offset();
    let len = span.len();

    // Calculate line and column from offset
    let (line, column) = offset_to_line_col(source, offset);

    SourceLocation::new(line, column, offset, len)
}

/// Extract source location from a KDL entry (for "to" values)
fn get_entry_location(entry: &kdl::KdlEntry, source: &str) -> SourceLocation {
    let span = entry.span();
    let offset = span.offset();
    let len = span.len();

    // Calculate line and column from offset
    let (line, column) = offset_to_line_col(source, offset);

    SourceLocation::new(line, column, offset, len)
}

/// Convert byte offset to line and column (1-indexed)
fn offset_to_line_col(source: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;

    for (i, ch) in source.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }

    (line, col)
}

/// Parse a configuration file from the given path
pub fn parse_config(path: &Path) -> Result<Config, ConfigError> {
    let content = std::fs::read_to_string(path)?;
    parse_config_str(&content)
}

/// Parse configuration from a string
pub fn parse_config_str(content: &str) -> Result<Config, ConfigError> {
    let doc: kdl::KdlDocument = content.parse().map_err(|e: kdl::KdlError| {
        // Convert span from kdl's miette version to our miette version
        // kdl uses an older miette version, so we need to extract offset/len manually
        let offset = e.span.offset();
        let len = e.span.len();
        let span = miette::SourceSpan::from((offset, len));
        ConfigError::ParseError {
            src: content.to_string(),
            span,
            source: e,
        }
    })?;

    let mut config = Config::default();

    for node in doc.nodes() {
        match node.name().value() {
            "global" => {
                config.global = parse_global(node)?;
            }
            "device" => {
                config.devices.push(parse_device(node, content)?);
            }
            name => {
                tracing::warn!("Unknown top-level node: {}", name);
            }
        }
    }

    Ok(config)
}

fn parse_global(node: &kdl::KdlNode) -> Result<GlobalConfig, ConfigError> {
    let mut global = GlobalConfig::default();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "log-level" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(val) = entry.value().as_string() {
                            global.log_level = val.parse().map_err(|e| ConfigError::Invalid {
                                message: e,
                            })?;
                        }
                    }
                }
                "niri-keybinds-path" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(val) = entry.value().as_string() {
                            global.niri_keybinds_path = shellexpand::tilde(val).into_owned().into();
                        }
                    }
                }
                "niri-ipc-enabled" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(val) = entry.value().as_bool() {
                            global.niri_ipc_enabled = val;
                        }
                    }
                }
                "niri-ipc-retry-count" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(val) = entry.value().as_i64() {
                            if val >= 0 {
                                global.niri_ipc_retry_count = val as u32;
                            } else {
                                return Err(ConfigError::Invalid {
                                    message: format!(
                                        "niri-ipc-retry-count must be non-negative, got {}",
                                        val
                                    ),
                                });
                            }
                        }
                    }
                }
                name => {
                    tracing::warn!("Unknown global config option: {}", name);
                }
            }
        }
    }

    Ok(global)
}

fn parse_device(node: &kdl::KdlNode, source: &str) -> Result<DeviceConfig, ConfigError> {
    let name = node
        .entries()
        .first()
        .and_then(|e| e.value().as_string())
        .map(|s| s.to_string());

    let mut device = DeviceConfig {
        name,
        vendor_product: None,
        profiles: std::collections::HashMap::new(),
        profile_switch: std::collections::HashMap::new(),
    };

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "vendor-product" => {
                    if let Some(entry) = child.entries().first() {
                        device.vendor_product = entry.value().as_string().map(|s| s.to_string());
                    }
                }
                "profile" => {
                    let profile_name = child
                        .entries()
                        .first()
                        .and_then(|e| e.value().as_string())
                        .unwrap_or("default")
                        .to_string();

                    // Check for duplicate profile names
                    if device.profiles.contains_key(&profile_name) {
                        return Err(ConfigError::Invalid {
                            message: format!(
                                "Duplicate profile name '{}' in device '{}'. Profile names must be unique within a device.",
                                profile_name,
                                device.name.as_deref().unwrap_or("<unnamed>")
                            ),
                        });
                    }

                    let profile = parse_profile(child, source)?;
                    device.profiles.insert(profile_name, profile);
                }
                "profile-switch" => {
                    device.profile_switch = parse_profile_switch(child)?;
                }
                name => {
                    tracing::warn!("Unknown device config option: {}", name);
                }
            }
        }
    }

    // Validation: device must have a name (required in v0.1.0)
    if device.name.is_none() {
        return Err(ConfigError::MissingField {
            field: "device name (e.g., `device \"My Keyboard\" { ... }`)".to_string(),
        });
    }

    // Validation: if device has profiles with remappings, it must have a "default" profile
    let has_remappings = device.profiles.values().any(|p| {
        !p.remap.is_empty() || !p.combo.is_empty() || !p.macros.is_empty()
    });

    if has_remappings && !device.profiles.contains_key("default") {
        return Err(ConfigError::Invalid {
            message: format!(
                "Device '{}' has remapping rules but no 'default' profile. \
                 Add a profile named 'default' or rename an existing profile.",
                device.name.as_ref().unwrap()
            ),
        });
    }

    // Validation: profile-switch must reference existing profiles
    for (keybind, profile_name) in &device.profile_switch {
        if !device.profiles.contains_key(profile_name) {
            let available_profiles: Vec<&str> = device.profiles.keys().map(|s| s.as_str()).collect();
            let available_str = if available_profiles.is_empty() {
                "no profiles defined".to_string()
            } else {
                format!("available profiles: {}", available_profiles.join(", "))
            };
            return Err(ConfigError::Invalid {
                message: format!(
                    "profile-switch in device '{}' references non-existent profile '{}' for keybind '{}'. {}",
                    device.name.as_deref().unwrap_or("<unnamed>"),
                    profile_name,
                    keybind,
                    available_str
                ),
            });
        }
    }

    // Warning: device has multiple profiles but no profile-switch keybinds
    // This is a usability warning - user might have forgotten to add keybinds to switch between profiles
    if device.profiles.len() >= 2 && device.profile_switch.is_empty() {
        let profile_names: Vec<&str> = device.profiles.keys().map(|s| s.as_str()).collect();
        tracing::warn!(
            "Device '{}' has {} profiles ({}) but no profile-switch keybinds configured. \
             Consider adding a profile-switch block to enable switching between profiles.",
            device.name.as_deref().unwrap_or("<unnamed>"),
            device.profiles.len(),
            profile_names.join(", ")
        );
    }

    Ok(device)
}

fn parse_profile(node: &kdl::KdlNode, source: &str) -> Result<Profile, ConfigError> {
    let mut profile = Profile::default();
    let mut all_invalid_keys = Vec::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "app-id-hint" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(val) = entry.value().as_string() {
                            profile.app_id_hint = Some(val.to_string());
                        }
                    }
                }
                "remap" => {
                    match parse_key_value_block(child, "remap", source) {
                        Ok(remap) => profile.remap = remap,
                        Err(ConfigError::InvalidKeys { invalid_keys, .. }) => {
                            all_invalid_keys.extend(invalid_keys);
                        }
                        Err(e) => return Err(e),
                    }
                }
                "combo" => {
                    match parse_key_value_block(child, "combo", source) {
                        Ok(combo) => profile.combo = combo,
                        Err(ConfigError::InvalidKeys { invalid_keys, .. }) => {
                            all_invalid_keys.extend(invalid_keys);
                        }
                        Err(e) => return Err(e),
                    }
                }
                "macro" => {
                    match parse_macro_block(child, source) {
                        Ok(macros) => profile.macros = macros,
                        Err(ConfigError::InvalidKeys { invalid_keys, .. }) => {
                            all_invalid_keys.extend(invalid_keys);
                        }
                        Err(e) => return Err(e),
                    }
                }
                "niri-passthrough" => {
                    profile.niri_passthrough = parse_niri_passthrough(child)?;
                }
                name => {
                    tracing::warn!("Unknown profile option: {}", name);
                }
            }
        }
    }

    // If we accumulated any invalid keys, return them all together with source context
    if !all_invalid_keys.is_empty() {
        return Err(ConfigError::InvalidKeys {
            src: Some(source.to_string()),
            invalid_keys: all_invalid_keys,
        });
    }

    // Check for macro trigger key conflicts with remap source keys and combo triggers
    let mut conflicts = Vec::new();

    for macro_key in profile.macros.keys() {
        let macro_key_upper = macro_key.to_uppercase();

        // Check conflict with remap source keys
        for remap_key in profile.remap.keys() {
            if remap_key.to_uppercase() == macro_key_upper {
                conflicts.push(format!(
                    "Macro trigger key '{}' conflicts with remap source key '{}'",
                    macro_key, remap_key
                ));
            }
        }

        // Check conflict with combo trigger keys
        // Combo keys may contain modifiers like "Ctrl+A", so we extract the base key
        for combo_key in profile.combo.keys() {
            // Get the last component after '+' (the trigger key without modifiers)
            let combo_base = combo_key
                .rsplit('+')
                .next()
                .unwrap_or(combo_key);

            if combo_base.to_uppercase() == macro_key_upper {
                conflicts.push(format!(
                    "Macro trigger key '{}' conflicts with combo trigger '{}' (base key '{}')",
                    macro_key, combo_key, combo_base
                ));
            }
        }
    }

    if !conflicts.is_empty() {
        return Err(ConfigError::Invalid {
            message: format!(
                "Key conflicts detected in profile:\n  - {}",
                conflicts.join("\n  - ")
            ),
        });
    }

    Ok(profile)
}

/// Validate a key combo string (e.g., "Ctrl+C", "A", "Shift+Alt+X")
/// Returns a list of invalid key names found in the combo, or an empty vec if all are valid
fn validate_key_combo(combo: &str) -> Vec<String> {
    let mut invalid_keys = Vec::new();

    // Split on '+' to handle combos like "Ctrl+C" or "Shift+Alt+X"
    for part in combo.split('+') {
        let trimmed = part.trim();
        if !trimmed.is_empty() && !is_valid_key(trimmed) {
            invalid_keys.push(trimmed.to_string());
        }
    }

    invalid_keys
}

/// Check if a key name is valid
/// Returns true if the key is recognized, false otherwise
/// NOTE: This must stay in sync with parse_key() in niri-mapper-daemon/src/remapper.rs
fn is_valid_key(name: &str) -> bool {
    match name.to_uppercase().as_str() {
        // Special keys
        "CAPSLOCK" | "CAPS_LOCK" | "CAPS" => true,
        "ESCAPE" | "ESC" => true,
        "ENTER" | "RETURN" => true,
        "TAB" => true,
        "SPACE" => true,
        "BACKSPACE" => true,

        // Letters
        "A" | "B" | "C" | "D" | "E" | "F" | "G" | "H" | "I" | "J" | "K" | "L" | "M" | "N" | "O"
        | "P" | "Q" | "R" | "S" | "T" | "U" | "V" | "W" | "X" | "Y" | "Z" => true,

        // Modifiers
        "LEFTCTRL" | "LCTRL" | "CTRL" => true,
        "RIGHTCTRL" | "RCTRL" => true,
        "LEFTSHIFT" | "LSHIFT" | "SHIFT" => true,
        "RIGHTSHIFT" | "RSHIFT" => true,
        "LEFTALT" | "LALT" | "ALT" => true,
        "RIGHTALT" | "RALT" => true,
        "LEFTMETA" | "LMETA" | "SUPER" | "META" => true,
        "RIGHTMETA" | "RMETA" => true,

        // Number keys
        "0" | "KEY_0" => true,
        "1" | "KEY_1" => true,
        "2" | "KEY_2" => true,
        "3" | "KEY_3" => true,
        "4" | "KEY_4" => true,
        "5" | "KEY_5" => true,
        "6" | "KEY_6" => true,
        "7" | "KEY_7" => true,
        "8" | "KEY_8" => true,
        "9" | "KEY_9" => true,

        // Symbol keys
        "MINUS" | "-" => true,
        "EQUALS" | "EQUAL" | "=" => true,
        "LEFTBRACE" | "LBRACE" | "[" => true,
        "RIGHTBRACE" | "RBRACE" | "]" => true,
        "SEMICOLON" | ";" => true,
        "APOSTROPHE" | "'" => true,
        "GRAVE" | "`" => true,
        "BACKSLASH" | "\\" => true,
        "COMMA" | "," => true,
        "DOT" | "PERIOD" | "." => true,
        "SLASH" | "/" => true,

        // Arrow keys
        "UP" | "UPARROW" => true,
        "DOWN" | "DOWNARROW" => true,
        "LEFT" | "LEFTARROW" => true,
        "RIGHT" | "RIGHTARROW" => true,

        // Navigation keys
        "HOME" => true,
        "END" => true,
        "PAGEUP" | "PGUP" => true,
        "PAGEDOWN" | "PGDN" | "PGDOWN" => true,
        "INSERT" | "INS" => true,
        "DELETE" | "DEL" => true,

        // Function keys F1-F12
        "F1" | "F2" | "F3" | "F4" | "F5" | "F6" | "F7" | "F8" | "F9" | "F10" | "F11" | "F12" => true,

        // Function keys F13-F24
        "F13" | "F14" | "F15" | "F16" | "F17" | "F18" | "F19" | "F20" | "F21" | "F22" | "F23" | "F24" => true,

        // Numpad keys
        "KP0" | "NUMPAD0" => true,
        "KP1" | "NUMPAD1" => true,
        "KP2" | "NUMPAD2" => true,
        "KP3" | "NUMPAD3" => true,
        "KP4" | "NUMPAD4" => true,
        "KP5" | "NUMPAD5" => true,
        "KP6" | "NUMPAD6" => true,
        "KP7" | "NUMPAD7" => true,
        "KP8" | "NUMPAD8" => true,
        "KP9" | "NUMPAD9" => true,
        "KPDOT" | "KPDECIMAL" | "NUMPAD_DOT" => true,
        "KPENTER" | "NUMPAD_ENTER" => true,
        "KPPLUS" | "KPADD" | "NUMPAD_PLUS" => true,
        "KPMINUS" | "KPSUBTRACT" | "NUMPAD_MINUS" => true,
        "KPASTERISK" | "KPMULTIPLY" | "NUMPAD_MULTIPLY" => true,
        "KPSLASH" | "KPDIVIDE" | "NUMPAD_DIVIDE" => true,
        "NUMLOCK" | "NUM_LOCK" => true,

        // Media keys
        "XF86BACK" | "XF86FORWARD" => true,

        _ => {
            // Fallback: Accept KEY_* format strings as valid
            // These will be parsed by evdev's FromStr in the daemon
            name.to_uppercase().starts_with("KEY_")
        }
    }
}

fn parse_key_value_block(
    node: &kdl::KdlNode,
    context: &str,
    source: &str,
) -> Result<std::collections::HashMap<String, String>, ConfigError> {
    let mut map = std::collections::HashMap::new();
    let mut invalid_keys = Vec::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let from_key = child.name().value().to_string();

            // Validate the "from" key
            if !is_valid_key(&from_key) {
                invalid_keys.push(InvalidKeyInfo {
                    key: from_key.clone(),
                    position: KeyPosition::From,
                    context: context.to_string(),
                    location: get_node_location(child, source),
                });
            }

            if let Some(entry) = child.entries().first() {
                if let Some(to_key) = entry.value().as_string() {
                    // Validate the "to" key
                    if !is_valid_key(to_key) {
                        invalid_keys.push(InvalidKeyInfo {
                            key: to_key.to_string(),
                            position: KeyPosition::To,
                            context: context.to_string(),
                            location: get_entry_location(entry, source),
                        });
                    }
                    map.insert(from_key, to_key.to_string());
                }
            }
        }
    }

    // Return error with all invalid keys if any were found
    if !invalid_keys.is_empty() {
        return Err(ConfigError::InvalidKeys {
            src: None, // Source will be added by caller if needed
            invalid_keys,
        });
    }

    Ok(map)
}

fn parse_macro_block(
    node: &kdl::KdlNode,
    source: &str,
) -> Result<std::collections::HashMap<String, Vec<MacroAction>>, ConfigError> {
    let mut map = std::collections::HashMap::new();
    let mut invalid_keys = Vec::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let key = child.name().value().to_string();

            // Validate the macro trigger key
            if !is_valid_key(&key) {
                invalid_keys.push(InvalidKeyInfo {
                    key: key.clone(),
                    position: KeyPosition::From,
                    context: "macro".to_string(),
                    location: get_node_location(child, source),
                });
            }

            let mut actions = Vec::new();

            for entry in child.entries() {
                if let Some(val) = entry.value().as_string() {
                    if val.starts_with("delay(") && val.ends_with(')') {
                        // Parse delay(ms)
                        let ms_str = &val[6..val.len() - 1];
                        if let Ok(ms) = ms_str.parse::<u64>() {
                            // Validate delay value: must be positive and <= 10000ms (10 seconds)
                            if ms == 0 {
                                return Err(ConfigError::Invalid {
                                    message: format!(
                                        "Invalid delay value '{}': delay must be a positive integer (got 0)",
                                        val
                                    ),
                                });
                            }
                            const MAX_DELAY_MS: u64 = 10000;
                            if ms > MAX_DELAY_MS {
                                return Err(ConfigError::Invalid {
                                    message: format!(
                                        "Invalid delay value '{}': maximum delay is {}ms (10 seconds), got {}ms",
                                        val, MAX_DELAY_MS, ms
                                    ),
                                });
                            }
                            actions.push(MacroAction::Delay(ms));
                        }
                    } else {
                        // Validate the key/combo in the action
                        let invalid_action_keys = validate_key_combo(val);
                        for invalid_key in invalid_action_keys {
                            invalid_keys.push(InvalidKeyInfo {
                                key: invalid_key,
                                position: KeyPosition::Action,
                                context: "macro".to_string(),
                                location: get_entry_location(entry, source),
                            });
                        }
                        actions.push(MacroAction::Key(val.to_string()));
                    }
                }
            }

            map.insert(key, actions);
        }
    }

    // Return error with all invalid keys if any were found
    if !invalid_keys.is_empty() {
        return Err(ConfigError::InvalidKeys {
            src: None, // Source will be added by caller if needed
            invalid_keys,
        });
    }

    Ok(map)
}

fn parse_niri_passthrough(node: &kdl::KdlNode) -> Result<Vec<NiriKeybind>, ConfigError> {
    let mut keybinds = Vec::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let key = child.name().value().to_string();

            // The action is in the child's children block
            if let Some(action_children) = child.children() {
                // Reconstruct the action from the children
                let action = action_children
                    .nodes()
                    .iter()
                    .map(|n| {
                        let mut s = n.name().value().to_string();
                        for entry in n.entries() {
                            if let Some(v) = entry.value().as_string() {
                                s.push_str(&format!(" \"{}\"", v));
                            } else if let Some(v) = entry.value().as_i64() {
                                s.push_str(&format!(" {}", v));
                            }
                        }
                        s
                    })
                    .collect::<Vec<_>>()
                    .join("; ");

                keybinds.push(NiriKeybind {
                    key,
                    action: format!("{};", action),
                });
            }
        }
    }

    Ok(keybinds)
}

/// Parse profile-switch block: maps keybind strings to profile names
/// Example KDL:
/// ```kdl
/// profile-switch {
///     Ctrl+Shift+1 "default"
///     Ctrl+Shift+2 "gaming"
/// }
/// ```
fn parse_profile_switch(
    node: &kdl::KdlNode,
) -> Result<std::collections::HashMap<String, String>, ConfigError> {
    let mut map = std::collections::HashMap::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            // The node name is the keybind string (e.g., "Ctrl+Shift+1")
            let keybind = child.name().value().to_string();

            // The first argument is the profile name
            if let Some(entry) = child.entries().first() {
                if let Some(profile_name) = entry.value().as_string() {
                    map.insert(keybind, profile_name.to_string());
                }
            }
        }
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_config() {
        let config = r#"
            global {
                log-level "debug"
            }

            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config).unwrap();
        assert_eq!(result.global.log_level, LogLevel::Debug);
        assert_eq!(result.devices.len(), 1);
        assert_eq!(result.devices[0].name, Some("Test Keyboard".to_string()));
    }

    #[test]
    fn test_device_missing_name_error() {
        let config = r#"
            device {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::MissingField { field } => {
                assert!(field.contains("device name"));
            }
            _ => panic!("Expected MissingField error, got: {:?}", err),
        }
    }

    #[test]
    fn test_device_with_remappings_missing_default_profile() {
        let config = r#"
            device "Test Keyboard" {
                profile "custom" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(message.contains("default"));
                assert!(message.contains("Test Keyboard"));
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_device_with_default_profile_succeeds() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_device_without_remappings_no_default_required() {
        // A device with no remappings doesn't need a default profile
        let config = r#"
            device "Test Keyboard" {
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_device_with_empty_profile_no_default_required() {
        // A device with an empty profile doesn't need a default profile
        let config = r#"
            device "Test Keyboard" {
                profile "custom" {
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_unknown_key_in_remap_from_fails() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        UnknownKey "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                assert_eq!(invalid_keys.len(), 1);
                assert_eq!(invalid_keys[0].key, "UnknownKey");
                assert_eq!(invalid_keys[0].position, KeyPosition::From);
                assert_eq!(invalid_keys[0].context, "remap");
            }
            _ => panic!("Expected InvalidKeys error, got: {:?}", err),
        }
    }

    #[test]
    fn test_unknown_key_in_remap_to_fails() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "NonExistentKey"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                assert_eq!(invalid_keys.len(), 1);
                assert_eq!(invalid_keys[0].key, "NonExistentKey");
                assert_eq!(invalid_keys[0].position, KeyPosition::To);
                assert_eq!(invalid_keys[0].context, "remap");
            }
            _ => panic!("Expected InvalidKeys error, got: {:?}", err),
        }
    }

    #[test]
    fn test_multiple_unknown_keys_all_reported() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        BadKey1 "BadKey2"
                        BadKey3 "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                assert_eq!(invalid_keys.len(), 3);
                let keys: Vec<&str> = invalid_keys.iter().map(|k| k.key.as_str()).collect();
                assert!(keys.contains(&"BadKey1"));
                assert!(keys.contains(&"BadKey2"));
                assert!(keys.contains(&"BadKey3"));
            }
            _ => panic!("Expected InvalidKeys error, got: {:?}", err),
        }
    }

    #[test]
    fn test_valid_keys_parse_successfully() {
        // Test various valid key names
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                        F1 "F2"
                        LeftShift "RightShift"
                        A "B"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_extended_keys_valid() {
        // Test that all extended keys (numbers, symbols, arrows, etc.) are valid
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        Up "Down"
                        Home "End"
                        PageUp "PageDown"
                        Insert "Delete"
                        F13 "F24"
                        KP0 "KP9"
                        Minus "Equals"
                        LeftBrace "RightBrace"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_raw_evdev_key_format_valid() {
        // Test that KEY_* format strings are accepted as valid
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        KEY_LEFTMETA "KEY_A"
                        KEY_CAPSLOCK "KEY_ESC"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_tilde_expansion_in_niri_keybinds_path() {
        let config = r#"
            global {
                niri-keybinds-path "~/.config/niri/keybinds.kdl"
            }
        "#;

        let result = parse_config_str(config).unwrap();

        // The path should NOT contain a tilde after expansion
        let path_str = result.global.niri_keybinds_path.to_string_lossy();
        assert!(
            !path_str.starts_with('~'),
            "Tilde should be expanded, but got: {}",
            path_str
        );

        // The path should contain the home directory
        let home = std::env::var("HOME").expect("HOME environment variable not set");
        assert!(
            path_str.starts_with(&home),
            "Path should start with home directory '{}', but got: {}",
            home,
            path_str
        );

        // The path should end with the correct suffix
        assert!(
            path_str.ends_with("/.config/niri/keybinds.kdl"),
            "Path should end with '/.config/niri/keybinds.kdl', but got: {}",
            path_str
        );
    }

    #[test]
    fn test_absolute_path_unchanged_in_niri_keybinds_path() {
        let config = r#"
            global {
                niri-keybinds-path "/etc/niri/keybinds.kdl"
            }
        "#;

        let result = parse_config_str(config).unwrap();

        // Absolute paths should remain unchanged
        let path_str = result.global.niri_keybinds_path.to_string_lossy();
        assert_eq!(
            path_str, "/etc/niri/keybinds.kdl",
            "Absolute path should remain unchanged"
        );
    }

    #[test]
    fn test_minimal_valid_config() {
        // Minimal valid config: single device with name, single default profile, one remap entry
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        // Should parse without error
        let result = parse_config_str(config);
        assert!(result.is_ok(), "Minimal valid config should parse successfully");

        let config = result.unwrap();

        // Verify parsed values are correct
        assert_eq!(config.devices.len(), 1, "Should have exactly one device");

        let device = &config.devices[0];
        assert_eq!(
            device.name.as_ref(),
            Some(&"Test Keyboard".to_string()),
            "Device name should be 'Test Keyboard'"
        );

        // Verify default profile exists
        assert!(
            device.profiles.contains_key("default"),
            "Device should have a 'default' profile"
        );

        let default_profile = &device.profiles["default"];

        // Verify remap entry is correct
        assert_eq!(
            default_profile.remap.len(),
            1,
            "Default profile should have exactly one remap entry"
        );
        assert_eq!(
            default_profile.remap.get("CapsLock"),
            Some(&"Escape".to_string()),
            "CapsLock should be remapped to Escape"
        );
    }

    #[test]
    fn test_profile_switch_parsing() {
        let config = r#"
            device "Test Keyboard" {
                profile-switch {
                    Ctrl+Shift+1 "default"
                    Ctrl+Shift+2 "gaming"
                }
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
                profile "gaming" {
                    remap {
                        CapsLock "LeftCtrl"
                    }
                }
            }
        "#;

        let result = parse_config_str(config).unwrap();
        assert_eq!(result.devices.len(), 1);

        let device = &result.devices[0];

        // Verify profile-switch entries were parsed
        assert_eq!(device.profile_switch.len(), 2);
        assert_eq!(
            device.profile_switch.get("Ctrl+Shift+1"),
            Some(&"default".to_string())
        );
        assert_eq!(
            device.profile_switch.get("Ctrl+Shift+2"),
            Some(&"gaming".to_string())
        );

        // Verify profiles were also parsed
        assert!(device.profiles.contains_key("default"));
        assert!(device.profiles.contains_key("gaming"));
    }

    #[test]
    fn test_empty_profile_switch() {
        let config = r#"
            device "Test Keyboard" {
                profile-switch {
                }
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config).unwrap();
        let device = &result.devices[0];

        // Empty profile-switch should result in empty HashMap
        assert!(device.profile_switch.is_empty());
    }

    #[test]
    fn test_device_without_profile_switch() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config).unwrap();
        let device = &result.devices[0];

        // Device without profile-switch should have empty HashMap
        assert!(device.profile_switch.is_empty());
    }

    #[test]
    fn test_duplicate_profile_names_fails() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
                profile "default" {
                    remap {
                        A "B"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(
                    message.contains("Duplicate profile name"),
                    "Error should mention duplicate profile: {}",
                    message
                );
                assert!(
                    message.contains("default"),
                    "Error should mention the profile name: {}",
                    message
                );
                assert!(
                    message.contains("Test Keyboard"),
                    "Error should mention the device name: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_same_profile_name_different_devices_succeeds() {
        // Same profile name in different devices should be allowed
        let config = r#"
            device "Keyboard 1" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
            device "Keyboard 2" {
                profile "default" {
                    remap {
                        A "B"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.devices.len(), 2);
        assert!(config.devices[0].profiles.contains_key("default"));
        assert!(config.devices[1].profiles.contains_key("default"));
    }

    #[test]
    fn test_invalid_macro_trigger_key_fails() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        InvalidTriggerKey "A" "B" "C"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                assert_eq!(invalid_keys.len(), 1);
                assert_eq!(invalid_keys[0].key, "InvalidTriggerKey");
                assert_eq!(invalid_keys[0].position, KeyPosition::From);
                assert_eq!(invalid_keys[0].context, "macro");
            }
            _ => panic!("Expected InvalidKeys error, got: {:?}", err),
        }
    }

    #[test]
    fn test_valid_macro_trigger_key_succeeds() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "A" "B" "C"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
        let config = result.unwrap();
        let profile = &config.devices[0].profiles["default"];
        assert!(profile.macros.contains_key("F12"));
    }

    #[test]
    fn test_macro_trigger_conflicts_with_remap_source() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                    macro {
                        CapsLock "A" "B" "C"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(
                    message.contains("conflicts"),
                    "Error should mention conflict: {}",
                    message
                );
                assert!(
                    message.contains("CapsLock"),
                    "Error should mention conflicting key: {}",
                    message
                );
                assert!(
                    message.contains("remap"),
                    "Error should mention remap: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_macro_trigger_conflicts_with_combo_trigger() {
        // Note: combo keys like "Ctrl+A" are stored as strings and not validated as single keys.
        // The conflict detection extracts the base key (A) from "Ctrl+A" and checks against macro triggers.
        // For this test, we use a simple key as combo trigger to avoid combo key validation issues.
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    combo {
                        A "B"
                    }
                    macro {
                        A "X" "Y" "Z"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(
                    message.contains("conflicts"),
                    "Error should mention conflict: {}",
                    message
                );
                assert!(
                    message.contains("combo"),
                    "Error should mention combo trigger: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_macro_trigger_no_conflict_with_different_keys() {
        // Use simple keys for combo to avoid combo key validation issues
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                    combo {
                        B "C"
                    }
                    macro {
                        F12 "X" "Y" "Z"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_multiple_invalid_macro_trigger_keys_all_reported() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        BadKey1 "A" "B"
                        BadKey2 "C" "D"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                assert_eq!(invalid_keys.len(), 2);
                let keys: Vec<&str> = invalid_keys.iter().map(|k| k.key.as_str()).collect();
                assert!(keys.contains(&"BadKey1"));
                assert!(keys.contains(&"BadKey2"));
            }
            _ => panic!("Expected InvalidKeys error, got: {:?}", err),
        }
    }

    #[test]
    fn test_macro_trigger_case_insensitive_conflict() {
        // Test that conflict detection is case-insensitive
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        capslock "Escape"
                    }
                    macro {
                        CAPSLOCK "A" "B" "C"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(
                    message.contains("conflicts"),
                    "Error should mention conflict: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_invalid_macro_action_key_fails() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "A" "InvalidKey" "C"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                assert_eq!(invalid_keys.len(), 1);
                assert_eq!(invalid_keys[0].key, "InvalidKey");
                assert_eq!(invalid_keys[0].position, KeyPosition::Action);
                assert_eq!(invalid_keys[0].context, "macro");
            }
            _ => panic!("Expected InvalidKeys error, got: {:?}", err),
        }
    }

    #[test]
    fn test_invalid_macro_action_combo_fails() {
        // Test that invalid keys in combos like "Ctrll+C" are caught
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "A" "Ctrll+C" "B"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                assert_eq!(invalid_keys.len(), 1);
                assert_eq!(invalid_keys[0].key, "Ctrll");
                assert_eq!(invalid_keys[0].position, KeyPosition::Action);
            }
            _ => panic!("Expected InvalidKeys error, got: {:?}", err),
        }
    }

    #[test]
    fn test_multiple_invalid_macro_action_keys_all_reported() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "BadKey1" "BadKey2" "delay(50)" "BadKey3"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                assert_eq!(invalid_keys.len(), 3);
                let keys: Vec<&str> = invalid_keys.iter().map(|k| k.key.as_str()).collect();
                assert!(keys.contains(&"BadKey1"));
                assert!(keys.contains(&"BadKey2"));
                assert!(keys.contains(&"BadKey3"));
            }
            _ => panic!("Expected InvalidKeys error, got: {:?}", err),
        }
    }

    #[test]
    fn test_valid_macro_action_keys_succeed() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "A" "Ctrl+C" "delay(50)" "Shift+Alt+V"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
        let config = result.unwrap();
        let profile = &config.devices[0].profiles["default"];
        let actions = profile.macros.get("F12").unwrap();
        assert_eq!(actions.len(), 4);
    }

    #[test]
    fn test_invalid_trigger_and_action_keys_both_reported() {
        // Both trigger key and action keys should be validated and reported
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        BadTrigger "BadAction1" "A" "BadAction2"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                assert_eq!(invalid_keys.len(), 3);
                let keys: Vec<&str> = invalid_keys.iter().map(|k| k.key.as_str()).collect();
                assert!(keys.contains(&"BadTrigger"));
                assert!(keys.contains(&"BadAction1"));
                assert!(keys.contains(&"BadAction2"));

                // Check positions are correct
                let trigger = invalid_keys.iter().find(|k| k.key == "BadTrigger").unwrap();
                assert_eq!(trigger.position, KeyPosition::From);

                let action1 = invalid_keys.iter().find(|k| k.key == "BadAction1").unwrap();
                assert_eq!(action1.position, KeyPosition::Action);
            }
            _ => panic!("Expected InvalidKeys error, got: {:?}", err),
        }
    }

    #[test]
    fn test_macro_delay_exceeds_maximum_fails() {
        // Delay value exceeding 10000ms (10 seconds) should fail
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "A" "delay(10001)" "B"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(
                    message.contains("10000"),
                    "Error should mention maximum delay: {}",
                    message
                );
                assert!(
                    message.contains("10001"),
                    "Error should mention the invalid value: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_macro_delay_zero_fails() {
        // Delay value of 0 should fail (must be positive)
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "A" "delay(0)" "B"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(
                    message.contains("positive"),
                    "Error should mention positive integer: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_macro_delay_at_maximum_succeeds() {
        // Delay value of exactly 10000ms should succeed
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "A" "delay(10000)" "B"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
        let config = result.unwrap();
        let profile = &config.devices[0].profiles["default"];
        let actions = profile.macros.get("F12").unwrap();
        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[1], MacroAction::Delay(10000)));
    }

    #[test]
    fn test_macro_delay_valid_values_succeed() {
        // Various valid delay values should succeed
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "delay(1)" "delay(50)" "delay(100)" "delay(1000)" "delay(5000)"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
        let config = result.unwrap();
        let profile = &config.devices[0].profiles["default"];
        let actions = profile.macros.get("F12").unwrap();
        assert_eq!(actions.len(), 5);
        assert!(matches!(actions[0], MacroAction::Delay(1)));
        assert!(matches!(actions[1], MacroAction::Delay(50)));
        assert!(matches!(actions[2], MacroAction::Delay(100)));
        assert!(matches!(actions[3], MacroAction::Delay(1000)));
        assert!(matches!(actions[4], MacroAction::Delay(5000)));
    }

    #[test]
    fn test_macro_delay_very_large_value_fails() {
        // Very large delay value should fail
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    macro {
                        F12 "A" "delay(999999)" "B"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(
                    message.contains("maximum"),
                    "Error should mention maximum delay: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_profile_switch_references_nonexistent_profile_fails() {
        let config = r#"
            device "Test Keyboard" {
                profile-switch {
                    Ctrl+Shift+1 "default"
                    Ctrl+Shift+2 "nonexistent"
                }
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(
                    message.contains("nonexistent"),
                    "Error should mention the invalid profile name: {}",
                    message
                );
                assert!(
                    message.contains("profile-switch"),
                    "Error should mention profile-switch: {}",
                    message
                );
                assert!(
                    message.contains("Test Keyboard"),
                    "Error should mention the device name: {}",
                    message
                );
                assert!(
                    message.contains("Ctrl+Shift+2"),
                    "Error should mention the keybind: {}",
                    message
                );
                assert!(
                    message.contains("default"),
                    "Error should mention available profiles: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_profile_switch_valid_references_succeed() {
        let config = r#"
            device "Test Keyboard" {
                profile-switch {
                    Ctrl+Shift+1 "default"
                    Ctrl+Shift+2 "gaming"
                }
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
                profile "gaming" {
                    remap {
                        CapsLock "LeftCtrl"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_profile_switch_no_profiles_defined_fails() {
        // A profile-switch with no profiles defined should fail
        let config = r#"
            device "Test Keyboard" {
                profile-switch {
                    Ctrl+Shift+1 "default"
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                assert!(
                    message.contains("non-existent profile"),
                    "Error should mention non-existent profile: {}",
                    message
                );
                assert!(
                    message.contains("no profiles defined"),
                    "Error should mention no profiles defined: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_profile_switch_multiple_invalid_refs_first_reported() {
        // When multiple profile-switch entries reference non-existent profiles,
        // the first one encountered should be reported
        let config = r#"
            device "Test Keyboard" {
                profile-switch {
                    Ctrl+Shift+1 "nonexistent1"
                    Ctrl+Shift+2 "nonexistent2"
                }
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            ConfigError::Invalid { message } => {
                // Should report one of the invalid profiles
                assert!(
                    message.contains("nonexistent1") || message.contains("nonexistent2"),
                    "Error should mention an invalid profile name: {}",
                    message
                );
            }
            _ => panic!("Expected Invalid error, got: {:?}", err),
        }
    }

    #[test]
    fn test_multiple_profiles_without_profile_switch_succeeds_with_warning() {
        // Device with multiple profiles but no profile-switch should succeed
        // (warning is logged but parsing should not fail)
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
                profile "gaming" {
                    remap {
                        CapsLock "LeftCtrl"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok(), "Parsing should succeed even without profile-switch");

        let config = result.unwrap();
        assert_eq!(config.devices.len(), 1);

        let device = &config.devices[0];
        assert_eq!(device.profiles.len(), 2);
        assert!(device.profiles.contains_key("default"));
        assert!(device.profiles.contains_key("gaming"));
        assert!(device.profile_switch.is_empty());
    }

    #[test]
    fn test_single_profile_without_profile_switch_no_warning() {
        // Device with only one profile should not trigger warning
        // (nothing to switch to, so profile-switch is not needed)
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());

        let config = result.unwrap();
        let device = &config.devices[0];
        assert_eq!(device.profiles.len(), 1);
        assert!(device.profile_switch.is_empty());
    }

    #[test]
    fn test_app_id_hint_parsing() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
                profile "firefox" {
                    app-id-hint "org.mozilla.firefox"
                    remap {
                        CapsLock "LeftCtrl"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());

        let config = result.unwrap();
        let device = &config.devices[0];

        // Default profile should have no app-id-hint
        let default_profile = &device.profiles["default"];
        assert!(
            default_profile.app_id_hint.is_none(),
            "default profile should have no app-id-hint"
        );

        // Firefox profile should have app-id-hint
        let firefox_profile = &device.profiles["firefox"];
        assert_eq!(
            firefox_profile.app_id_hint,
            Some("org.mozilla.firefox".to_string()),
            "firefox profile should have app-id-hint"
        );
    }

    #[test]
    fn test_app_id_hint_missing_is_none() {
        // Profile without app-id-hint should have None
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());

        let config = result.unwrap();
        let device = &config.devices[0];
        let profile = &device.profiles["default"];

        assert!(
            profile.app_id_hint.is_none(),
            "profile without app-id-hint should have None"
        );
    }

    #[test]
    fn test_app_id_hint_with_simple_string() {
        let config = r#"
            device "Test Keyboard" {
                profile "default" {
                    app-id-hint "firefox"
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config);
        assert!(result.is_ok());

        let config = result.unwrap();
        let device = &config.devices[0];
        let profile = &device.profiles["default"];

        assert_eq!(
            profile.app_id_hint,
            Some("firefox".to_string()),
            "app-id-hint should be parsed correctly"
        );
    }
}
