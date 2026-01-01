//! Key remapping logic
//!
//! This module provides the core remapping functionality for niri-mapper, including:
//! - Simple 1:1 key remapping
//! - Combo (chord) remapping with modifier tracking
//! - Macro execution
//! - Per-device profile management with runtime switching
//!
//! # Per-Application Profile Foundation (v0.4.0)
//!
//! The [`DeviceRemapper`] struct provides the foundation for per-application
//! profile switching. v0.4.0 implements:
//!
//! ## Current Capabilities
//!
//! - **Active profile tracking**: Each device tracks which profile is currently active
//! - **Runtime profile switching**: [`DeviceRemapper::switch_profile()`] swaps the
//!   active remapping rules at runtime
//! - **Profile switch keybinds**: Configure key combos to switch profiles directly
//!   from the keyboard
//!
//! ## Manual Switching Methods
//!
//! 1. **Keybinds**: Configure `profile-switch` block in device config
//!    ```kdl
//!    profile-switch {
//!        Ctrl+Shift+1 "default"
//!        Ctrl+Shift+2 "gaming"
//!    }
//!    ```
//!
//! 2. **CLI**: `niri-mapper switch-profile <device> <profile>`
//!
//! 3. **Control socket**: `{"switch_profile": {"device": "...", "profile": "..."}}`
//!
//! ## Future: Automatic Switching (Backlog)
//!
//! Automatic profile switching based on focused application's `app_id` is **not
//! implemented** in v0.4.0. The intended future behavior:
//!
//! 1. Daemon monitors niri focus change events
//! 2. On focus change, looks up profiles with matching `app_id_hint`
//! 3. Calls `switch_profile()` automatically for matching profiles
//!
//! The [`Profile::app_id_hint`](niri_mapper_config::Profile::app_id_hint) field
//! exists for users to annotate their profiles in preparation for this feature.
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
use niri_mapper_config::{MacroAction, Profile};

// ============================================================================
// RemapResult (Task 030-1.2.4, 030-3.3.3)
// ============================================================================

/// Result of processing an input event through the remapper.
///
/// This enum allows the caller to distinguish between different types of
/// remapping results that require different handling:
/// - Direct events can be emitted immediately to the virtual device
/// - Macros require async execution with delays
/// - Profile switches require updating the remapper state
#[derive(Debug, Clone)]
pub enum RemapResult {
    /// Direct key events to emit to the virtual device.
    ///
    /// These events can be written directly to uinput without any additional processing.
    Events(Vec<InputEvent>),

    /// A macro sequence that should be executed asynchronously.
    ///
    /// The caller should spawn an async task to execute these actions,
    /// which may include delays between key presses.
    Macro(Vec<MacroAction>),

    /// A profile switch was triggered by a keybind.
    ///
    /// The caller should switch to the named profile. The profile switch
    /// keybind itself is consumed (no events are emitted for it).
    ProfileSwitch(String),
}

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

/// Tracks an active combo remapping session.
///
/// When a combo is matched (e.g., Ctrl+Shift+Q -> Alt+F4), we need to remember
/// what output combo was injected so that when the trigger key (Q) is released,
/// we can inject the correct release events (release F4, not Q).
///
/// This struct stores all the information needed to generate the correct
/// release event sequence when the trigger key is released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCombo {
    /// The trigger key that activated this combo (e.g., KEY_Q)
    pub trigger_key: Key,
    /// The modifiers that were held when the combo was activated
    pub input_modifiers: HashSet<Modifier>,
    /// The output combo that was injected (modifiers + key)
    pub output_combo: KeyCombo,
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
/// - Any currently active combo (for generating correct release events)
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
    /// Currently active combo (if any) - tracks what output was injected
    /// so we can generate correct release events when the trigger key is released
    pub active_combo: Option<ActiveCombo>,
}

impl Default for ComboTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of checking for a combo match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComboMatchResult {
    /// No combo matched for the given key press
    NoMatch,
    /// A combo was matched; contains the output combo to inject
    Matched {
        /// The input combo that was matched
        input: KeyCombo,
        /// The output combo to inject
        output: KeyCombo,
    },
}

/// Event value constants for key events.
pub mod event_value {
    /// Key release event value
    pub const RELEASE: i32 = 0;
    /// Key press event value
    pub const PRESS: i32 = 1;
    /// Key repeat event value (autorepeat)
    #[allow(dead_code)]
    pub const REPEAT: i32 = 2;
}

/// Generates the synthetic event sequence for combo output injection.
///
/// When a combo like `Ctrl+Shift+Q` is remapped to `Alt+F4`, this function
/// generates the proper sequence of evdev events to:
/// 1. Release the input modifiers that aren't in the output combo
/// 2. Press the output modifiers that aren't already held
/// 3. Press the output trigger key
///
/// The release sequence (generated by [`generate_combo_release_events`]) handles:
/// 4. Release the output trigger key
/// 5. Release the output modifiers
/// 6. Restore any input modifiers that were released
///
/// # Arguments
///
/// * `input_modifiers` - The modifiers that are currently held (from input combo)
/// * `output_combo` - The output combo to inject
///
/// # Returns
///
/// A `Vec<InputEvent>` containing the synthetic events to inject for the key press.
///
/// # Example
///
/// For `Ctrl+Shift+Q` -> `Alt+F4`:
/// - Input modifiers: `{Ctrl, Shift}`
/// - Output combo: `modifiers: {Alt}, key: F4`
///
/// Generated press events:
/// 1. Release Ctrl (synthetic)
/// 2. Release Shift (synthetic)
/// 3. Press Alt (synthetic)
/// 4. Press F4 (synthetic)
///
/// Note: SYN_REPORT events are NOT included; the caller should add them as needed.
pub fn generate_combo_press_events(
    input_modifiers: &HashSet<Modifier>,
    output_combo: &KeyCombo,
) -> Vec<InputEvent> {
    let mut events = Vec::new();

    // Step 1: Release input modifiers that are NOT in the output combo
    // Sort for deterministic ordering in tests
    let mut to_release: Vec<_> = input_modifiers
        .difference(&output_combo.modifiers)
        .copied()
        .collect();
    to_release.sort();

    for modifier in to_release {
        events.push(InputEvent::new(
            evdev::EventType::KEY,
            modifier.to_key().code(),
            event_value::RELEASE,
        ));
    }

    // Step 2: Press output modifiers that are NOT already held
    let mut to_press: Vec<_> = output_combo
        .modifiers
        .difference(input_modifiers)
        .copied()
        .collect();
    to_press.sort();

    for modifier in to_press {
        events.push(InputEvent::new(
            evdev::EventType::KEY,
            modifier.to_key().code(),
            event_value::PRESS,
        ));
    }

    // Step 3: Press the output trigger key
    events.push(InputEvent::new(
        evdev::EventType::KEY,
        output_combo.key.code(),
        event_value::PRESS,
    ));

    events
}

/// Generates the synthetic event sequence for combo release.
///
/// When the trigger key is released, this function generates events to:
/// 1. Release the output trigger key
/// 2. Release the output modifiers that weren't in the input combo
/// 3. Restore (re-press) input modifiers that are still physically held
///
/// # Arguments
///
/// * `input_modifiers` - The modifiers from the original input combo
/// * `output_combo` - The output combo that was injected
/// * `still_held_modifiers` - Modifiers that are still physically held
///
/// # Returns
///
/// A `Vec<InputEvent>` containing the synthetic events to inject for the key release.
///
/// # Example
///
/// For `Ctrl+Shift+Q` -> `Alt+F4` (when Q is released, Ctrl+Shift still held):
/// - Input modifiers: `{Ctrl, Shift}`
/// - Output combo: `modifiers: {Alt}, key: F4`
/// - Still held: `{Ctrl, Shift}`
///
/// Generated release events:
/// 1. Release F4
/// 2. Release Alt
/// 3. Press Ctrl (restore)
/// 4. Press Shift (restore)
pub fn generate_combo_release_events(
    input_modifiers: &HashSet<Modifier>,
    output_combo: &KeyCombo,
    still_held_modifiers: &HashSet<Modifier>,
) -> Vec<InputEvent> {
    let mut events = Vec::new();

    // Step 1: Release the output trigger key
    events.push(InputEvent::new(
        evdev::EventType::KEY,
        output_combo.key.code(),
        event_value::RELEASE,
    ));

    // Step 2: Release output modifiers that weren't in the input combo
    let mut to_release: Vec<_> = output_combo
        .modifiers
        .difference(input_modifiers)
        .copied()
        .collect();
    to_release.sort();

    for modifier in to_release {
        events.push(InputEvent::new(
            evdev::EventType::KEY,
            modifier.to_key().code(),
            event_value::RELEASE,
        ));
    }

    // Step 3: Restore input modifiers that are still physically held
    // These are modifiers that were in the input combo, were released for the output,
    // and are still being physically held by the user
    let released_input_mods: HashSet<_> = input_modifiers
        .difference(&output_combo.modifiers)
        .copied()
        .collect();

    let mut to_restore: Vec<_> = released_input_mods
        .intersection(still_held_modifiers)
        .copied()
        .collect();
    to_restore.sort();

    for modifier in to_restore {
        events.push(InputEvent::new(
            evdev::EventType::KEY,
            modifier.to_key().code(),
            event_value::PRESS,
        ));
    }

    events
}

impl ComboTracker {
    /// Create a new combo tracker with no registered combos.
    pub fn new() -> Self {
        Self {
            held_modifiers: HashSet::new(),
            state: ComboState::Idle,
            combos: HashMap::new(),
            active_combo: None,
        }
    }

    /// Register a combo mapping.
    ///
    /// When `input` combo is detected, `output` events will be injected instead.
    pub fn register_combo(&mut self, input: KeyCombo, output: KeyCombo) {
        self.combos.insert(input, output);
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
    pub fn update_held_modifiers(&mut self, key: Key, value: i32) {
        if let Some(modifier) = Modifier::from_key(key) {
            match value {
                0 => {
                    // Release: remove modifier from held set
                    self.held_modifiers.remove(&modifier);
                }
                1 => {
                    // Press: add modifier to held set
                    self.held_modifiers.insert(modifier);
                }
                2 => {
                    // Repeat: modifier is already held, no action needed
                }
                _ => {
                    // Unknown event value, ignore
                }
            }
        }

        // Update state based on whether any modifiers are held
        self.update_state();
    }

    /// Update the combo state based on current held modifiers.
    fn update_state(&mut self) {
        match &self.state {
            ComboState::Idle => {
                if !self.held_modifiers.is_empty() {
                    self.state = ComboState::ModifiersHeld;
                }
            }
            ComboState::ModifiersHeld => {
                if self.held_modifiers.is_empty() {
                    self.state = ComboState::Idle;
                }
            }
            ComboState::ComboMatched { .. } => {
                // Stay in ComboMatched state until trigger key is released
                // (handled externally)
            }
        }
    }

    /// Check if a key press matches any registered combo.
    ///
    /// This method checks if the combination of currently held modifiers plus
    /// the pressed key EXACTLY matches any registered combo. An exact match
    /// requires:
    /// 1. The pressed key matches the combo's trigger key
    /// 2. The held modifiers are EXACTLY equal to the combo's required modifiers
    ///    (no extra modifiers, no missing modifiers)
    ///
    /// # Arguments
    ///
    /// * `key` - The non-modifier key that was pressed
    ///
    /// # Returns
    ///
    /// * `ComboMatchResult::NoMatch` - No combo matched
    /// * `ComboMatchResult::Matched { input, output }` - A combo matched
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut tracker = ComboTracker::new();
    /// tracker.register_combo(
    ///     parse_combo("Ctrl+Shift+Q").unwrap(),
    ///     parse_combo("Alt+F4").unwrap(),
    /// );
    ///
    /// // Simulate Ctrl+Shift held
    /// tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
    /// tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);
    ///
    /// // Check if Q press matches
    /// let result = tracker.check_combo_match(Key::KEY_Q);
    /// assert!(matches!(result, ComboMatchResult::Matched { .. }));
    /// ```
    pub fn check_combo_match(&self, key: Key) -> ComboMatchResult {
        // Skip modifier keys - they cannot be trigger keys
        if Modifier::from_key(key).is_some() {
            return ComboMatchResult::NoMatch;
        }

        // Look for a combo that matches exactly:
        // - Same trigger key
        // - Exactly the same set of modifiers (not a subset, not a superset)
        for (input_combo, output_combo) in &self.combos {
            if input_combo.key == key && input_combo.modifiers == self.held_modifiers {
                return ComboMatchResult::Matched {
                    input: input_combo.clone(),
                    output: output_combo.clone(),
                };
            }
        }

        ComboMatchResult::NoMatch
    }

    /// Handle a key press event and check for combo match.
    ///
    /// This is a convenience method that combines modifier tracking with combo
    /// matching. It should be called for every key press event.
    ///
    /// # Arguments
    ///
    /// * `key` - The key that was pressed
    ///
    /// # Returns
    ///
    /// The result of checking for a combo match (after updating modifiers).
    pub fn handle_key_press(&mut self, key: Key) -> ComboMatchResult {
        // First, update held modifiers if this is a modifier key
        self.update_held_modifiers(key, 1);

        // Then check for combo match
        self.check_combo_match(key)
    }

    /// Handle a key release event.
    ///
    /// This updates the modifier tracking state. For combo state transitions
    /// on trigger key release, see task 020-2.6.
    ///
    /// # Arguments
    ///
    /// * `key` - The key that was released
    pub fn handle_key_release(&mut self, key: Key) {
        self.update_held_modifiers(key, 0);
    }

    /// Activate a combo after it has been matched.
    ///
    /// This should be called when a combo is detected (via `check_combo_match`
    /// or `handle_key_press` returning `Matched`). It stores the active combo
    /// state so that when the trigger key is released, we can generate the
    /// correct release events.
    ///
    /// # Arguments
    ///
    /// * `trigger_key` - The key that triggered the combo (e.g., KEY_Q)
    /// * `input_modifiers` - The modifiers that were held (from input combo)
    /// * `output_combo` - The output combo that was injected
    ///
    /// # Example
    ///
    /// ```ignore
    /// // After detecting Ctrl+Shift+Q -> Alt+F4 match:
    /// tracker.activate_combo(
    ///     Key::KEY_Q,
    ///     [Modifier::Ctrl, Modifier::Shift].into_iter().collect(),
    ///     parse_combo("Alt+F4").unwrap(),
    /// );
    /// ```
    pub fn activate_combo(
        &mut self,
        trigger_key: Key,
        input_modifiers: HashSet<Modifier>,
        output_combo: KeyCombo,
    ) {
        self.active_combo = Some(ActiveCombo {
            trigger_key,
            input_modifiers,
            output_combo,
        });
        // State transition is already handled by ComboState::ComboMatched
        // but we keep active_combo for generating release events
    }

    /// Handle the release of a trigger key and generate release events if needed.
    ///
    /// When the trigger key of an active combo is released, this method:
    /// 1. Checks if there's an active combo for this key
    /// 2. If so, generates release events using `generate_combo_release_events()`
    /// 3. Clears the active combo state
    /// 4. Updates the combo state machine
    ///
    /// # Arguments
    ///
    /// * `key` - The key that was released
    ///
    /// # Returns
    ///
    /// A `Vec<InputEvent>` containing the synthetic release events to inject,
    /// or an empty vector if no active combo was found for this key.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // User pressed Ctrl+Shift+Q (remapped to Alt+F4)
    /// // Now user releases Q
    /// let release_events = tracker.handle_trigger_release(Key::KEY_Q);
    /// // release_events contains: release F4, release Alt, restore Ctrl, restore Shift
    /// ```
    pub fn handle_trigger_release(&mut self, key: Key) -> Vec<InputEvent> {
        // First, update modifier tracking (in case this is a modifier key)
        self.update_held_modifiers(key, 0);

        // Check if there's an active combo for this trigger key
        let events = if let Some(ref active) = self.active_combo {
            if active.trigger_key == key {
                // Generate release events for the output combo
                let events = generate_combo_release_events(
                    &active.input_modifiers,
                    &active.output_combo,
                    &self.held_modifiers,
                );
                events
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Clear active combo if this was its trigger key
        if self.active_combo.as_ref().map_or(false, |a| a.trigger_key == key) {
            self.active_combo = None;
            // Update state: transition back based on whether modifiers are still held
            if self.held_modifiers.is_empty() {
                self.state = ComboState::Idle;
            } else {
                self.state = ComboState::ModifiersHeld;
            }
        }

        events
    }

    /// Check if there's an active combo for the given trigger key.
    ///
    /// This is useful for determining whether a key release event should
    /// be handled as a combo release or passed through normally.
    pub fn has_active_combo_for(&self, key: Key) -> bool {
        self.active_combo.as_ref().map_or(false, |a| a.trigger_key == key)
    }

    /// Get the currently active combo, if any.
    pub fn get_active_combo(&self) -> Option<&ActiveCombo> {
        self.active_combo.as_ref()
    }

    /// Clear the active combo state.
    ///
    /// This should be called if the combo needs to be cancelled for any reason.
    pub fn clear_active_combo(&mut self) {
        self.active_combo = None;
    }
}

// ============================================================================
// Remapper
// ============================================================================

/// Remapper handles translating input events according to a profile
#[derive(Debug)]
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
    /// Combo tracker for handling key combination remappings
    ///
    /// This tracks modifier state and matches input combos against registered
    /// combo mappings. When a combo like "Ctrl+Shift+Q" is detected, it can
    /// be remapped to a different combo like "Alt+F4".
    combo_tracker: ComboTracker,
    /// Macro triggers: maps a trigger key to a sequence of macro actions
    ///
    /// When a key in this map is pressed, the associated macro action sequence
    /// should be executed instead of the normal key event. The sequence can
    /// contain key presses/releases and delays.
    ///
    /// Populated by `from_profile()` from `profile.macros` configuration.
    macro_triggers: HashMap<Key, Vec<MacroAction>>,
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

        // Parse combo mappings from profile
        let mut combo_tracker = ComboTracker::new();
        for (input_str, output_str) in &profile.combo {
            match (parse_combo(input_str), parse_combo(output_str)) {
                (Ok(input_combo), Ok(output_combo)) => {
                    tracing::debug!(
                        "Registered combo: {} -> {}",
                        input_combo,
                        output_combo
                    );
                    combo_tracker.register_combo(input_combo, output_combo);
                }
                (Err(e), _) => {
                    tracing::warn!("Failed to parse input combo '{}': {}", input_str, e);
                }
                (_, Err(e)) => {
                    tracing::warn!("Failed to parse output combo '{}': {}", output_str, e);
                }
            }
        }

        // TODO: Parse passthrough keys from niri_passthrough

        // Parse macro triggers from profile.macros (Task 030-1.2.2)
        // For v0.3.0, only single-key triggers are supported (combo triggers are Out of Scope)
        let mut macro_triggers = HashMap::new();
        for (trigger_str, actions) in &profile.macros {
            match parse_key(trigger_str) {
                Some(trigger_key) => {
                    tracing::debug!(
                        "Registered macro trigger: {} -> {} actions",
                        trigger_str,
                        actions.len()
                    );
                    macro_triggers.insert(trigger_key, actions.clone());
                }
                None => {
                    // Fail hard on invalid trigger keys as per task requirements
                    panic!(
                        "Invalid macro trigger key '{}': unknown key name. \
                         Valid keys include: A-Z, 0-9, F1-F24, Escape, Tab, etc.",
                        trigger_str
                    );
                }
            }
        }

        Self {
            remap,
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
            combo_tracker,
            macro_triggers,
        }
    }

    /// Switch to a new profile, replacing all remap/combo/macro rules.
    ///
    /// This method reloads the remapper with rules from a new profile while
    /// preserving the current modifier key state (since physical keys may still
    /// be pressed during the switch).
    ///
    /// # Arguments
    ///
    /// * `profile` - The new profile to switch to
    ///
    /// # Behavior
    ///
    /// - All remap rules are replaced with the new profile's rules
    /// - All combo mappings are replaced with the new profile's combos
    /// - All macro triggers are replaced with the new profile's macros
    /// - Currently held modifiers are preserved (not cleared)
    /// - Any active combo is cleared (to avoid stale state)
    ///
    /// # Panics
    ///
    /// Panics if the profile contains invalid macro trigger key names (same as
    /// `from_profile()`).
    ///
    /// # Example
    ///
    /// ```ignore
    /// // In the event loop, when profile switch is requested:
    /// if let Some(profile) = device_config.profiles.get(&new_profile_name) {
    ///     remapper.switch_profile(profile);
    ///     tracing::info!("Switched to profile '{}'", new_profile_name);
    /// }
    /// ```
    pub fn switch_profile(&mut self, profile: &Profile) {
        // Clear existing rules
        self.remap.clear();
        self.passthrough.clear();
        self.macro_triggers.clear();

        // Clear active combo state to avoid stale output tracking
        // Note: held_modifiers is preserved since physical keys may still be pressed
        self.combo_tracker.clear_active_combo();
        self.combo_tracker.combos.clear();

        // Reload remap rules from new profile
        for (from, to) in &profile.remap {
            if let (Some(from_key), Some(to_key)) = (parse_key(from), parse_key(to)) {
                self.remap.insert(from_key, to_key);
            }
        }

        // Reload combo mappings from new profile
        for (input_str, output_str) in &profile.combo {
            match (parse_combo(input_str), parse_combo(output_str)) {
                (Ok(input_combo), Ok(output_combo)) => {
                    tracing::debug!(
                        "Registered combo: {} -> {}",
                        input_combo,
                        output_combo
                    );
                    self.combo_tracker.register_combo(input_combo, output_combo);
                }
                (Err(e), _) => {
                    tracing::warn!("Failed to parse input combo '{}': {}", input_str, e);
                }
                (_, Err(e)) => {
                    tracing::warn!("Failed to parse output combo '{}': {}", output_str, e);
                }
            }
        }

        // Reload macro triggers from new profile
        for (trigger_str, actions) in &profile.macros {
            match parse_key(trigger_str) {
                Some(trigger_key) => {
                    tracing::debug!(
                        "Registered macro trigger: {} -> {} actions",
                        trigger_str,
                        actions.len()
                    );
                    self.macro_triggers.insert(trigger_key, actions.clone());
                }
                None => {
                    // Fail hard on invalid trigger keys as per task requirements
                    panic!(
                        "Invalid macro trigger key '{}': unknown key name. \
                         Valid keys include: A-Z, 0-9, F1-F24, Escape, Tab, etc.",
                        trigger_str
                    );
                }
            }
        }

        tracing::debug!(
            "Profile switched: {} remaps, {} combos, {} macros",
            self.remap.len(),
            self.combo_tracker.combos.len(),
            self.macro_triggers.len()
        );
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
    pub fn update_held_modifiers(&mut self, key: Key, value: i32) {
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

    /// Process an input event, returning the remapped result.
    ///
    /// Key event values are preserved through remapping:
    /// - `0` = key release
    /// - `1` = key press
    /// - `2` = key repeat (autorepeat)
    ///
    /// This method also updates the internal modifier tracking state and handles
    /// combo detection, remapping, and macro trigger detection.
    ///
    /// # Processing Order
    ///
    /// On key press (value == 1), events are checked in this order:
    /// 1. **Macro triggers**: If the key matches a registered macro trigger,
    ///    return `RemapResult::Macro` with the action sequence.
    /// 2. **Combo matching**: If the key + held modifiers match a registered combo,
    ///    the combo's output events are generated.
    /// 3. **Simple remaps**: If the key is in the remap table, remap it.
    /// 4. **Passthrough**: Otherwise, pass through unchanged.
    ///
    /// On key release/repeat, macro triggers are NOT checked (only on press).
    pub fn process(&mut self, event: InputEvent) -> RemapResult {
        // Only process key events
        if event.event_type() != evdev::EventType::KEY {
            return RemapResult::Events(vec![event]);
        }

        let key = Key::new(event.code());
        let value = event.value();

        // Update modifier tracking state for every key event (in both trackers)
        self.update_held_modifiers(key, value);
        self.combo_tracker.update_held_modifiers(key, value);

        match value {
            event_value::PRESS => {
                // Task 030-1.2.3: Check for macro trigger on key press FIRST
                // Macros take priority over combos and simple remaps
                if let Some(actions) = self.macro_triggers.get(&key) {
                    return RemapResult::Macro(actions.clone());
                }

                // Check for combo match on key press
                let match_result = self.combo_tracker.check_combo_match(key);

                match match_result {
                    ComboMatchResult::Matched { input, output } => {
                        // Generate press events for the output combo
                        let press_events = generate_combo_press_events(&input.modifiers, &output);

                        // Activate the combo to track it for release handling
                        self.combo_tracker.activate_combo(key, input.modifiers, output);

                        return RemapResult::Events(press_events);
                    }
                    ComboMatchResult::NoMatch => {
                        // No combo match, check for simple remap
                        if let Some(&remapped_key) = self.remap.get(&key) {
                            return RemapResult::Events(vec![InputEvent::new(
                                evdev::EventType::KEY,
                                remapped_key.code(),
                                value,
                            )]);
                        }
                    }
                }
            }
            event_value::RELEASE => {
                // Check if there's an active combo for this key
                if self.combo_tracker.has_active_combo_for(key) {
                    // Generate release events for the output combo
                    let release_events = self.combo_tracker.handle_trigger_release(key);
                    return RemapResult::Events(release_events);
                }

                // No active combo, check for simple remap
                if let Some(&remapped_key) = self.remap.get(&key) {
                    return RemapResult::Events(vec![InputEvent::new(
                        evdev::EventType::KEY,
                        remapped_key.code(),
                        value,
                    )]);
                }
            }
            event_value::REPEAT => {
                // For repeat events, check if there's an active combo
                if let Some(active) = self.combo_tracker.get_active_combo() {
                    if active.trigger_key == key {
                        // Repeat the output key, not the input key
                        return RemapResult::Events(vec![InputEvent::new(
                            evdev::EventType::KEY,
                            active.output_combo.key.code(),
                            value,
                        )]);
                    }
                }

                // No active combo, check for simple remap
                if let Some(&remapped_key) = self.remap.get(&key) {
                    return RemapResult::Events(vec![InputEvent::new(
                        evdev::EventType::KEY,
                        remapped_key.code(),
                        value,
                    )]);
                }
            }
            _ => {
                // Unknown event value, pass through
            }
        }

        // Pass through unmodified
        RemapResult::Events(vec![event])
    }

    /// Get the combo tracker for inspection (useful for testing and debugging).
    pub fn combo_tracker(&self) -> &ComboTracker {
        &self.combo_tracker
    }
}

// ============================================================================
// DeviceRemapper (Task 030-3.1.1)
// ============================================================================

/// Error returned when profile operations fail
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError {
    /// The profile name that caused the error
    pub profile: String,
    /// Description of what went wrong
    pub reason: String,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "profile '{}': {}", self.profile, self.reason)
    }
}

impl std::error::Error for ProfileError {}

/// Per-device profile management and remapping.
///
/// `DeviceRemapper` wraps a `Remapper` and adds the ability to manage multiple
/// named profiles for a single device. It tracks which profile is currently
/// active and allows switching between profiles at runtime.
///
/// # Per-Application Profile Foundation (v0.4.0)
///
/// This struct is the core component of the per-application profile system.
/// In v0.4.0, it provides the foundation for profile switching:
///
/// ## Current Capabilities
///
/// - **Profile storage**: Holds all configured profiles for the device
/// - **Active profile tracking**: Tracks which profile is currently in use
/// - **Runtime switching**: [`switch_profile()`](Self::switch_profile) replaces
///   the active remapper with a new profile's rules
/// - **Profile switch keybinds**: Detects keybind combos and returns
///   [`RemapResult::ProfileSwitch`] for the caller to handle
///
/// ## How Profile Switching Works
///
/// 1. User triggers a switch (via keybind, CLI, or control socket)
/// 2. `switch_profile()` is called with the target profile name
/// 3. A new `Remapper` is built from the target profile's configuration
/// 4. The active profile name and remapper are updated atomically
/// 5. Subsequent key events use the new profile's remapping rules
///
/// ## Future: Automatic Switching (Backlog)
///
/// In a future release, the daemon will automatically call `switch_profile()`
/// when the focused window's `app_id` matches a profile's `app_id_hint`.
/// This is **not implemented** in v0.4.0 - profiles must be switched manually.
///
/// The `app_id_hint` field in [`Profile`](niri_mapper_config::Profile) exists
/// for users to annotate their profiles in preparation for automatic switching.
///
/// # Example
///
/// ```ignore
/// use std::collections::HashMap;
/// use niri_mapper_config::Profile;
///
/// let mut profiles = HashMap::new();
/// profiles.insert("default".to_string(), Profile::default());
/// profiles.insert("gaming".to_string(), Profile::default());
///
/// let mut device_remapper = DeviceRemapper::new(
///     "Keychron K3 Pro".to_string(),
///     profiles,
///     "default",
/// )?;
///
/// assert_eq!(device_remapper.active_profile(), "default");
///
/// // Manual profile switch
/// device_remapper.switch_profile("gaming")?;
/// assert_eq!(device_remapper.active_profile(), "gaming");
/// ```
#[derive(Debug)]
pub struct DeviceRemapper {
    /// The name of the device this remapper is for
    device_name: String,
    /// All available profiles for this device
    profiles: HashMap<String, Profile>,
    /// The name of the currently active profile
    active_profile: String,
    /// The current remapper instance, derived from the active profile
    remapper: Remapper,
    /// Profile switch keybinds: maps combo -> profile name (Task 030-3.3.3)
    ///
    /// When a key event matches one of these combos (on key press), the remapper
    /// returns `RemapResult::ProfileSwitch` instead of normal events. The caller
    /// is responsible for handling the actual profile switch.
    profile_switch_combos: HashMap<KeyCombo, String>,
}

impl DeviceRemapper {
    /// Create a new DeviceRemapper for a device with the given profiles.
    ///
    /// # Arguments
    ///
    /// * `device_name` - The name of the device (for logging/identification)
    /// * `profiles` - A map of profile names to Profile configurations
    /// * `default_profile` - The name of the profile to use initially
    ///
    /// # Errors
    ///
    /// Returns a `ProfileError` if `default_profile` does not exist in `profiles`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut profiles = HashMap::new();
    /// profiles.insert("default".to_string(), Profile::default());
    ///
    /// let remapper = DeviceRemapper::new(
    ///     "My Keyboard".to_string(),
    ///     profiles,
    ///     "default",
    /// )?;
    /// ```
    pub fn new(
        device_name: String,
        profiles: HashMap<String, Profile>,
        default_profile: &str,
    ) -> Result<Self, ProfileError> {
        Self::new_with_profile_switch(device_name, profiles, default_profile, HashMap::new())
    }

    /// Create a new DeviceRemapper with profile switch keybinds.
    ///
    /// # Arguments
    ///
    /// * `device_name` - The name of the device (for logging/identification)
    /// * `profiles` - A map of profile names to Profile configurations
    /// * `default_profile` - The name of the profile to use initially
    /// * `profile_switch` - Map of combo strings (e.g., "Ctrl+Shift+1") to profile names
    ///
    /// # Errors
    ///
    /// Returns a `ProfileError` if:
    /// - `default_profile` does not exist in `profiles`
    /// - A profile_switch combo string fails to parse
    /// - A profile_switch references a non-existent profile
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut profiles = HashMap::new();
    /// profiles.insert("default".to_string(), Profile::default());
    /// profiles.insert("gaming".to_string(), Profile::default());
    ///
    /// let mut profile_switch = HashMap::new();
    /// profile_switch.insert("Ctrl+Shift+1".to_string(), "default".to_string());
    /// profile_switch.insert("Ctrl+Shift+2".to_string(), "gaming".to_string());
    ///
    /// let remapper = DeviceRemapper::new_with_profile_switch(
    ///     "My Keyboard".to_string(),
    ///     profiles,
    ///     "default",
    ///     profile_switch,
    /// )?;
    /// ```
    pub fn new_with_profile_switch(
        device_name: String,
        profiles: HashMap<String, Profile>,
        default_profile: &str,
        profile_switch: HashMap<String, String>,
    ) -> Result<Self, ProfileError> {
        // Validate that the default profile exists
        let profile = profiles.get(default_profile).ok_or_else(|| ProfileError {
            profile: default_profile.to_string(),
            reason: format!(
                "profile not found in device '{}' (available profiles: {})",
                device_name,
                profiles
                    .keys()
                    .map(|s| format!("'{}'", s))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;

        // Build the initial Remapper from the default profile
        let remapper = Remapper::from_profile(profile);

        // Parse profile switch keybinds (Task 030-3.3.3)
        let mut profile_switch_combos = HashMap::new();
        for (combo_str, target_profile) in profile_switch {
            // Validate that the target profile exists
            if !profiles.contains_key(&target_profile) {
                return Err(ProfileError {
                    profile: target_profile.clone(),
                    reason: format!(
                        "profile-switch keybind '{}' references non-existent profile '{}' \
                         (available profiles: {})",
                        combo_str,
                        target_profile,
                        profiles
                            .keys()
                            .map(|s| format!("'{}'", s))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }

            // Parse the combo string
            match parse_combo(&combo_str) {
                Ok(combo) => {
                    tracing::debug!(
                        "Registered profile switch: {} -> '{}'",
                        combo,
                        target_profile
                    );
                    profile_switch_combos.insert(combo, target_profile);
                }
                Err(e) => {
                    return Err(ProfileError {
                        profile: target_profile,
                        reason: format!(
                            "failed to parse profile-switch keybind '{}': {}",
                            combo_str, e
                        ),
                    });
                }
            }
        }

        Ok(Self {
            device_name,
            profiles,
            active_profile: default_profile.to_string(),
            remapper,
            profile_switch_combos,
        })
    }

    /// Get the device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Get the name of the currently active profile.
    pub fn active_profile(&self) -> &str {
        &self.active_profile
    }

    /// Get a reference to the inner `Remapper` for event processing.
    ///
    /// Use this to call `process()` on incoming events.
    pub fn remapper(&self) -> &Remapper {
        &self.remapper
    }

    /// Get a mutable reference to the inner `Remapper` for event processing.
    ///
    /// Use this to call `process()` on incoming events (which requires `&mut self`).
    pub fn remapper_mut(&mut self) -> &mut Remapper {
        &mut self.remapper
    }

    /// Get a list of all available profile names for this device.
    pub fn profile_names(&self) -> Vec<&str> {
        self.profiles.keys().map(|s| s.as_str()).collect()
    }

    /// Check if a profile with the given name exists.
    pub fn has_profile(&self, name: &str) -> bool {
        self.profiles.contains_key(name)
    }

    /// Switch to a different profile by name.
    ///
    /// This method looks up the named profile, builds a new `Remapper` from it,
    /// and replaces the current remapper. The active profile name is also updated.
    ///
    /// # Arguments
    ///
    /// * `profile_name` - The name of the profile to switch to
    ///
    /// # Errors
    ///
    /// Returns `ProfileError` if the profile doesn't exist.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut device_remapper = DeviceRemapper::new(
    ///     "My Keyboard".to_string(),
    ///     profiles,
    ///     "default",
    /// )?;
    ///
    /// // Switch to a different profile
    /// device_remapper.switch_profile("gaming")?;
    /// assert_eq!(device_remapper.active_profile(), "gaming");
    /// ```
    pub fn switch_profile(&mut self, profile_name: &str) -> Result<(), ProfileError> {
        // 1. Validate profile exists in self.profiles
        let profile = self.profiles.get(profile_name).ok_or_else(|| ProfileError {
            profile: profile_name.to_string(),
            reason: format!(
                "profile not found in device '{}' (available profiles: {})",
                self.device_name,
                self.profiles
                    .keys()
                    .map(|s| format!("'{}'", s))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;

        // 2. Build a new Remapper from the profile
        let new_remapper = Remapper::from_profile(profile);

        // 3. Update active_profile
        self.active_profile = profile_name.to_string();

        // 4. Replace self.remapper with the new one
        self.remapper = new_remapper;

        Ok(())
    }

    /// Process an input event through the remapper (Task 030-3.3.3).
    ///
    /// This method first checks if the incoming key event matches a profile switch
    /// keybind. If so, it returns `RemapResult::ProfileSwitch` and the caller is
    /// responsible for actually switching the profile.
    ///
    /// If no profile switch keybind matches, the event is processed through the
    /// inner `Remapper` as usual.
    ///
    /// # Profile Switch Detection
    ///
    /// Profile switch combos are checked on key press events only (value == 1).
    /// When the held modifiers and the pressed key match a registered profile
    /// switch combo, the keybind is consumed (no events are emitted) and
    /// `RemapResult::ProfileSwitch` is returned.
    ///
    /// # Arguments
    ///
    /// * `event` - The input event to process
    ///
    /// # Returns
    ///
    /// * `RemapResult::ProfileSwitch(profile_name)` - A profile switch was triggered
    /// * `RemapResult::Events(events)` - Normal remapped/passthrough events
    ///
    /// # Example
    ///
    /// ```ignore
    /// match device_remapper.process(event) {
    ///     RemapResult::ProfileSwitch(profile_name) => {
    ///         device_remapper.switch_profile(&profile_name)?;
    ///     }
    ///     RemapResult::Events(events) => {
    ///         virtual_device.emit(&events)?;
    ///     }
    ///     RemapResult::Macro(actions) => {
    ///         executor.execute_macro(&actions).await?;
    ///     }
    /// }
    /// ```
    pub fn process(&mut self, event: InputEvent) -> RemapResult {
        // Only check for profile switch on key events
        if event.event_type() == evdev::EventType::KEY {
            let key = Key::new(event.code());
            let value = event.value();

            // Update the remapper's modifier tracking for every key event
            // This ensures we have accurate modifier state for combo matching
            self.remapper.update_held_modifiers(key, value);

            // Check for profile switch combo on key press only
            if value == event_value::PRESS {
                // Check if current key + held modifiers match any profile switch combo
                let held_mods = self.remapper.held_modifiers().clone();

                for (combo, profile_name) in &self.profile_switch_combos {
                    if combo.key == key && combo.modifiers == held_mods {
                        tracing::info!(
                            "Profile switch keybind detected: {} -> '{}'",
                            combo,
                            profile_name
                        );
                        return RemapResult::ProfileSwitch(profile_name.clone());
                    }
                }
            }
        }

        // No profile switch match, delegate to inner Remapper
        // Remapper::process() now returns RemapResult directly
        self.remapper.process(event)
    }
}

/// Parse a key name string to an evdev Key
pub(crate) fn parse_key(name: &str) -> Option<Key> {
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

    /// Helper to extract events from RemapResult, panicking if not Events variant
    fn expect_events(result: RemapResult) -> Vec<InputEvent> {
        match result {
            RemapResult::Events(e) => e,
            RemapResult::Macro(actions) => panic!("Expected RemapResult::Events, got Macro({} actions)", actions.len()),
            RemapResult::ProfileSwitch(name) => panic!("Expected RemapResult::Events, got ProfileSwitch({})", name),
        }
    }

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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_PRESS: i32 = 1;
        let press_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_CAPSLOCK.code(), KEY_PRESS);
        let result = remapper.process(press_event);

        let events = match result {
            RemapResult::Events(e) => e,
            _ => panic!("Expected RemapResult::Events"),
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_ESC.code(), "Key should be remapped from CapsLock to Escape");
        assert_eq!(events[0].value(), KEY_PRESS, "Press event value (1) should be preserved");
        assert_eq!(events[0].event_type(), evdev::EventType::KEY, "Event type should remain KEY");
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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_RELEASE: i32 = 0;
        let release_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_CAPSLOCK.code(), KEY_RELEASE);
        let result = remapper.process(release_event);

        let events = match result {
            RemapResult::Events(e) => e,
            _ => panic!("Expected RemapResult::Events"),
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_ESC.code(), "Key should be remapped from CapsLock to Escape");
        assert_eq!(events[0].value(), KEY_RELEASE, "Release event value (0) should be preserved");
        assert_eq!(events[0].event_type(), evdev::EventType::KEY, "Event type should remain KEY");
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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_RELEASE: i32 = 0;

        // Test press event passthrough for unmapped key
        let press_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_PRESS);
        let result = remapper.process(press_event);
        let events = match result {
            RemapResult::Events(e) => e,
            _ => panic!("Expected RemapResult::Events"),
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_A.code(), "Unmapped key should pass through unchanged");
        assert_eq!(events[0].value(), KEY_PRESS, "Press event value should be preserved");
        assert_eq!(events[0].event_type(), evdev::EventType::KEY, "Event type should remain KEY");

        // Test release event passthrough for unmapped key
        let release_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_RELEASE);
        let result = remapper.process(release_event);
        let events = match result {
            RemapResult::Events(e) => e,
            _ => panic!("Expected RemapResult::Events"),
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_A.code(), "Unmapped key should pass through unchanged");
        assert_eq!(events[0].value(), KEY_RELEASE, "Release event value should be preserved");
        assert_eq!(events[0].event_type(), evdev::EventType::KEY, "Event type should remain KEY");
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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        // Event value constants
        const KEY_RELEASE: i32 = 0;
        const KEY_PRESS: i32 = 1;
        const KEY_REPEAT: i32 = 2;

        // Helper to extract events from RemapResult
        let get_events = |r: RemapResult| -> Vec<InputEvent> {
            match r {
                RemapResult::Events(e) => e,
                _ => panic!("Expected RemapResult::Events"),
            }
        };

        // Test press event (value=1) is preserved
        let press_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_PRESS);
        let events = get_events(remapper.process(press_event));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_B.code(), "Key should be remapped from A to B");
        assert_eq!(events[0].value(), KEY_PRESS, "Press event value (1) should be preserved");

        // Test release event (value=0) is preserved
        let release_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_RELEASE);
        let events = get_events(remapper.process(release_event));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_B.code(), "Key should be remapped from A to B");
        assert_eq!(events[0].value(), KEY_RELEASE, "Release event value (0) should be preserved");

        // Test repeat event (value=2) is preserved
        let repeat_event = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_REPEAT);
        let events = get_events(remapper.process(repeat_event));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_B.code(), "Key should be remapped from A to B");
        assert_eq!(events[0].value(), KEY_REPEAT, "Repeat event value (2) should be preserved");
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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_PRESS: i32 = 1;

        // Initially no modifiers held
        assert!(remapper.held_modifiers().is_empty());

        // Press left Ctrl
        let ctrl_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS);
        let _ = remapper.process(ctrl_press);
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl), "Ctrl should be tracked after press");
        assert_eq!(remapper.held_modifiers().len(), 1);

        // Press left Shift
        let shift_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), KEY_PRESS);
        let _ = remapper.process(shift_press);
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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_RELEASE: i32 = 0;

        // Press Ctrl and Shift
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), KEY_PRESS));
        assert_eq!(remapper.held_modifiers().len(), 2);

        // Release Ctrl
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_RELEASE));
        assert!(!remapper.held_modifiers().contains(&Modifier::Ctrl), "Ctrl should be removed after release");
        assert!(remapper.held_modifiers().contains(&Modifier::Shift), "Shift should still be tracked");
        assert_eq!(remapper.held_modifiers().len(), 1);

        // Release Shift
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), KEY_RELEASE));
        assert!(remapper.held_modifiers().is_empty(), "No modifiers should remain after all released");
    }

    #[test]
    fn test_modifier_tracking_repeat_no_duplicate() {
        // Task 020-2.2: Works correctly with repeat events (should not add duplicates)
        let mut remapper = Remapper {
            remap: HashMap::new(),
            passthrough: Vec::new(),
            held_modifiers: HashSet::new(),
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_REPEAT: i32 = 2;

        // Press Ctrl
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        assert_eq!(remapper.held_modifiers().len(), 1);

        // Multiple repeat events should not add duplicates
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_REPEAT));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_REPEAT));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_REPEAT));
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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_PRESS: i32 = 1;

        // Press all modifiers
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), KEY_PRESS));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTALT.code(), KEY_PRESS));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTMETA.code(), KEY_PRESS));

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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_RELEASE: i32 = 0;

        // Press left Ctrl
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl));
        assert_eq!(remapper.held_modifiers().len(), 1);

        // Press right Ctrl - should NOT add a duplicate since both normalize to Modifier::Ctrl
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_RIGHTCTRL.code(), KEY_PRESS));
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl));
        assert_eq!(remapper.held_modifiers().len(), 1, "Left and right Ctrl should normalize to same modifier");

        // Release left Ctrl
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_RELEASE));
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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_PRESS: i32 = 1;
        const KEY_RELEASE: i32 = 0;

        // Press some non-modifier keys
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_PRESS));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_B.code(), KEY_PRESS));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_SPACE.code(), KEY_PRESS));

        assert!(remapper.held_modifiers().is_empty(), "Non-modifier keys should not be tracked");

        // Release them
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), KEY_RELEASE));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_B.code(), KEY_RELEASE));

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
            combo_tracker: ComboTracker::new(),
            macro_triggers: HashMap::new(),
        };

        const KEY_PRESS: i32 = 1;

        // Press Ctrl (modifier) and CapsLock (remapped)
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), KEY_PRESS));
        let result = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_CAPSLOCK.code(), KEY_PRESS));

        // Modifier should be tracked
        assert!(remapper.held_modifiers().contains(&Modifier::Ctrl));

        // Remapping should still work
        let events = match result {
            RemapResult::Events(e) => e,
            _ => panic!("Expected RemapResult::Events"),
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_ESC.code());
    }

    // ========================================================================
    // Combo Matching Tests (Task 020-2.4)
    // ========================================================================

    #[test]
    fn test_combo_match_exact_modifiers() {
        // Task 020-2.4: Ctrl+Shift+Q matches when exactly Ctrl, Shift held and Q pressed
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Press Ctrl, then Shift
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);

        // Check combo match with Q
        let result = tracker.check_combo_match(Key::KEY_Q);
        match result {
            ComboMatchResult::Matched { input, output } => {
                assert_eq!(input.key, Key::KEY_Q);
                assert!(input.modifiers.contains(&Modifier::Ctrl));
                assert!(input.modifiers.contains(&Modifier::Shift));
                assert_eq!(input.modifiers.len(), 2);

                assert_eq!(output.key, Key::KEY_F4);
                assert!(output.modifiers.contains(&Modifier::Alt));
                assert_eq!(output.modifiers.len(), 1);
            }
            ComboMatchResult::NoMatch => {
                panic!("Expected combo match for Ctrl+Shift+Q");
            }
        }
    }

    #[test]
    fn test_combo_no_match_extra_modifiers() {
        // Task 020-2.4: No match if extra modifiers are held
        // Ctrl+Shift+Alt+Q should NOT match Ctrl+Shift+Q
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Press Ctrl, Shift, AND Alt (extra modifier)
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTALT, 1);

        // Should NOT match because Alt is an extra modifier
        let result = tracker.check_combo_match(Key::KEY_Q);
        assert_eq!(result, ComboMatchResult::NoMatch, "Should not match with extra modifiers");
    }

    #[test]
    fn test_combo_no_match_missing_modifiers() {
        // Ctrl+Q should NOT match Ctrl+Shift+Q (missing Shift)
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Press only Ctrl (missing Shift)
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);

        // Should NOT match because Shift is missing
        let result = tracker.check_combo_match(Key::KEY_Q);
        assert_eq!(result, ComboMatchResult::NoMatch, "Should not match with missing modifiers");
    }

    #[test]
    fn test_combo_no_match_wrong_key() {
        // Ctrl+Shift+W should NOT match Ctrl+Shift+Q combo
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Press Ctrl and Shift
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);

        // Press W instead of Q
        let result = tracker.check_combo_match(Key::KEY_W);
        assert_eq!(result, ComboMatchResult::NoMatch, "Should not match with wrong trigger key");
    }

    #[test]
    fn test_combo_no_match_modifier_key() {
        // Pressing a modifier key should never trigger a combo match
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Press Ctrl
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);

        // Checking for combo match with Shift (a modifier) should not match
        let result = tracker.check_combo_match(Key::KEY_LEFTSHIFT);
        assert_eq!(result, ComboMatchResult::NoMatch, "Modifier keys should not trigger combos");
    }

    #[test]
    fn test_combo_match_no_modifiers() {
        // A combo with no modifiers (just a key) should match when no modifiers are held
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Escape").expect("should parse"),
            parse_combo("CapsLock").expect("should parse"),
        );

        // No modifiers held
        assert!(tracker.held_modifiers.is_empty());

        // Check combo match with Escape
        let result = tracker.check_combo_match(Key::KEY_ESC);
        match result {
            ComboMatchResult::Matched { input, output } => {
                assert_eq!(input.key, Key::KEY_ESC);
                assert!(input.modifiers.is_empty());
                assert_eq!(output.key, Key::KEY_CAPSLOCK);
            }
            ComboMatchResult::NoMatch => {
                panic!("Expected combo match for Escape");
            }
        }
    }

    #[test]
    fn test_combo_no_match_when_modifiers_held_for_plain_key() {
        // A combo with no modifiers should NOT match when modifiers are held
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Escape").expect("should parse"),
            parse_combo("CapsLock").expect("should parse"),
        );

        // Press Ctrl
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);

        // Should NOT match because Ctrl is held but combo expects no modifiers
        let result = tracker.check_combo_match(Key::KEY_ESC);
        assert_eq!(result, ComboMatchResult::NoMatch, "Should not match when modifiers are held for plain key combo");
    }

    #[test]
    fn test_combo_match_multiple_combos() {
        // Test with multiple registered combos
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Super+Q").expect("should parse"),
        );

        // Test Ctrl+Q match
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        let result = tracker.check_combo_match(Key::KEY_Q);
        match result {
            ComboMatchResult::Matched { output, .. } => {
                assert_eq!(output.key, Key::KEY_F4);
                assert!(output.modifiers.contains(&Modifier::Alt));
            }
            ComboMatchResult::NoMatch => {
                panic!("Expected Ctrl+Q to match");
            }
        }

        // Now add Shift and test Ctrl+Shift+Q
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);
        let result = tracker.check_combo_match(Key::KEY_Q);
        match result {
            ComboMatchResult::Matched { output, .. } => {
                assert_eq!(output.key, Key::KEY_Q);
                assert!(output.modifiers.contains(&Modifier::Super));
            }
            ComboMatchResult::NoMatch => {
                panic!("Expected Ctrl+Shift+Q to match");
            }
        }
    }

    #[test]
    fn test_combo_tracker_state_transitions() {
        // Test that state transitions work correctly
        let mut tracker = ComboTracker::new();

        // Should start in Idle
        assert_eq!(tracker.state, ComboState::Idle);

        // Press a modifier -> should transition to ModifiersHeld
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        assert_eq!(tracker.state, ComboState::ModifiersHeld);

        // Add another modifier -> should stay in ModifiersHeld
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);
        assert_eq!(tracker.state, ComboState::ModifiersHeld);

        // Release one modifier -> should stay in ModifiersHeld (still have one)
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 0);
        assert_eq!(tracker.state, ComboState::ModifiersHeld);

        // Release last modifier -> should transition to Idle
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 0);
        assert_eq!(tracker.state, ComboState::Idle);
    }

    #[test]
    fn test_combo_handle_key_press() {
        // Test the convenience method handle_key_press
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Press Ctrl using handle_key_press
        let result = tracker.handle_key_press(Key::KEY_LEFTCTRL);
        assert_eq!(result, ComboMatchResult::NoMatch); // Modifier press doesn't trigger combo
        assert!(tracker.held_modifiers.contains(&Modifier::Ctrl));

        // Press Q using handle_key_press
        let result = tracker.handle_key_press(Key::KEY_Q);
        assert!(matches!(result, ComboMatchResult::Matched { .. }));
    }

    #[test]
    fn test_combo_handle_key_release() {
        // Test the handle_key_release method
        let mut tracker = ComboTracker::new();

        // Press Ctrl
        tracker.handle_key_press(Key::KEY_LEFTCTRL);
        assert!(tracker.held_modifiers.contains(&Modifier::Ctrl));

        // Release Ctrl
        tracker.handle_key_release(Key::KEY_LEFTCTRL);
        assert!(!tracker.held_modifiers.contains(&Modifier::Ctrl));
        assert_eq!(tracker.state, ComboState::Idle);
    }

    #[test]
    fn test_combo_match_right_modifiers() {
        // Test that right-hand modifiers also work for matching
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Press RIGHT Ctrl and RIGHT Shift
        tracker.update_held_modifiers(Key::KEY_RIGHTCTRL, 1);
        tracker.update_held_modifiers(Key::KEY_RIGHTSHIFT, 1);

        // Should still match because left/right are normalized
        let result = tracker.check_combo_match(Key::KEY_Q);
        assert!(matches!(result, ComboMatchResult::Matched { .. }));
    }

    #[test]
    fn test_combo_match_all_four_modifiers() {
        // Test combo with all four modifiers
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Alt+Super+Q").expect("should parse"),
            parse_combo("F1").expect("should parse"),
        );

        // Press all four modifiers
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTALT, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTMETA, 1);

        // Should match
        let result = tracker.check_combo_match(Key::KEY_Q);
        match result {
            ComboMatchResult::Matched { input, output } => {
                assert_eq!(input.modifiers.len(), 4);
                assert_eq!(output.key, Key::KEY_F1);
            }
            ComboMatchResult::NoMatch => {
                panic!("Expected combo match for Ctrl+Shift+Alt+Super+Q");
            }
        }
    }

    #[test]
    fn test_combo_no_registered_combos() {
        // Test behavior when no combos are registered
        let mut tracker = ComboTracker::new();

        // Press Ctrl
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);

        // No combo should match
        let result = tracker.check_combo_match(Key::KEY_Q);
        assert_eq!(result, ComboMatchResult::NoMatch);
    }

    // ========================================================================
    // Combo Output Injection Tests (Task 020-2.5)
    // ========================================================================

    #[test]
    fn test_generate_combo_press_events_ctrl_shift_q_to_alt_f4() {
        // Task 020-2.5: Ctrl+Shift+Q -> Alt+F4 injects correct event sequence
        //
        // Input: Ctrl+Shift held, Q pressed
        // Output: Alt+F4
        //
        // Expected sequence:
        // 1. Release Ctrl (not in output)
        // 2. Release Shift (not in output)
        // 3. Press Alt (new modifier)
        // 4. Press F4 (output key)
        use super::{generate_combo_press_events, event_value};

        let input_modifiers: HashSet<Modifier> = [Modifier::Ctrl, Modifier::Shift].into_iter().collect();
        let output_combo = parse_combo("Alt+F4").expect("should parse");

        let events = generate_combo_press_events(&input_modifiers, &output_combo);

        // Should have 4 events: release Ctrl, release Shift, press Alt, press F4
        assert_eq!(events.len(), 4, "Expected 4 events for Ctrl+Shift+Q -> Alt+F4");

        // Event 0: Release Ctrl (Ctrl < Shift in sort order)
        assert_eq!(events[0].code(), Key::KEY_LEFTCTRL.code());
        assert_eq!(events[0].value(), event_value::RELEASE);

        // Event 1: Release Shift
        assert_eq!(events[1].code(), Key::KEY_LEFTSHIFT.code());
        assert_eq!(events[1].value(), event_value::RELEASE);

        // Event 2: Press Alt
        assert_eq!(events[2].code(), Key::KEY_LEFTALT.code());
        assert_eq!(events[2].value(), event_value::PRESS);

        // Event 3: Press F4
        assert_eq!(events[3].code(), Key::KEY_F4.code());
        assert_eq!(events[3].value(), event_value::PRESS);
    }

    #[test]
    fn test_generate_combo_release_events_ctrl_shift_q_to_alt_f4() {
        // Task 020-2.5: Test release sequence for Ctrl+Shift+Q -> Alt+F4
        //
        // When Q is released (Ctrl+Shift still physically held):
        // 1. Release F4
        // 2. Release Alt
        // 3. Restore Ctrl (still held)
        // 4. Restore Shift (still held)
        use super::{generate_combo_release_events, event_value};

        let input_modifiers: HashSet<Modifier> = [Modifier::Ctrl, Modifier::Shift].into_iter().collect();
        let output_combo = parse_combo("Alt+F4").expect("should parse");
        let still_held: HashSet<Modifier> = [Modifier::Ctrl, Modifier::Shift].into_iter().collect();

        let events = generate_combo_release_events(&input_modifiers, &output_combo, &still_held);

        // Should have 4 events: release F4, release Alt, press Ctrl, press Shift
        assert_eq!(events.len(), 4, "Expected 4 events for release sequence");

        // Event 0: Release F4
        assert_eq!(events[0].code(), Key::KEY_F4.code());
        assert_eq!(events[0].value(), event_value::RELEASE);

        // Event 1: Release Alt
        assert_eq!(events[1].code(), Key::KEY_LEFTALT.code());
        assert_eq!(events[1].value(), event_value::RELEASE);

        // Event 2: Restore Ctrl
        assert_eq!(events[2].code(), Key::KEY_LEFTCTRL.code());
        assert_eq!(events[2].value(), event_value::PRESS);

        // Event 3: Restore Shift
        assert_eq!(events[3].code(), Key::KEY_LEFTSHIFT.code());
        assert_eq!(events[3].value(), event_value::PRESS);
    }

    #[test]
    fn test_generate_combo_press_events_overlapping_modifiers() {
        // Test case where input and output share some modifiers
        // Ctrl+Shift+Q -> Ctrl+F4
        // Ctrl is in both, so it should NOT be released or pressed
        use super::{generate_combo_press_events, event_value};

        let input_modifiers: HashSet<Modifier> = [Modifier::Ctrl, Modifier::Shift].into_iter().collect();
        let output_combo = parse_combo("Ctrl+F4").expect("should parse");

        let events = generate_combo_press_events(&input_modifiers, &output_combo);

        // Should have 2 events: release Shift (not in output), press F4
        // Ctrl is in both, so no action needed
        assert_eq!(events.len(), 2, "Expected 2 events for overlapping modifier case");

        // Event 0: Release Shift
        assert_eq!(events[0].code(), Key::KEY_LEFTSHIFT.code());
        assert_eq!(events[0].value(), event_value::RELEASE);

        // Event 1: Press F4
        assert_eq!(events[1].code(), Key::KEY_F4.code());
        assert_eq!(events[1].value(), event_value::PRESS);
    }

    #[test]
    fn test_generate_combo_press_events_no_modifiers_to_no_modifiers() {
        // Simple key remap: Escape -> CapsLock (no modifiers)
        use super::{generate_combo_press_events, event_value};

        let input_modifiers: HashSet<Modifier> = HashSet::new();
        let output_combo = parse_combo("CapsLock").expect("should parse");

        let events = generate_combo_press_events(&input_modifiers, &output_combo);

        // Should have 1 event: press CapsLock
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_CAPSLOCK.code());
        assert_eq!(events[0].value(), event_value::PRESS);
    }

    #[test]
    fn test_generate_combo_release_events_modifiers_no_longer_held() {
        // Test release when input modifiers are no longer physically held
        // Ctrl+Shift+Q -> Alt+F4, but user released Ctrl+Shift before Q
        use super::{generate_combo_release_events, event_value};

        let input_modifiers: HashSet<Modifier> = [Modifier::Ctrl, Modifier::Shift].into_iter().collect();
        let output_combo = parse_combo("Alt+F4").expect("should parse");
        let still_held: HashSet<Modifier> = HashSet::new(); // User released everything

        let events = generate_combo_release_events(&input_modifiers, &output_combo, &still_held);

        // Should have 2 events: release F4, release Alt
        // No restoration needed since nothing is still held
        assert_eq!(events.len(), 2, "Expected 2 events when modifiers not still held");

        assert_eq!(events[0].code(), Key::KEY_F4.code());
        assert_eq!(events[0].value(), event_value::RELEASE);

        assert_eq!(events[1].code(), Key::KEY_LEFTALT.code());
        assert_eq!(events[1].value(), event_value::RELEASE);
    }

    #[test]
    fn test_generate_combo_press_events_same_modifiers_different_key() {
        // Ctrl+Q -> Ctrl+W (same modifiers, different key)
        use super::{generate_combo_press_events, event_value};

        let input_modifiers: HashSet<Modifier> = [Modifier::Ctrl].into_iter().collect();
        let output_combo = parse_combo("Ctrl+W").expect("should parse");

        let events = generate_combo_press_events(&input_modifiers, &output_combo);

        // Should have 1 event: just press W (Ctrl stays held)
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_W.code());
        assert_eq!(events[0].value(), event_value::PRESS);
    }

    #[test]
    fn test_generate_combo_full_sequence_ctrl_shift_q_to_alt_f4() {
        // Task 020-2.5: Complete integration test showing full event sequence
        // This demonstrates the complete flow: press events followed by release events
        use super::{generate_combo_press_events, generate_combo_release_events, event_value};

        // Setup: Ctrl+Shift+Q -> Alt+F4
        let input = parse_combo("Ctrl+Shift+Q").expect("should parse input");
        let output = parse_combo("Alt+F4").expect("should parse output");
        let input_modifiers = input.modifiers.clone();

        // Simulate: User holds Ctrl+Shift, presses Q
        // The system should generate press events
        let press_events = generate_combo_press_events(&input_modifiers, &output);

        // Verify press sequence
        assert_eq!(press_events.len(), 4);
        // Release Ctrl, Release Shift, Press Alt, Press F4
        assert_eq!(press_events[0].value(), event_value::RELEASE); // Ctrl
        assert_eq!(press_events[1].value(), event_value::RELEASE); // Shift
        assert_eq!(press_events[2].value(), event_value::PRESS);   // Alt
        assert_eq!(press_events[3].value(), event_value::PRESS);   // F4

        // Simulate: User releases Q (Ctrl+Shift still held)
        let still_held = input_modifiers.clone();
        let release_events = generate_combo_release_events(&input_modifiers, &output, &still_held);

        // Verify release sequence
        assert_eq!(release_events.len(), 4);
        // Release F4, Release Alt, Press Ctrl, Press Shift
        assert_eq!(release_events[0].value(), event_value::RELEASE); // F4
        assert_eq!(release_events[1].value(), event_value::RELEASE); // Alt
        assert_eq!(release_events[2].value(), event_value::PRESS);   // Ctrl (restore)
        assert_eq!(release_events[3].value(), event_value::PRESS);   // Shift (restore)

        // Verify the full sequence produces a balanced press/release for each synthetic key
        let all_events: Vec<_> = press_events.into_iter().chain(release_events).collect();

        // Count presses and releases for each key
        let mut press_count: HashMap<u16, i32> = HashMap::new();
        let mut release_count: HashMap<u16, i32> = HashMap::new();

        for event in &all_events {
            if event.value() == event_value::PRESS {
                *press_count.entry(event.code()).or_insert(0) += 1;
            } else if event.value() == event_value::RELEASE {
                *release_count.entry(event.code()).or_insert(0) += 1;
            }
        }

        // F4: 1 press, 1 release
        assert_eq!(press_count.get(&Key::KEY_F4.code()), Some(&1));
        assert_eq!(release_count.get(&Key::KEY_F4.code()), Some(&1));

        // Alt: 1 press, 1 release
        assert_eq!(press_count.get(&Key::KEY_LEFTALT.code()), Some(&1));
        assert_eq!(release_count.get(&Key::KEY_LEFTALT.code()), Some(&1));

        // Ctrl: 1 press (restore), 1 release (initial) - balanced
        assert_eq!(press_count.get(&Key::KEY_LEFTCTRL.code()), Some(&1));
        assert_eq!(release_count.get(&Key::KEY_LEFTCTRL.code()), Some(&1));

        // Shift: 1 press (restore), 1 release (initial) - balanced
        assert_eq!(press_count.get(&Key::KEY_LEFTSHIFT.code()), Some(&1));
        assert_eq!(release_count.get(&Key::KEY_LEFTSHIFT.code()), Some(&1));
    }

    // ========================================================================
    // Combo Release Handling Tests (Task 020-2.6)
    // ========================================================================

    #[test]
    fn test_combo_release_basic_ctrl_shift_q_to_alt_f4() {
        // Task 020-2.6: Test that releasing Q after combo releases F4, not Q
        //
        // Scenario:
        // 1. User holds Ctrl+Shift
        // 2. User presses Q (combo matches, Alt+F4 is injected)
        // 3. User releases Q (should release F4 and restore Ctrl+Shift)
        use super::event_value;

        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Step 1: Press Ctrl+Shift
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);

        // Step 2: Press Q and detect combo match
        let result = tracker.check_combo_match(Key::KEY_Q);
        match result {
            ComboMatchResult::Matched { input, output } => {
                // Activate the combo (simulate what the event loop would do)
                tracker.activate_combo(Key::KEY_Q, input.modifiers.clone(), output.clone());

                // Verify combo is now active
                assert!(tracker.has_active_combo_for(Key::KEY_Q));
                let active = tracker.get_active_combo().expect("should have active combo");
                assert_eq!(active.trigger_key, Key::KEY_Q);
                assert_eq!(active.output_combo.key, Key::KEY_F4);
            }
            _ => panic!("Expected combo match"),
        }

        // Step 3: Release Q and get release events
        let release_events = tracker.handle_trigger_release(Key::KEY_Q);

        // Verify release events were generated correctly
        assert_eq!(release_events.len(), 4, "Expected 4 release events: release F4, release Alt, restore Ctrl, restore Shift");

        // Event 0: Release F4 (the output key)
        assert_eq!(release_events[0].code(), Key::KEY_F4.code());
        assert_eq!(release_events[0].value(), event_value::RELEASE);

        // Event 1: Release Alt (output modifier not in input)
        assert_eq!(release_events[1].code(), Key::KEY_LEFTALT.code());
        assert_eq!(release_events[1].value(), event_value::RELEASE);

        // Event 2: Restore Ctrl (input modifier that was released)
        assert_eq!(release_events[2].code(), Key::KEY_LEFTCTRL.code());
        assert_eq!(release_events[2].value(), event_value::PRESS);

        // Event 3: Restore Shift (input modifier that was released)
        assert_eq!(release_events[3].code(), Key::KEY_LEFTSHIFT.code());
        assert_eq!(release_events[3].value(), event_value::PRESS);

        // Verify combo is no longer active
        assert!(!tracker.has_active_combo_for(Key::KEY_Q));
        assert!(tracker.active_combo.is_none());

        // State should transition back to ModifiersHeld (Ctrl+Shift still held)
        assert_eq!(tracker.state, ComboState::ModifiersHeld);
    }

    #[test]
    fn test_combo_release_modifiers_already_released() {
        // Task 020-2.6: Test release when modifiers were released before trigger key
        //
        // Scenario:
        // 1. User holds Ctrl+Shift, presses Q (combo matches)
        // 2. User releases Ctrl and Shift (before releasing Q)
        // 3. User releases Q (should still release F4, but no modifier restoration)
        use super::event_value;

        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Press Ctrl+Shift+Q and activate combo
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);

        let result = tracker.check_combo_match(Key::KEY_Q);
        if let ComboMatchResult::Matched { input, output } = result {
            tracker.activate_combo(Key::KEY_Q, input.modifiers.clone(), output);
        }

        // Release modifiers before releasing Q
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 0);
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 0);
        assert!(tracker.held_modifiers.is_empty(), "No modifiers should be held");

        // Release Q
        let release_events = tracker.handle_trigger_release(Key::KEY_Q);

        // Should only release F4 and Alt (no restoration since modifiers not held)
        assert_eq!(release_events.len(), 2, "Expected 2 events: release F4, release Alt");
        assert_eq!(release_events[0].code(), Key::KEY_F4.code());
        assert_eq!(release_events[0].value(), event_value::RELEASE);
        assert_eq!(release_events[1].code(), Key::KEY_LEFTALT.code());
        assert_eq!(release_events[1].value(), event_value::RELEASE);

        // State should transition to Idle (no modifiers held)
        assert_eq!(tracker.state, ComboState::Idle);
    }

    #[test]
    fn test_combo_release_no_active_combo() {
        // Test that releasing a key with no active combo returns empty events
        let mut tracker = ComboTracker::new();

        // No combo registered or active
        let release_events = tracker.handle_trigger_release(Key::KEY_Q);

        // Should return empty vector
        assert!(release_events.is_empty(), "Should return no events when no active combo");
    }

    #[test]
    fn test_combo_release_wrong_key() {
        // Test that releasing a different key doesn't affect active combo
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Activate combo with Q
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        if let ComboMatchResult::Matched { input, output } = tracker.check_combo_match(Key::KEY_Q) {
            tracker.activate_combo(Key::KEY_Q, input.modifiers, output);
        }
        assert!(tracker.has_active_combo_for(Key::KEY_Q));

        // Release W (not Q)
        let release_events = tracker.handle_trigger_release(Key::KEY_W);

        // Should return no events since W is not the trigger
        assert!(release_events.is_empty(), "Should return no events for wrong key");

        // Active combo should still be set
        assert!(tracker.has_active_combo_for(Key::KEY_Q), "Combo should still be active");
    }

    #[test]
    fn test_combo_activate_and_clear() {
        // Test activate_combo and clear_active_combo methods
        let mut tracker = ComboTracker::new();
        let output = parse_combo("Alt+F4").expect("should parse");
        let input_modifiers: HashSet<Modifier> = [Modifier::Ctrl].into_iter().collect();

        // Initially no active combo
        assert!(tracker.get_active_combo().is_none());

        // Activate combo
        tracker.activate_combo(Key::KEY_Q, input_modifiers.clone(), output.clone());

        // Verify active combo
        let active = tracker.get_active_combo().expect("should have active combo");
        assert_eq!(active.trigger_key, Key::KEY_Q);
        assert_eq!(active.input_modifiers, input_modifiers);
        assert_eq!(active.output_combo, output);

        // Clear active combo
        tracker.clear_active_combo();
        assert!(tracker.get_active_combo().is_none());
        assert!(!tracker.has_active_combo_for(Key::KEY_Q));
    }

    #[test]
    fn test_combo_release_same_modifiers_different_key() {
        // Test: Ctrl+Q -> Ctrl+W (same modifiers, different key)
        // Releasing Q should release W (not Q)
        use super::event_value;

        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Q").expect("should parse"),
            parse_combo("Ctrl+W").expect("should parse"),
        );

        // Press Ctrl+Q and activate combo
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        if let ComboMatchResult::Matched { input, output } = tracker.check_combo_match(Key::KEY_Q) {
            tracker.activate_combo(Key::KEY_Q, input.modifiers, output);
        }

        // Release Q
        let release_events = tracker.handle_trigger_release(Key::KEY_Q);

        // Should only release W (Ctrl is in both input and output, so no change needed)
        assert_eq!(release_events.len(), 1, "Expected 1 event: release W");
        assert_eq!(release_events[0].code(), Key::KEY_W.code());
        assert_eq!(release_events[0].value(), event_value::RELEASE);
    }

    #[test]
    fn test_combo_release_state_transition_to_idle() {
        // Test that state transitions to Idle when all modifiers are released
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Activate combo
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        if let ComboMatchResult::Matched { input, output } = tracker.check_combo_match(Key::KEY_Q) {
            tracker.activate_combo(Key::KEY_Q, input.modifiers, output);
        }

        // Release Ctrl first (before Q)
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 0);

        // Release Q - should transition to Idle since no modifiers held
        let _ = tracker.handle_trigger_release(Key::KEY_Q);
        assert_eq!(tracker.state, ComboState::Idle);
    }

    #[test]
    fn test_combo_release_state_transition_to_modifiers_held() {
        // Test that state transitions to ModifiersHeld when some modifiers still held
        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // Activate combo with Ctrl+Shift
        tracker.update_held_modifiers(Key::KEY_LEFTCTRL, 1);
        tracker.update_held_modifiers(Key::KEY_LEFTSHIFT, 1);
        if let ComboMatchResult::Matched { input, output } = tracker.check_combo_match(Key::KEY_Q) {
            tracker.activate_combo(Key::KEY_Q, input.modifiers, output);
        }

        // Release Q - Ctrl+Shift still held, should transition to ModifiersHeld
        let _ = tracker.handle_trigger_release(Key::KEY_Q);
        assert_eq!(tracker.state, ComboState::ModifiersHeld);
        assert!(tracker.held_modifiers.contains(&Modifier::Ctrl));
        assert!(tracker.held_modifiers.contains(&Modifier::Shift));
    }

    #[test]
    fn test_combo_release_full_integration() {
        // Task 020-2.6: Full integration test demonstrating correct release behavior
        // This test simulates the complete user interaction flow
        use super::event_value;

        let mut tracker = ComboTracker::new();
        tracker.register_combo(
            parse_combo("Ctrl+Shift+Q").expect("should parse"),
            parse_combo("Alt+F4").expect("should parse"),
        );

        // === User presses Ctrl ===
        tracker.handle_key_press(Key::KEY_LEFTCTRL);
        assert_eq!(tracker.state, ComboState::ModifiersHeld);

        // === User presses Shift ===
        tracker.handle_key_press(Key::KEY_LEFTSHIFT);
        assert_eq!(tracker.state, ComboState::ModifiersHeld);

        // === User presses Q - combo matches ===
        let press_result = tracker.handle_key_press(Key::KEY_Q);
        match press_result {
            ComboMatchResult::Matched { input, output } => {
                // In real usage, the event loop would:
                // 1. Call generate_combo_press_events() to get press events
                // 2. Inject those events
                // 3. Call activate_combo() to track the active combo
                tracker.activate_combo(Key::KEY_Q, input.modifiers.clone(), output.clone());

                // Verify the expected press events
                let press_events = generate_combo_press_events(&input.modifiers, &output);
                assert_eq!(press_events.len(), 4);
                // Release Ctrl, Release Shift, Press Alt, Press F4
            }
            ComboMatchResult::NoMatch => panic!("Should have matched"),
        }

        // === User releases Q ===
        let release_events = tracker.handle_trigger_release(Key::KEY_Q);

        // Key insight from task: Releasing Q should release F4, not Q
        assert_eq!(release_events.len(), 4);

        // Verify F4 is released (the output key), NOT Q
        assert_eq!(release_events[0].code(), Key::KEY_F4.code(), "Should release F4, not Q!");
        assert_eq!(release_events[0].value(), event_value::RELEASE);

        // Verify Alt is released (output modifier)
        assert_eq!(release_events[1].code(), Key::KEY_LEFTALT.code());
        assert_eq!(release_events[1].value(), event_value::RELEASE);

        // Verify Ctrl and Shift are restored (since they're still physically held)
        assert_eq!(release_events[2].code(), Key::KEY_LEFTCTRL.code());
        assert_eq!(release_events[2].value(), event_value::PRESS);
        assert_eq!(release_events[3].code(), Key::KEY_LEFTSHIFT.code());
        assert_eq!(release_events[3].value(), event_value::PRESS);

        // State should be ModifiersHeld (Ctrl+Shift still held)
        assert_eq!(tracker.state, ComboState::ModifiersHeld);
        assert!(!tracker.has_active_combo_for(Key::KEY_Q), "Combo should no longer be active");
    }

    // ========================================================================
    // Remapper Combo Integration Tests (Task 020-2.7)
    // ========================================================================

    #[test]
    fn test_remapper_from_profile_loads_combos() {
        // Task 020-2.7: Test that combos are loaded from profile
        use niri_mapper_config::Profile;

        let mut profile = Profile::default();
        profile.combo.insert("Ctrl+Q".to_string(), "Alt+F4".to_string());
        profile.combo.insert("Ctrl+Shift+A".to_string(), "Super+A".to_string());

        let remapper = Remapper::from_profile(&profile);

        // Verify combos were loaded
        assert_eq!(remapper.combo_tracker.combos.len(), 2);
    }

    #[test]
    fn test_remapper_from_profile_invalid_combo_ignored() {
        // Task 020-2.7: Invalid combos should be skipped with warning
        use niri_mapper_config::Profile;

        let mut profile = Profile::default();
        profile.combo.insert("Ctrl+Q".to_string(), "Alt+F4".to_string());
        profile.combo.insert("InvalidKey+Q".to_string(), "Alt+F4".to_string()); // Invalid
        profile.combo.insert("Ctrl+A".to_string(), "UnknownOutput".to_string()); // Invalid

        let remapper = Remapper::from_profile(&profile);

        // Only valid combo should be loaded
        assert_eq!(remapper.combo_tracker.combos.len(), 1);
    }

    #[test]
    fn test_remapper_process_combo_press() {
        // Task 020-2.7: Test combo detection in process()
        use niri_mapper_config::Profile;

        let mut profile = Profile::default();
        profile.combo.insert("Ctrl+Q".to_string(), "Alt+F4".to_string());

        let mut remapper = Remapper::from_profile(&profile);

        // Press Ctrl
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS));

        // Press Q - should trigger combo
        let events = expect_events(remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_Q.code(), event_value::PRESS)));

        // Should generate: Release Ctrl, Press Alt, Press F4
        assert_eq!(events.len(), 3, "Combo press should generate 3 events");

        // Event 0: Release Ctrl (input modifier not in output)
        assert_eq!(events[0].code(), Key::KEY_LEFTCTRL.code());
        assert_eq!(events[0].value(), event_value::RELEASE);

        // Event 1: Press Alt (output modifier)
        assert_eq!(events[1].code(), Key::KEY_LEFTALT.code());
        assert_eq!(events[1].value(), event_value::PRESS);

        // Event 2: Press F4 (output key)
        assert_eq!(events[2].code(), Key::KEY_F4.code());
        assert_eq!(events[2].value(), event_value::PRESS);
    }

    #[test]
    fn test_remapper_process_combo_release() {
        // Task 020-2.7: Test combo release in process()
        use niri_mapper_config::Profile;

        let mut profile = Profile::default();
        profile.combo.insert("Ctrl+Q".to_string(), "Alt+F4".to_string());

        let mut remapper = Remapper::from_profile(&profile);

        // Activate combo: Ctrl+Q
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_Q.code(), event_value::PRESS));

        // Release Q - should release F4 and restore Ctrl
        let events = expect_events(remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_Q.code(), event_value::RELEASE)));

        // Should generate: Release F4, Release Alt, Press Ctrl (restore)
        assert_eq!(events.len(), 3, "Combo release should generate 3 events");

        // Event 0: Release F4
        assert_eq!(events[0].code(), Key::KEY_F4.code());
        assert_eq!(events[0].value(), event_value::RELEASE);

        // Event 1: Release Alt
        assert_eq!(events[1].code(), Key::KEY_LEFTALT.code());
        assert_eq!(events[1].value(), event_value::RELEASE);

        // Event 2: Restore Ctrl (still held)
        assert_eq!(events[2].code(), Key::KEY_LEFTCTRL.code());
        assert_eq!(events[2].value(), event_value::PRESS);
    }

    #[test]
    fn test_remapper_process_combo_repeat() {
        // Task 020-2.7: Test that repeat events emit output key
        use niri_mapper_config::Profile;

        let mut profile = Profile::default();
        profile.combo.insert("Ctrl+Q".to_string(), "Alt+F4".to_string());

        let mut remapper = Remapper::from_profile(&profile);

        // Activate combo
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS));
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_Q.code(), event_value::PRESS));

        // Repeat Q - should repeat F4, not Q
        let events = expect_events(remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_Q.code(), event_value::REPEAT)));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_F4.code(), "Repeat should output F4, not Q");
        assert_eq!(events[0].value(), event_value::REPEAT);
    }

    #[test]
    fn test_remapper_process_combo_fallback_to_remap() {
        // Task 020-2.7: Test that simple remaps still work when no combo matches
        use niri_mapper_config::Profile;

        let mut profile = Profile::default();
        profile.remap.insert("CapsLock".to_string(), "Escape".to_string());
        profile.combo.insert("Ctrl+Q".to_string(), "Alt+F4".to_string());

        let mut remapper = Remapper::from_profile(&profile);

        // Press CapsLock (should use simple remap, not combo)
        let events = expect_events(remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_CAPSLOCK.code(), event_value::PRESS)));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_ESC.code(), "CapsLock should be remapped to Escape");
    }

    #[test]
    fn test_remapper_process_combo_priority_over_remap() {
        // Task 020-2.7: Combos should take priority over simple remaps
        use niri_mapper_config::Profile;

        let mut profile = Profile::default();
        profile.remap.insert("Q".to_string(), "W".to_string()); // Q -> W remap
        profile.combo.insert("Ctrl+Q".to_string(), "Alt+F4".to_string()); // Ctrl+Q combo

        let mut remapper = Remapper::from_profile(&profile);

        // Press Ctrl
        let _ = remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS));

        // Press Q - should trigger combo, not simple remap
        let events = expect_events(remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_Q.code(), event_value::PRESS)));

        // Should be combo output, not W
        assert!(events.len() > 1, "Should trigger combo, not simple remap");
        // Last event should be F4 (combo output), not W
        assert_eq!(events.last().unwrap().code(), Key::KEY_F4.code());
    }

    #[test]
    fn test_remapper_process_no_combo_uses_remap() {
        // Task 020-2.7: When no combo matches, simple remap should be used
        use niri_mapper_config::Profile;

        let mut profile = Profile::default();
        profile.remap.insert("Q".to_string(), "W".to_string()); // Q -> W remap
        profile.combo.insert("Ctrl+Shift+Q".to_string(), "Alt+F4".to_string()); // Different combo

        let mut remapper = Remapper::from_profile(&profile);

        // Press Q without modifiers - should use simple remap
        let events = expect_events(remapper.process(InputEvent::new(evdev::EventType::KEY, Key::KEY_Q.code(), event_value::PRESS)));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_W.code(), "Q should be remapped to W when no combo matches");
    }

    // ========================================================================
    // Smoke Test: Combo Remapping (Task 020-2.8)
    // ========================================================================

    #[test]
    fn test_smoke_combo_remapping_ctrl_shift_q_to_alt_f4() {
        // Task 020-2.8: Smoke test for combo remapping
        //
        // This is an end-to-end test that verifies the complete combo remapping flow:
        // 1. Pressing modifiers (Ctrl, Shift) updates held_modifiers
        // 2. Pressing Q with those modifiers triggers the combo
        // 3. The output events contain Alt press, F4 press/release, Alt release
        //
        // Scenario: User presses Ctrl+Shift+Q to trigger Alt+F4
        use niri_mapper_config::Profile;

        let mut profile = Profile::default();
        profile.combo.insert("Ctrl+Shift+Q".to_string(), "Alt+F4".to_string());

        let mut remapper = Remapper::from_profile(&profile);

        // ====================================================================
        // Step 1: Press Ctrl - verify held_modifiers is updated
        // ====================================================================
        let events = expect_events(remapper.process(InputEvent::new(
            evdev::EventType::KEY,
            Key::KEY_LEFTCTRL.code(),
            event_value::PRESS,
        )));

        // Ctrl press should pass through (no combo match yet)
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_LEFTCTRL.code());
        assert_eq!(events[0].value(), event_value::PRESS);

        // Verify Ctrl is now tracked in held_modifiers
        assert!(
            remapper.held_modifiers().contains(&Modifier::Ctrl),
            "Ctrl should be tracked in held_modifiers after press"
        );

        // ====================================================================
        // Step 2: Press Shift - verify held_modifiers is updated
        // ====================================================================
        let events = expect_events(remapper.process(InputEvent::new(
            evdev::EventType::KEY,
            Key::KEY_LEFTSHIFT.code(),
            event_value::PRESS,
        )));

        // Shift press should pass through (no combo match yet)
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_LEFTSHIFT.code());
        assert_eq!(events[0].value(), event_value::PRESS);

        // Verify both Ctrl and Shift are now tracked
        assert!(
            remapper.held_modifiers().contains(&Modifier::Ctrl),
            "Ctrl should still be tracked"
        );
        assert!(
            remapper.held_modifiers().contains(&Modifier::Shift),
            "Shift should be tracked in held_modifiers after press"
        );
        assert_eq!(remapper.held_modifiers().len(), 2, "Should have exactly 2 modifiers");

        // ====================================================================
        // Step 3: Press Q - verify combo triggers and output events are correct
        // ====================================================================
        let events = expect_events(remapper.process(InputEvent::new(
            evdev::EventType::KEY,
            Key::KEY_Q.code(),
            event_value::PRESS,
        )));

        // Combo should trigger: Ctrl+Shift+Q -> Alt+F4
        // Expected events:
        // 1. Release Ctrl (input modifier not in output)
        // 2. Release Shift (input modifier not in output)
        // 3. Press Alt (output modifier)
        // 4. Press F4 (output key)
        assert_eq!(events.len(), 4, "Combo should generate 4 events");

        // Event 0: Release Ctrl
        assert_eq!(events[0].code(), Key::KEY_LEFTCTRL.code(), "First event should release Ctrl");
        assert_eq!(events[0].value(), event_value::RELEASE);

        // Event 1: Release Shift
        assert_eq!(events[1].code(), Key::KEY_LEFTSHIFT.code(), "Second event should release Shift");
        assert_eq!(events[1].value(), event_value::RELEASE);

        // Event 2: Press Alt
        assert_eq!(events[2].code(), Key::KEY_LEFTALT.code(), "Third event should press Alt");
        assert_eq!(events[2].value(), event_value::PRESS);

        // Event 3: Press F4
        assert_eq!(events[3].code(), Key::KEY_F4.code(), "Fourth event should press F4");
        assert_eq!(events[3].value(), event_value::PRESS);

        // ====================================================================
        // Step 4: Release Q - verify release sequence is correct
        // ====================================================================
        let events = expect_events(remapper.process(InputEvent::new(
            evdev::EventType::KEY,
            Key::KEY_Q.code(),
            event_value::RELEASE,
        )));

        // Release sequence for Q (combo trigger):
        // 1. Release F4 (output key)
        // 2. Release Alt (output modifier)
        // 3. Restore Ctrl (input modifier still physically held)
        // 4. Restore Shift (input modifier still physically held)
        assert_eq!(events.len(), 4, "Combo release should generate 4 events");

        // Event 0: Release F4
        assert_eq!(events[0].code(), Key::KEY_F4.code(), "First release event should be F4");
        assert_eq!(events[0].value(), event_value::RELEASE);

        // Event 1: Release Alt
        assert_eq!(events[1].code(), Key::KEY_LEFTALT.code(), "Second release event should be Alt");
        assert_eq!(events[1].value(), event_value::RELEASE);

        // Event 2: Restore Ctrl (still physically held)
        assert_eq!(events[2].code(), Key::KEY_LEFTCTRL.code(), "Third event should restore Ctrl");
        assert_eq!(events[2].value(), event_value::PRESS);

        // Event 3: Restore Shift (still physically held)
        assert_eq!(events[3].code(), Key::KEY_LEFTSHIFT.code(), "Fourth event should restore Shift");
        assert_eq!(events[3].value(), event_value::PRESS);
    }

    // ========================================================================
    // DeviceRemapper Tests (Task 030-3.1.1)
    // ========================================================================

    #[test]
    fn test_device_remapper_new_success() {
        // Test creating a DeviceRemapper with valid profiles and default profile
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());

        let device_remapper = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "default",
        );

        assert!(device_remapper.is_ok(), "should create DeviceRemapper successfully");
        let device_remapper = device_remapper.unwrap();
        assert_eq!(device_remapper.device_name(), "Test Keyboard");
        assert_eq!(device_remapper.active_profile(), "default");
    }

    #[test]
    fn test_device_remapper_new_invalid_default_profile() {
        // Test that new() returns an error when default profile doesn't exist
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());

        let result = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "nonexistent",
        );

        assert!(result.is_err(), "should return error for nonexistent default profile");
        let err = result.unwrap_err();
        assert_eq!(err.profile, "nonexistent");
        assert!(err.reason.contains("profile not found"));
    }

    #[test]
    fn test_device_remapper_new_empty_profiles() {
        // Test that new() returns an error when profiles is empty
        let profiles: HashMap<String, Profile> = HashMap::new();

        let result = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "default",
        );

        assert!(result.is_err(), "should return error for empty profiles");
    }

    #[test]
    fn test_device_remapper_profile_names() {
        // Test that profile_names() returns all available profiles
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());
        profiles.insert("work".to_string(), Profile::default());

        let device_remapper = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "default",
        ).unwrap();

        let names = device_remapper.profile_names();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"default"));
        assert!(names.contains(&"gaming"));
        assert!(names.contains(&"work"));
    }

    #[test]
    fn test_device_remapper_has_profile() {
        // Test has_profile() method
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());

        let device_remapper = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "default",
        ).unwrap();

        assert!(device_remapper.has_profile("default"));
        assert!(device_remapper.has_profile("gaming"));
        assert!(!device_remapper.has_profile("nonexistent"));
    }

    #[test]
    fn test_device_remapper_remapper_access() {
        // Test that we can access the inner remapper
        let mut profiles = HashMap::new();
        let mut profile = Profile::default();
        profile.remap.insert("A".to_string(), "B".to_string());
        profiles.insert("default".to_string(), profile);

        let mut device_remapper = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "default",
        ).unwrap();

        // Test immutable access
        let _remapper_ref = device_remapper.remapper();

        // Test mutable access - process an event
        let press_event = InputEvent::new(
            evdev::EventType::KEY,
            Key::KEY_A.code(),
            event_value::PRESS,
        );
        let result = expect_events(device_remapper.remapper_mut().process(press_event));

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].code(), Key::KEY_B.code(), "A should be remapped to B");
    }

    // switch_profile() Tests (Task 030-3.1.2)
    // ========================================================================

    #[test]
    fn test_device_remapper_switch_profile_success() {
        // Test successfully switching between profiles
        let mut profiles = HashMap::new();

        // Default profile: A -> B
        let mut default_profile = Profile::default();
        default_profile.remap.insert("A".to_string(), "B".to_string());
        profiles.insert("default".to_string(), default_profile);

        // Gaming profile: A -> C
        let mut gaming_profile = Profile::default();
        gaming_profile.remap.insert("A".to_string(), "C".to_string());
        profiles.insert("gaming".to_string(), gaming_profile);

        let mut device_remapper = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "default",
        ).unwrap();

        // Verify initial state
        assert_eq!(device_remapper.active_profile(), "default");

        // Switch to gaming profile
        let result = device_remapper.switch_profile("gaming");
        assert!(result.is_ok(), "switch_profile should succeed for existing profile");
        assert_eq!(device_remapper.active_profile(), "gaming");

        // Verify the remapper now uses gaming profile mappings (A -> C)
        let press_event = InputEvent::new(
            evdev::EventType::KEY,
            Key::KEY_A.code(),
            event_value::PRESS,
        );
        let events = expect_events(device_remapper.remapper_mut().process(press_event));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_C.code(), "A should be remapped to C in gaming profile");
    }

    #[test]
    fn test_device_remapper_switch_profile_nonexistent() {
        // Test that switching to a nonexistent profile returns an error
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());

        let mut device_remapper = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "default",
        ).unwrap();

        let result = device_remapper.switch_profile("nonexistent");
        assert!(result.is_err(), "switch_profile should fail for nonexistent profile");

        let err = result.unwrap_err();
        assert_eq!(err.profile, "nonexistent");
        assert!(err.reason.contains("profile not found"));

        // Verify the active profile hasn't changed
        assert_eq!(device_remapper.active_profile(), "default");
    }

    #[test]
    fn test_device_remapper_switch_profile_same_profile() {
        // Test switching to the same profile (should succeed)
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());

        let mut device_remapper = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "default",
        ).unwrap();

        let result = device_remapper.switch_profile("default");
        assert!(result.is_ok(), "switch_profile should succeed even for the same profile");
        assert_eq!(device_remapper.active_profile(), "default");
    }

    #[test]
    fn test_device_remapper_switch_profile_multiple_switches() {
        // Test switching between profiles multiple times
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());
        profiles.insert("work".to_string(), Profile::default());

        let mut device_remapper = DeviceRemapper::new(
            "Test Keyboard".to_string(),
            profiles,
            "default",
        ).unwrap();

        // Switch through profiles
        device_remapper.switch_profile("gaming").unwrap();
        assert_eq!(device_remapper.active_profile(), "gaming");

        device_remapper.switch_profile("work").unwrap();
        assert_eq!(device_remapper.active_profile(), "work");

        device_remapper.switch_profile("default").unwrap();
        assert_eq!(device_remapper.active_profile(), "default");
    }

    // ========================================================================
    // Profile Switch via Keybind Tests (Task 030-3.7.1)
    // ========================================================================
    //
    // These tests verify that pressing a profile switch keybind (e.g., Ctrl+Shift+1)
    // correctly triggers a profile switch via RemapResult::ProfileSwitch.

    #[test]
    fn test_profile_switch_keybind_detection_basic() {
        // Task 030-3.7.1: Test that pressing a profile switch keybind
        // returns RemapResult::ProfileSwitch with the correct profile name.
        //
        // Setup:
        // - Two profiles: "default" (A->B) and "gaming" (A->C)
        // - Keybind: Ctrl+Shift+2 -> "gaming"
        let mut profiles = HashMap::new();

        let mut default_profile = Profile::default();
        default_profile.remap.insert("A".to_string(), "B".to_string());
        profiles.insert("default".to_string(), default_profile);

        let mut gaming_profile = Profile::default();
        gaming_profile.remap.insert("A".to_string(), "C".to_string());
        profiles.insert("gaming".to_string(), gaming_profile);

        let mut profile_switch = HashMap::new();
        profile_switch.insert("Ctrl+Shift+2".to_string(), "gaming".to_string());

        let mut device_remapper = DeviceRemapper::new_with_profile_switch(
            "Test Keyboard".to_string(),
            profiles,
            "default",
            profile_switch,
        ).unwrap();

        // Verify initial state
        assert_eq!(device_remapper.active_profile(), "default");

        // Simulate pressing Ctrl+Shift+2:
        // 1. Press LeftCtrl
        let ctrl_press = InputEvent::new(
            evdev::EventType::KEY,
            Key::KEY_LEFTCTRL.code(),
            event_value::PRESS,
        );
        let result = device_remapper.process(ctrl_press);
        assert!(matches!(result, RemapResult::Events(_)), "Ctrl press should passthrough");

        // 2. Press LeftShift
        let shift_press = InputEvent::new(
            evdev::EventType::KEY,
            Key::KEY_LEFTSHIFT.code(),
            event_value::PRESS,
        );
        let result = device_remapper.process(shift_press);
        assert!(matches!(result, RemapResult::Events(_)), "Shift press should passthrough");

        // 3. Press '2' - this should trigger profile switch to "gaming"
        let key_2_press = InputEvent::new(
            evdev::EventType::KEY,
            Key::KEY_2.code(),
            event_value::PRESS,
        );
        let result = device_remapper.process(key_2_press);

        // Verify ProfileSwitch result
        match result {
            RemapResult::ProfileSwitch(profile_name) => {
                assert_eq!(profile_name, "gaming", "Profile switch should be to 'gaming'");
            }
            other => panic!("Expected RemapResult::ProfileSwitch, got {:?}", other),
        }
    }

    #[test]
    fn test_profile_switch_keybind_full_workflow() {
        // Task 030-3.7.1: Full workflow test for keybind-based profile switching
        //
        // This test simulates the complete workflow:
        // 1. Start with default profile
        // 2. Press keybind to switch to gaming profile
        // 3. Verify gaming profile's remapping is active
        // 4. Press keybind to switch back to default
        // 5. Verify default profile's remapping is active again

        let mut profiles = HashMap::new();

        // Default profile: A -> B
        let mut default_profile = Profile::default();
        default_profile.remap.insert("A".to_string(), "B".to_string());
        profiles.insert("default".to_string(), default_profile);

        // Gaming profile: A -> C
        let mut gaming_profile = Profile::default();
        gaming_profile.remap.insert("A".to_string(), "C".to_string());
        profiles.insert("gaming".to_string(), gaming_profile);

        // Profile switch keybinds
        let mut profile_switch = HashMap::new();
        profile_switch.insert("Ctrl+Shift+1".to_string(), "default".to_string());
        profile_switch.insert("Ctrl+Shift+2".to_string(), "gaming".to_string());

        let mut device_remapper = DeviceRemapper::new_with_profile_switch(
            "Test Keyboard".to_string(),
            profiles,
            "default",
            profile_switch,
        ).unwrap();

        // ============================================================
        // Step 1: Verify default profile is active (A -> B)
        // ============================================================
        assert_eq!(device_remapper.active_profile(), "default");

        let a_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), event_value::PRESS);
        let result = device_remapper.process(a_press);
        let events = expect_events(result);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_B.code(), "A should be remapped to B in default profile");

        // Release A
        let a_release = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), event_value::RELEASE);
        let _ = device_remapper.process(a_release);

        // ============================================================
        // Step 2: Press Ctrl+Shift+2 to switch to gaming profile
        // ============================================================
        let ctrl_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS);
        let _ = device_remapper.process(ctrl_press);

        let shift_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), event_value::PRESS);
        let _ = device_remapper.process(shift_press);

        let key_2_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_2.code(), event_value::PRESS);
        let result = device_remapper.process(key_2_press);

        // Handle the profile switch
        match result {
            RemapResult::ProfileSwitch(profile_name) => {
                assert_eq!(profile_name, "gaming");
                // Actually switch the profile
                device_remapper.switch_profile(&profile_name).unwrap();
            }
            other => panic!("Expected ProfileSwitch for Ctrl+Shift+2, got {:?}", other),
        }

        // Release modifiers
        let shift_release = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), event_value::RELEASE);
        let _ = device_remapper.process(shift_release);
        let ctrl_release = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::RELEASE);
        let _ = device_remapper.process(ctrl_release);

        // ============================================================
        // Step 3: Verify gaming profile is now active (A -> C)
        // ============================================================
        assert_eq!(device_remapper.active_profile(), "gaming");

        let a_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), event_value::PRESS);
        let result = device_remapper.process(a_press);
        let events = expect_events(result);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_C.code(), "A should be remapped to C in gaming profile");

        // Release A
        let a_release = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), event_value::RELEASE);
        let _ = device_remapper.process(a_release);

        // ============================================================
        // Step 4: Press Ctrl+Shift+1 to switch back to default profile
        // ============================================================
        let ctrl_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS);
        let _ = device_remapper.process(ctrl_press);

        let shift_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), event_value::PRESS);
        let _ = device_remapper.process(shift_press);

        let key_1_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_1.code(), event_value::PRESS);
        let result = device_remapper.process(key_1_press);

        // Handle the profile switch
        match result {
            RemapResult::ProfileSwitch(profile_name) => {
                assert_eq!(profile_name, "default");
                device_remapper.switch_profile(&profile_name).unwrap();
            }
            other => panic!("Expected ProfileSwitch for Ctrl+Shift+1, got {:?}", other),
        }

        // Release modifiers
        let shift_release = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), event_value::RELEASE);
        let _ = device_remapper.process(shift_release);
        let ctrl_release = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::RELEASE);
        let _ = device_remapper.process(ctrl_release);

        // ============================================================
        // Step 5: Verify default profile is active again (A -> B)
        // ============================================================
        assert_eq!(device_remapper.active_profile(), "default");

        let a_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_A.code(), event_value::PRESS);
        let result = device_remapper.process(a_press);
        let events = expect_events(result);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), Key::KEY_B.code(), "A should be remapped to B after switching back to default");
    }

    #[test]
    fn test_profile_switch_keybind_does_not_emit_key_events() {
        // Task 030-3.7.1: Verify that pressing a profile switch keybind
        // does NOT emit the trigger key's events (the keybind is "consumed")
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());

        let mut profile_switch = HashMap::new();
        profile_switch.insert("Ctrl+Shift+G".to_string(), "gaming".to_string());

        let mut device_remapper = DeviceRemapper::new_with_profile_switch(
            "Test Keyboard".to_string(),
            profiles,
            "default",
            profile_switch,
        ).unwrap();

        // Press Ctrl+Shift+G
        let ctrl_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS);
        let _ = device_remapper.process(ctrl_press);

        let shift_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), event_value::PRESS);
        let _ = device_remapper.process(shift_press);

        let g_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_G.code(), event_value::PRESS);
        let result = device_remapper.process(g_press);

        // The result should be ProfileSwitch, NOT Events containing 'G'
        match result {
            RemapResult::ProfileSwitch(profile_name) => {
                assert_eq!(profile_name, "gaming");
                // The keybind is consumed - no G key event should be emitted
            }
            RemapResult::Events(events) => {
                panic!(
                    "Profile switch keybind should NOT produce Events, got {} events: {:?}",
                    events.len(),
                    events.iter().map(|e| Key::new(e.code())).collect::<Vec<_>>()
                );
            }
            RemapResult::Macro(_) => {
                panic!("Profile switch keybind should NOT produce Macro");
            }
        }
    }

    #[test]
    fn test_profile_switch_keybind_without_modifiers_held_does_not_trigger() {
        // Task 030-3.7.1: Verify that pressing the trigger key without
        // the required modifiers does NOT trigger a profile switch
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());

        let mut profile_switch = HashMap::new();
        profile_switch.insert("Ctrl+Shift+2".to_string(), "gaming".to_string());

        let mut device_remapper = DeviceRemapper::new_with_profile_switch(
            "Test Keyboard".to_string(),
            profiles,
            "default",
            profile_switch,
        ).unwrap();

        // Press just '2' without Ctrl+Shift - should NOT trigger profile switch
        let key_2_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_2.code(), event_value::PRESS);
        let result = device_remapper.process(key_2_press);

        // Should be Events (passthrough), not ProfileSwitch
        match result {
            RemapResult::Events(events) => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].code(), Key::KEY_2.code(), "Key 2 should pass through");
            }
            RemapResult::ProfileSwitch(name) => {
                panic!("Should NOT trigger ProfileSwitch without modifiers, got '{}'", name);
            }
            RemapResult::Macro(_) => {
                panic!("Should NOT trigger Macro");
            }
        }

        // Verify profile hasn't changed
        assert_eq!(device_remapper.active_profile(), "default");
    }

    #[test]
    fn test_profile_switch_keybind_with_partial_modifiers_does_not_trigger() {
        // Task 030-3.7.1: Verify that pressing the trigger key with only
        // SOME of the required modifiers does NOT trigger a profile switch
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());

        let mut profile_switch = HashMap::new();
        profile_switch.insert("Ctrl+Shift+2".to_string(), "gaming".to_string());

        let mut device_remapper = DeviceRemapper::new_with_profile_switch(
            "Test Keyboard".to_string(),
            profiles,
            "default",
            profile_switch,
        ).unwrap();

        // Press Ctrl+2 (missing Shift) - should NOT trigger profile switch
        let ctrl_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS);
        let _ = device_remapper.process(ctrl_press);

        let key_2_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_2.code(), event_value::PRESS);
        let result = device_remapper.process(key_2_press);

        // Should be Events (passthrough), not ProfileSwitch
        match result {
            RemapResult::Events(events) => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].code(), Key::KEY_2.code(), "Key 2 should pass through");
            }
            RemapResult::ProfileSwitch(name) => {
                panic!("Should NOT trigger ProfileSwitch with partial modifiers (Ctrl only), got '{}'", name);
            }
            RemapResult::Macro(_) => {
                panic!("Should NOT trigger Macro");
            }
        }

        // Verify profile hasn't changed
        assert_eq!(device_remapper.active_profile(), "default");
    }

    #[test]
    fn test_profile_switch_keybind_with_extra_modifiers_does_not_trigger() {
        // Task 030-3.7.1: Verify that pressing the trigger key with EXTRA
        // modifiers beyond what's required does NOT trigger a profile switch
        // (exact match required)
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());

        let mut profile_switch = HashMap::new();
        profile_switch.insert("Ctrl+2".to_string(), "gaming".to_string());

        let mut device_remapper = DeviceRemapper::new_with_profile_switch(
            "Test Keyboard".to_string(),
            profiles,
            "default",
            profile_switch,
        ).unwrap();

        // Press Ctrl+Shift+2 (extra Shift) when keybind is just Ctrl+2
        // Should NOT trigger profile switch due to extra modifier
        let ctrl_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS);
        let _ = device_remapper.process(ctrl_press);

        let shift_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), event_value::PRESS);
        let _ = device_remapper.process(shift_press);

        let key_2_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_2.code(), event_value::PRESS);
        let result = device_remapper.process(key_2_press);

        // Should be Events (passthrough), not ProfileSwitch
        match result {
            RemapResult::Events(events) => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].code(), Key::KEY_2.code(), "Key 2 should pass through");
            }
            RemapResult::ProfileSwitch(name) => {
                panic!("Should NOT trigger ProfileSwitch with extra modifiers (Ctrl+Shift vs Ctrl), got '{}'", name);
            }
            RemapResult::Macro(_) => {
                panic!("Should NOT trigger Macro");
            }
        }

        // Verify profile hasn't changed
        assert_eq!(device_remapper.active_profile(), "default");
    }

    #[test]
    fn test_profile_switch_keybind_only_triggers_on_press() {
        // Task 030-3.7.1: Verify that profile switch keybinds only trigger
        // on key press (value=1), not on release or repeat
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());

        let mut profile_switch = HashMap::new();
        profile_switch.insert("Ctrl+Shift+2".to_string(), "gaming".to_string());

        let mut device_remapper = DeviceRemapper::new_with_profile_switch(
            "Test Keyboard".to_string(),
            profiles,
            "default",
            profile_switch,
        ).unwrap();

        // Press Ctrl+Shift, then test key release
        let ctrl_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTCTRL.code(), event_value::PRESS);
        let _ = device_remapper.process(ctrl_press);

        let shift_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_LEFTSHIFT.code(), event_value::PRESS);
        let _ = device_remapper.process(shift_press);

        // Test release event (value=0) - should NOT trigger
        let key_2_release = InputEvent::new(evdev::EventType::KEY, Key::KEY_2.code(), event_value::RELEASE);
        let result = device_remapper.process(key_2_release);
        assert!(matches!(result, RemapResult::Events(_)), "Release should not trigger profile switch");

        // Test repeat event (value=2) - should NOT trigger
        let key_2_repeat = InputEvent::new(evdev::EventType::KEY, Key::KEY_2.code(), 2); // repeat
        let result = device_remapper.process(key_2_repeat);
        assert!(matches!(result, RemapResult::Events(_)), "Repeat should not trigger profile switch");

        // Verify profile hasn't changed
        assert_eq!(device_remapper.active_profile(), "default");

        // Now test press event (value=1) - SHOULD trigger
        let key_2_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_2.code(), event_value::PRESS);
        let result = device_remapper.process(key_2_press);
        assert!(matches!(result, RemapResult::ProfileSwitch(name) if name == "gaming"),
            "Press should trigger profile switch");
    }

    #[test]
    fn test_profile_switch_keybind_left_right_modifier_equivalence() {
        // Task 030-3.7.1: Verify that left and right modifier variants
        // are treated equivalently for profile switch keybinds
        // (Keybind "Ctrl+2" should trigger with either LeftCtrl or RightCtrl)
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile::default());
        profiles.insert("gaming".to_string(), Profile::default());

        let mut profile_switch = HashMap::new();
        profile_switch.insert("Ctrl+2".to_string(), "gaming".to_string());

        // Test with RightCtrl instead of LeftCtrl
        let mut device_remapper = DeviceRemapper::new_with_profile_switch(
            "Test Keyboard".to_string(),
            profiles,
            "default",
            profile_switch,
        ).unwrap();

        let right_ctrl_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_RIGHTCTRL.code(), event_value::PRESS);
        let _ = device_remapper.process(right_ctrl_press);

        let key_2_press = InputEvent::new(evdev::EventType::KEY, Key::KEY_2.code(), event_value::PRESS);
        let result = device_remapper.process(key_2_press);

        match result {
            RemapResult::ProfileSwitch(name) => {
                assert_eq!(name, "gaming", "RightCtrl should be equivalent to Ctrl modifier");
            }
            other => panic!("Expected ProfileSwitch with RightCtrl, got {:?}", other),
        }
    }
}
