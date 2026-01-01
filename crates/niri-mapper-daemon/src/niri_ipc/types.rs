//! Internal types for niri IPC data
//!
//! This module defines internal representations of niri compositor data,
//! decoupled from the external `niri-ipc` crate. This provides:
//!
//! - Stability: Internal code is insulated from upstream API changes
//! - Flexibility: We only expose fields we actually need
//! - Clarity: Types are tailored to niri-mapper's use cases
//!
//! Use the `From` implementations to convert from `niri_ipc` types.

/// Information about the currently focused window
///
/// This is a simplified view of window state, containing only the fields
/// needed for application-aware input remapping.
///
/// # Example
///
/// ```ignore
/// let window: FocusedWindow = niri_window.into();
/// println!("Focused: {} ({})", window.app_id, window.title);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FocusedWindow {
    /// The application identifier (e.g., "firefox", "Alacritty")
    ///
    /// This corresponds to the Wayland `app_id` and is the primary key
    /// for matching application-specific profiles.
    pub app_id: String,

    /// The window title
    ///
    /// Can be used for more granular matching within an application.
    pub title: String,

    /// Unique window identifier assigned by niri
    ///
    /// This ID is stable for the lifetime of the window.
    pub id: u64,
}

/// Information about a window
///
/// This is a complete view of window state from the niri compositor,
/// containing all fields needed for window queries and application-aware
/// input remapping.
///
/// # Example
///
/// ```ignore
/// let windows = client.get_windows().await?;
/// for window in windows {
///     println!("{}: {} (workspace {})", window.app_id, window.title, window.workspace_id.unwrap_or(0));
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Window {
    /// Unique window identifier assigned by niri
    ///
    /// This ID is stable for the lifetime of the window.
    pub id: u64,

    /// The application identifier (e.g., "firefox", "Alacritty")
    ///
    /// This corresponds to the Wayland `app_id` and is the primary key
    /// for matching application-specific profiles.
    pub app_id: String,

    /// The window title
    ///
    /// Can be used for more granular matching within an application.
    pub title: String,

    /// The workspace ID this window belongs to
    ///
    /// May be `None` if the window is not assigned to a workspace.
    pub workspace_id: Option<u64>,
}

/// Information about a workspace
///
/// Contains workspace state relevant for workspace-aware input remapping.
///
/// # Example
///
/// ```ignore
/// let workspace: WorkspaceInfo = niri_workspace.into();
/// if workspace.is_active {
///     println!("Active workspace: {:?}", workspace.name);
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceInfo {
    /// Unique workspace identifier assigned by niri
    ///
    /// This ID remains constant regardless of the workspace moving around
    /// or across monitors.
    pub id: u64,

    /// Optional human-readable workspace name
    ///
    /// May be `None` if the workspace has no assigned name.
    pub name: Option<String>,

    /// Whether this workspace is currently active/visible
    ///
    /// A workspace is active if it's the currently visible workspace
    /// on its output.
    pub is_active: bool,

    /// Whether this workspace is currently focused
    ///
    /// A workspace is focused if it's the single workspace that has
    /// keyboard focus. Only one workspace can be focused at a time,
    /// even in multi-monitor setups.
    pub is_focused: bool,

    /// The output (monitor) this workspace is on
    ///
    /// This is the output name as reported by niri.
    pub output: String,
}

impl From<niri_ipc::Window> for FocusedWindow {
    fn from(window: niri_ipc::Window) -> Self {
        Self {
            app_id: window.app_id.unwrap_or_default(),
            title: window.title.unwrap_or_default(),
            id: window.id,
        }
    }
}

impl From<&niri_ipc::Window> for FocusedWindow {
    fn from(window: &niri_ipc::Window) -> Self {
        Self {
            app_id: window.app_id.clone().unwrap_or_default(),
            title: window.title.clone().unwrap_or_default(),
            id: window.id,
        }
    }
}

impl From<niri_ipc::Window> for Window {
    fn from(window: niri_ipc::Window) -> Self {
        Self {
            id: window.id,
            app_id: window.app_id.unwrap_or_default(),
            title: window.title.unwrap_or_default(),
            workspace_id: window.workspace_id,
        }
    }
}

impl From<&niri_ipc::Window> for Window {
    fn from(window: &niri_ipc::Window) -> Self {
        Self {
            id: window.id,
            app_id: window.app_id.clone().unwrap_or_default(),
            title: window.title.clone().unwrap_or_default(),
            workspace_id: window.workspace_id,
        }
    }
}

impl From<niri_ipc::Workspace> for WorkspaceInfo {
    fn from(workspace: niri_ipc::Workspace) -> Self {
        Self {
            id: workspace.id,
            name: workspace.name,
            is_active: workspace.is_active,
            is_focused: workspace.is_focused,
            output: workspace.output.unwrap_or_default(),
        }
    }
}

impl From<&niri_ipc::Workspace> for WorkspaceInfo {
    fn from(workspace: &niri_ipc::Workspace) -> Self {
        Self {
            id: workspace.id,
            name: workspace.name.clone(),
            is_active: workspace.is_active,
            is_focused: workspace.is_focused,
            output: workspace.output.clone().unwrap_or_default(),
        }
    }
}

// =============================================================================
// Focus Change Event Types
// =============================================================================

/// Event indicating a window focus change
///
/// This event is emitted when the focused window changes in the niri compositor.
/// The `window` field contains the newly focused window, or `None` if no window
/// is currently focused (e.g., when focusing the desktop).
///
/// # Example
///
/// ```ignore
/// match event {
///     FocusChangeEvent { window: Some(w) } => {
///         println!("Focused: {}", w.app_id);
///     }
///     FocusChangeEvent { window: None } => {
///         println!("No window focused");
///     }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusChangeEvent {
    /// The newly focused window, or `None` if no window is focused
    pub window: Option<FocusedWindow>,
}

/// Event indicating a workspace activation change
///
/// This event is emitted when a workspace becomes active on an output.
/// Note that a workspace becoming active doesn't necessarily mean it's focused;
/// use the `is_focused` field to determine if this is the single focused workspace.
///
/// # Example
///
/// ```ignore
/// if event.is_focused {
///     println!("Workspace {} is now focused on {}", event.workspace_id, event.output);
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceChangeEvent {
    /// The ID of the activated workspace
    pub workspace_id: u64,

    /// The output (monitor) this workspace is on
    pub output: String,

    /// Whether this workspace also became focused
    ///
    /// If `true`, this is now the single focused workspace across all outputs.
    /// All other workspaces are no longer focused, but may remain active on
    /// their respective outputs.
    pub is_focused: bool,
}

/// Internal representation of niri compositor events
///
/// This enum provides a simplified view of niri IPC events, containing only
/// the event types relevant to niri-mapper's remapping functionality.
/// Events not needed for remapping (like layout changes) are filtered out
/// at the event parsing stage.
///
/// # Conversion
///
/// Use `NiriEvent::try_from(niri_ipc::Event)` to convert from the external
/// niri-ipc event type. Returns `None` for events not relevant to remapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NiriEvent {
    /// Window focus changed
    FocusChanged(FocusChangeEvent),

    /// Workspace was activated
    WorkspaceActivated(WorkspaceChangeEvent),
}

impl NiriEvent {
    /// Try to convert a niri-ipc Event to an internal NiriEvent
    ///
    /// Returns `Some(NiriEvent)` for events relevant to remapping,
    /// or `None` for events that should be ignored.
    ///
    /// # Relevant Events
    ///
    /// - `WindowFocusChanged` -> `NiriEvent::FocusChanged`
    /// - `WorkspaceActivated` -> `NiriEvent::WorkspaceActivated`
    ///
    /// # Ignored Events
    ///
    /// - Window opened/closed (not needed for focus-based remapping)
    /// - Layout changes
    /// - Keyboard layout changes
    /// - Other compositor state changes
    pub fn from_niri_event(event: niri_ipc::Event, windows: &[niri_ipc::Window]) -> Option<Self> {
        match event {
            niri_ipc::Event::WindowFocusChanged { id } => {
                let window = id.and_then(|window_id| {
                    windows
                        .iter()
                        .find(|w| w.id == window_id)
                        .map(FocusedWindow::from)
                });
                Some(NiriEvent::FocusChanged(FocusChangeEvent { window }))
            }
            niri_ipc::Event::WorkspaceActivated { id, focused } => {
                // We need workspace info to get the output, but for now we'll
                // use an empty string as we don't have access to workspace list
                // in this context. The caller should enrich this if needed.
                Some(NiriEvent::WorkspaceActivated(WorkspaceChangeEvent {
                    workspace_id: id,
                    output: String::new(), // To be enriched by caller with workspace info
                    is_focused: focused,
                }))
            }
            // Ignore other events not relevant to remapping
            _ => None,
        }
    }

    /// Create a FocusChanged event with known window information
    ///
    /// Use this when you have the window information directly available.
    pub fn focus_changed(window: Option<FocusedWindow>) -> Self {
        NiriEvent::FocusChanged(FocusChangeEvent { window })
    }

    /// Create a WorkspaceActivated event
    pub fn workspace_activated(workspace_id: u64, output: String, is_focused: bool) -> Self {
        NiriEvent::WorkspaceActivated(WorkspaceChangeEvent {
            workspace_id,
            output,
            is_focused,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a test Window with required fields set
    fn test_window(id: u64, app_id: Option<String>, title: Option<String>) -> niri_ipc::Window {
        niri_ipc::Window {
            id,
            title,
            app_id,
            pid: None,
            workspace_id: Some(1),
            is_focused: true,
            is_urgent: false,
            is_floating: false,
            focus_timestamp: None,
            layout: niri_ipc::WindowLayout {
                pos_in_scrolling_layout: None,
                tile_size: (0.0, 0.0),
                window_size: (0, 0),
                tile_pos_in_workspace_view: None,
                window_offset_in_tile: (0.0, 0.0),
            },
        }
    }

    /// Helper to create a test Workspace with required fields set
    fn test_workspace(
        id: u64,
        name: Option<String>,
        output: Option<String>,
        is_active: bool,
    ) -> niri_ipc::Workspace {
        niri_ipc::Workspace {
            id,
            idx: 0,
            name,
            output,
            is_urgent: false,
            is_active,
            is_focused: is_active,
            active_window_id: None,
        }
    }

    #[test]
    fn test_focused_window_from_niri_window() {
        let niri_window = test_window(42, Some("my-app".to_string()), Some("My Window".to_string()));

        let focused: FocusedWindow = niri_window.into();

        assert_eq!(focused.id, 42);
        assert_eq!(focused.app_id, "my-app");
        assert_eq!(focused.title, "My Window");
    }

    #[test]
    fn test_focused_window_handles_none_fields() {
        let niri_window = test_window(1, None, None);

        let focused: FocusedWindow = niri_window.into();

        assert_eq!(focused.id, 1);
        assert_eq!(focused.app_id, "");
        assert_eq!(focused.title, "");
    }

    #[test]
    fn test_workspace_info_from_niri_workspace() {
        let niri_workspace = test_workspace(5, Some("coding".to_string()), Some("DP-1".to_string()), true);

        let info: WorkspaceInfo = niri_workspace.into();

        assert_eq!(info.id, 5);
        assert_eq!(info.name, Some("coding".to_string()));
        assert!(info.is_active);
        assert!(info.is_focused);
        assert_eq!(info.output, "DP-1");
    }

    #[test]
    fn test_workspace_info_handles_none_fields() {
        let niri_workspace = test_workspace(1, None, None, false);

        let info: WorkspaceInfo = niri_workspace.into();

        assert_eq!(info.id, 1);
        assert_eq!(info.name, None);
        assert!(!info.is_active);
        assert!(!info.is_focused);
        assert_eq!(info.output, "");
    }

    // =========================================================================
    // Focus Change Event Tests
    // =========================================================================

    #[test]
    fn test_focus_change_event_with_window() {
        let window = FocusedWindow {
            id: 42,
            app_id: "firefox".to_string(),
            title: "Mozilla Firefox".to_string(),
        };

        let event = FocusChangeEvent {
            window: Some(window.clone()),
        };

        assert_eq!(event.window, Some(window));
    }

    #[test]
    fn test_focus_change_event_no_window() {
        let event = FocusChangeEvent { window: None };

        assert_eq!(event.window, None);
    }

    #[test]
    fn test_workspace_change_event() {
        let event = WorkspaceChangeEvent {
            workspace_id: 5,
            output: "DP-1".to_string(),
            is_focused: true,
        };

        assert_eq!(event.workspace_id, 5);
        assert_eq!(event.output, "DP-1");
        assert!(event.is_focused);
    }

    #[test]
    fn test_niri_event_focus_changed_with_window() {
        let window = FocusedWindow {
            id: 10,
            app_id: "alacritty".to_string(),
            title: "Terminal".to_string(),
        };

        let event = NiriEvent::focus_changed(Some(window.clone()));

        match event {
            NiriEvent::FocusChanged(focus_event) => {
                assert_eq!(focus_event.window, Some(window));
            }
            _ => panic!("Expected FocusChanged event"),
        }
    }

    #[test]
    fn test_niri_event_focus_changed_no_window() {
        let event = NiriEvent::focus_changed(None);

        match event {
            NiriEvent::FocusChanged(focus_event) => {
                assert_eq!(focus_event.window, None);
            }
            _ => panic!("Expected FocusChanged event"),
        }
    }

    #[test]
    fn test_niri_event_workspace_activated() {
        let event = NiriEvent::workspace_activated(3, "HDMI-1".to_string(), true);

        match event {
            NiriEvent::WorkspaceActivated(ws_event) => {
                assert_eq!(ws_event.workspace_id, 3);
                assert_eq!(ws_event.output, "HDMI-1");
                assert!(ws_event.is_focused);
            }
            _ => panic!("Expected WorkspaceActivated event"),
        }
    }

    #[test]
    fn test_niri_event_from_window_focus_changed() {
        let windows = vec![
            test_window(1, Some("app1".to_string()), Some("Title 1".to_string())),
            test_window(2, Some("app2".to_string()), Some("Title 2".to_string())),
        ];

        // Test focus change to a known window
        let niri_event = niri_ipc::Event::WindowFocusChanged { id: Some(2) };
        let event = NiriEvent::from_niri_event(niri_event, &windows);

        match event {
            Some(NiriEvent::FocusChanged(focus_event)) => {
                let window = focus_event.window.expect("Should have window");
                assert_eq!(window.id, 2);
                assert_eq!(window.app_id, "app2");
                assert_eq!(window.title, "Title 2");
            }
            _ => panic!("Expected Some(FocusChanged) event"),
        }
    }

    #[test]
    fn test_niri_event_from_window_focus_changed_unknown_id() {
        let windows = vec![
            test_window(1, Some("app1".to_string()), Some("Title 1".to_string())),
        ];

        // Test focus change to an unknown window ID
        let niri_event = niri_ipc::Event::WindowFocusChanged { id: Some(999) };
        let event = NiriEvent::from_niri_event(niri_event, &windows);

        match event {
            Some(NiriEvent::FocusChanged(focus_event)) => {
                assert_eq!(focus_event.window, None);
            }
            _ => panic!("Expected Some(FocusChanged) event with None window"),
        }
    }

    #[test]
    fn test_niri_event_from_window_focus_changed_no_focus() {
        let windows = vec![];

        // Test focus change with no window focused
        let niri_event = niri_ipc::Event::WindowFocusChanged { id: None };
        let event = NiriEvent::from_niri_event(niri_event, &windows);

        match event {
            Some(NiriEvent::FocusChanged(focus_event)) => {
                assert_eq!(focus_event.window, None);
            }
            _ => panic!("Expected Some(FocusChanged) event with None window"),
        }
    }

    #[test]
    fn test_niri_event_from_workspace_activated() {
        let windows = vec![];

        let niri_event = niri_ipc::Event::WorkspaceActivated { id: 7, focused: true };
        let event = NiriEvent::from_niri_event(niri_event, &windows);

        match event {
            Some(NiriEvent::WorkspaceActivated(ws_event)) => {
                assert_eq!(ws_event.workspace_id, 7);
                assert!(ws_event.is_focused);
                // Output is empty since we don't have workspace info in this context
                assert_eq!(ws_event.output, "");
            }
            _ => panic!("Expected Some(WorkspaceActivated) event"),
        }
    }

    #[test]
    fn test_niri_event_from_workspace_activated_not_focused() {
        let windows = vec![];

        let niri_event = niri_ipc::Event::WorkspaceActivated { id: 2, focused: false };
        let event = NiriEvent::from_niri_event(niri_event, &windows);

        match event {
            Some(NiriEvent::WorkspaceActivated(ws_event)) => {
                assert_eq!(ws_event.workspace_id, 2);
                assert!(!ws_event.is_focused);
            }
            _ => panic!("Expected Some(WorkspaceActivated) event"),
        }
    }

    #[test]
    fn test_niri_event_ignores_irrelevant_events() {
        let windows = vec![];

        // WindowOpenedOrChanged should be ignored
        let niri_event = niri_ipc::Event::WindowOpenedOrChanged {
            window: test_window(1, Some("app".to_string()), Some("title".to_string())),
        };
        assert!(NiriEvent::from_niri_event(niri_event, &windows).is_none());

        // WindowClosed should be ignored
        let niri_event = niri_ipc::Event::WindowClosed { id: 1 };
        assert!(NiriEvent::from_niri_event(niri_event, &windows).is_none());
    }
}
