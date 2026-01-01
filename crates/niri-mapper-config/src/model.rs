//! Configuration data model
//!
//! # Per-Application Profile Foundation (v0.4.0)
//!
//! This module defines the configuration data structures for niri-mapper, including
//! support for per-application profile switching. v0.4.0 implements the **foundation**
//! for this feature:
//!
//! ## Current Capabilities (v0.4.0)
//!
//! - **`app_id_hint` field**: Profiles can optionally include an `app_id_hint` string
//!   that associates the profile with a specific application's `app_id` (e.g.,
//!   `"org.mozilla.firefox"`). This field is parsed and stored but **not used for
//!   automatic switching** in v0.4.0.
//!
//! - **Manual profile switching**: Users can switch profiles manually via:
//!   - CLI: `niri-mapper switch-profile <device> <profile>`
//!   - Control socket: `{"switch_profile": {"device": "...", "profile": "..."}}`
//!   - Keybinds: Configure `profile-switch` block in device config
//!
//! - **Active profile tracking**: The daemon tracks which profile is currently active
//!   for each device.
//!
//! ## Future Capabilities (Backlog)
//!
//! Automatic profile switching based on focused application is **explicitly out of
//! scope** for v0.4.0 and is planned for a future release. The intended behavior:
//!
//! 1. Listen to niri's focus change events (via IPC event stream)
//! 2. When focus changes to a window with `app_id` matching a profile's `app_id_hint`,
//!    automatically switch to that profile
//! 3. When focus changes to a window with no matching profile, switch to "default"
//!
//! The `app_id_hint` field is provided now so users can begin annotating their
//! profiles in preparation for this feature.
//!
//! ## Example Configuration
//!
//! ```kdl
//! device "My Keyboard" {
//!     profile "default" {
//!         remap {
//!             CapsLock "Escape"
//!         }
//!     }
//!     profile "firefox" {
//!         app-id-hint "org.mozilla.firefox"  // For future auto-switching
//!         remap {
//!             CapsLock "LeftCtrl"
//!         }
//!     }
//!     profile-switch {
//!         Ctrl+Shift+1 "default"
//!         Ctrl+Shift+2 "firefox"
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

/// Root configuration structure
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub global: GlobalConfig,
    pub devices: Vec<DeviceConfig>,
}

/// Global settings
#[derive(Debug, Clone)]
pub struct GlobalConfig {
    pub log_level: LogLevel,
    pub niri_keybinds_path: PathBuf,
    /// Whether to enable niri IPC integration (default: true)
    pub niri_ipc_enabled: bool,
    /// Number of retry attempts for niri IPC connections (default: 3)
    pub niri_ipc_retry_count: u32,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            niri_keybinds_path: PathBuf::from("~/.config/niri/niri-mapper-keybinds.kdl"),
            niri_ipc_enabled: true,
            niri_ipc_retry_count: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl std::str::FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warn" | "warning" => Ok(Self::Warn),
            "error" => Ok(Self::Error),
            _ => Err(format!("Unknown log level: {}", s)),
        }
    }
}

/// Device-specific configuration
#[derive(Debug, Clone)]
pub struct DeviceConfig {
    /// Device name to match (from evdev)
    pub name: Option<String>,
    /// Vendor:Product ID to match (e.g., "3434:0361")
    pub vendor_product: Option<String>,
    /// Profiles for this device
    pub profiles: HashMap<String, Profile>,
    /// Profile switch keybindings: maps key combo string (e.g., "Ctrl+Shift+1") to profile name
    pub profile_switch: HashMap<String, String>,
}

/// A named profile containing remapping rules.
///
/// Profiles group remapping rules that can be switched at runtime. Each device
/// can have multiple profiles, with one active at any time.
///
/// # Per-Application Profile Hints (v0.4.0 Foundation)
///
/// The `app_id_hint` field allows associating a profile with a specific
/// application's `app_id` (e.g., `"org.mozilla.firefox"`). This is a **foundation
/// feature** for future automatic profile switching:
///
/// - **v0.4.0**: The field is parsed and stored, but automatic switching is NOT
///   implemented. Users must switch profiles manually (via CLI, control socket,
///   or keybinds).
///
/// - **Future release**: When a window gains focus, niri-mapper will check if
///   any profile has a matching `app_id_hint` and automatically switch to it.
///
/// # Example
///
/// ```kdl
/// profile "firefox" {
///     app-id-hint "org.mozilla.firefox"  // Parsed but not used for auto-switching yet
///     remap {
///         CapsLock "LeftCtrl"
///     }
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// Optional app ID hint for future auto-switching based on focused application.
    ///
    /// When set, this profile is intended to be activated when a window with a
    /// matching `app_id` gains focus. This field is parsed in v0.4.0 but automatic
    /// switching based on it is **not yet implemented** (backlog).
    ///
    /// To use this profile now, switch to it manually via CLI, control socket,
    /// or configure a `profile-switch` keybind.
    pub app_id_hint: Option<String>,
    /// Simple 1:1 key remaps
    pub remap: HashMap<String, String>,
    /// Key combination remaps
    pub combo: HashMap<String, String>,
    /// Macro definitions (key -> sequence of actions)
    pub macros: HashMap<String, Vec<MacroAction>>,
    /// Keys to pass through to niri with their actions
    pub niri_passthrough: Vec<NiriKeybind>,
}

/// A single action in a macro sequence
#[derive(Debug, Clone)]
pub enum MacroAction {
    /// Press and release a key/combo
    Key(String),
    /// Delay in milliseconds
    Delay(u64),
}

/// A keybind to pass through to niri
#[derive(Debug, Clone)]
pub struct NiriKeybind {
    /// The key combination (e.g., "Super+Return")
    pub key: String,
    /// The niri action (e.g., "spawn \"alacritty\"")
    pub action: String,
}
