//! KDL configuration parser

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
                    let profile = parse_profile(child, source)?;
                    device.profiles.insert(profile_name, profile);
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

    Ok(device)
}

fn parse_profile(node: &kdl::KdlNode, source: &str) -> Result<Profile, ConfigError> {
    let mut profile = Profile::default();
    let mut all_invalid_keys = Vec::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
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
                    profile.macros = parse_macro_block(child)?;
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

    Ok(profile)
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
) -> Result<std::collections::HashMap<String, Vec<MacroAction>>, ConfigError> {
    let mut map = std::collections::HashMap::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let key = child.name().value().to_string();
            let mut actions = Vec::new();

            for entry in child.entries() {
                if let Some(val) = entry.value().as_string() {
                    if val.starts_with("delay(") && val.ends_with(')') {
                        // Parse delay(ms)
                        let ms_str = &val[6..val.len() - 1];
                        if let Ok(ms) = ms_str.parse::<u64>() {
                            actions.push(MacroAction::Delay(ms));
                        }
                    } else {
                        actions.push(MacroAction::Key(val.to_string()));
                    }
                }
            }

            map.insert(key, actions);
        }
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
}
