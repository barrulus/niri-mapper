//! Key remapping logic
//!
//! # Combo State Machine Design (Task 020-2.1)
//!
//! The combo remapping system uses a state machine to track modifier keys and detect
//! key combinations (chords) like `Ctrl+Shift+Q`.
//!
//! ## States
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────────┐
//! │                           COMBO STATE MACHINE                               │
//! └─────────────────────────────────────────────────────────────────────────────┘
//!
//!  ┌───────┐
//!  │ IDLE  │ ◄───────────────────────────────────────────────────────┐
//!  └───┬───┘                                                         │
//!      │                                                             │
//!      │ Modifier press                                              │
//!      │ (Ctrl/Shift/Alt/Super)                                      │
//!      ▼                                                             │
//!  ┌─────────────────┐                                               │
//!  │ MODIFIERS_HELD  │ ◄────────────┐                                │
//!  │                 │              │                                │
//!  │ held_modifiers: │              │ Additional modifier            │
//!  │ HashSet<Key>    │──────────────┘ press/release                  │
//!  └────────┬────────┘                                               │
//!           │                                                        │
//!           │ Trigger key press (matches combo)                      │
//!           ▼                                                        │
//!  ┌───────────────────┐                                             │
//!  │  COMBO_MATCHED    │                                             │
//!  │                   │                                             │
//!  │ active_combo:     │                                             │
//!  │ Some(ComboAction) │                                             │
//!  │                   │                                             │
//!  │ - Inject output   │                                             │
//!  │   key events      │                                             │
//!  └────────┬──────────┘                                             │
//!           │                                                        │
//!           │ Trigger key release                                    │
//!           │ - Inject release events                                │
//!           │ - Clear active_combo                                   │
//!           └────────────────────────────────────────────────────────┘
//! ```
//!
//! ## State Transitions
//!
//! 1. **Idle -> ModifiersHeld**: When any modifier key (Ctrl, Shift, Alt, Super) is pressed
//! 2. **ModifiersHeld -> ModifiersHeld**: When modifiers are added/removed (still tracking)
//! 3. **ModifiersHeld -> ComboMatched**: When a non-modifier key is pressed that, combined
//!    with the currently held modifiers, matches a registered combo
//! 4. **ModifiersHeld -> Idle**: When all modifiers are released without triggering a combo
//! 5. **ComboMatched -> ModifiersHeld**: When the trigger key is released (combo completes)
//! 6. **ComboMatched -> Idle**: When both trigger key and all modifiers are released
//!
//! ## Key Tracking
//!
//! The state machine tracks:
//! - `held_modifiers: HashSet<Modifier>` - Currently pressed modifier keys (normalized)
//! - `active_combo: Option<ActiveCombo>` - The currently active (matched) combo, if any
//!
//! ## Modifier Normalization
//!
//! Left and right variants of modifiers are normalized to a single `Modifier` enum:
//! - `KEY_LEFTCTRL` / `KEY_RIGHTCTRL` -> `Modifier::Ctrl`
//! - `KEY_LEFTSHIFT` / `KEY_RIGHTSHIFT` -> `Modifier::Shift`
//! - `KEY_LEFTALT` / `KEY_RIGHTALT` -> `Modifier::Alt`
//! - `KEY_LEFTMETA` / `KEY_RIGHTMETA` -> `Modifier::Super`
//!
//! This allows `Ctrl+Q` to match whether the user presses left or right Ctrl.
//!
//! ## Combo Matching
//!
//! A combo matches when:
//! 1. The set of held modifiers EXACTLY matches the combo's required modifiers
//! 2. The pressed key matches the combo's trigger key
//!
//! Example: For combo `Ctrl+Shift+Q`:
//! - Matches: Ctrl held, Shift held, Q pressed
//! - No match: Ctrl held, Q pressed (missing Shift)
//! - No match: Ctrl held, Shift held, Alt held, Q pressed (extra modifier)

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use evdev::{InputEvent, Key};
use niri_mapper_config::Profile;

// ============================================================================
// Combo Types (Task 020-2.1, 020-2.3)
// ============================================================================

/// Normalized modifier key representation.
///
/// Left and right variants are combined into a single modifier type to allow
/// flexible combo matching (e.g., either left or right Ctrl satisfies "Ctrl").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Modifier {
    /// Control key (left or right)
    Ctrl,
    /// Shift key (left or right)
    Shift,
    /// Alt key (left or right)
    Alt,
    /// Super/Meta/Windows key (left or right)
    Super,
}

impl Modifier {
    /// Check if an evdev key is a modifier and return its normalized form.
    pub fn from_key(key: Key) -> Option<Self> {
        match key {
            Key::KEY_LEFTCTRL | Key::KEY_RIGHTCTRL => Some(Modifier::Ctrl),
            Key::KEY_LEFTSHIFT | Key::KEY_RIGHTSHIFT => Some(Modifier::Shift),
            Key::KEY_LEFTALT | Key::KEY_RIGHTALT => Some(Modifier::Alt),
            Key::KEY_LEFTMETA | Key::KEY_RIGHTMETA => Some(Modifier::Super),
            _ => None,
        }
    }

    /// Parse a modifier name string (case-insensitive).
    ///
    /// Recognized names:
    /// - Ctrl: "ctrl", "control"
    /// - Shift: "shift"
    /// - Alt: "alt"
    /// - Super: "super", "meta", "mod", "win", "windows"
    pub fn from_str_name(name: &str) -> Option<Self> {
        match name.to_uppercase().as_str() {
            "CTRL" | "CONTROL" => Some(Modifier::Ctrl),
            "SHIFT" => Some(Modifier::Shift),
            "ALT" => Some(Modifier::Alt),
            "SUPER" | "META" | "MOD" | "WIN" | "WINDOWS" => Some(Modifier::Super),
            _ => None,
        }
    }

    /// Get the default evdev key for this modifier (left variant).
    pub fn to_key(self) -> Key {
        match self {
            Modifier::Ctrl => Key::KEY_LEFTCTRL,
            Modifier::Shift => Key::KEY_LEFTSHIFT,
            Modifier::Alt => Key::KEY_LEFTALT,
            Modifier::Super => Key::KEY_LEFTMETA,
        }
    }
}

impl fmt::Display for Modifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Modifier::Ctrl => write!(f, "Ctrl"),
            Modifier::Shift => write!(f, "Shift"),
            Modifier::Alt => write!(f, "Alt"),
            Modifier::Super => write!(f, "Super"),
        }
    }
}

/// A parsed key combination (chord).
///
/// Represents a combination like `Ctrl+Shift+Q` as:
/// - `modifiers`: Set of modifier keys that must be held
/// - `key`: The trigger key that activates the combo
///
/// # Example
///
/// ```ignore
/// let combo = KeyCombo::parse("Ctrl+Shift+Q")?;
/// assert_eq!(combo.modifiers, HashSet::from([Modifier::Ctrl, Modifier::Shift]));
/// assert_eq!(combo.key, Key::KEY_Q);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCombo {
    /// Set of modifiers that must be held for this combo
    pub modifiers: HashSet<Modifier>,
    /// The trigger key (non-modifier) that activates the combo
    pub key: Key,
}

impl std::hash::Hash for KeyCombo {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Hash modifiers in sorted order for consistent hashing
        let mut mods: Vec<_> = self.modifiers.iter().collect();
        mods.sort();
        for m in mods {
            m.hash(state);
        }
        self.key.hash(state);
    }
}

impl KeyCombo {
    /// Create a new KeyCombo with no modifiers.
    pub fn new(key: Key) -> Self {
        Self {
            modifiers: HashSet::new(),
            key,
        }
    }

    /// Create a new KeyCombo with the specified modifiers.
    pub fn with_modifiers(modifiers: HashSet<Modifier>, key: Key) -> Self {
        Self { modifiers, key }
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Sort modifiers for consistent output order: Ctrl, Shift, Alt, Super
        let mut mods: Vec<_> = self.modifiers.iter().collect();
        mods.sort();

        for modifier in mods {
            write!(f, "{}+", modifier)?;
        }

        // Format the key name (strip KEY_ prefix if present)
        let key_name = format!("{:?}", self.key);
        let display_name = key_name.strip_prefix("KEY_").unwrap_or(&key_name);
        write!(f, "{}", display_name)
    }
}

/// Error type for combo parsing failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComboParseError {
    /// The original input string that failed to parse
    pub input: String,
    /// Description of what went wrong
    pub reason: String,
}

impl fmt::Display for ComboParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to parse combo '{}': {}", self.input, self.reason)
    }
}

impl std::error::Error for ComboParseError {}

/// Parse a combo key string into a structured representation.
///
/// Parses strings like `"Ctrl+Shift+Q"` into a [`KeyCombo`] with:
/// - `modifiers`: `[Ctrl, Shift]`
/// - `key`: `KEY_Q`
///
/// # Format
///
/// The expected format is `[Modifier+]...[Modifier+]Key` where:
/// - Modifiers are `Ctrl`, `Shift`, `Alt`, `Super` (case-insensitive)
/// - Key is any valid key name (see [`parse_key`])
/// - Components are separated by `+`
///
/// # Modifier Order Independence
///
/// Modifier order does not matter: `"Ctrl+Shift+Q"` and `"Shift+Ctrl+Q"` are equivalent.
///
/// # Errors
///
/// Returns [`ComboParseError`] if:
/// - The input is empty
/// - No trigger key is found (only modifiers)
/// - The trigger key name is unrecognized
/// - Duplicate modifiers are specified
///
/// # Examples
///
/// ```ignore
/// // Simple combo with modifiers
/// let combo = parse_combo("Ctrl+Shift+Q")?;
/// assert!(combo.modifiers.contains(&Modifier::Ctrl));
/// assert!(combo.modifiers.contains(&Modifier::Shift));
/// assert_eq!(combo.key, Key::KEY_Q);
///
/// // Single key (no modifiers) - still valid
/// let combo = parse_combo("Escape")?;
/// assert!(combo.modifiers.is_empty());
/// assert_eq!(combo.key, Key::KEY_ESC);
///
/// // Modifier order doesn't matter
/// let combo1 = parse_combo("Ctrl+Alt+Delete")?;
/// let combo2 = parse_combo("Alt+Ctrl+Delete")?;
/// assert_eq!(combo1.modifiers, combo2.modifiers);
/// ```
pub fn parse_combo(input: &str) -> Result<KeyCombo, ComboParseError> {
    let input = input.trim();

    if input.is_empty() {
        return Err(ComboParseError {
            input: input.to_string(),
            reason: "empty input".to_string(),
        });
    }

    let parts: Vec<&str> = input.split('+').map(|s| s.trim()).collect();

    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(ComboParseError {
            input: input.to_string(),
            reason: "invalid format: empty component in combo string".to_string(),
        });
    }

    let mut modifiers = HashSet::new();
    let mut trigger_key: Option<Key> = None;

    for part in &parts {
        // Try to parse as a modifier first
        if let Some(modifier) = Modifier::from_str_name(part) {
            if !modifiers.insert(modifier) {
                return Err(ComboParseError {
                    input: input.to_string(),
                    reason: format!("duplicate modifier: {}", modifier),
                });
            }
        } else {
            // Not a modifier, must be the trigger key
            if trigger_key.is_some() {
                return Err(ComboParseError {
                    input: input.to_string(),
                    reason: format!(
                        "multiple non-modifier keys found: expected exactly one trigger key, got '{}' after already finding one",
                        part
                    ),
                });
            }

            match parse_key(part) {
                Some(key) => trigger_key = Some(key),
                None => {
                    return Err(ComboParseError {
                        input: input.to_string(),
                        reason: format!("unknown key: '{}'", part),
                    });
                }
            }
        }
    }

    match trigger_key {
        Some(key) => Ok(KeyCombo::with_modifiers(modifiers, key)),
        None => Err(ComboParseError {
            input: input.to_string(),
            reason: "no trigger key found (only modifiers specified)".to_string(),
        }),
    }
}

// ============================================================================
// Combo State Machine Types (Task 020-2.1)
// ============================================================================

/// The current state of the combo detection state machine.
///
/// See module-level documentation for the full state machine diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComboState {
    /// No modifiers are held, waiting for input
    Idle,
    /// One or more modifiers are held, waiting for trigger key or more modifiers
    ModifiersHeld,
    /// A combo has been matched and is currently active
    ComboMatched {
        /// The input combo that was matched
        input_combo: KeyCombo,
        /// The output combo to inject
        output_combo: KeyCombo,
    },
}

impl Default for ComboState {
    fn default() -> Self {
        ComboState::Idle
    }
}

/// Tracks the state of an active combo remapping session.
///
/// This struct is used by the combo state machine to track:
/// - Which modifiers are currently held
/// - The current state (Idle, ModifiersHeld, ComboMatched)
/// - The registered combos to match against
///
/// # Future Implementation
///
/// This struct will be integrated with `Remapper` in task 020-2.2 (modifier tracking)
/// and 020-2.7 (integration).
#[derive(Debug, Clone)]
pub struct ComboTracker {
    /// Currently held modifiers (normalized)
    pub held_modifiers: HashSet<Modifier>,
    /// Current state of the combo state machine
    pub state: ComboState,
    /// Registered combo mappings: input combo -> output combo
    pub combos: HashMap<KeyCombo, KeyCombo>,
}

impl Default for ComboTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ComboTracker {
    /// Create a new combo tracker with no registered combos.
    pub fn new() -> Self {
        Self {
            held_modifiers: HashSet::new(),
            state: ComboState::Idle,
            combos: HashMap::new(),
        }
    }

    /// Register a combo mapping.
    ///
    /// When `input` combo is detected, `output` events will be injected instead.
    pub fn register_combo(&mut self, input: KeyCombo, output: KeyCombo) {
        self.combos.insert(input, output);
    }
}

// ============================================================================
// Remapper
// ============================================================================

/// Remapper handles translating input events according to a profile
pub struct Remapper {
    /// Simple key remaps (from -> to)
    remap: HashMap<Key, Key>,
    /// Keys that should be passed through unmodified
    passthrough: Vec<Key>,
    /// Currently held modifier keys (normalized to Modifier enum)
    ///
    /// This tracks the state of Ctrl, Shift, Alt, and Super keys.
    /// Left and right variants are normalized (e.g., both KEY_LEFTCTRL and
    /// KEY_RIGHTCTRL map to Modifier::Ctrl).
    ///
    /// Updated on each key event:
    /// - Press (value=1): modifier is added to the set
    /// - Release (value=0): modifier is removed from the set
    /// - Repeat (value=2): no change (set already contains the modifier)
    held_modifiers: HashSet<Modifier>,
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
            held_modifiers: HashSet::new(),
        }
    }

    /// Get the currently held modifiers.
    ///
    /// Returns a reference to the set of modifier keys that are currently pressed.
    /// This is useful for combo detection and debugging.
    pub fn held_modifiers(&self) -> &HashSet<Modifier> {
        &self.held_modifiers
    }

    /// Update the held modifiers state based on a key event.
    ///
    /// This method should be called for every key event to maintain accurate
    /// modifier tracking state. It handles:
    /// - Press events (value=1): adds the modifier to the held set
    /// - Release events (value=0): removes the modifier from the held set
    /// - Repeat events (value=2): no change (idempotent, already held)
    ///
    /// Non-modifier keys are ignored.
    ///
    /// # Arguments
    ///
    /// * `key` - The key that generated the event
    /// * `value` - The event value (0=release, 1=press, 2=repeat)
    fn update_held_modifiers(&mut self, key: Key, value: i32) {
        // Check if this key is a modifier
        if let Some(modifier) = Modifier::from_key(key) {
            match value {
                0 => {
                    // Release: remove modifier from held set
                    self.held_modifiers.remove(&modifier);
                }
                1 => {
                    // Press: add modifier to held set
                    // HashSet::insert is idempotent, so this handles edge cases
                    self.held_modifiers.insert(modifier);
                }
                2 => {
                    // Repeat: modifier is already held, no action needed
                    // The modifier should already be in the set from the initial press
                }
                _ => {
                    // Unknown event value, ignore
                }
            }
        }
    }

    /// Process an input event, returning the remapped event(s)
    ///
    /// Key event values are preserved through remapping:
    /// - `0` = key release
    /// - `1` = key press
    /// - `2` = key repeat (autorepeat)
    ///
    /// This method also updates the internal modifier tracking state.
    pub fn process(&mut self, event: InputEvent) -> Vec<InputEvent> {
        // Only process key events
        if event.event_type() != evdev::EventType::KEY {
            return vec![event];
        }

        let key = Key::new(event.code());

        // Update modifier tracking state for every key event
        self.update_held_modifiers(key, event.value());

        if let Some(&remapped_key) = self.remap.get(&key) {
            // Remap the key while preserving the event value (press/release/repeat)
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
    let upper = name.to_uppercase();

    // Common key mappings
    match upper.as_str() {
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

        // Number keys
        "0" => Some(Key::KEY_0),
        "1" => Some(Key::KEY_1),
        "2" => Some(Key::KEY_2),
        "3" => Some(Key::KEY_3),
        "4" => Some(Key::KEY_4),
        "5" => Some(Key::KEY_5),
        "6" => Some(Key::KEY_6),
        "7" => Some(Key::KEY_7),
        "8" => Some(Key::KEY_8),
        "9" => Some(Key::KEY_9),

        // Symbol keys
        "MINUS" | "-" => Some(Key::KEY_MINUS),
        "EQUALS" | "EQUAL" | "=" => Some(Key::KEY_EQUAL),
        "LEFTBRACE" | "LBRACE" | "[" => Some(Key::KEY_LEFTBRACE),
        "RIGHTBRACE" | "RBRACE" | "]" => Some(Key::KEY_RIGHTBRACE),
        "SEMICOLON" | ";" => Some(Key::KEY_SEMICOLON),
        "APOSTROPHE" | "'" => Some(Key::KEY_APOSTROPHE),
        "GRAVE" | "`" => Some(Key::KEY_GRAVE),
        "BACKSLASH" | "\\" => Some(Key::KEY_BACKSLASH),
        "COMMA" | "," => Some(Key::KEY_COMMA),
        "DOT" | "PERIOD" | "." => Some(Key::KEY_DOT),
        "SLASH" | "/" => Some(Key::KEY_SLASH),

        // Arrow keys
        "UP" | "UPARROW" => Some(Key::KEY_UP),
        "DOWN" | "DOWNARROW" => Some(Key::KEY_DOWN),
        "LEFT" | "LEFTARROW" => Some(Key::KEY_LEFT),
        "RIGHT" | "RIGHTARROW" => Some(Key::KEY_RIGHT),

        // Navigation keys
        "HOME" => Some(Key::KEY_HOME),
        "END" => Some(Key::KEY_END),
        "PAGEUP" | "PGUP" => Some(Key::KEY_PAGEUP),
        "PAGEDOWN" | "PGDN" | "PGDOWN" => Some(Key::KEY_PAGEDOWN),
        "INSERT" | "INS" => Some(Key::KEY_INSERT),
        "DELETE" | "DEL" => Some(Key::KEY_DELETE),

        // Function keys F1-F12
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

        // Function keys F13-F24
        "F13" => Some(Key::KEY_F13),
        "F14" => Some(Key::KEY_F14),
        "F15" => Some(Key::KEY_F15),
        "F16" => Some(Key::KEY_F16),
        "F17" => Some(Key::KEY_F17),
        "F18" => Some(Key::KEY_F18),
        "F19" => Some(Key::KEY_F19),
        "F20" => Some(Key::KEY_F20),
        "F21" => Some(Key::KEY_F21),
        "F22" => Some(Key::KEY_F22),
        "F23" => Some(Key::KEY_F23),
        "F24" => Some(Key::KEY_F24),

        // Numpad keys
        "KP0" | "NUMPAD0" => Some(Key::KEY_KP0),
        "KP1" | "NUMPAD1" => Some(Key::KEY_KP1),
        "KP2" | "NUMPAD2" => Some(Key::KEY_KP2),
        "KP3" | "NUMPAD3" => Some(Key::KEY_KP3),
        "KP4" | "NUMPAD4" => Some(Key::KEY_KP4),
        "KP5" | "NUMPAD5" => Some(Key::KEY_KP5),
        "KP6" | "NUMPAD6" => Some(Key::KEY_KP6),
        "KP7" | "NUMPAD7" => Some(Key::KEY_KP7),
        "KP8" | "NUMPAD8" => Some(Key::KEY_KP8),
        "KP9" | "NUMPAD9" => Some(Key::KEY_KP9),
        "KPDOT" | "KPDECIMAL" | "NUMPAD_DOT" => Some(Key::KEY_KPDOT),
        "KPENTER" | "NUMPAD_ENTER" => Some(Key::KEY_KPENTER),
        "KPPLUS" | "KPADD" | "NUMPAD_PLUS" => Some(Key::KEY_KPPLUS),
        "KPMINUS" | "KPSUBTRACT" | "NUMPAD_MINUS" => Some(Key::KEY_KPMINUS),
        "KPASTERISK" | "KPMULTIPLY" | "NUMPAD_MULTIPLY" => Some(Key::KEY_KPASTERISK),
        "KPSLASH" | "KPDIVIDE" | "NUMPAD_DIVIDE" => Some(Key::KEY_KPSLASH),
        "NUMLOCK" | "NUM_LOCK" => Some(Key::KEY_NUMLOCK),

        // Media keys
        "XF86BACK" => Some(Key::KEY_BACK),
        "XF86FORWARD" => Some(Key::KEY_FORWARD),

        _ => {
            // Fallback: Try to parse KEY_* format strings directly using evdev's FromStr
            // This allows users to use raw kernel key names as an escape hatch
            if upper.starts_with("KEY_") {
                match Key::from_str(&upper) {
                    Ok(key) => return Some(key),
                    Err(_) => {
                        tracing::warn!("Unknown evdev key: {}", name);
                        return None;
                    }
                }
            }
            tracing::warn!("Unknown key: {}", name);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_basic() {
        assert_eq!(parse_key("CapsLock"), Some(Key::KEY_CAPSLOCK));
        assert_eq!(parse_key("Escape"), Some(Key::KEY_ESC));
        assert_eq!(parse_key("A"), Some(Key::KEY_A));
    }

    #[test]
    fn test_parse_key_numbers() {
        assert_eq!(parse_key("0"), Some(Key::KEY_0));
        assert_eq!(parse_key("5"), Some(Key::KEY_5));
        assert_eq!(parse_key("9"), Some(Key::KEY_9));
    }

    #[test]
    fn test_parse_key_symbols() {
        assert_eq!(parse_key("Minus"), Some(Key::KEY_MINUS));
        assert_eq!(parse_key("-"), Some(Key::KEY_MINUS));
        assert_eq!(parse_key("Equals"), Some(Key::KEY_EQUAL));
        assert_eq!(parse_key("LeftBrace"), Some(Key::KEY_LEFTBRACE));
        assert_eq!(parse_key("["), Some(Key::KEY_LEFTBRACE));
        assert_eq!(parse_key("Semicolon"), Some(Key::KEY_SEMICOLON));
        assert_eq!(parse_key("Grave"), Some(Key::KEY_GRAVE));
        assert_eq!(parse_key("Backslash"), Some(Key::KEY_BACKSLASH));
        assert_eq!(parse_key("Comma"), Some(Key::KEY_COMMA));
        assert_eq!(parse_key("Dot"), Some(Key::KEY_DOT));
        assert_eq!(parse_key("Slash"), Some(Key::KEY_SLASH));
    }

    #[test]
    fn test_parse_key_arrows() {
        assert_eq!(parse_key("Up"), Some(Key::KEY_UP));
        assert_eq!(parse_key("Down"), Some(Key::KEY_DOWN));
        assert_eq!(parse_key("Left"), Some(Key::KEY_LEFT));
        assert_eq!(parse_key("Right"), Some(Key::KEY_RIGHT));
    }

    #[test]
    fn test_parse_key_navigation() {
        assert_eq!(parse_key("Home"), Some(Key::KEY_HOME));
        assert_eq!(parse_key("End"), Some(Key::KEY_END));
        assert_eq!(parse_key("PageUp"), Some(Key::KEY_PAGEUP));
        assert_eq!(parse_key("PgDn"), Some(Key::KEY_PAGEDOWN));
        assert_eq!(parse_key("Insert"), Some(Key::KEY_INSERT));
        assert_eq!(parse_key("Delete"), Some(Key::KEY_DELETE));
    }

    #[test]
    fn test_parse_key_function_keys() {
        assert_eq!(parse_key("F1"), Some(Key::KEY_F1));
        assert_eq!(parse_key("F12"), Some(Key::KEY_F12));
        assert_eq!(parse_key("F13"), Some(Key::KEY_F13));
        assert_eq!(parse_key("F24"), Some(Key::KEY_F24));
    }

    #[test]
    fn test_parse_key_numpad() {
        assert_eq!(parse_key("KP0"), Some(Key::KEY_KP0));
        assert_eq!(parse_key("Numpad5"), Some(Key::KEY_KP5));
        assert_eq!(parse_key("KPEnter"), Some(Key::KEY_KPENTER));
        assert_eq!(parse_key("KPPlus"), Some(Key::KEY_KPPLUS));
        assert_eq!(parse_key("KPMinus"), Some(Key::KEY_KPMINUS));
        assert_eq!(parse_key("KPAsterisk"), Some(Key::KEY_KPASTERISK));
        assert_eq!(parse_key("KPSlash"), Some(Key::KEY_KPSLASH));
        assert_eq!(parse_key("KPDot"), Some(Key::KEY_KPDOT));
    }

    #[test]
    fn test_parse_key_modifiers() {
        assert_eq!(parse_key("LeftShift"), Some(Key::KEY_LEFTSHIFT));
        assert_eq!(parse_key("RightShift"), Some(Key::KEY_RIGHTSHIFT));
        assert_eq!(parse_key("LeftCtrl"), Some(Key::KEY_LEFTCTRL));
        assert_eq!(parse_key("RightCtrl"), Some(Key::KEY_RIGHTCTRL));
        assert_eq!(parse_key("LeftAlt"), Some(Key::KEY_LEFTALT));
        assert_eq!(parse_key("RightAlt"), Some(Key::KEY_RIGHTALT));
        assert_eq!(parse_key("LeftMeta"), Some(Key::KEY_LEFTMETA));
        assert_eq!(parse_key("RightMeta"), Some(Key::KEY_RIGHTMETA));
    }

    #[test]
    fn test_parse_key_case_insensitive() {
        assert_eq!(parse_key("capslock"), Some(Key::KEY_CAPSLOCK));
        assert_eq!(parse_key("CAPSLOCK"), Some(Key::KEY_CAPSLOCK));
        assert_eq!(parse_key("CapsLock"), Some(Key::KEY_CAPSLOCK));
        assert_eq!(parse_key("leftshift"), Some(Key::KEY_LEFTSHIFT));
        assert_eq!(parse_key("LEFTSHIFT"), Some(Key::KEY_LEFTSHIFT));
    }

    #[test]
    fn test_parse_key_unknown() {
        assert_eq!(parse_key("UnknownKey"), None);
        assert_eq!(parse_key("InvalidKey123"), None);
    }

    #[test]
    fn test_parse_key_raw_evdev_format() {
        // Test that KEY_* format strings are parsed correctly via evdev's FromStr
        assert_eq!(parse_key("KEY_LEFTMETA"), Some(Key::KEY_LEFTMETA));
        assert_eq!(parse_key("KEY_A"), Some(Key::KEY_A));
        assert_eq!(parse_key("KEY_CAPSLOCK"), Some(Key::KEY_CAPSLOCK));
        assert_eq!(parse_key("KEY_ESC"), Some(Key::KEY_ESC));
        // Test case insensitivity for KEY_* format
        assert_eq!(parse_key("key_leftmeta"), Some(Key::KEY_LEFTMETA));
        assert_eq!(parse_key("Key_A"), Some(Key::KEY_A));
    }

    #[test]
    fn test_parse_key_unknown_raw_evdev() {
        // Unknown KEY_* names should still return None
        assert_eq!(parse_key("KEY_INVALIDKEY"), None);
        assert_eq!(parse_key("KEY_NOTAKEY123"), None);
    }

    #[test]
    fn test_remap_press_event() {
        // Task 010-8.2.1: Test 1:1 remap for press event
        // Verify that when a key press event (value=1) for a remapped key is processed,
        // the output event has the correct remapped key code with value=1.
        let mut remap = HashMap::new();
        remap.insert(Key::KEY_CAPSLOCK, Key::KEY_ESC);
        let mut remapper = Remapper {
            remap,
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_PRESS: i32 = 1;
        let press_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_CAPSLOCK.code(), KEY_PRESS);
        let result = remapper.process(press_event);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code(), Key::KEY_ESC.code(), "Key should be remapped from CapsLock to Escape");
        assert_eq!(result[0].value(), KEY_PRESS, "Press event value (1) should be preserved");
        assert_eq!(result[0].event_type(), evdev::EventType::KEY, "Event type should remain KEY");
    }

    #[test]
    fn test_remap_release_event() {
        // Task 010-8.2.2: Test 1:1 remap for release event
        // Verify that when a key release event (value=0) for a remapped key is processed,
        // the output event has the correct remapped key code with value=0.
        let mut remap = HashMap::new();
        remap.insert(Key::KEY_CAPSLOCK, Key::KEY_ESC);
        let mut remapper = Remapper {
            remap,
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_RELEASE: i32 = 0;
        let release_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_CAPSLOCK.code(), KEY_RELEASE);
        let result = remapper.process(release_event);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code(), Key::KEY_ESC.code(), "Key should be remapped from CapsLock to Escape");
        assert_eq!(result[0].value(), KEY_RELEASE, "Release event value (0) should be preserved");
        assert_eq!(result[0].event_type(), evdev::EventType::KEY, "Event type should remain KEY");
    }

    #[test]
    fn test_passthrough_unmapped_key() {
        // Task 010-8.2.3: Test passthrough for unmapped key
        // Verify that when a key event for a key NOT in the remap table is processed,
        // it passes through unchanged.
        let mut remap = HashMap::new();
        remap.insert(Key::KEY_CAPSLOCK, Key::KEY_ESC);
        let mut remapper = Remapper {
            remap,
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_RELEASE: i32 = 0;

        // Test press event passthrough for unmapped key
        let press_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_PRESS);
        let result = remapper.process(press_event);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code(), Key::KEY_A.code(), "Unmapped key should pass through unchanged");
        assert_eq!(result[0].value(), KEY_PRESS, "Press event value should be preserved");
        assert_eq!(result[0].event_type(), evdev::EventType::KEY, "Event type should remain KEY");

        // Test release event passthrough for unmapped key
        let release_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_RELEASE);
        let result = remapper.process(release_event);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code(), Key::KEY_A.code(), "Unmapped key should pass through unchanged");
        assert_eq!(result[0].value(), KEY_RELEASE, "Release event value should be preserved");
        assert_eq!(result[0].event_type(), evdev::EventType::KEY, "Event type should remain KEY");
    }

    #[test]
    fn test_event_value_preservation() {
        // Create a remapper with a simple A -> B remap
        let mut remap = HashMap::new();
        remap.insert(Key::KEY_A, Key::KEY_B);
        let mut remapper = Remapper {
            remap,
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        // Event value constants
        const KEY_RELEASE: i32 = 0;
        const KEY_PRESS: i32 = 1;
        const KEY_REPEAT: i32 = 2;

        // Test press event (value=1) is preserved
        let press_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_PRESS);
        let result = remapper.process(press_event);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code(), Key::KEY_B.code(), "Key should be remapped from A to B");
        assert_eq!(result[0].value(), KEY_PRESS, "Press event value (1) should be preserved");

        // Test release event (value=0) is preserved
        let release_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_RELEASE);
        let result = remapper.process(release_event);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code(), Key::KEY_B.code(), "Key should be remapped from A to B");
        assert_eq!(result[0].value(), KEY_RELEASE, "Release event value (0) should be preserved");

        // Test repeat event (value=2) is preserved
        let repeat_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_REPEAT);
        let result = remapper.process(repeat_event);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code(), Key::KEY_B.code(), "Key should be remapped from A to B");
        assert_eq!(result[0].value(), KEY_REPEAT, "Repeat event value (2) should be preserved");
    }

    // ========================================================================
    // Combo Parsing Tests (Task 020-2.3)
    // ========================================================================

    #[test]
    fn test_parse_combo_basic() {
        // Task 020-2.3: Parse combo key strings into structured representation
        // Test basic combo parsing with two modifiers and a trigger key
        let combo = parse_combo("Ctrl+Shift+Q").expect("should parse valid combo");
        assert!(combo.modifiers.contains(&Modifier::Ctrl), "should contain Ctrl modifier");
        assert!(combo.modifiers.contains(&Modifier::Shift), "should contain Shift modifier");
        assert_eq!(combo.modifiers.len(), 2, "should have exactly 2 modifiers");
        assert_eq!(combo.key, Key::KEY_Q, "trigger key should be Q");
    }

    #[test]
    fn test_parse_combo_single_modifier() {
        // Test combo with a single modifier
        let combo = parse_combo("Ctrl+A").expect("should parse valid combo");
        assert!(combo.modifiers.contains(&Modifier::Ctrl));
        assert_eq!(combo.modifiers.len(), 1);
        assert_eq!(combo.key, Key::KEY_A);
    }

    #[test]
    fn test_parse_combo_all_modifiers() {
        // Test combo with all four modifiers
        let combo = parse_combo("Ctrl+Shift+Alt+Super+Q").expect("should parse valid combo");
        assert!(combo.modifiers.contains(&Modifier::Ctrl));
        assert!(combo.modifiers.contains(&Modifier::Shift));
        assert!(combo.modifiers.contains(&Modifier::Alt));
        assert!(combo.modifiers.contains(&Modifier::Super));
        assert_eq!(combo.modifiers.len(), 4);
        assert_eq!(combo.key, Key::KEY_Q);
    }

    #[test]
    fn test_parse_combo_no_modifiers() {
        // A single key without modifiers is still a valid "combo"
        let combo = parse_combo("Escape").expect("should parse single key");
        assert!(combo.modifiers.is_empty(), "should have no modifiers");
        assert_eq!(combo.key, Key::KEY_ESC);
    }

    #[test]
    fn test_parse_combo_modifier_order_independence() {
        // Task 020-2.3: Handle modifier order variations (Shift+Ctrl same as Ctrl+Shift)
        let combo1 = parse_combo("Ctrl+Shift+Q").expect("should parse");
        let combo2 = parse_combo("Shift+Ctrl+Q").expect("should parse");
        assert_eq!(combo1.modifiers, combo2.modifiers, "modifier order should not matter");
        assert_eq!(combo1.key, combo2.key);
    }

    #[test]
    fn test_parse_combo_alt_ctrl_delete() {
        // Classic combo test
        let combo = parse_combo("Ctrl+Alt+Delete").expect("should parse");
        assert!(combo.modifiers.contains(&Modifier::Ctrl));
        assert!(combo.modifiers.contains(&Modifier::Alt));
        assert_eq!(combo.modifiers.len(), 2);
        assert_eq!(combo.key, Key::KEY_DELETE);
    }

    #[test]
    fn test_parse_combo_case_insensitive() {
        // Modifiers and keys should be case-insensitive
        let combo1 = parse_combo("CTRL+SHIFT+Q").expect("should parse uppercase");
        let combo2 = parse_combo("ctrl+shift+q").expect("should parse lowercase");
        let combo3 = parse_combo("Ctrl+Shift+Q").expect("should parse mixed case");

        assert_eq!(combo1.modifiers, combo2.modifiers);
        assert_eq!(combo2.modifiers, combo3.modifiers);
        assert_eq!(combo1.key, combo2.key);
        assert_eq!(combo2.key, combo3.key);
    }

    #[test]
    fn test_parse_combo_with_spaces() {
        // Spaces around + should be handled
        let combo = parse_combo("Ctrl + Shift + Q").expect("should parse with spaces");
        assert!(combo.modifiers.contains(&Modifier::Ctrl));
        assert!(combo.modifiers.contains(&Modifier::Shift));
        assert_eq!(combo.key, Key::KEY_Q);
    }

    #[test]
    fn test_parse_combo_super_modifier_aliases() {
        // Test various aliases for Super modifier
        let combo_super = parse_combo("Super+Q").expect("Super should work");
        let combo_meta = parse_combo("Meta+Q").expect("Meta should work");
        let combo_mod = parse_combo("Mod+Q").expect("Mod should work");
        let combo_win = parse_combo("Win+Q").expect("Win should work");

        assert_eq!(combo_super.modifiers, combo_meta.modifiers);
        assert_eq!(combo_meta.modifiers, combo_mod.modifiers);
        assert_eq!(combo_mod.modifiers, combo_win.modifiers);
        assert!(combo_super.modifiers.contains(&Modifier::Super));
    }

    #[test]
    fn test_parse_combo_ctrl_aliases() {
        // Test Control alias for Ctrl
        let combo_ctrl = parse_combo("Ctrl+Q").expect("Ctrl should work");
        let combo_control = parse_combo("Control+Q").expect("Control should work");
        assert_eq!(combo_ctrl.modifiers, combo_control.modifiers);
    }

    #[test]
    fn test_parse_combo_function_keys() {
        // Test combos with function keys
        let combo = parse_combo("Alt+F4").expect("should parse Alt+F4");
        assert!(combo.modifiers.contains(&Modifier::Alt));
        assert_eq!(combo.key, Key::KEY_F4);
    }

    #[test]
    fn test_parse_combo_number_keys() {
        // Test combos with number keys
        let combo = parse_combo("Super+1").expect("should parse Super+1");
        assert!(combo.modifiers.contains(&Modifier::Super));
        assert_eq!(combo.key, Key::KEY_1);
    }

    // ========================================================================
    // Combo Parsing Error Tests (Task 020-2.3)
    // ========================================================================

    #[test]
    fn test_parse_combo_fail_empty() {
        // Task 020-2.3: Fail hard on unparseable combo strings
        let result = parse_combo("");
        assert!(result.is_err(), "empty string should fail");
        assert!(result.unwrap_err().reason.contains("empty"));
    }

    #[test]
    fn test_parse_combo_fail_only_modifiers() {
        // Only modifiers without a trigger key should fail
        let result = parse_combo("Ctrl+Shift");
        assert!(result.is_err(), "only modifiers should fail");
        assert!(result.unwrap_err().reason.contains("no trigger key"));
    }

    #[test]
    fn test_parse_combo_fail_unknown_key() {
        // Unknown key names should fail
        let result = parse_combo("Ctrl+UnknownKey");
        assert!(result.is_err(), "unknown key should fail");
        assert!(result.unwrap_err().reason.contains("unknown key"));
    }

    #[test]
    fn test_parse_combo_fail_duplicate_modifier() {
        // Duplicate modifiers should fail
        let result = parse_combo("Ctrl+Ctrl+Q");
        assert!(result.is_err(), "duplicate modifier should fail");
        assert!(result.unwrap_err().reason.contains("duplicate"));
    }

    #[test]
    fn test_parse_combo_fail_multiple_trigger_keys() {
        // Multiple non-modifier keys should fail
        let result = parse_combo("Ctrl+A+B");
        assert!(result.is_err(), "multiple trigger keys should fail");
        assert!(result.unwrap_err().reason.contains("multiple"));
    }

    #[test]
    fn test_parse_combo_fail_empty_component() {
        // Empty components (double +) should fail
        let result = parse_combo("Ctrl++Q");
        assert!(result.is_err(), "empty component should fail");
    }

    #[test]
    fn test_parse_combo_fail_trailing_plus() {
        // Trailing + should fail
        let result = parse_combo("Ctrl+Q+");
        assert!(result.is_err(), "trailing + should fail");
    }

    #[test]
    fn test_parse_combo_fail_leading_plus() {
        // Leading + should fail
        let result = parse_combo("+Ctrl+Q");
        assert!(result.is_err(), "leading + should fail");
    }

    // ========================================================================
    // Modifier Type Tests (Task 020-2.1)
    // ========================================================================

    #[test]
    fn test_modifier_from_key() {
        // Test that evdev keys are correctly identified as modifiers
        assert_eq!(Modifier::from_key(Key::KEY_LEFTCTRL), Some(Modifier::Ctrl));
        assert_eq!(Modifier::from_key(Key::KEY_RIGHTCTRL), Some(Modifier::Ctrl));
        assert_eq!(Modifier::from_key(Key::KEY_LEFTSHIFT), Some(Modifier::Shift));
        assert_eq!(Modifier::from_key(Key::KEY_RIGHTSHIFT), Some(Modifier::Shift));
        assert_eq!(Modifier::from_key(Key::KEY_LEFTALT), Some(Modifier::Alt));
        assert_eq!(Modifier::from_key(Key::KEY_RIGHTALT), Some(Modifier::Alt));
        assert_eq!(Modifier::from_key(Key::KEY_LEFTMETA), Some(Modifier::Super));
        assert_eq!(Modifier::from_key(Key::KEY_RIGHTMETA), Some(Modifier::Super));

        // Non-modifier keys should return None
        assert_eq!(Modifier::from_key(Key::KEY_A), None);
        assert_eq!(Modifier::from_key(Key::KEY_SPACE), None);
        assert_eq!(Modifier::from_key(Key::KEY_ENTER), None);
    }

    #[test]
    fn test_modifier_to_key() {
        // Modifiers should convert back to left variants
        assert_eq!(Modifier::Ctrl.to_key(), Key::KEY_LEFTCTRL);
        assert_eq!(Modifier::Shift.to_key(), Key::KEY_LEFTSHIFT);
        assert_eq!(Modifier::Alt.to_key(), Key::KEY_LEFTALT);
        assert_eq!(Modifier::Super.to_key(), Key::KEY_LEFTMETA);
    }

    #[test]
    fn test_key_combo_display() {
        // Test Display implementation for KeyCombo
        let combo = parse_combo("Ctrl+Shift+Q").expect("should parse");
        let display = format!("{}", combo);
        // Should contain all components (order may vary for modifiers, but key is last)
        assert!(display.contains("Ctrl"));
        assert!(display.contains("Shift"));
        assert!(display.ends_with("Q"));
    }

    #[test]
    fn test_key_combo_hash_equality() {
        // Two KeyCombos with same content should hash the same
        let combo1 = parse_combo("Ctrl+Shift+Q").expect("should parse");
        let combo2 = parse_combo("Shift+Ctrl+Q").expect("should parse");

        // They should be equal
        assert_eq!(combo1, combo2);

        // They should hash to the same value
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher1 = DefaultHasher::new();
        combo1.hash(&mut hasher1);
        let hash1 = hasher1.finish();

        let mut hasher2 = DefaultHasher::new();
        combo2.hash(&mut hasher2);
        let hash2 = hasher2.finish();

        assert_eq!(hash1, hash2, "equal KeyCombos should have same hash");
    }

    #[test]
    fn test_combo_tracker_register() {
        // Test ComboTracker registration
        let mut tracker = ComboTracker::new();
        let input = parse_combo("Ctrl+Shift+Q").expect("should parse");
        let output = parse_combo("Alt+F4").expect("should parse");

        tracker.register_combo(input.clone(), output.clone());

        assert!(tracker.combos.contains_key(&input));
        assert_eq!(tracker.combos.get(&input), Some(&output));
    }

    #[test]
    fn test_combo_state_default() {
        // ComboState should default to Idle
        let state = ComboState::default();
        assert_eq!(state, ComboState::Idle);
    }

    #[test]
    fn test_combo_tracker_default() {
        // ComboTracker should start in Idle state with no modifiers
        let tracker = ComboTracker::default();
        assert_eq!(tracker.state, ComboState::Idle);
        assert!(tracker.held_modifiers.is_empty());
        assert!(tracker.combos.is_empty());
    }

    // ========================================================================
    // Modifier Tracking Tests (Task 020-2.2)
    // ========================================================================

    #[test]
    fn test_modifier_tracking_press() {
        // Task 020-2.2: Modifiers are added on press
        let mut remapper = Remapper {
            remap: HashMap::new(),
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_PRESS: i32 = 1;

        // Initially no modifiers held
        assert!(remapper.held_modifiers().is_empty());

        // Press left Ctrl
        let ctrl_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS);
        remapper.process(ctrl_press);
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl), "Ctrl should be tracked after press");
        assert_eq!(remapper.held_modifiers().len(), 1);

        // Press left Shift
        let shift_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), KEY_PRESS);
        remapper.process(shift_press);
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl), "Ctrl should still be tracked");
        assert!(remapper.held_modifiers().contains(&Modifier::Shift), "Shift should be tracked after press");
        assert_eq!(remapper.held_modifiers().len(), 2);
    }

    #[test]
    fn test_modifier_tracking_release() {
        // Task 020-2.2: Modifiers are removed on release
        let mut remapper = Remapper {
            remap: HashMap::new(),
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_RELEASE: i32 = 0;

        // Press Ctrl and Shift
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), KEY_PRESS));
        assert_eq!(remapper.held_modifiers().len(), 2);

        // Release Ctrl
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_RELEASE));
        assert!(!remapper.held_modifiers().contains(&Modifier::Ctrl), "Ctrl should be removed after release");
        assert!(remapper.held_modifiers().contains(&Modifier::Shift), "Shift should still be tracked");
        assert_eq!(remapper.held_modifiers().len(), 1);

        // Release Shift
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), KEY_RELEASE));
        assert!(remapper.held_modifiers().is_empty(), "No modifiers should remain after all released");
    }

    #[test]
    fn test_modifier_tracking_repeat_no_duplicate() {
        // Task 020-2.2: Works correctly with repeat events (should not add duplicates)
        let mut remapper = Remapper {
            remap: HashMap::new(),
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_REPEAT: i32 = 2;

        // Press Ctrl
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        assert_eq!(remapper.held_modifiers().len(), 1);

        // Multiple repeat events should not add duplicates
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_REPEAT));
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_REPEAT));
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_REPEAT));
        assert_eq!(remapper.held_modifiers().len(), 1, "Repeat events should not add duplicates");
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl));
    }

    #[test]
    fn test_modifier_tracking_all_modifiers() {
        // Task 020-2.2: Track all modifier keys (Ctrl, Shift, Alt, Super)
        let mut remapper = Remapper {
            remap: HashMap::new(),
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_PRESS: i32 = 1;

        // Press all modifiers
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), KEY_PRESS));
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTALT.code(), KEY_PRESS));
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTMETA.code(), KEY_PRESS));

        assert_eq!(remapper.held_modifiers().len(), 4);
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl));
        assert!(remapper.held_modifiers().contains(&Modifier::Shift));
        assert!(remapper.held_modifiers().contains(&Modifier::Alt));
        assert!(remapper.held_modifiers().contains(&Modifier::Super));
    }

    #[test]
    fn test_modifier_tracking_left_right_normalization() {
        // Task 020-2.2: Left and right variants are normalized
        let mut remapper = Remapper {
            remap: HashMap::new(),
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_RELEASE: i32 = 0;

        // Press left Ctrl
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl));
        assert_eq!(remapper.held_modifiers().len(), 1);

        // Press right Ctrl - should NOT add a duplicate since both normalize to Modifier::Ctrl
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_RIGHTCTRL.code(), KEY_PRESS));
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl));
        assert_eq!(remapper.held_modifiers().len(), 1, "Left and right Ctrl should normalize to same modifier");

        // Release left Ctrl
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_RELEASE));
        // Modifier is removed even though right Ctrl is still "physically" held
        // This is expected behavior - the normalized modifier tracks logical state
        assert!(!remapper.held_modifiers().contains(&Modifier::Ctrl));
    }

    #[test]
    fn test_modifier_tracking_non_modifier_keys_ignored() {
        // Non-modifier keys should not affect the held_modifiers set
        let mut remapper = Remapper {
            remap: HashMap::new(),
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_RELEASE: i32 = 0;

        // Press some non-modifier keys
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_PRESS));
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_B.code(), KEY_PRESS));
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_SPACE.code(), KEY_PRESS));

        assert!(remapper.held_modifiers().is_empty(), "Non-modifier keys should not be tracked");

        // Release them
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_RELEASE));
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_B.code(), KEY_RELEASE));

        assert!(remapper.held_modifiers().is_empty());
    }

    #[test]
    fn test_modifier_tracking_mixed_with_remapping() {
        // Modifier tracking should work alongside key remapping
        let mut remap = HashMap::new();
        remap.insert(Key::KEY_CAPSLOCK, Key::KEY_ESC);

        let mut remapper = Remapper {
            remap,
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
        };

        const KEY_PRESS: i32 = 1;

        // Press Ctrl (modifier) and CapsLock (remapped)
        remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        let result = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_CAPSLOCK.code(), KEY_PRESS));

        // Modifier should be tracked
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl));

        // Remapping should still work
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code(), Key::KEY_ESC.code());
    }
}
