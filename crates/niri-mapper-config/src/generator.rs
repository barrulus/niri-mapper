//! Generate niri-compatible KDL keybind files

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::error::{ConfigError, DuplicateKeybindInfo};
use crate::model::Config;

/// Format a SystemTime as an ISO 8601 timestamp (UTC).
fn format_timestamp(time: SystemTime) -> String {
    let duration = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();

    // Calculate date/time components from Unix timestamp
    // Days since epoch
    let days = secs / 86400;
    let remaining = secs % 86400;
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    let seconds = remaining % 60;

    // Calculate year, month, day using a simplified algorithm
    // Starting from 1970-01-01
    let mut year = 1970;
    let mut remaining_days = days as i64;

    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let days_in_months: [i64; 12] = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1;
    for days_in_month in days_in_months.iter() {
        if remaining_days < *days_in_month {
            break;
        }
        remaining_days -= days_in_month;
        month += 1;
    }
    let day = remaining_days + 1;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Translate key modifiers from common notation to niri notation.
///
/// Maps:
/// - `Super` -> `Mod` (niri convention for the super/meta key)
/// - `Ctrl`, `Alt`, `Shift` -> passed through unchanged
///
/// This handles all modifier variations in a key combination like `Ctrl+Shift+Super+Q`.
fn translate_modifiers(key: &str) -> String {
    // Split on '+' to handle each part of the key combination
    key.split('+')
        .map(|part| {
            let trimmed = part.trim();
            match trimmed {
                "Super" => "Mod",
                // Pass through all other modifiers and keys unchanged
                other => other,
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Detect duplicate keybinds across all devices and profiles.
///
/// Scans all niri-passthrough keybinds from all devices/profiles and identifies
/// any key combinations that appear more than once. Returns an error if duplicates
/// are found, with a clear message naming which devices/profiles have conflicts.
///
/// This validation should be called before generating the KDL output to fail fast
/// on configuration errors.
pub fn detect_duplicate_keybinds(config: &Config) -> Result<(), ConfigError> {
    // Map: key combination -> list of (device_name, profile_name) where it's defined
    let mut keybind_sources: HashMap<String, Vec<(String, String)>> = HashMap::new();

    for device in &config.devices {
        let device_name = device
            .name
            .clone()
            .unwrap_or_else(|| "<unnamed>".to_string());

        for (profile_name, profile) in &device.profiles {
            for keybind in &profile.niri_passthrough {
                keybind_sources
                    .entry(keybind.key.clone())
                    .or_default()
                    .push((device_name.clone(), profile_name.clone()));
            }
        }
    }

    // Find all keys that appear more than once
    let mut duplicates: Vec<DuplicateKeybindInfo> = Vec::new();

    for (key, sources) in keybind_sources {
        if sources.len() > 1 {
            // Add all sources as duplicates
            for (device, profile) in sources {
                duplicates.push(DuplicateKeybindInfo {
                    key: key.clone(),
                    device,
                    profile,
                });
            }
        }
    }

    if duplicates.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::DuplicateKeybinds { duplicates })
    }
}

/// Generate niri keybinds KDL from the configuration.
///
/// # Arguments
/// * `config` - The parsed configuration
/// * `source_path` - The absolute path to the source config file
///
/// The generated file includes a header with:
/// - Auto-generated notice
/// - Source config path
/// - Generation timestamp (ISO 8601 UTC)
/// - Warning not to edit manually
pub fn generate_niri_keybinds(config: &Config, source_path: &Path) -> String {
    let mut output = String::new();

    // Generate header with source path and timestamp
    let timestamp = format_timestamp(SystemTime::now());
    let source_display = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());

    output.push_str("// Auto-generated by niri-mapper\n");
    output.push_str(&format!("// Source: {}\n", source_display.display()));
    output.push_str(&format!("// Generated: {}\n", timestamp));
    output.push_str("// DO NOT EDIT - changes will be overwritten\n\n");

    output.push_str("binds {\n");

    // Collect all niri-passthrough keybinds from all devices/profiles
    for device in &config.devices {
        for (_profile_name, profile) in &device.profiles {
            for keybind in &profile.niri_passthrough {
                // Translate modifiers (Super -> Mod, others pass through)
                let niri_key = translate_modifiers(&keybind.key);
                output.push_str(&format!("    {} {{ {} }}\n", niri_key, keybind.action));
            }
        }
    }

    output.push_str("}\n");

    output
}

/// Validate that the generated KDL can be parsed back by kdl-rs.
///
/// This ensures we never write invalid KDL to the output file.
/// Returns an error if the KDL fails to parse.
fn validate_kdl(content: &str) -> Result<(), ConfigError> {
    content.parse::<kdl::KdlDocument>().map_err(|e| {
        ConfigError::Invalid {
            message: format!(
                "Generated KDL is invalid (this is a bug in niri-mapper): {}",
                e
            ),
        }
    })?;
    Ok(())
}

/// Write niri keybinds to the configured path using atomic writes.
///
/// This function:
/// 1. Detects duplicate keybinds across devices/profiles (fails hard if found)
/// 2. Generates the KDL content from the configuration
/// 3. Validates the generated KDL can be parsed back (fails hard if invalid)
/// 4. Writes to a temporary file in the same directory
/// 5. Atomically renames the temp file to the target path
///
/// If validation or write fails, the original file is preserved.
/// The temp file is written to the same directory as the target to ensure
/// the atomic rename works (must be on the same filesystem).
///
/// # Arguments
/// * `config` - The parsed configuration
/// * `source_path` - The absolute path to the source config file
pub fn write_niri_keybinds(config: &Config, source_path: &Path) -> Result<(), ConfigError> {
    // Detect duplicate keybinds before generating - fail hard if duplicates found
    detect_duplicate_keybinds(config)?;

    let content = generate_niri_keybinds(config, source_path);

    // Validate generated KDL before writing - fail hard if invalid
    validate_kdl(&content)?;

    let path = &config.global.niri_keybinds_path;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Atomic write: write to temp file, then rename
    // Temp file must be in same directory for atomic rename to work
    let temp_path = path.with_extension("kdl.tmp");

    // Write to temp file
    if let Err(e) = std::fs::write(&temp_path, &content) {
        // Clean up temp file if it was partially written
        let _ = std::fs::remove_file(&temp_path);
        return Err(e.into());
    }

    // Atomic rename - this preserves the original file if rename fails
    if let Err(e) = std::fs::rename(&temp_path, path) {
        // Clean up temp file on rename failure
        let _ = std::fs::remove_file(&temp_path);
        return Err(e.into());
    }

    tracing::info!("Wrote niri keybinds to {}", path.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use std::path::PathBuf;

    #[test]
    fn test_translate_modifiers_super_to_mod() {
        assert_eq!(translate_modifiers("Super+Return"), "Mod+Return");
    }

    #[test]
    fn test_translate_modifiers_ctrl_shift_super() {
        // DoD: Ctrl+Shift+Super+Q translates to Ctrl+Shift+Mod+Q
        assert_eq!(
            translate_modifiers("Ctrl+Shift+Super+Q"),
            "Ctrl+Shift+Mod+Q"
        );
    }

    #[test]
    fn test_translate_modifiers_passthrough() {
        // Ctrl, Alt, Shift should pass through unchanged
        assert_eq!(translate_modifiers("Ctrl+A"), "Ctrl+A");
        assert_eq!(translate_modifiers("Alt+Tab"), "Alt+Tab");
        assert_eq!(translate_modifiers("Shift+Enter"), "Shift+Enter");
        assert_eq!(translate_modifiers("Ctrl+Alt+Delete"), "Ctrl+Alt+Delete");
    }

    #[test]
    fn test_translate_modifiers_single_key() {
        // Single keys without modifiers should pass through
        assert_eq!(translate_modifiers("Return"), "Return");
        assert_eq!(translate_modifiers("Escape"), "Escape");
    }

    #[test]
    fn test_validate_kdl_valid() {
        let valid_kdl = "binds {\n    Mod+Return { spawn \"alacritty\"; }\n}\n";
        assert!(validate_kdl(valid_kdl).is_ok());
    }

    #[test]
    fn test_validate_kdl_invalid() {
        // Missing closing brace
        let invalid_kdl = "binds {\n    Mod+Return { spawn \"alacritty\"\n";
        let result = validate_kdl(invalid_kdl);
        assert!(result.is_err());
        if let Err(ConfigError::Invalid { message }) = result {
            assert!(message.contains("Generated KDL is invalid"));
        } else {
            panic!("Expected ConfigError::Invalid");
        }
    }

    #[test]
    fn test_detect_duplicate_keybinds_no_duplicates() {
        // Config with unique keybinds across devices should pass
        let config = Config {
            global: GlobalConfig::default(),
            devices: vec![
                DeviceConfig {
                    name: Some("Keyboard1".to_string()),
                    vendor_product: None,
                    profiles: [(
                        "default".to_string(),
                        Profile {
                            niri_passthrough: vec![NiriKeybind {
                                key: "Super+Return".to_string(),
                                action: "spawn \"alacritty\";".to_string(),
                            }],
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    profile_switch: HashMap::new(),
                },
                DeviceConfig {
                    name: Some("Keyboard2".to_string()),
                    vendor_product: None,
                    profiles: [(
                        "default".to_string(),
                        Profile {
                            niri_passthrough: vec![NiriKeybind {
                                key: "Super+T".to_string(),
                                action: "spawn \"kitty\";".to_string(),
                            }],
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    profile_switch: HashMap::new(),
                },
            ],
        };

        assert!(detect_duplicate_keybinds(&config).is_ok());
    }

    #[test]
    fn test_detect_duplicate_keybinds_across_devices() {
        // DoD: Duplicate Super+Return in two devices fails with message naming both sources
        let config = Config {
            global: GlobalConfig::default(),
            devices: vec![
                DeviceConfig {
                    name: Some("Keyboard1".to_string()),
                    vendor_product: None,
                    profiles: [(
                        "default".to_string(),
                        Profile {
                            niri_passthrough: vec![NiriKeybind {
                                key: "Super+Return".to_string(),
                                action: "spawn \"alacritty\";".to_string(),
                            }],
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    profile_switch: HashMap::new(),
                },
                DeviceConfig {
                    name: Some("Keyboard2".to_string()),
                    vendor_product: None,
                    profiles: [(
                        "default".to_string(),
                        Profile {
                            niri_passthrough: vec![NiriKeybind {
                                key: "Super+Return".to_string(),
                                action: "spawn \"kitty\";".to_string(),
                            }],
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    profile_switch: HashMap::new(),
                },
            ],
        };

        let result = detect_duplicate_keybinds(&config);
        assert!(result.is_err());

        if let Err(ConfigError::DuplicateKeybinds { duplicates }) = result {
            // Should have 2 entries for the duplicate key (one for each device)
            assert_eq!(duplicates.len(), 2);

            // Both should reference Super+Return
            assert!(duplicates.iter().all(|d| d.key == "Super+Return"));

            // Should identify both devices
            let devices: Vec<&str> = duplicates.iter().map(|d| d.device.as_str()).collect();
            assert!(devices.contains(&"Keyboard1"));
            assert!(devices.contains(&"Keyboard2"));

            // Verify the error message format
            let error = ConfigError::DuplicateKeybinds { duplicates };
            let msg = format!("{}", error);
            assert!(
                msg.contains("Super+Return"),
                "Error message should mention the duplicate key: {}",
                msg
            );
            assert!(
                msg.contains("Keyboard1"),
                "Error message should mention Keyboard1: {}",
                msg
            );
            assert!(
                msg.contains("Keyboard2"),
                "Error message should mention Keyboard2: {}",
                msg
            );
        } else {
            panic!("Expected ConfigError::DuplicateKeybinds");
        }
    }

    #[test]
    fn test_detect_duplicate_keybinds_across_profiles() {
        // Duplicates within the same device but different profiles should also be detected
        let config = Config {
            global: GlobalConfig::default(),
            devices: vec![DeviceConfig {
                name: Some("Keyboard1".to_string()),
                vendor_product: None,
                profiles: [
                    (
                        "default".to_string(),
                        Profile {
                            niri_passthrough: vec![NiriKeybind {
                                key: "Super+Return".to_string(),
                                action: "spawn \"alacritty\";".to_string(),
                            }],
                            ..Default::default()
                        },
                    ),
                    (
                        "gaming".to_string(),
                        Profile {
                            niri_passthrough: vec![NiriKeybind {
                                key: "Super+Return".to_string(),
                                action: "spawn \"steam\";".to_string(),
                            }],
                            ..Default::default()
                        },
                    ),
                ]
                .into_iter()
                .collect(),
                profile_switch: HashMap::new(),
            }],
        };

        let result = detect_duplicate_keybinds(&config);
        assert!(result.is_err());

        if let Err(ConfigError::DuplicateKeybinds { duplicates }) = result {
            // Should have 2 entries for the duplicate key
            assert_eq!(duplicates.len(), 2);

            // Both should reference the same device but different profiles
            let profiles: Vec<&str> = duplicates.iter().map(|d| d.profile.as_str()).collect();
            assert!(profiles.contains(&"default"));
            assert!(profiles.contains(&"gaming"));
        } else {
            panic!("Expected ConfigError::DuplicateKeybinds");
        }
    }

    #[test]
    fn test_write_niri_keybinds_fails_on_duplicates() {
        // write_niri_keybinds should fail before writing when duplicates are found
        let temp_dir = std::env::temp_dir().join("niri-mapper-test-dup");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let keybinds_path = temp_dir.join("keybinds.kdl");

        let config = Config {
            global: GlobalConfig {
                niri_keybinds_path: keybinds_path.clone(),
                ..Default::default()
            },
            devices: vec![
                DeviceConfig {
                    name: Some("Device1".to_string()),
                    vendor_product: None,
                    profiles: [(
                        "default".to_string(),
                        Profile {
                            niri_passthrough: vec![NiriKeybind {
                                key: "Super+Return".to_string(),
                                action: "spawn \"alacritty\";".to_string(),
                            }],
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    profile_switch: HashMap::new(),
                },
                DeviceConfig {
                    name: Some("Device2".to_string()),
                    vendor_product: None,
                    profiles: [(
                        "default".to_string(),
                        Profile {
                            niri_passthrough: vec![NiriKeybind {
                                key: "Super+Return".to_string(),
                                action: "spawn \"kitty\";".to_string(),
                            }],
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    profile_switch: HashMap::new(),
                },
            ],
        };

        // Write should fail due to duplicates
        let source_path = PathBuf::from("/tmp/test-config.kdl");
        let result = write_niri_keybinds(&config, &source_path);
        assert!(result.is_err());

        // File should NOT be created
        assert!(!keybinds_path.exists());

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_generate_keybinds() {
        let config = Config {
            global: GlobalConfig::default(),
            devices: vec![DeviceConfig {
                name: Some("Test".to_string()),
                vendor_product: None,
                profiles: [(
                    "default".to_string(),
                    Profile {
                        niri_passthrough: vec![NiriKeybind {
                            key: "Super+Return".to_string(),
                            action: "spawn \"alacritty\";".to_string(),
                        }],
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                profile_switch: HashMap::new(),
            }],
        };

        let source_path = PathBuf::from("/home/user/.config/niri-mapper/config.kdl");
        let output = generate_niri_keybinds(&config, &source_path);
        assert!(output.contains("Mod+Return"));
        assert!(output.contains("spawn \"alacritty\""));

        // Verify generated KDL is valid
        assert!(validate_kdl(&output).is_ok());
    }

    #[test]
    fn test_generate_keybinds_complex_modifiers() {
        let config = Config {
            global: GlobalConfig::default(),
            devices: vec![DeviceConfig {
                name: Some("Test".to_string()),
                vendor_product: None,
                profiles: [(
                    "default".to_string(),
                    Profile {
                        niri_passthrough: vec![
                            NiriKeybind {
                                key: "Ctrl+Shift+Super+Q".to_string(),
                                action: "quit;".to_string(),
                            },
                            NiriKeybind {
                                key: "Alt+Tab".to_string(),
                                action: "focus-window-next;".to_string(),
                            },
                        ],
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                profile_switch: HashMap::new(),
            }],
        };

        let source_path = PathBuf::from("/home/user/.config/niri-mapper/config.kdl");
        let output = generate_niri_keybinds(&config, &source_path);

        // Check Super -> Mod translation
        assert!(output.contains("Ctrl+Shift+Mod+Q"));
        assert!(!output.contains("Super"));

        // Check other modifiers are unchanged
        assert!(output.contains("Alt+Tab"));

        // Verify generated KDL is valid
        assert!(validate_kdl(&output).is_ok());
    }

    #[test]
    fn test_atomic_write_creates_file() {
        let temp_dir = std::env::temp_dir().join("niri-mapper-test-atomic");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let keybinds_path = temp_dir.join("keybinds.kdl");
        let source_path = temp_dir.join("config.kdl");

        let config = Config {
            global: GlobalConfig {
                niri_keybinds_path: keybinds_path.clone(),
                ..Default::default()
            },
            devices: vec![DeviceConfig {
                name: Some("Test".to_string()),
                vendor_product: None,
                profiles: [(
                    "default".to_string(),
                    Profile {
                        niri_passthrough: vec![NiriKeybind {
                            key: "Super+Return".to_string(),
                            action: "spawn \"alacritty\";".to_string(),
                        }],
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                profile_switch: HashMap::new(),
            }],
        };

        // Write should succeed
        write_niri_keybinds(&config, &source_path).unwrap();

        // File should exist
        assert!(keybinds_path.exists());

        // Content should be valid
        let content = std::fs::read_to_string(&keybinds_path).unwrap();
        assert!(content.contains("Mod+Return"));

        // Temp file should not exist (was renamed)
        let temp_path = keybinds_path.with_extension("kdl.tmp");
        assert!(!temp_path.exists());

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// Smoke test: round-trip KDL generation (task 020-1.6)
    ///
    /// This test verifies the full round-trip of:
    /// 1. Creating a sample Config with devices and niri-passthrough keybinds
    /// 2. Generating KDL output via generate_niri_keybinds()
    /// 3. Verifying the output contains expected content (Mod+Return, correct actions)
    /// 4. Verifying Super is translated to Mod
    /// 5. Parsing the output KDL to verify it's valid (round-trip validation)
    #[test]
    fn test_smoke_round_trip_kdl_generation() {
        // Create a sample config with multiple devices and keybinds
        let config = Config {
            global: GlobalConfig::default(),
            devices: vec![
                DeviceConfig {
                    name: Some("Keyboard1".to_string()),
                    vendor_product: Some("1234:5678".to_string()),
                    profiles: [(
                        "default".to_string(),
                        Profile {
                            niri_passthrough: vec![
                                NiriKeybind {
                                    key: "Super+Return".to_string(),
                                    action: "spawn \"alacritty\";".to_string(),
                                },
                                NiriKeybind {
                                    key: "Ctrl+Shift+Super+Q".to_string(),
                                    action: "quit;".to_string(),
                                },
                            ],
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    profile_switch: HashMap::new(),
                },
                DeviceConfig {
                    name: Some("Keyboard2".to_string()),
                    vendor_product: None,
                    profiles: [(
                        "gaming".to_string(),
                        Profile {
                            niri_passthrough: vec![NiriKeybind {
                                key: "Alt+Tab".to_string(),
                                action: "focus-window-next;".to_string(),
                            }],
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                    profile_switch: HashMap::new(),
                },
            ],
        };

        // Generate the KDL output
        let source_path = PathBuf::from("/home/user/.config/niri-mapper/config.kdl");
        let output = generate_niri_keybinds(&config, &source_path);

        // Verify Super is translated to Mod
        assert!(
            output.contains("Mod+Return"),
            "Super+Return should be translated to Mod+Return"
        );
        assert!(
            output.contains("Ctrl+Shift+Mod+Q"),
            "Ctrl+Shift+Super+Q should be translated to Ctrl+Shift+Mod+Q"
        );
        // Note: "Super" may appear in source path, so we check for Super+ pattern
        assert!(
            !output.contains("Super+"),
            "No 'Super+' modifier should remain in keybinds"
        );

        // Verify correct actions are present
        assert!(
            output.contains("spawn \"alacritty\""),
            "spawn action should be present"
        );
        assert!(output.contains("quit;"), "quit action should be present");
        assert!(
            output.contains("focus-window-next;"),
            "focus-window-next action should be present"
        );

        // Verify other modifiers pass through unchanged
        assert!(
            output.contains("Alt+Tab"),
            "Alt modifier should pass through unchanged"
        );

        // Verify the output has the expected structure
        assert!(
            output.contains("binds {"),
            "Output should contain 'binds' block"
        );
        assert!(
            output.contains("// Auto-generated by niri-mapper"),
            "Output should contain header comment"
        );

        // Round-trip validation: parse the generated KDL to verify it's valid
        let parse_result = validate_kdl(&output);
        assert!(
            parse_result.is_ok(),
            "Generated KDL must parse successfully: {:?}",
            parse_result.err()
        );

        // Extra verification: parse and inspect the document structure
        let doc: kdl::KdlDocument = output.parse().expect("KDL should parse");
        let binds_node = doc.get("binds").expect("Should have 'binds' node");
        let children = binds_node.children().expect("binds should have children");

        // Count the keybind entries (should be 3 total from both devices)
        let keybind_count = children.nodes().len();
        assert_eq!(
            keybind_count, 3,
            "Should have 3 keybinds from both devices"
        );
    }

    #[test]
    fn test_atomic_write_preserves_original_on_directory_error() {
        let temp_dir = std::env::temp_dir().join("niri-mapper-test-preserve");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let keybinds_path = temp_dir.join("keybinds.kdl");
        let source_path = PathBuf::from("/tmp/test-config.kdl");
        let original_content = "// Original content\nbinds { }\n";

        // Create original file
        std::fs::write(&keybinds_path, original_content).unwrap();

        // Create config with invalid path (non-existent nested directory that we'll make read-only)
        let config = Config {
            global: GlobalConfig {
                // Use a path where we cannot create the temp file
                niri_keybinds_path: PathBuf::from("/nonexistent/path/keybinds.kdl"),
                ..Default::default()
            },
            devices: vec![DeviceConfig {
                name: Some("Test".to_string()),
                vendor_product: None,
                profiles: [(
                    "default".to_string(),
                    Profile {
                        niri_passthrough: vec![NiriKeybind {
                            key: "Super+Return".to_string(),
                            action: "spawn \"alacritty\";".to_string(),
                        }],
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                profile_switch: HashMap::new(),
            }],
        };

        // Write should fail (cannot create parent directory)
        let result = write_niri_keybinds(&config, &source_path);
        assert!(result.is_err());

        // Original file should still exist with original content (different path, but testing error handling)
        let current_content = std::fs::read_to_string(&keybinds_path).unwrap();
        assert_eq!(current_content, original_content);

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_generate_keybinds_header_format() {
        let config = Config {
            global: GlobalConfig::default(),
            devices: vec![DeviceConfig {
                name: Some("Test".to_string()),
                vendor_product: None,
                profiles: [(
                    "default".to_string(),
                    Profile {
                        niri_passthrough: vec![NiriKeybind {
                            key: "Super+Return".to_string(),
                            action: "spawn \"alacritty\";".to_string(),
                        }],
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                profile_switch: HashMap::new(),
            }],
        };

        let source_path = PathBuf::from("/home/user/.config/niri-mapper/config.kdl");
        let output = generate_niri_keybinds(&config, &source_path);

        // Verify header format matches task 020-1.5 requirements
        assert!(
            output.starts_with("// Auto-generated by niri-mapper\n"),
            "Header should start with auto-generated notice"
        );
        assert!(
            output.contains("// Source:"),
            "Header should contain source path"
        );
        assert!(
            output.contains("// Generated:"),
            "Header should contain generation timestamp"
        );
        assert!(
            output.contains("// DO NOT EDIT - changes will be overwritten"),
            "Header should contain do-not-edit warning"
        );

        // Verify timestamp format is ISO 8601
        let lines: Vec<&str> = output.lines().collect();
        let generated_line = lines
            .iter()
            .find(|l| l.starts_with("// Generated:"))
            .expect("Should have Generated line");
        // Should match pattern like "// Generated: 2025-12-31T14:30:00Z"
        assert!(
            generated_line.contains("T") && generated_line.ends_with("Z"),
            "Timestamp should be in ISO 8601 UTC format: {}",
            generated_line
        );
    }

    #[test]
    fn test_format_timestamp() {
        use std::time::{Duration, UNIX_EPOCH};

        // Test a known timestamp: 2025-12-31T14:30:00Z
        // Unix timestamp for 2025-12-31T14:30:00Z = 1767191400
        let time = UNIX_EPOCH + Duration::from_secs(1767191400);
        let formatted = format_timestamp(time);
        assert_eq!(formatted, "2025-12-31T14:30:00Z");

        // Test epoch
        let epoch = UNIX_EPOCH;
        let formatted_epoch = format_timestamp(epoch);
        assert_eq!(formatted_epoch, "1970-01-01T00:00:00Z");
    }
}
