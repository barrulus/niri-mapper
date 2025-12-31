//! Configuration data model

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
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            niri_keybinds_path: PathBuf::from("~/.config/niri/niri-mapper-keybinds.kdl"),
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
}

/// A named profile containing remapping rules
#[derive(Debug, Clone, Default)]
pub struct Profile {
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
