//! Control socket for daemon communication
//!
//! This module provides the JSON command format for the daemon control socket,
//! enabling runtime profile switching and status queries.
//!
//! # Per-Application Profile Switching (v0.4.0)
//!
//! This control socket is part of the v0.4.0 per-application profile foundation.
//! It provides **manual** profile switching capability while automatic switching
//! based on `app_id` is deferred to a future release.
//!
//! ## Current Capabilities (v0.4.0)
//!
//! The control socket accepts these JSON commands:
//!
//! - `{"switch_profile": {"device": "...", "profile": "..."}}`
//!   Switch a specific device to a named profile.
//!
//! - `{"list_profiles": {}}`
//!   List all devices and their available profiles.
//!
//! - `{"status": {}}`
//!   Query daemon status including active profiles per device.
//!
//! ## How Manual Switching Works
//!
//! 1. CLI sends a `switch_profile` command via this socket
//! 2. Daemon looks up the device by name
//! 3. Daemon calls `DeviceRemapper::switch_profile()` to load the new profile
//! 4. The remapper's rules are replaced with the new profile's rules
//! 5. Subsequent key events use the new profile's mappings
//!
//! ## Future: Automatic Switching (Backlog)
//!
//! In a future release, the daemon will:
//! 1. Subscribe to niri's focus change events via IPC
//! 2. On focus change, check if any profile's `app_id_hint` matches the focused
//!    window's `app_id`
//! 3. Automatically invoke `switch_profile` for matching profiles
//!
//! The control socket will continue to work for manual overrides even when
//! automatic switching is implemented.
//!
//! # Socket Location
//!
//! The control socket is created at `$XDG_RUNTIME_DIR/niri-mapper.sock` if the
//! environment variable is set, otherwise falls back to `/tmp/niri-mapper-$UID.sock`.
//!
//! # Example Usage
//!
//! ```bash
//! # Switch to gaming profile
//! echo '{"switch_profile":{"device":"Keychron K3 Pro","profile":"gaming"}}' | \
//!   socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/niri-mapper.sock
//!
//! # Or use the CLI
//! niri-mapper switch-profile "Keychron K3 Pro" gaming
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// Re-export the socket infrastructure from ipc module
pub use crate::ipc::{DeviceStatus, IpcServer};

// ============================================================================
// Control Command Types (v0.4.0 JSON format)
// ============================================================================

/// Control commands sent to the daemon via the control socket.
///
/// These commands use the exact JSON format specified in v0.4.0 task 4.6:
/// - `{"switch_profile": {"device": "...", "profile": "..."}}`
/// - `{"list_profiles": {}}`
/// - `{"status": {}}`
///
/// This is an alternative to the `IpcRequest` format which uses `{"type": "..."}`.
/// Both formats can be supported by the daemon for flexibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlCommand {
    /// Switch a device to a specific profile
    ///
    /// JSON format: `{"switch_profile": {"device": "Keychron K3 Pro", "profile": "gaming"}}`
    SwitchProfile(SwitchProfileArgs),

    /// List available profiles for all devices or a specific device
    ///
    /// JSON format: `{"list_profiles": {}}` or `{"list_profiles": {"device": "..."}}`
    ListProfiles(ListProfilesArgs),

    /// Query overall daemon status
    ///
    /// JSON format: `{"status": {}}`
    Status(StatusArgs),
}

/// Arguments for the `switch_profile` command
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwitchProfileArgs {
    /// Name of the device to switch
    pub device: String,
    /// Name of the profile to activate
    pub profile: String,
}

/// Arguments for the `list_profiles` command
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ListProfilesArgs {
    /// Optional device name to filter profiles (if None, list all devices)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// Arguments for the `status` command (currently empty, but extensible)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StatusArgs {}

// ============================================================================
// Control Response Types
// ============================================================================

/// Response to control commands
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlResponse {
    /// Operation completed successfully
    Success {
        /// Optional message with additional details
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },

    /// List of profiles for one or more devices
    Profiles {
        /// Profile information per device
        devices: Vec<DeviceProfiles>,
    },

    /// Daemon status information
    Status {
        /// Status of each grabbed device
        devices: Vec<DeviceStatus>,
    },

    /// Error occurred while processing command
    Error {
        /// Error description
        message: String,
    },
}

/// Profile information for a single device
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceProfiles {
    /// Device name
    pub device: String,
    /// Device path (e.g., /dev/input/event5)
    pub path: PathBuf,
    /// Currently active profile name
    pub active: String,
    /// List of available profile names
    pub profiles: Vec<String>,
}

// ============================================================================
// Conversion between Control and IPC types
// ============================================================================

impl From<crate::ipc::IpcRequest> for ControlCommand {
    fn from(request: crate::ipc::IpcRequest) -> Self {
        match request {
            crate::ipc::IpcRequest::ProfileSwitch { device, profile } => {
                ControlCommand::SwitchProfile(SwitchProfileArgs { device, profile })
            }
            crate::ipc::IpcRequest::ProfileList { device } => {
                ControlCommand::ListProfiles(ListProfilesArgs {
                    device: Some(device),
                })
            }
            crate::ipc::IpcRequest::Status => {
                ControlCommand::Status(StatusArgs {})
            }
        }
    }
}

impl From<ControlCommand> for crate::ipc::IpcRequest {
    fn from(command: ControlCommand) -> Self {
        match command {
            ControlCommand::SwitchProfile(args) => crate::ipc::IpcRequest::ProfileSwitch {
                device: args.device,
                profile: args.profile,
            },
            ControlCommand::ListProfiles(args) => {
                // If no device specified, use empty string (caller should handle)
                crate::ipc::IpcRequest::ProfileList {
                    device: args.device.unwrap_or_default(),
                }
            }
            ControlCommand::Status(_) => crate::ipc::IpcRequest::Status,
        }
    }
}

impl From<crate::ipc::IpcResponse> for ControlResponse {
    fn from(response: crate::ipc::IpcResponse) -> Self {
        match response {
            crate::ipc::IpcResponse::Success { message } => {
                ControlResponse::Success { message }
            }
            crate::ipc::IpcResponse::ProfileList { profiles, active } => {
                // Single device response - wrap in DeviceProfiles
                // Note: This loses device name/path info; caller should provide
                ControlResponse::Profiles {
                    devices: vec![DeviceProfiles {
                        device: String::new(), // Caller should fill this
                        path: PathBuf::new(),
                        active,
                        profiles,
                    }],
                }
            }
            crate::ipc::IpcResponse::Status { devices } => {
                ControlResponse::Status { devices }
            }
            crate::ipc::IpcResponse::Error { message } => {
                ControlResponse::Error { message }
            }
        }
    }
}

// ============================================================================
// Socket Path Helper (duplicated from ipc.rs for standalone use)
// ============================================================================

/// Get the control socket path based on environment
///
/// Returns `$XDG_RUNTIME_DIR/niri-mapper.sock` if the environment variable is set,
/// otherwise falls back to `/tmp/niri-mapper-$UID.sock`.
///
/// This function is provided for CLI tools that need to connect to the daemon.
pub fn get_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("niri-mapper.sock")
    } else {
        let uid = unsafe { nix::libc::getuid() };
        PathBuf::from(format!("/tmp/niri-mapper-{}.sock", uid))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // Manual Test Procedure: Profile Switching (Task 040-4.10)
    // ========================================================================
    //
    // ## Prerequisites
    //
    // 1. Daemon running with a config that has multiple profiles:
    //
    //    ```kdl
    //    device {
    //        name "Your Keyboard Name"
    //        profile "default" {
    //            remap "CapsLock" "Escape"
    //        }
    //        profile "gaming" {
    //            remap "CapsLock" "LeftCtrl"
    //        }
    //    }
    //    ```
    //
    // 2. Start the daemon:
    //    `RUST_LOG=debug cargo run --bin niri-mapperd -- -c /path/to/config.kdl`
    //
    // 3. Verify IPC socket is created:
    //    `ls -la $XDG_RUNTIME_DIR/niri-mapper.sock`
    //
    // ## Test Cases
    //
    // ### Test 1: Verify Profile Switch (Valid Device, Valid Profile)
    //
    // Command:
    //   `niri-mapper switch-profile "Your Keyboard Name" gaming`
    //
    // Expected output:
    //   `Switched device 'Your Keyboard Name' to profile 'gaming'.`
    //
    // Verification:
    //   - CapsLock should now produce LeftCtrl instead of Escape
    //   - Daemon logs should show: "IPC: Profile switch request for device..."
    //
    // ### Test 2: Verify Invalid Device Name Fails Explicitly
    //
    // Command:
    //   `niri-mapper switch-profile "NonExistent Device" gaming`
    //
    // Expected output:
    //   `Error: Profile switch failed: Device 'NonExistent Device' not found. Available devices: ...`
    //
    // ### Test 3: Verify Invalid Profile Name Fails Explicitly
    //
    // Command:
    //   `niri-mapper switch-profile "Your Keyboard Name" nonexistent`
    //
    // Expected output:
    //   `Error: Profile switch failed: Profile 'nonexistent' not found for device...`
    //
    // ### Test 4: Raw Socket Test (Alternative to CLI)
    //
    // Using socat:
    //   ```bash
    //   echo '{"switch_profile":{"device":"Your Keyboard Name","profile":"gaming"}}' | \
    //     socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/niri-mapper.sock
    //   ```
    //
    // Expected response (success):
    //   `{"success":{"message":"Switched device 'Your Keyboard Name' to profile 'gaming'"}}`
    //
    // Expected response (error):
    //   `{"error":{"message":"Device 'X' not found..."}}`
    //
    // ### Test 5: Profile List Query
    //
    // Using the profile subcommand:
    //   `niri-mapper profile --list "Your Keyboard Name"`
    //
    // Expected output:
    //   ```
    //   Profiles for device 'Your Keyboard Name':
    //     * default [active]
    //       gaming
    //   ```
    //
    // ### Test 6: Status Query
    //
    // Raw socket test:
    //   ```bash
    //   echo '{"status":{}}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/niri-mapper.sock
    //   ```
    //
    // Expected response:
    //   JSON with list of devices, their paths, and active profiles
    //
    // ## Known Limitations (v0.4.0)
    //
    // 1. **Profile switching via IPC is partially implemented**: The daemon validates
    //    requests and updates tracking state, but full DeviceRemapper integration
    //    (swapping the actual remapping rules at runtime) is pending. Currently,
    //    only switching to "default" profile succeeds; other profiles return an
    //    error noting that DeviceRemapper integration is not yet complete.
    //
    // 2. **Keybind-based profile switching**: Profile switching via keybinds
    //    (profile-switch blocks in config) works correctly via the DeviceRemapper
    //    in remapper.rs.
    //
    // ========================================================================

    // ========================================================================
    // Integration Test: Profile Switch Command Format
    // ========================================================================

    /// Tests that the switch_profile command format matches what the CLI sends.
    ///
    /// This verifies the contract between CLI (cmd_switch_profile in main.rs)
    /// and the daemon (IPC handler in main.rs).
    #[test]
    fn test_switch_profile_command_format_matches_cli() {
        // This is the exact format that cmd_switch_profile() in CLI sends
        let cli_format = r#"{"switch_profile":{"device":"Keychron K3 Pro","profile":"gaming"}}"#;

        // Verify it deserializes to our ControlCommand type
        let parsed: ControlCommand = serde_json::from_str(cli_format).unwrap();

        match parsed {
            ControlCommand::SwitchProfile(args) => {
                assert_eq!(args.device, "Keychron K3 Pro");
                assert_eq!(args.profile, "gaming");
            }
            _ => panic!("Expected SwitchProfile command"),
        }
    }

    /// Tests that error responses have the expected format for CLI parsing.
    #[test]
    fn test_error_response_format_for_cli() {
        let error = ControlResponse::Error {
            message: "Device 'NonExistent' not found".to_string(),
        };
        let json = serde_json::to_string(&error).unwrap();

        // CLI expects this format for error handling
        assert!(json.contains(r#""error""#));
        assert!(json.contains(r#""message""#));
        assert!(json.contains("Device 'NonExistent' not found"));
    }

    /// Tests that success responses have the expected format for CLI parsing.
    #[test]
    fn test_success_response_format_for_cli() {
        let success = ControlResponse::Success {
            message: Some("Switched to profile 'gaming'".to_string()),
        };
        let json = serde_json::to_string(&success).unwrap();

        // CLI expects this format for success handling
        assert!(json.contains(r#""success""#));
        assert!(json.contains(r#""message""#));
    }

    // ========================================================================
    // Control Command Serialization Tests
    // ========================================================================

    #[test]
    fn test_switch_profile_serialization() {
        let cmd = ControlCommand::SwitchProfile(SwitchProfileArgs {
            device: "Keychron K3 Pro".to_string(),
            profile: "gaming".to_string(),
        });
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            json,
            r#"{"switch_profile":{"device":"Keychron K3 Pro","profile":"gaming"}}"#
        );

        // Round-trip
        let parsed: ControlCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn test_list_profiles_serialization() {
        // Without device filter
        let cmd = ControlCommand::ListProfiles(ListProfilesArgs { device: None });
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"list_profiles":{}}"#);

        // With device filter
        let cmd = ControlCommand::ListProfiles(ListProfilesArgs {
            device: Some("Keychron K3 Pro".to_string()),
        });
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            json,
            r#"{"list_profiles":{"device":"Keychron K3 Pro"}}"#
        );

        // Round-trip
        let parsed: ControlCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn test_status_serialization() {
        let cmd = ControlCommand::Status(StatusArgs {});
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"status":{}}"#);

        // Round-trip
        let parsed: ControlCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn test_response_success_serialization() {
        let response = ControlResponse::Success {
            message: Some("Profile switched".to_string()),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains(r#""success""#));
        assert!(json.contains(r#""message":"Profile switched""#));

        // Without message
        let response = ControlResponse::Success { message: None };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"success":{}}"#);
    }

    #[test]
    fn test_response_error_serialization() {
        let response = ControlResponse::Error {
            message: "Device not found".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"error":{"message":"Device not found"}}"#);

        // Round-trip
        let parsed: ControlResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn test_response_profiles_serialization() {
        let response = ControlResponse::Profiles {
            devices: vec![DeviceProfiles {
                device: "Keychron K3 Pro".to_string(),
                path: PathBuf::from("/dev/input/event5"),
                active: "default".to_string(),
                profiles: vec!["default".to_string(), "gaming".to_string()],
            }],
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains(r#""profiles""#));
        assert!(json.contains(r#""Keychron K3 Pro""#));
        assert!(json.contains(r#""active":"default""#));
    }

    #[test]
    fn test_response_status_serialization() {
        let response = ControlResponse::Status {
            devices: vec![DeviceStatus {
                name: "Keychron K3 Pro".to_string(),
                path: PathBuf::from("/dev/input/event5"),
                active_profile: "default".to_string(),
                available_profiles: vec!["default".to_string(), "gaming".to_string()],
            }],
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains(r#""status""#));
        assert!(json.contains(r#""name":"Keychron K3 Pro""#));
    }

    // ========================================================================
    // Command Format Verification Tests
    // ========================================================================

    #[test]
    fn test_spec_format_switch_profile() {
        // Verify the exact JSON format from the spec works
        let json = r#"{"switch_profile": {"device": "Keychron K3 Pro", "profile": "gaming"}}"#;
        let cmd: ControlCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCommand::SwitchProfile(_)));

        if let ControlCommand::SwitchProfile(args) = cmd {
            assert_eq!(args.device, "Keychron K3 Pro");
            assert_eq!(args.profile, "gaming");
        }
    }

    #[test]
    fn test_spec_format_list_profiles() {
        // Verify the exact JSON format from the spec works
        let json = r#"{"list_profiles": {}}"#;
        let cmd: ControlCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCommand::ListProfiles(_)));

        if let ControlCommand::ListProfiles(args) = cmd {
            assert!(args.device.is_none());
        }
    }

    #[test]
    fn test_spec_format_status() {
        // Verify the exact JSON format from the spec works
        let json = r#"{"status": {}}"#;
        let cmd: ControlCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCommand::Status(_)));
    }

    // ========================================================================
    // Conversion Tests
    // ========================================================================

    #[test]
    fn test_ipc_to_control_conversion() {
        use crate::ipc::IpcRequest;

        let ipc = IpcRequest::ProfileSwitch {
            device: "Test".to_string(),
            profile: "gaming".to_string(),
        };
        let control: ControlCommand = ipc.into();
        assert!(matches!(control, ControlCommand::SwitchProfile(_)));

        let ipc = IpcRequest::Status;
        let control: ControlCommand = ipc.into();
        assert!(matches!(control, ControlCommand::Status(_)));
    }

    #[test]
    fn test_control_to_ipc_conversion() {
        use crate::ipc::IpcRequest;

        let control = ControlCommand::SwitchProfile(SwitchProfileArgs {
            device: "Test".to_string(),
            profile: "gaming".to_string(),
        });
        let ipc: IpcRequest = control.into();
        assert!(matches!(ipc, IpcRequest::ProfileSwitch { .. }));

        let control = ControlCommand::Status(StatusArgs {});
        let ipc: IpcRequest = control.into();
        assert!(matches!(ipc, IpcRequest::Status));
    }

    // ========================================================================
    // Socket Path Tests
    // ========================================================================

    #[test]
    fn test_socket_path_with_xdg() {
        // Save current value
        let old_value = std::env::var("XDG_RUNTIME_DIR").ok();

        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        let path = get_socket_path();
        assert_eq!(path, PathBuf::from("/run/user/1000/niri-mapper.sock"));

        // Restore
        match old_value {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn test_socket_path_fallback() {
        // Save current value
        let old_value = std::env::var("XDG_RUNTIME_DIR").ok();

        std::env::remove_var("XDG_RUNTIME_DIR");
        let path = get_socket_path();
        let uid = unsafe { nix::libc::getuid() };
        assert_eq!(path, PathBuf::from(format!("/tmp/niri-mapper-{}.sock", uid)));

        // Restore
        if let Some(v) = old_value {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        }
    }
}
