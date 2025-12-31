use miette::{Diagnostic, LabeledSpan, SourceCode};
use thiserror::Error;

// Note: SourceCode is used in the Diagnostic impl for source_code() method

/// Source location in a configuration file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLocation {
    /// Line number (1-indexed)
    pub line: usize,
    /// Column number (1-indexed)
    pub column: usize,
    /// Byte offset from start of file
    pub offset: usize,
    /// Length of the span in bytes
    pub len: usize,
}

impl SourceLocation {
    /// Create a new source location
    pub fn new(line: usize, column: usize, offset: usize, len: usize) -> Self {
        Self { line, column, offset, len }
    }

    /// Create an unknown source location (when span info is unavailable)
    pub fn unknown() -> Self {
        Self { line: 0, column: 0, offset: 0, len: 0 }
    }

    /// Convert to miette::SourceSpan
    pub fn to_source_span(&self) -> miette::SourceSpan {
        (self.offset, self.len).into()
    }
}

impl std::fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line > 0 {
            write!(f, "line {}, column {}", self.line, self.column)
        } else {
            write!(f, "unknown location")
        }
    }
}

/// Details about an invalid key in the configuration
#[derive(Debug, Clone)]
pub struct InvalidKeyInfo {
    /// The invalid key name
    pub key: String,
    /// Whether this was a "from" key or "to" key
    pub position: KeyPosition,
    /// Context about where the key was found (e.g., "remap", "combo")
    pub context: String,
    /// Source location of the invalid key
    pub location: SourceLocation,
}

/// Information about a duplicate keybind conflict
#[derive(Debug, Clone)]
pub struct DuplicateKeybindInfo {
    /// The key combination that is duplicated (e.g., "Super+Return")
    pub key: String,
    /// Device name where this keybind was found
    pub device: String,
    /// Profile name where this keybind was found
    pub profile: String,
}

/// Position of a key in a remap definition
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPosition {
    From,
    To,
}

impl std::fmt::Display for KeyPosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyPosition::From => write!(f, "from"),
            KeyPosition::To => write!(f, "to"),
        }
    }
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to parse KDL configuration")]
    ParseError {
        src: String,
        span: miette::SourceSpan,
        #[source]
        source: kdl::KdlError,
    },

    #[error("Invalid configuration: {message}")]
    Invalid { message: String },

    #[error("Missing required field: {field}")]
    MissingField { field: String },

    #[error("Unknown key: {key}")]
    UnknownKey { key: String },

    #[error("{}", format_invalid_keys_message(.invalid_keys))]
    InvalidKeys {
        /// Source code for displaying context
        src: Option<String>,
        /// All invalid keys found during parsing
        invalid_keys: Vec<InvalidKeyInfo>,
    },

    #[error("{}", format_duplicate_keybinds_message(.duplicates))]
    DuplicateKeybinds {
        /// All duplicate keybind conflicts found
        duplicates: Vec<DuplicateKeybindInfo>,
    },

    #[error("Failed to read configuration file")]
    Io(#[from] std::io::Error),
}

impl Diagnostic for ConfigError {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        let code = match self {
            ConfigError::ParseError { .. } => "niri_mapper::config::parse_error",
            ConfigError::Invalid { .. } => "niri_mapper::config::invalid",
            ConfigError::MissingField { .. } => "niri_mapper::config::missing_field",
            ConfigError::UnknownKey { .. } => "niri_mapper::config::unknown_key",
            ConfigError::InvalidKeys { .. } => "niri_mapper::config::invalid_keys",
            ConfigError::DuplicateKeybinds { .. } => "niri_mapper::config::duplicate_keybinds",
            ConfigError::Io(_) => "niri_mapper::config::io_error",
        };
        Some(Box::new(code))
    }

    fn help<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        let help: Option<String> = match self {
            ConfigError::ParseError { .. } => {
                Some("Check the KDL syntax. Common issues: missing quotes around strings, unclosed braces, or invalid node names.".to_string())
            }
            ConfigError::Invalid { .. } => {
                Some("Review the configuration structure and ensure all values are valid.".to_string())
            }
            ConfigError::MissingField { field } => {
                if field.contains("device name") {
                    Some("Add a name argument to the device block, e.g.: device \"My Keyboard\" { ... }".to_string())
                } else {
                    Some(format!("Add the required '{}' field to your configuration.", field))
                }
            }
            ConfigError::UnknownKey { .. } => {
                Some("Use 'niri-mapper devices' to list available device names, or check the documentation for valid key names.".to_string())
            }
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                let mut help = String::from("Valid key names include:\n");
                help.push_str("  - Letters: A-Z\n");
                help.push_str("  - Modifiers: LeftCtrl, RightShift, LeftAlt, LeftMeta, etc.\n");
                help.push_str("  - Special: Escape, Enter, Tab, Space, Backspace, CapsLock\n");
                help.push_str("  - Function: F1-F24\n");
                help.push_str("  - Navigation: Up, Down, Left, Right, Home, End, PageUp, PageDown\n");
                help.push_str("  - Raw evdev: KEY_* format (e.g., KEY_A, KEY_LEFTMETA)\n");

                // Add specific suggestion if there's only one invalid key
                if invalid_keys.len() == 1 {
                    let key = &invalid_keys[0];
                    help.push_str(&format!("\nCheck the spelling of '{}' in your {} block.", key.key, key.context));
                }
                Some(help)
            }
            ConfigError::DuplicateKeybinds { .. } => {
                Some("Each keybind can only be defined once across all devices and profiles. Remove or rename the duplicate keybinds.".to_string())
            }
            ConfigError::Io(e) => {
                match e.kind() {
                    std::io::ErrorKind::NotFound => {
                        Some("Verify the configuration file path exists. Default location: ~/.config/niri-mapper/config.kdl".to_string())
                    }
                    std::io::ErrorKind::PermissionDenied => {
                        Some("Check file permissions. The configuration file must be readable.".to_string())
                    }
                    _ => None,
                }
            }
        };
        help.map(|s| Box::new(s) as Box<dyn std::fmt::Display>)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        match self {
            ConfigError::ParseError { src, .. } => Some(src as &dyn SourceCode),
            ConfigError::InvalidKeys { src: Some(src), .. } => Some(src as &dyn SourceCode),
            _ => None,
        }
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        match self {
            ConfigError::ParseError { span, .. } => {
                Some(Box::new(std::iter::once(LabeledSpan::new_with_span(
                    Some("syntax error here".to_string()),
                    *span,
                ))))
            }
            ConfigError::InvalidKeys { invalid_keys, .. } => {
                let labels = invalid_keys.iter().map(|key_info| {
                    let label = format!(
                        "unknown {} key '{}'",
                        key_info.position, key_info.key
                    );
                    LabeledSpan::new_with_span(Some(label), key_info.location.to_source_span())
                });
                Some(Box::new(labels))
            }
            _ => None,
        }
    }

    fn severity(&self) -> Option<miette::Severity> {
        Some(miette::Severity::Error)
    }

    fn url<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        // Could add documentation URLs here in the future
        None
    }
}

/// Format the list of invalid keys for the error message
fn format_invalid_keys_message(keys: &[InvalidKeyInfo]) -> String {
    if keys.is_empty() {
        return "No invalid keys".to_string();
    }

    if keys.len() == 1 {
        let key = &keys[0];
        return format!(
            "Invalid key '{}' in {} block at {}",
            key.key, key.context, key.location
        );
    }

    format!("Found {} invalid key(s) in configuration", keys.len())
}

/// Format the list of duplicate keybinds for the error message
fn format_duplicate_keybinds_message(duplicates: &[DuplicateKeybindInfo]) -> String {
    use std::collections::HashMap;

    if duplicates.is_empty() {
        return "No duplicate keybinds".to_string();
    }

    // Group duplicates by key
    let mut by_key: HashMap<&str, Vec<&DuplicateKeybindInfo>> = HashMap::new();
    for dup in duplicates {
        by_key.entry(&dup.key).or_default().push(dup);
    }

    // Build message listing each duplicate key and its sources
    let mut lines = vec!["Duplicate keybinds detected:".to_string()];

    for (key, sources) in &by_key {
        let source_list: Vec<String> = sources
            .iter()
            .map(|s| format!("{}:{}", s.device, s.profile))
            .collect();
        lines.push(format!("  '{}' defined in: {}", key, source_list.join(", ")));
    }

    lines.join("\n")
}
