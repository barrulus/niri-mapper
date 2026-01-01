//! Niri IPC client for compositor integration
//!
//! This module provides communication with the niri compositor via its IPC socket.
//! It enables niri-mapper to:
//! - Query the currently focused window and workspace
//! - Subscribe to focus change events
//! - React to application switches for profile-based remapping
//!
//! ## Architecture
//!
//! - `NiriClient`: Main client for sending IPC requests and receiving responses
//! - `NiriError`: Error types for IPC operations
//!
//! ## Protocol
//!
//! Niri exposes a Unix socket at `$NIRI_SOCKET`. Clients send JSON-formatted
//! `Request` messages (one per line) and receive JSON `Reply` responses.
//!
//! For event streaming, send `Request::EventStream` to initiate continuous
//! event delivery until the connection closes.

mod client;
mod error;
mod events;
mod types;

pub use client::{get_socket_path, NiriClient};
pub use error::NiriError;
pub use events::{
    filter_focus_event, is_focus_relevant, EventReaderHandle, NiriEventDispatcher,
    NiriEventReceiver, NiriEventStream, WindowProvider, DEFAULT_CHANNEL_BUFFER,
};
pub use types::{FocusChangeEvent, FocusedWindow, NiriEvent, Window, WorkspaceChangeEvent, WorkspaceInfo};
