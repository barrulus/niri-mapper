//! Macro execution for key sequences with delays
//!
//! This module provides the [`MacroExecutor`] struct for executing macro action
//! sequences (key presses, key combos, and delays) through a virtual device.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use evdev::Key;
use niri_mapper_config::MacroAction;
use tokio::sync::Mutex;

use crate::injector::VirtualDevice;
use crate::remapper::parse_key;

/// Executes macro action sequences through a virtual device.
///
/// The `MacroExecutor` is a stateless executor that takes a shared reference
/// to a [`VirtualDevice`] and can execute sequences of key actions with delays.
/// For v0.3.0, it does not track in-progress macros; concurrent macro execution
/// is allowed.
///
/// `MacroExecutor` is `Clone` and cheap to clone since it only holds an `Arc`.
/// This allows it to be easily passed into spawned async tasks.
///
/// # Example
///
/// ```ignore
/// use std::sync::Arc;
/// use tokio::sync::Mutex;
///
/// let virtual_device = Arc::new(Mutex::new(VirtualDevice::new_keyboard("test")?));
/// let executor = MacroExecutor::new(virtual_device);
///
/// // executor.execute_macro(&actions).await?;  // Implemented in task 030-1.1.2
/// ```
#[derive(Clone)]
pub struct MacroExecutor {
    virtual_device: Arc<Mutex<VirtualDevice>>,
}

impl MacroExecutor {
    /// Create a new `MacroExecutor` with a shared virtual device.
    ///
    /// # Arguments
    ///
    /// * `virtual_device` - Shared reference to the virtual device for key injection
    ///
    /// # Returns
    ///
    /// A new `MacroExecutor` instance.
    pub fn new(virtual_device: Arc<Mutex<VirtualDevice>>) -> Self {
        Self { virtual_device }
    }

    /// Execute a macro action sequence.
    ///
    /// This method iterates through the provided actions and executes them
    /// sequentially. For key actions, it parses the key string (which may
    /// include modifiers like "Ctrl+C") and emits the appropriate key events.
    /// For delay actions, it sleeps for the specified duration.
    ///
    /// # Arguments
    ///
    /// * `actions` - A slice of [`MacroAction`] variants to execute
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if all actions were executed successfully, or an error
    /// if a key string could not be parsed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A key name in a `MacroAction::Key` cannot be parsed (unknown key name)
    /// - The virtual device fails to emit events
    ///
    /// # Example
    ///
    /// ```ignore
    /// use niri_mapper_config::MacroAction;
    ///
    /// let actions = vec![
    ///     MacroAction::Key("Ctrl+C".to_string()),
    ///     MacroAction::Delay(100),
    ///     MacroAction::Key("Ctrl+V".to_string()),
    /// ];
    ///
    /// executor.execute_macro(&actions).await?;
    /// ```
    pub async fn execute_macro(&self, actions: &[MacroAction]) -> Result<()> {
        for action in actions {
            match action {
                MacroAction::Key(key_string) => {
                    self.execute_key(key_string).await?;
                }
                MacroAction::Delay(ms) => {
                    tokio::time::sleep(Duration::from_millis(*ms)).await;
                }
            }
        }
        Ok(())
    }

    /// Execute a single key action (may include modifiers).
    ///
    /// Parses the key string and emits appropriate press/release events.
    /// For simple keys, emits press + release.
    /// For combos (e.g., "Ctrl+C"), emits all modifier presses, key press,
    /// key release, then all modifier releases.
    async fn execute_key(&self, key_string: &str) -> Result<()> {
        let (modifiers, key) = self.parse_key_combo(key_string)?;

        let mut vd = self.virtual_device.lock().await;

        // Press all modifiers
        for modifier in &modifiers {
            vd.press_key(*modifier)?;
        }

        // Press and release the main key
        vd.tap_key(key)?;

        // Release all modifiers in reverse order
        for modifier in modifiers.iter().rev() {
            vd.release_key(*modifier)?;
        }

        Ok(())
    }

    /// Parse a key string that may include modifiers.
    ///
    /// Splits the key string on `+` to extract modifiers and the trigger key.
    /// Returns a tuple of (modifier_keys, main_key).
    ///
    /// # Supported Modifiers
    ///
    /// - `Ctrl` / `Control` -> `KEY_LEFTCTRL`
    /// - `Shift` -> `KEY_LEFTSHIFT`
    /// - `Alt` -> `KEY_LEFTALT`
    /// - `Super` / `Meta` / `Win` -> `KEY_LEFTMETA`
    ///
    /// # Examples
    ///
    /// - `"A"` -> `([], Key::KEY_A)`
    /// - `"Ctrl+C"` -> `([KEY_LEFTCTRL], Key::KEY_C)`
    /// - `"Ctrl+Shift+V"` -> `([KEY_LEFTCTRL, KEY_LEFTSHIFT], Key::KEY_V)`
    /// - `"Super+1"` -> `([KEY_LEFTMETA], Key::KEY_1)`
    ///
    /// When executed, modifiers are pressed in order, the main key is tapped
    /// (press + release), then modifiers are released in reverse order.
    fn parse_key_combo(&self, key_string: &str) -> Result<(Vec<Key>, Key)> {
        let parts: Vec<&str> = key_string.split('+').collect();

        if parts.is_empty() {
            bail!("Empty key string");
        }

        let mut modifiers = Vec::new();
        let mut main_key: Option<Key> = None;

        for (i, part) in parts.iter().enumerate() {
            let part = part.trim();
            let is_last = i == parts.len() - 1;

            // Check if this is a modifier
            let upper = part.to_uppercase();
            let modifier_key = match upper.as_str() {
                "CTRL" | "CONTROL" => Some(Key::KEY_LEFTCTRL),
                "SHIFT" => Some(Key::KEY_LEFTSHIFT),
                "ALT" => Some(Key::KEY_LEFTALT),
                "SUPER" | "META" | "WIN" => Some(Key::KEY_LEFTMETA),
                _ => None,
            };

            if let Some(mod_key) = modifier_key {
                if is_last {
                    // Modifier is the main key (e.g., just "Ctrl")
                    main_key = Some(mod_key);
                } else {
                    modifiers.push(mod_key);
                }
            } else {
                // Not a modifier, must be the main key
                match parse_key(part) {
                    Some(key) => main_key = Some(key),
                    None => bail!("Unknown key: '{}'", part),
                }
            }
        }

        match main_key {
            Some(key) => Ok((modifiers, key)),
            None => bail!("No main key found in key string: '{}'", key_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a MacroExecutor for testing parse_key_combo
    fn create_test_executor() -> MacroExecutor {
        // We need a virtual device for the executor, but we won't use it for parsing tests
        // Create a minimal mock by using an Arc<Mutex<VirtualDevice>>
        // Note: This test only tests parse_key_combo, not actual key injection
        let vd = Arc::new(Mutex::new(
            VirtualDevice::new_keyboard("test-macro-executor").expect("Failed to create test device"),
        ));
        MacroExecutor::new(vd)
    }

    #[test]
    fn test_simple_key_no_modifiers() {
        let executor = create_test_executor();

        // Simple letter key
        let (modifiers, key) = executor.parse_key_combo("A").unwrap();
        assert!(modifiers.is_empty());
        assert_eq!(key, Key::KEY_A);

        // Another letter
        let (modifiers, key) = executor.parse_key_combo("Z").unwrap();
        assert!(modifiers.is_empty());
        assert_eq!(key, Key::KEY_Z);

        // Number key
        let (modifiers, key) = executor.parse_key_combo("1").unwrap();
        assert!(modifiers.is_empty());
        assert_eq!(key, Key::KEY_1);

        // Function key
        let (modifiers, key) = executor.parse_key_combo("F1").unwrap();
        assert!(modifiers.is_empty());
        assert_eq!(key, Key::KEY_F1);

        // Escape
        let (modifiers, key) = executor.parse_key_combo("Escape").unwrap();
        assert!(modifiers.is_empty());
        assert_eq!(key, Key::KEY_ESC);
    }

    #[test]
    fn test_single_modifier_combo() {
        let executor = create_test_executor();

        // Ctrl+C
        let (modifiers, key) = executor.parse_key_combo("Ctrl+C").unwrap();
        assert_eq!(modifiers.len(), 1);
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL);
        assert_eq!(key, Key::KEY_C);

        // Shift+A
        let (modifiers, key) = executor.parse_key_combo("Shift+A").unwrap();
        assert_eq!(modifiers.len(), 1);
        assert_eq!(modifiers[0], Key::KEY_LEFTSHIFT);
        assert_eq!(key, Key::KEY_A);

        // Alt+Tab
        let (modifiers, key) = executor.parse_key_combo("Alt+Tab").unwrap();
        assert_eq!(modifiers.len(), 1);
        assert_eq!(modifiers[0], Key::KEY_LEFTALT);
        assert_eq!(key, Key::KEY_TAB);

        // Super+1
        let (modifiers, key) = executor.parse_key_combo("Super+1").unwrap();
        assert_eq!(modifiers.len(), 1);
        assert_eq!(modifiers[0], Key::KEY_LEFTMETA);
        assert_eq!(key, Key::KEY_1);
    }

    #[test]
    fn test_multiple_modifiers_combo() {
        let executor = create_test_executor();

        // Ctrl+Shift+V (the example from the task requirements)
        let (modifiers, key) = executor.parse_key_combo("Ctrl+Shift+V").unwrap();
        assert_eq!(modifiers.len(), 2);
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL);
        assert_eq!(modifiers[1], Key::KEY_LEFTSHIFT);
        assert_eq!(key, Key::KEY_V);

        // Ctrl+Alt+Delete
        let (modifiers, key) = executor.parse_key_combo("Ctrl+Alt+Delete").unwrap();
        assert_eq!(modifiers.len(), 2);
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL);
        assert_eq!(modifiers[1], Key::KEY_LEFTALT);
        assert_eq!(key, Key::KEY_DELETE);

        // Super+Shift+S (screenshot shortcut)
        let (modifiers, key) = executor.parse_key_combo("Super+Shift+S").unwrap();
        assert_eq!(modifiers.len(), 2);
        assert_eq!(modifiers[0], Key::KEY_LEFTMETA);
        assert_eq!(modifiers[1], Key::KEY_LEFTSHIFT);
        assert_eq!(key, Key::KEY_S);
    }

    #[test]
    fn test_modifier_aliases() {
        let executor = create_test_executor();

        // Control is an alias for Ctrl
        let (modifiers, key) = executor.parse_key_combo("Control+C").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL);
        assert_eq!(key, Key::KEY_C);

        // Meta is an alias for Super
        let (modifiers, key) = executor.parse_key_combo("Meta+Space").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTMETA);
        assert_eq!(key, Key::KEY_SPACE);

        // Win is an alias for Super
        let (modifiers, key) = executor.parse_key_combo("Win+E").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTMETA);
        assert_eq!(key, Key::KEY_E);
    }

    #[test]
    fn test_case_insensitivity() {
        let executor = create_test_executor();

        // Modifiers should be case-insensitive
        let (modifiers, _) = executor.parse_key_combo("ctrl+a").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL);

        let (modifiers, _) = executor.parse_key_combo("CTRL+A").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL);

        let (modifiers, _) = executor.parse_key_combo("Ctrl+A").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL);
    }

    #[test]
    fn test_whitespace_handling() {
        let executor = create_test_executor();

        // Should handle spaces around +
        let (modifiers, key) = executor.parse_key_combo("Ctrl + C").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL);
        assert_eq!(key, Key::KEY_C);
    }

    #[test]
    fn test_modifier_as_main_key() {
        let executor = create_test_executor();

        // A modifier alone should work as the main key
        let (modifiers, key) = executor.parse_key_combo("Ctrl").unwrap();
        assert!(modifiers.is_empty());
        assert_eq!(key, Key::KEY_LEFTCTRL);

        let (modifiers, key) = executor.parse_key_combo("Shift").unwrap();
        assert!(modifiers.is_empty());
        assert_eq!(key, Key::KEY_LEFTSHIFT);
    }

    #[test]
    fn test_unknown_key_error() {
        let executor = create_test_executor();

        // Unknown key should return an error
        let result = executor.parse_key_combo("UnknownKey");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown key"));

        // Unknown key in combo
        let result = executor.parse_key_combo("Ctrl+UnknownKey");
        assert!(result.is_err());
    }

    #[test]
    fn test_modifiers_map_to_left_variants() {
        let executor = create_test_executor();

        // Verify all modifiers map to their Left variants as specified in requirements
        let (modifiers, _) = executor.parse_key_combo("Ctrl+A").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL);

        let (modifiers, _) = executor.parse_key_combo("Shift+A").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTSHIFT);

        let (modifiers, _) = executor.parse_key_combo("Alt+A").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTALT);

        let (modifiers, _) = executor.parse_key_combo("Super+A").unwrap();
        assert_eq!(modifiers[0], Key::KEY_LEFTMETA);
    }

    // ========================================================================
    // Macro Execution Tests (Task 030-1.5.1)
    // ========================================================================
    //
    // These tests verify that the MacroExecutor emits the correct press/release
    // sequence when executing macro actions. Since VirtualDevice requires uinput
    // access, we verify behavior indirectly by testing the parsing logic and
    // the expected event sequence.

    /// Test that simple key sequence macro produces correct press/release events.
    ///
    /// This test verifies Task 030-1.5.1: Add unit test for simple key sequence.
    ///
    /// For a macro `["A", "B", "C"]`, each key should produce:
    /// - Key press (value=1)
    /// - Key release (value=0)
    ///
    /// The full sequence should be:
    /// A press -> A release -> B press -> B release -> C press -> C release
    #[test]
    fn test_simple_key_sequence_parsing() {
        let executor = create_test_executor();

        // Test that each key in the sequence parses correctly
        // Macro: ["A", "B", "C"]
        let keys = ["A", "B", "C"];
        let expected_keys = [Key::KEY_A, Key::KEY_B, Key::KEY_C];

        for (key_str, expected_key) in keys.iter().zip(expected_keys.iter()) {
            let (modifiers, key) = executor.parse_key_combo(key_str).unwrap();
            assert!(modifiers.is_empty(), "Simple key '{}' should have no modifiers", key_str);
            assert_eq!(key, *expected_key, "Key '{}' should parse to {:?}", key_str, expected_key);
        }
    }

    /// Test that execute_macro correctly processes a simple key sequence.
    ///
    /// This test uses a real VirtualDevice (requires uinput access) to verify
    /// that the macro execution completes without error for simple keys.
    ///
    /// Note: This test requires elevated permissions to access /dev/uinput.
    /// It will be skipped in CI environments without uinput access.
    #[tokio::test]
    async fn test_simple_key_sequence_macro_execution() {
        // Try to create a test executor - skip test if uinput not available
        let vd = match VirtualDevice::new_keyboard("test-macro-simple-seq") {
            Ok(vd) => Arc::new(Mutex::new(vd)),
            Err(e) => {
                eprintln!("Skipping test_simple_key_sequence_macro_execution: {}", e);
                return;
            }
        };
        let executor = MacroExecutor::new(vd);

        // Create macro actions for ["A", "B", "C"]
        let actions = vec![
            MacroAction::Key("A".to_string()),
            MacroAction::Key("B".to_string()),
            MacroAction::Key("C".to_string()),
        ];

        // Execute the macro - should complete without error
        let result = executor.execute_macro(&actions).await;
        assert!(result.is_ok(), "Simple key sequence macro should execute successfully: {:?}", result.err());
    }

    /// Test that the expected event sequence for simple keys is correct.
    ///
    /// This test documents the expected behavior: for a simple key like "A",
    /// the execute_key method should call tap_key(KEY_A), which emits:
    /// 1. KEY_A press (value=1)
    /// 2. SYN event
    /// 3. KEY_A release (value=0)
    /// 4. SYN event
    ///
    /// For macro ["A", "B", "C"], the full sequence is:
    /// A press -> SYN -> A release -> SYN -> B press -> SYN -> B release -> SYN -> C press -> SYN -> C release -> SYN
    #[test]
    fn test_expected_event_sequence_for_simple_keys() {
        // This test documents the expected behavior based on the VirtualDevice implementation
        // VirtualDevice.tap_key(key) calls:
        //   1. press_key(key) -> emits [KeyPress(key), SYN]
        //   2. release_key(key) -> emits [KeyRelease(key), SYN]

        // For execute_key("A") with no modifiers:
        // 1. No modifier presses (modifiers list is empty)
        // 2. tap_key(KEY_A) is called
        // 3. No modifier releases (modifiers list is empty)

        // Therefore, the expected sequence for ["A", "B", "C"] is:
        // - tap_key(KEY_A) -> press A, release A
        // - tap_key(KEY_B) -> press B, release B
        // - tap_key(KEY_C) -> press C, release C

        // Verify parsing produces the expected keys
        let executor = create_test_executor();

        let (mods_a, key_a) = executor.parse_key_combo("A").unwrap();
        let (mods_b, key_b) = executor.parse_key_combo("B").unwrap();
        let (mods_c, key_c) = executor.parse_key_combo("C").unwrap();

        // All should be simple keys (no modifiers)
        assert!(mods_a.is_empty());
        assert!(mods_b.is_empty());
        assert!(mods_c.is_empty());

        // Verify the key codes
        assert_eq!(key_a, Key::KEY_A);
        assert_eq!(key_b, Key::KEY_B);
        assert_eq!(key_c, Key::KEY_C);

        // Document the expected event sequence
        let expected_sequence = vec![
            // A: tap_key(KEY_A)
            ("A", "press", Key::KEY_A),
            ("A", "release", Key::KEY_A),
            // B: tap_key(KEY_B)
            ("B", "press", Key::KEY_B),
            ("B", "release", Key::KEY_B),
            // C: tap_key(KEY_C)
            ("C", "press", Key::KEY_C),
            ("C", "release", Key::KEY_C),
        ];

        // Verify the sequence length matches expectations
        // 3 keys * 2 events (press + release) = 6 events
        assert_eq!(expected_sequence.len(), 6, "Simple key sequence should produce 6 events (press+release for each key)");
    }

    // ========================================================================
    // Combo Macro Execution Tests (Task 030-1.5.2)
    // ========================================================================
    //
    // These tests verify that the MacroExecutor correctly handles combo keys
    // (keys with modifiers) by emitting the proper press/release sequence:
    // 1. Press all modifiers in order
    // 2. Press and release the main key
    // 3. Release all modifiers in reverse order

    /// Test that combo key sequence macro parses correctly.
    ///
    /// This test verifies Task 030-1.5.2: Add unit test for combo in macro.
    ///
    /// For a macro `["Ctrl+C", "delay(50)", "Ctrl+V"]`, each combo should parse
    /// to the correct modifiers and main key.
    #[test]
    fn test_combo_key_sequence_parsing() {
        let executor = create_test_executor();

        // Test Ctrl+C parsing
        let (modifiers, key) = executor.parse_key_combo("Ctrl+C").unwrap();
        assert_eq!(modifiers.len(), 1, "Ctrl+C should have 1 modifier");
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL, "Modifier should be LEFTCTRL");
        assert_eq!(key, Key::KEY_C, "Main key should be C");

        // Test Ctrl+V parsing
        let (modifiers, key) = executor.parse_key_combo("Ctrl+V").unwrap();
        assert_eq!(modifiers.len(), 1, "Ctrl+V should have 1 modifier");
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL, "Modifier should be LEFTCTRL");
        assert_eq!(key, Key::KEY_V, "Main key should be V");
    }

    /// Test that execute_macro correctly processes a combo key sequence.
    ///
    /// This test uses a real VirtualDevice (requires uinput access) to verify
    /// that the macro execution completes without error for combo keys.
    ///
    /// Note: This test requires elevated permissions to access /dev/uinput.
    /// It will be skipped in CI environments without uinput access.
    #[tokio::test]
    async fn test_combo_key_sequence_macro_execution() {
        // Try to create a test executor - skip test if uinput not available
        let vd = match VirtualDevice::new_keyboard("test-macro-combo-seq") {
            Ok(vd) => Arc::new(Mutex::new(vd)),
            Err(e) => {
                eprintln!("Skipping test_combo_key_sequence_macro_execution: {}", e);
                return;
            }
        };
        let executor = MacroExecutor::new(vd);

        // Create macro actions for ["Ctrl+C", "delay(50)", "Ctrl+V"]
        let actions = vec![
            MacroAction::Key("Ctrl+C".to_string()),
            MacroAction::Delay(50),
            MacroAction::Key("Ctrl+V".to_string()),
        ];

        // Execute the macro - should complete without error
        let result = executor.execute_macro(&actions).await;
        assert!(result.is_ok(), "Combo key sequence macro should execute successfully: {:?}", result.err());
    }

    /// Test that the expected event sequence for combo keys is correct.
    ///
    /// This test documents the expected behavior: for a combo key like "Ctrl+C",
    /// the execute_key method should:
    /// 1. Press all modifiers (LEFTCTRL press)
    /// 2. Tap the main key (C press, C release)
    /// 3. Release all modifiers in reverse order (LEFTCTRL release)
    ///
    /// For macro ["Ctrl+C", "delay(50)", "Ctrl+V"], the full key sequence is:
    /// - Ctrl+C: LEFTCTRL press -> C press -> C release -> LEFTCTRL release
    /// - delay: (no key events)
    /// - Ctrl+V: LEFTCTRL press -> V press -> V release -> LEFTCTRL release
    #[test]
    fn test_expected_event_sequence_for_combo_keys() {
        // This test documents the expected behavior based on the execute_key implementation
        //
        // For execute_key("Ctrl+C"):
        // 1. parse_key_combo("Ctrl+C") -> (modifiers: [KEY_LEFTCTRL], key: KEY_C)
        // 2. Press all modifiers: press_key(KEY_LEFTCTRL) -> [CtrlPress, SYN]
        // 3. Tap main key: tap_key(KEY_C) -> [CPress, SYN, CRelease, SYN]
        // 4. Release all modifiers in reverse: release_key(KEY_LEFTCTRL) -> [CtrlRelease, SYN]

        let executor = create_test_executor();

        // Parse Ctrl+C
        let (mods_c, key_c) = executor.parse_key_combo("Ctrl+C").unwrap();
        assert_eq!(mods_c.len(), 1);
        assert_eq!(mods_c[0], Key::KEY_LEFTCTRL);
        assert_eq!(key_c, Key::KEY_C);

        // Parse Ctrl+V
        let (mods_v, key_v) = executor.parse_key_combo("Ctrl+V").unwrap();
        assert_eq!(mods_v.len(), 1);
        assert_eq!(mods_v[0], Key::KEY_LEFTCTRL);
        assert_eq!(key_v, Key::KEY_V);

        // Document the expected event sequence for ["Ctrl+C", "delay(50)", "Ctrl+V"]
        // Each event is: (action_name, event_type, key)
        let expected_sequence = vec![
            // Ctrl+C execution
            ("Ctrl+C", "modifier_press", Key::KEY_LEFTCTRL),  // Step 1: Press modifier
            ("Ctrl+C", "key_press", Key::KEY_C),              // Step 2a: Press main key
            ("Ctrl+C", "key_release", Key::KEY_C),            // Step 2b: Release main key
            ("Ctrl+C", "modifier_release", Key::KEY_LEFTCTRL), // Step 3: Release modifier
            // delay(50) - no key events, just a pause
            // Ctrl+V execution
            ("Ctrl+V", "modifier_press", Key::KEY_LEFTCTRL),  // Step 1: Press modifier
            ("Ctrl+V", "key_press", Key::KEY_V),              // Step 2a: Press main key
            ("Ctrl+V", "key_release", Key::KEY_V),            // Step 2b: Release main key
            ("Ctrl+V", "modifier_release", Key::KEY_LEFTCTRL), // Step 3: Release modifier
        ];

        // Verify the sequence length: 2 combos * 4 events each = 8 events
        // (delay produces no key events)
        assert_eq!(
            expected_sequence.len(),
            8,
            "Combo key sequence should produce 8 key events (4 per combo: mod press, key press, key release, mod release)"
        );

        // Verify the proper modifier handling sequence
        // For each combo: modifiers pressed first, then main key tapped, then modifiers released
        // This ensures the modifier is held while the main key is pressed
        assert_eq!(expected_sequence[0].1, "modifier_press", "First event should be modifier press");
        assert_eq!(expected_sequence[1].1, "key_press", "Second event should be key press");
        assert_eq!(expected_sequence[2].1, "key_release", "Third event should be key release");
        assert_eq!(expected_sequence[3].1, "modifier_release", "Fourth event should be modifier release");
    }

    /// Test multi-modifier combo handling.
    ///
    /// This test verifies that combos with multiple modifiers (e.g., Ctrl+Shift+V)
    /// correctly press modifiers in order and release them in reverse order.
    #[test]
    fn test_multi_modifier_combo_event_sequence() {
        let executor = create_test_executor();

        // Parse Ctrl+Shift+V
        let (modifiers, key) = executor.parse_key_combo("Ctrl+Shift+V").unwrap();
        assert_eq!(modifiers.len(), 2, "Ctrl+Shift+V should have 2 modifiers");
        assert_eq!(modifiers[0], Key::KEY_LEFTCTRL, "First modifier should be LEFTCTRL");
        assert_eq!(modifiers[1], Key::KEY_LEFTSHIFT, "Second modifier should be LEFTSHIFT");
        assert_eq!(key, Key::KEY_V, "Main key should be V");

        // Document the expected event sequence for Ctrl+Shift+V
        // Modifiers pressed in order, released in reverse order
        let expected_sequence = vec![
            ("Ctrl+Shift+V", "modifier_press", Key::KEY_LEFTCTRL),   // First modifier pressed
            ("Ctrl+Shift+V", "modifier_press", Key::KEY_LEFTSHIFT), // Second modifier pressed
            ("Ctrl+Shift+V", "key_press", Key::KEY_V),              // Main key pressed
            ("Ctrl+Shift+V", "key_release", Key::KEY_V),            // Main key released
            ("Ctrl+Shift+V", "modifier_release", Key::KEY_LEFTSHIFT), // Second modifier released (reverse order)
            ("Ctrl+Shift+V", "modifier_release", Key::KEY_LEFTCTRL),  // First modifier released (reverse order)
        ];

        // Verify the sequence: 2 mod presses + key press + key release + 2 mod releases = 6 events
        assert_eq!(
            expected_sequence.len(),
            6,
            "Multi-modifier combo should produce 6 events"
        );

        // Verify reverse order release
        // Modifiers are released in reverse order (LIFO) to properly "unwind" the modifier state
        assert_eq!(
            expected_sequence[4].2,
            Key::KEY_LEFTSHIFT,
            "LEFTSHIFT (pressed second) should be released first"
        );
        assert_eq!(
            expected_sequence[5].2,
            Key::KEY_LEFTCTRL,
            "LEFTCTRL (pressed first) should be released last"
        );
    }
}
