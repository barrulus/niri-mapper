//! Niri IPC event stream implementation
//!
//! This module provides the `NiriEventStream` for subscribing to real-time
//! compositor events from niri. Unlike `NiriClient` which uses request/response,
//! the event stream is a one-way connection that continuously receives events.
//!
//! ## Protocol
//!
//! 1. Connect to the niri socket (separate connection from NiriClient)
//! 2. Send `Request::EventStream` as JSON + newline
//! 3. Receive initial `Ok(Handled)` response
//! 4. Continuously receive `Event` messages (one JSON per line)
//!
//! Note: After sending `Request::EventStream`, the socket only receives events;
//! no more request/reply interactions are possible on this connection.
//!
//! ## Reconnection Logic (Task 040-3.8)
//!
//! The event stream handles disconnections gracefully:
//!
//! - On EOF or connection reset: attempts reconnection with exponential backoff
//! - Backoff starts at 500ms, doubles each retry, caps at 10 seconds
//! - Default max retries: 5 attempts
//! - If reconnection fails: IPC features are disabled, daemon continues running
//!
//! This ensures niri restarts do not crash the niri-mapper daemon.
//!
//! ## Architecture
//!
//! ```text
//! +-----------------+      +--------+      +---------------+
//! | NiriEventStream | ---> | mpsc   | ---> | Daemon Event  |
//! | (reader task)   |      | channel|      | Loop          |
//! +-----------------+      +--------+      +---------------+
//! ```
//!
//! The `NiriEventDispatcher` provides channel-based architecture where:
//! 1. An async reader task reads events from the niri socket
//! 2. Events are filtered and sent through an mpsc channel
//! 3. The daemon's main event loop receives events from the channel
//!
//! ## Testing
//!
//! See the `tests` module for manual test procedures covering:
//! - Basic event stream connection
//! - Focus change event delivery
//! - Workspace activation events
//! - Reconnection after niri restart
//! - Max retries exceeded behavior
//! - Direct IPC testing with socat

use std::path::PathBuf;

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use super::error::NiriError;
use super::client::get_socket_path;
use super::types::NiriEvent;

/// Default number of reconnection retry attempts for event stream
const DEFAULT_MAX_RETRIES: u32 = 5;

/// Initial delay between retry attempts (500ms)
const INITIAL_RETRY_DELAY_MS: u64 = 500;

/// Maximum delay between retry attempts (10 seconds)
const MAX_RETRY_DELAY_MS: u64 = 10_000;

/// Event stream for receiving real-time compositor events from niri
///
/// This struct manages a dedicated socket connection for event streaming.
/// Unlike `NiriClient`, which uses request/response patterns, `NiriEventStream`
/// is optimized for continuous event delivery.
///
/// # Connection Lifecycle
///
/// 1. `connect()` establishes a new socket connection
/// 2. Sends `Request::EventStream` to initiate subscription
/// 3. Validates the initial `Ok(Handled)` response
/// 4. The stream is then ready to receive events via `next_event()` (task 3.2)
///
/// # Example
///
/// ```ignore
/// let mut stream = NiriEventStream::connect().await?;
/// // Stream is now ready to receive events
/// // Use next_event() to read events (implemented in task 3.2)
/// ```
#[derive(Debug)]
pub struct NiriEventStream {
    /// Buffered reader for line-based JSON event reading
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,

    /// The socket path (stored for error messages and reconnection)
    #[allow(dead_code)]
    socket_path: PathBuf,
}

impl NiriEventStream {
    /// Connect to niri and initiate event stream subscription
    ///
    /// This establishes a new socket connection (separate from any `NiriClient`)
    /// and sends `Request::EventStream` to start receiving compositor events.
    ///
    /// # Protocol
    ///
    /// 1. Discovers socket path from `$NIRI_SOCKET`
    /// 2. Connects to the Unix socket
    /// 3. Sends `"EventStream"` + newline
    /// 4. Reads and validates the initial response (`Ok(Handled)`)
    /// 5. Returns the stream ready for event reading
    ///
    /// # Errors
    ///
    /// Returns `NiriError::SocketNotSet` if `$NIRI_SOCKET` is not set.
    /// Returns `NiriError::SocketNotFound` if the socket path doesn't exist.
    /// Returns `NiriError::ConnectionFailed` if the connection fails.
    /// Returns `NiriError::SendFailed` if sending the request fails.
    /// Returns `NiriError::ReceiveFailed` if reading the response fails.
    /// Returns `NiriError::DeserializeFailed` if the response cannot be parsed.
    /// Returns `NiriError::NiriError` if niri returns an error response.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let stream = NiriEventStream::connect().await?;
    /// println!("Event stream connected, ready to receive events");
    /// ```
    pub async fn connect() -> Result<Self, NiriError> {
        // Discover the socket path from the environment
        let socket_path = get_socket_path()?;

        // Attempt to connect to the Unix socket
        let socket = UnixStream::connect(&socket_path)
            .await
            .map_err(|e| NiriError::ConnectionFailed {
                path: socket_path.clone(),
                source: e,
            })?;

        // Split the socket into read and write halves
        // We need owned halves since we'll keep the reader for the lifetime of the stream
        let (read_half, mut write_half) = socket.into_split();

        // Send the EventStream request
        let request = niri_ipc::Request::EventStream;
        let request_json =
            serde_json::to_string(&request).map_err(NiriError::SerializeFailed)?;

        write_half
            .write_all(request_json.as_bytes())
            .await
            .map_err(NiriError::SendFailed)?;
        write_half
            .write_all(b"\n")
            .await
            .map_err(NiriError::SendFailed)?;
        write_half.flush().await.map_err(NiriError::SendFailed)?;

        // Create buffered reader for the read half
        let mut reader = BufReader::new(read_half);

        // Read and validate the initial response
        // Niri should respond with Ok(Handled) to confirm the event stream subscription
        let mut response_line = String::new();
        let bytes_read = reader
            .read_line(&mut response_line)
            .await
            .map_err(NiriError::ReceiveFailed)?;

        if bytes_read == 0 {
            return Err(NiriError::ConnectionClosed);
        }

        // Deserialize the reply (Result<Response, String>)
        let reply: niri_ipc::Reply =
            serde_json::from_str(&response_line).map_err(NiriError::DeserializeFailed)?;

        // Validate the response - should be Ok(Handled) for EventStream
        match reply {
            Ok(niri_ipc::Response::Handled) => {
                // Success - event stream is now active
            }
            Ok(other) => {
                // Unexpected response type
                return Err(NiriError::NiriError {
                    message: format!(
                        "Unexpected response to EventStream request: expected Handled, got {:?}",
                        std::mem::discriminant(&other)
                    ),
                });
            }
            Err(message) => {
                return Err(NiriError::NiriError { message });
            }
        }

        Ok(Self {
            reader,
            socket_path,
        })
    }

    /// Connect to niri with retry logic and exponential backoff
    ///
    /// Retries connection up to `max_retries` times with exponential backoff.
    /// Useful for handling niri restarts or transient socket unavailability.
    ///
    /// # Backoff Strategy
    ///
    /// - Initial delay: 500ms
    /// - Each retry: delay *= 2
    /// - Maximum delay: 10 seconds (capped)
    ///
    /// # Arguments
    ///
    /// * `max_retries` - Maximum number of connection attempts (0 means try once with no retries)
    ///
    /// # Errors
    ///
    /// Returns `NiriError::MaxRetriesExceeded` if all attempts fail.
    /// The error contains the total number of attempts made.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Try up to 5 times with exponential backoff
    /// let stream = NiriEventStream::connect_with_retry(5).await?;
    /// ```
    pub async fn connect_with_retry(max_retries: u32) -> Result<Self, NiriError> {
        let mut attempt = 0;
        let mut delay_ms = INITIAL_RETRY_DELAY_MS;
        let mut last_error: Option<NiriError> = None;

        loop {
            attempt += 1;

            match Self::connect().await {
                Ok(stream) => {
                    if attempt > 1 {
                        info!(
                            "Niri event stream reconnected after {} attempt(s)",
                            attempt
                        );
                    }
                    return Ok(stream);
                }
                Err(e) => {
                    // For non-retryable errors (env var not set), fail immediately
                    if matches!(e, NiriError::SocketNotSet) {
                        return Err(e);
                    }

                    last_error = Some(e);

                    // Check if we've exhausted retries
                    if attempt > max_retries {
                        break;
                    }

                    // Log the retry attempt
                    warn!(
                        attempt = attempt,
                        max_retries = max_retries,
                        delay_ms = delay_ms,
                        "Niri event stream connection failed, retrying..."
                    );

                    // Wait before retrying
                    sleep(Duration::from_millis(delay_ms)).await;

                    // Exponential backoff with cap
                    delay_ms = (delay_ms * 2).min(MAX_RETRY_DELAY_MS);
                }
            }
        }

        // All retries exhausted
        warn!(
            attempts = attempt,
            last_error = ?last_error,
            "Failed to connect to niri event stream after all retry attempts"
        );

        Err(NiriError::MaxRetriesExceeded { attempts: attempt })
    }

    /// Connect to niri with default retry settings
    ///
    /// Uses the default maximum retries (5 attempts) with exponential backoff.
    /// This is a convenience method for the common case.
    ///
    /// # Errors
    ///
    /// Returns `NiriError::MaxRetriesExceeded` if all attempts fail.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let stream = NiriEventStream::connect_with_default_retry().await?;
    /// ```
    pub async fn connect_with_default_retry() -> Result<Self, NiriError> {
        Self::connect_with_retry(DEFAULT_MAX_RETRIES).await
    }

    /// Get a reference to the underlying buffered reader
    ///
    /// This is useful for implementing event reading in task 3.2.
    #[allow(dead_code)]
    pub(crate) fn reader(&mut self) -> &mut BufReader<tokio::net::unix::OwnedReadHalf> {
        &mut self.reader
    }

    /// Read the next event from the event stream
    ///
    /// This method asynchronously reads a single JSON line from the niri event stream
    /// and deserializes it to a `niri_ipc::Event`. Each call blocks until an event
    /// is available or the connection is closed.
    ///
    /// # Protocol
    ///
    /// Events are sent as single-line JSON messages, one per line. This method:
    /// 1. Reads a complete line from the buffered reader
    /// 2. Deserializes the JSON to `niri_ipc::Event`
    /// 3. Returns the event
    ///
    /// # Errors
    ///
    /// Returns `NiriError::ConnectionClosed` if the connection is closed (EOF).
    /// Returns `NiriError::ReceiveFailed` if reading from the socket fails.
    /// Returns `NiriError::DeserializeFailed` if the JSON cannot be parsed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut stream = NiriEventStream::connect().await?;
    /// loop {
    ///     match stream.next_event().await {
    ///         Ok(event) => {
    ///             println!("Received event: {:?}", event);
    ///         }
    ///         Err(NiriError::ConnectionClosed) => {
    ///             println!("Event stream closed");
    ///             break;
    ///         }
    ///         Err(e) => {
    ///             eprintln!("Error reading event: {}", e);
    ///             break;
    ///         }
    ///     }
    /// }
    /// ```
    pub async fn next_event(&mut self) -> Result<niri_ipc::Event, NiriError> {
        let mut line = String::new();

        let bytes_read = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(NiriError::ReceiveFailed)?;

        if bytes_read == 0 {
            return Err(NiriError::ConnectionClosed);
        }

        let event: niri_ipc::Event =
            serde_json::from_str(&line).map_err(NiriError::DeserializeFailed)?;

        Ok(event)
    }

    /// Read the next focus-relevant event from the event stream
    ///
    /// This method filters the raw event stream to only return events relevant
    /// to focus-based remapping. Events not related to focus changes (like
    /// layout changes, keyboard layouts, window geometry updates, etc.) are
    /// silently discarded.
    ///
    /// # Window Context
    ///
    /// The `windows` parameter provides the current window list for looking up
    /// window details when a `WindowFocusChanged` event is received. The caller
    /// is responsible for keeping this list up-to-date (e.g., by tracking
    /// `WindowOpenedOrChanged` and `WindowClosed` events separately, or by
    /// querying the window list via `NiriClient`).
    ///
    /// # Focus-Relevant Events
    ///
    /// - `WindowFocusChanged` -> `NiriEvent::FocusChanged`
    /// - `WorkspaceActivated` -> `NiriEvent::WorkspaceActivated`
    ///
    /// # Ignored Events
    ///
    /// - `WindowOpenedOrChanged` (window metadata changes)
    /// - `WindowClosed`
    /// - `WorkspacesChanged`
    /// - `KeyboardLayoutsChanged`
    /// - `KeyboardLayoutSwitched`
    /// - All other compositor state events
    ///
    /// # Errors
    ///
    /// Returns `NiriError::ConnectionClosed` if the connection is closed (EOF).
    /// Returns `NiriError::ReceiveFailed` if reading from the socket fails.
    /// Returns `NiriError::DeserializeFailed` if the JSON cannot be parsed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut stream = NiriEventStream::connect().await?;
    /// let mut windows = Vec::new(); // Maintain window list from NiriClient
    ///
    /// loop {
    ///     match stream.next_focus_event(&windows).await {
    ///         Ok(NiriEvent::FocusChanged(event)) => {
    ///             if let Some(window) = event.window {
    ///                 println!("Focus changed to: {}", window.app_id);
    ///             } else {
    ///                 println!("No window focused");
    ///             }
    ///         }
    ///         Ok(NiriEvent::WorkspaceActivated(event)) => {
    ///             println!("Workspace {} activated", event.workspace_id);
    ///         }
    ///         Err(NiriError::ConnectionClosed) => {
    ///             println!("Event stream closed");
    ///             break;
    ///         }
    ///         Err(e) => {
    ///             eprintln!("Error: {}", e);
    ///             break;
    ///         }
    ///     }
    /// }
    /// ```
    pub async fn next_focus_event(
        &mut self,
        windows: &[niri_ipc::Window],
    ) -> Result<NiriEvent, NiriError> {
        loop {
            let raw_event = self.next_event().await?;

            // Use the filtering logic from NiriEvent to convert/filter the event
            if let Some(focus_event) = NiriEvent::from_niri_event(raw_event, windows) {
                return Ok(focus_event);
            }
            // Event not relevant to focus - continue reading
        }
    }
}

/// Check if a niri event is focus-relevant
///
/// This is a lightweight check that only examines the event type without
/// performing any conversion. Use this when you need to quickly filter
/// events without the overhead of conversion.
///
/// # Returns
///
/// Returns `true` if the event is one of:
/// - `WindowFocusChanged`
/// - `WorkspaceActivated`
///
/// Returns `false` for all other events.
///
/// # Example
///
/// ```ignore
/// if is_focus_relevant(&event) {
///     // Process the event
/// } else {
///     // Ignore the event
/// }
/// ```
pub fn is_focus_relevant(event: &niri_ipc::Event) -> bool {
    matches!(
        event,
        niri_ipc::Event::WindowFocusChanged { .. }
            | niri_ipc::Event::WorkspaceActivated { .. }
    )
}

/// Filter a niri event and convert it to an internal event type
///
/// This is a convenience function that wraps `NiriEvent::from_niri_event()`.
/// It's provided as a standalone function for cases where you have an event
/// and want to filter/convert it without going through the event stream.
///
/// # Arguments
///
/// * `event` - The raw niri IPC event to filter
/// * `windows` - Current window list for looking up window details
///
/// # Returns
///
/// Returns `Some(NiriEvent)` for focus-relevant events, or `None` for
/// events that should be ignored.
///
/// # Example
///
/// ```ignore
/// let raw_event = stream.next_event().await?;
/// if let Some(focus_event) = filter_focus_event(raw_event, &windows) {
///     handle_focus_event(focus_event);
/// }
/// ```
pub fn filter_focus_event(
    event: niri_ipc::Event,
    windows: &[niri_ipc::Window],
) -> Option<NiriEvent> {
    NiriEvent::from_niri_event(event, windows)
}

// =============================================================================
// Event Dispatch Channel
// =============================================================================

use tokio::sync::mpsc;

/// Default channel buffer size for event dispatch
///
/// This determines how many events can be buffered before the sender
/// blocks. A moderate size allows for brief bursts of events while
/// preventing unbounded memory growth.
pub const DEFAULT_CHANNEL_BUFFER: usize = 64;

/// Event dispatcher that decouples event reading from event handling
///
/// `NiriEventDispatcher` provides a channel-based architecture where:
/// 1. An async reader task reads events from the niri socket
/// 2. Events are filtered and sent through an mpsc channel
/// 3. The daemon's main event loop receives events from the channel
///
/// This separation allows the event reader to run independently, making
/// the system more resilient to processing delays and enabling clean
/// shutdown semantics.
///
/// # Architecture
///
/// ```text
/// +-----------------+      +--------+      +---------------+
/// | NiriEventStream | ---> | mpsc   | ---> | Daemon Event  |
/// | (reader task)   |      | channel|      | Loop          |
/// +-----------------+      +--------+      +---------------+
/// ```
///
/// # Example
///
/// ```ignore
/// // Create dispatcher with default buffer size
/// let (dispatcher, rx) = NiriEventDispatcher::new(64);
///
/// // Spawn the reader task
/// let windows = Arc::new(RwLock::new(Vec::new()));
/// let handle = dispatcher.spawn_reader(windows).await?;
///
/// // Receive events in the main loop
/// while let Some(event) = rx.recv().await {
///     match event {
///         NiriEvent::FocusChanged(e) => { /* handle focus change */ }
///         NiriEvent::WorkspaceActivated(e) => { /* handle workspace change */ }
///     }
/// }
///
/// // Reader task will shut down when all senders are dropped
/// handle.await?;
/// ```
#[derive(Debug)]
pub struct NiriEventDispatcher {
    /// Sender half of the event channel
    sender: mpsc::Sender<NiriEvent>,
}

/// Receiver for NiriEvents from the dispatcher
///
/// This is the receiving end of the event channel. The daemon's main
/// event loop should await on this receiver to process focus events.
pub type NiriEventReceiver = mpsc::Receiver<NiriEvent>;

/// Handle to a spawned event reader task
///
/// This handle can be used to await the reader task's completion or
/// to check if it's still running. The task will complete when:
/// - The niri connection is closed
/// - An unrecoverable error occurs
/// - The receiver is dropped (all events would be discarded)
pub type EventReaderHandle = tokio::task::JoinHandle<Result<(), NiriError>>;

impl NiriEventDispatcher {
    /// Create a new event dispatcher with the specified buffer size
    ///
    /// Returns a tuple of the dispatcher and the event receiver. The receiver
    /// should be held by the daemon's main event loop.
    ///
    /// # Arguments
    ///
    /// * `buffer_size` - Maximum number of events to buffer. Use
    ///   `DEFAULT_CHANNEL_BUFFER` for the recommended default.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (dispatcher, rx) = NiriEventDispatcher::new(DEFAULT_CHANNEL_BUFFER);
    /// ```
    pub fn new(buffer_size: usize) -> (Self, NiriEventReceiver) {
        let (sender, receiver) = mpsc::channel(buffer_size);
        (Self { sender }, receiver)
    }

    /// Spawn an event reader task that reads from niri and dispatches events
    ///
    /// This method connects to the niri event stream and spawns a background
    /// task that:
    /// 1. Reads raw events from the niri socket
    /// 2. Filters events to only focus-relevant ones
    /// 3. Sends filtered events through the channel
    ///
    /// The task runs until one of these conditions:
    /// - The niri connection is closed (`NiriError::ConnectionClosed`)
    /// - An unrecoverable error occurs
    /// - The receiver is dropped (returns `Ok(())` in this case)
    ///
    /// # Arguments
    ///
    /// * `windows` - Shared window list for resolving window IDs to window info.
    ///   This should be kept up-to-date by the caller (e.g., by handling
    ///   `WindowOpenedOrChanged` and `WindowClosed` events in a separate task).
    ///
    /// # Returns
    ///
    /// Returns `Ok(EventReaderHandle)` if the connection was established and
    /// the task was spawned. Returns `Err(NiriError)` if the initial connection
    /// fails.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let windows = Arc::new(RwLock::new(client.get_windows().await?));
    /// let handle = dispatcher.spawn_reader(windows).await?;
    ///
    /// // The reader is now running in the background
    /// // Use rx.recv() to receive events
    /// ```
    pub async fn spawn_reader<W>(self, windows: W) -> Result<EventReaderHandle, NiriError>
    where
        W: WindowProvider + Send + 'static,
    {
        // Connect to the event stream
        let stream = NiriEventStream::connect().await?;

        // Spawn the reader task
        let handle = tokio::spawn(async move {
            self.run_reader_loop(stream, windows).await
        });

        Ok(handle)
    }

    /// Internal event reading loop with reconnection support
    ///
    /// This method runs the core event reading and dispatching logic.
    /// It's separated from `spawn_reader` for testability and clarity.
    ///
    /// ## Reconnection Behavior (Task 040-3.8)
    ///
    /// When the event stream is disconnected (EOF or connection closed):
    /// 1. A warning is logged indicating disconnection
    /// 2. Reconnection is attempted with exponential backoff
    /// 3. If reconnection succeeds, event reading continues
    /// 4. If max retries are exhausted, this function returns and IPC features
    ///    are disabled (the daemon continues running without niri IPC)
    ///
    /// This ensures that niri restarts do not crash the niri-mapper daemon.
    async fn run_reader_loop<W>(
        self,
        mut stream: NiriEventStream,
        windows: W,
    ) -> Result<(), NiriError>
    where
        W: WindowProvider,
    {
        loop {
            // Get current window list from the provider
            let window_list = windows.get_windows();

            // Read and filter the next focus-relevant event
            let event = match stream.next_focus_event(&window_list).await {
                Ok(event) => event,
                Err(NiriError::ConnectionClosed) => {
                    // Connection closed (EOF) - attempt reconnection
                    warn!(
                        "Niri event stream disconnected (EOF). \
                         Attempting to reconnect..."
                    );

                    match Self::attempt_reconnection().await {
                        Ok(new_stream) => {
                            info!("Niri event stream reconnected successfully");
                            stream = new_stream;
                            continue;
                        }
                        Err(e) => {
                            // Max retries exhausted - IPC features will be disabled
                            warn!(
                                "Failed to reconnect to niri event stream: {}. \
                                 Niri IPC features will be unavailable. \
                                 Daemon will continue running without niri integration.",
                                e
                            );
                            return Err(e);
                        }
                    }
                }
                Err(NiriError::ReceiveFailed(ref io_err))
                    if io_err.kind() == std::io::ErrorKind::ConnectionReset
                        || io_err.kind() == std::io::ErrorKind::BrokenPipe =>
                {
                    // Connection reset or broken pipe - attempt reconnection
                    warn!(
                        "Niri event stream connection lost ({}). \
                         Attempting to reconnect...",
                        io_err.kind()
                    );

                    match Self::attempt_reconnection().await {
                        Ok(new_stream) => {
                            info!("Niri event stream reconnected successfully");
                            stream = new_stream;
                            continue;
                        }
                        Err(e) => {
                            warn!(
                                "Failed to reconnect to niri event stream: {}. \
                                 Niri IPC features will be unavailable.",
                                e
                            );
                            return Err(e);
                        }
                    }
                }
                Err(e) => {
                    // Other errors - propagate without retry
                    warn!("Niri event stream error: {}", e);
                    return Err(e);
                }
            };

            // Send the event through the channel
            // If the receiver is dropped, we should shut down gracefully
            if self.sender.send(event).await.is_err() {
                // Receiver dropped - no one is listening
                debug!("Niri event receiver dropped, shutting down event reader");
                return Ok(());
            }
        }
    }

    /// Attempt to reconnect to the niri event stream with exponential backoff
    ///
    /// This is used when the event stream is disconnected (e.g., niri restarted).
    /// Uses the default retry count (5 attempts) with exponential backoff.
    async fn attempt_reconnection() -> Result<NiriEventStream, NiriError> {
        NiriEventStream::connect_with_default_retry().await
    }

    /// Get a clone of the sender for testing or additional dispatch points
    ///
    /// This is primarily useful for testing scenarios where you want to
    /// inject synthetic events into the event channel.
    #[cfg(test)]
    pub fn sender(&self) -> mpsc::Sender<NiriEvent> {
        self.sender.clone()
    }
}

/// Trait for providing the current window list
///
/// This trait abstracts how the window list is accessed, allowing
/// different implementations for production vs testing:
///
/// - In production: Use `Arc<RwLock<Vec<niri_ipc::Window>>>`
/// - In tests: Use a simple `Vec<niri_ipc::Window>`
///
/// # Example
///
/// ```ignore
/// // Production usage with shared state
/// let windows = Arc::new(RwLock::new(Vec::new()));
/// dispatcher.spawn_reader(windows).await?;
///
/// // Test usage with static data
/// let windows = vec![test_window(1, "app", "title")];
/// dispatcher.spawn_reader(windows).await?;
/// ```
pub trait WindowProvider: Send + Sync {
    /// Get a snapshot of the current window list
    fn get_windows(&self) -> Vec<niri_ipc::Window>;
}

// Implementation for shared mutable state (production use case)
impl WindowProvider for std::sync::Arc<std::sync::RwLock<Vec<niri_ipc::Window>>> {
    fn get_windows(&self) -> Vec<niri_ipc::Window> {
        self.read().unwrap().clone()
    }
}

// Implementation for tokio's RwLock (async-friendly)
impl WindowProvider for std::sync::Arc<tokio::sync::RwLock<Vec<niri_ipc::Window>>> {
    fn get_windows(&self) -> Vec<niri_ipc::Window> {
        // Use blocking_read for sync context, or return empty if lock is contested
        // In practice, this should rarely block since we're the primary reader
        match self.try_read() {
            Ok(guard) => guard.clone(),
            Err(_) => Vec::new(), // Return empty if lock is contested
        }
    }
}

// Implementation for a simple Vec (testing use case)
impl WindowProvider for Vec<niri_ipc::Window> {
    fn get_windows(&self) -> Vec<niri_ipc::Window> {
        self.clone()
    }
}

// Implementation for empty window list (simplest case)
impl WindowProvider for () {
    fn get_windows(&self) -> Vec<niri_ipc::Window> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    //! # Smoke Test: Niri Event Stream (Task 040-3.9)
    //!
    //! This module contains unit tests and documents manual testing procedures
    //! for the niri event stream functionality.
    //!
    //! ## Overview
    //!
    //! The event stream provides real-time compositor events from niri. Key features:
    //! - Connects to `$NIRI_SOCKET` and sends `Request::EventStream`
    //! - Receives continuous `Event` messages (one JSON per line)
    //! - Filters events to focus-relevant ones (`WindowFocusChanged`, `WorkspaceActivated`)
    //! - Handles disconnection with exponential backoff reconnection
    //!
    //! ## Manual Test Procedures
    //!
    //! ### Prerequisites
    //!
    //! - Niri compositor running (verify with `echo $NIRI_SOCKET`)
    //! - Build niri-mapper: `cargo build -p niri-mapper-daemon`
    //!
    //! ### Test 1: Basic Event Stream Connection
    //!
    //! **Purpose**: Verify the daemon can connect to niri's event stream.
    //!
    //! **Steps**:
    //! 1. Start niri compositor (or ensure already running)
    //! 2. Run daemon with debug logging:
    //!    ```bash
    //!    RUST_LOG=debug cargo run -p niri-mapper-daemon
    //!    ```
    //! 3. Watch for log output indicating event stream connection:
    //!    ```
    //!    DEBUG niri_mapper_daemon::niri_ipc: Niri event stream connected
    //!    ```
    //!
    //! **Expected Result**: Daemon starts and logs successful event stream connection.
    //!
    //! ### Test 2: Focus Change Event Delivery
    //!
    //! **Purpose**: Verify focus change events are received and logged.
    //!
    //! **Steps**:
    //! 1. Start daemon with debug logging (see Test 1)
    //! 2. Open multiple windows (e.g., terminal, browser)
    //! 3. Switch focus between windows using Alt+Tab or mouse
    //! 4. Watch daemon logs for focus change events:
    //!    ```
    //!    DEBUG niri_mapper_daemon: Focus changed to: app_id="firefox", title="..."
    //!    DEBUG niri_mapper_daemon: Focus changed to: app_id="Alacritty", title="..."
    //!    ```
    //!
    //! **Expected Result**: Each focus change is logged with the new window's app_id and title.
    //!
    //! ### Test 3: Workspace Activation Events
    //!
    //! **Purpose**: Verify workspace switch events are received.
    //!
    //! **Steps**:
    //! 1. Start daemon with debug logging
    //! 2. Create multiple workspaces in niri
    //! 3. Switch between workspaces using keybinds
    //! 4. Watch for workspace activation events in logs:
    //!    ```
    //!    DEBUG niri_mapper_daemon: Workspace activated: id=2, focused=true
    //!    ```
    //!
    //! **Expected Result**: Workspace switches are logged.
    //!
    //! ### Test 4: Reconnection After Niri Restart
    //!
    //! **Purpose**: Verify the daemon handles niri restarts gracefully.
    //!
    //! **Steps**:
    //! 1. Start daemon with debug logging
    //! 2. Verify event stream is connected (see Test 1)
    //! 3. Restart niri compositor:
    //!    - Save work in all windows
    //!    - Run: `niri msg action quit` or use niri's restart mechanism
    //!    - Start niri again
    //! 4. Watch daemon logs for reconnection attempts:
    //!    ```
    //!    WARN niri_mapper_daemon::niri_ipc: Niri event stream disconnected (EOF). Attempting to reconnect...
    //!    WARN niri_mapper_daemon::niri_ipc: Niri event stream connection failed, retrying...
    //!    INFO niri_mapper_daemon::niri_ipc: Niri event stream reconnected after 3 attempt(s)
    //!    ```
    //! 5. After reconnection, verify focus events are received (switch windows)
    //!
    //! **Expected Result**:
    //! - Daemon logs warning on disconnection
    //! - Daemon attempts reconnection with exponential backoff (500ms, 1s, 2s, 4s, 8s capped at 10s)
    //! - After niri restarts, daemon reconnects and resumes event streaming
    //! - Daemon does NOT crash during niri restart
    //!
    //! ### Test 5: Max Retries Exceeded
    //!
    //! **Purpose**: Verify daemon handles permanent niri unavailability.
    //!
    //! **Steps**:
    //! 1. Start daemon with debug logging
    //! 2. Quit niri and do NOT restart it
    //! 3. Watch daemon logs for max retries exceeded:
    //!    ```
    //!    WARN niri_mapper_daemon::niri_ipc: Failed to reconnect to niri event stream after all retry attempts
    //!    WARN niri_mapper_daemon::niri_ipc: Niri IPC features will be unavailable. Daemon will continue running without niri integration.
    //!    ```
    //!
    //! **Expected Result**:
    //! - Daemon logs warning after max retries (5 by default)
    //! - Daemon continues running (keyboard remapping still works)
    //! - IPC features (focus tracking) are disabled but daemon is stable
    //!
    //! ### Test 6: Verify with socat (Direct IPC Test)
    //!
    //! **Purpose**: Verify niri's event stream works independently of niri-mapper.
    //!
    //! **Steps**:
    //! 1. Ensure niri is running
    //! 2. Test direct event stream connection:
    //!    ```bash
    //!    echo '"EventStream"' | socat - "$NIRI_SOCKET"
    //!    ```
    //! 3. You should see initial "Handled" response, then continuous events:
    //!    ```json
    //!    {"Ok":"Handled"}
    //!    {"WindowFocusChanged":{"id":12}}
    //!    {"WorkspaceActivated":{"id":1,"focused":true}}
    //!    ```
    //! 4. Switch windows/workspaces and observe new events
    //! 5. Ctrl+C to stop
    //!
    //! **Expected Result**: Events flow continuously; this validates niri's IPC is working.
    //!
    //! ## Backoff Strategy Details
    //!
    //! The reconnection logic uses exponential backoff:
    //! - Initial delay: 500ms (`INITIAL_RETRY_DELAY_MS`)
    //! - Each retry: delay *= 2
    //! - Maximum delay: 10 seconds (`MAX_RETRY_DELAY_MS`)
    //! - Default max retries: 5 (`DEFAULT_MAX_RETRIES`)
    //!
    //! Example timing for 5 retries:
    //! - Attempt 1: immediate
    //! - Attempt 2: after 500ms
    //! - Attempt 3: after 1000ms (1s)
    //! - Attempt 4: after 2000ms (2s)
    //! - Attempt 5: after 4000ms (4s)
    //! - Attempt 6 (if configured): after 8000ms (8s)
    //!
    //! ## Edge Cases Handled
    //!
    //! 1. **Socket not set**: Returns `NiriError::SocketNotSet` immediately (no retry)
    //! 2. **Connection refused**: Retries with backoff (niri not ready yet)
    //! 3. **EOF (clean disconnect)**: Attempts reconnection (niri restarted)
    //! 4. **Connection reset**: Attempts reconnection (niri crashed)
    //! 5. **Broken pipe**: Attempts reconnection (socket closed)
    //! 6. **JSON parse error**: Propagates error (protocol mismatch, no retry)
    //!
    //! ## Unit Tests
    //!
    //! The following unit tests verify code structure and helper functions.
    //! Integration tests requiring a running niri compositor should follow
    //! the manual test procedures above.

    #[test]
    fn test_niri_event_stream_struct_exists() {
        // Compile-time check that the struct and its methods exist
        fn _assert_connect_exists() {
            // This won't actually run, just validates the type signature
            let _: fn() -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<super::NiriEventStream, super::NiriError>>>,
            > = || Box::pin(super::NiriEventStream::connect());
        }
    }

    #[test]
    fn test_next_event_method_exists() {
        // Compile-time check that next_event method exists with correct signature
        fn _assert_next_event_exists(stream: &mut super::NiriEventStream) {
            // This won't actually run, just validates the type signature
            let _: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<niri_ipc::Event, super::NiriError>> + '_>,
            > = Box::pin(stream.next_event());
        }
    }

    #[test]
    fn test_next_focus_event_method_exists() {
        // Compile-time check that next_focus_event method exists with correct signature
        fn _assert_next_focus_event_exists(
            stream: &mut super::NiriEventStream,
            windows: &[niri_ipc::Window],
        ) {
            // This won't actually run, just validates the type signature
            let _: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<super::NiriEvent, super::NiriError>> + '_>,
            > = Box::pin(stream.next_focus_event(windows));
        }
    }

    #[test]
    fn test_is_focus_relevant_window_focus_changed() {
        let event = niri_ipc::Event::WindowFocusChanged { id: Some(42) };
        assert!(super::is_focus_relevant(&event));

        let event = niri_ipc::Event::WindowFocusChanged { id: None };
        assert!(super::is_focus_relevant(&event));
    }

    #[test]
    fn test_is_focus_relevant_workspace_activated() {
        let event = niri_ipc::Event::WorkspaceActivated { id: 1, focused: true };
        assert!(super::is_focus_relevant(&event));

        let event = niri_ipc::Event::WorkspaceActivated { id: 2, focused: false };
        assert!(super::is_focus_relevant(&event));
    }

    #[test]
    fn test_is_focus_relevant_ignores_window_opened() {
        let window = niri_ipc::Window {
            id: 1,
            title: Some("Test".to_string()),
            app_id: Some("test-app".to_string()),
            pid: None,
            workspace_id: Some(1),
            is_focused: false,
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
        };
        let event = niri_ipc::Event::WindowOpenedOrChanged { window };
        assert!(!super::is_focus_relevant(&event));
    }

    #[test]
    fn test_is_focus_relevant_ignores_window_closed() {
        let event = niri_ipc::Event::WindowClosed { id: 42 };
        assert!(!super::is_focus_relevant(&event));
    }

    #[test]
    fn test_filter_focus_event_passes_through() {
        let windows = vec![];
        let event = niri_ipc::Event::WindowFocusChanged { id: None };
        let result = super::filter_focus_event(event, &windows);
        assert!(result.is_some());
    }

    #[test]
    fn test_filter_focus_event_filters_irrelevant() {
        let windows = vec![];
        let event = niri_ipc::Event::WindowClosed { id: 42 };
        let result = super::filter_focus_event(event, &windows);
        assert!(result.is_none());
    }

    // =========================================================================
    // Event Dispatcher Tests
    // =========================================================================

    use super::{NiriEventDispatcher, WindowProvider, DEFAULT_CHANNEL_BUFFER};

    #[test]
    fn test_dispatcher_creation() {
        let (dispatcher, _rx) = NiriEventDispatcher::new(DEFAULT_CHANNEL_BUFFER);
        // Verify the dispatcher was created (compile-time check)
        let _ = dispatcher;
    }

    #[test]
    fn test_default_channel_buffer_is_reasonable() {
        // Buffer should be large enough for bursts but not excessive
        assert!(DEFAULT_CHANNEL_BUFFER >= 16);
        assert!(DEFAULT_CHANNEL_BUFFER <= 1024);
    }

    #[test]
    fn test_window_provider_for_unit() {
        let provider: () = ();
        let windows = provider.get_windows();
        assert!(windows.is_empty());
    }

    #[test]
    fn test_window_provider_for_vec() {
        let windows = vec![
            niri_ipc::Window {
                id: 1,
                title: Some("Test".to_string()),
                app_id: Some("test".to_string()),
                pid: None,
                workspace_id: Some(1),
                is_focused: false,
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
            },
        ];

        let result = windows.get_windows();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 1);
    }

    #[test]
    fn test_window_provider_for_arc_rwlock() {
        use std::sync::{Arc, RwLock};

        let windows = Arc::new(RwLock::new(vec![
            niri_ipc::Window {
                id: 42,
                title: Some("ArcTest".to_string()),
                app_id: Some("arc-test".to_string()),
                pid: None,
                workspace_id: Some(1),
                is_focused: false,
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
            },
        ]));

        let result = windows.get_windows();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 42);
    }

    #[tokio::test]
    async fn test_dispatcher_channel_communication() {
        use super::NiriEvent;

        let (dispatcher, mut rx) = NiriEventDispatcher::new(16);
        let sender = dispatcher.sender();

        // Send an event
        let event = NiriEvent::focus_changed(None);
        sender.send(event.clone()).await.unwrap();

        // Receive the event
        let received = rx.recv().await.unwrap();
        assert_eq!(received, event);
    }

    #[tokio::test]
    async fn test_dispatcher_channel_closes_on_drop() {
        let (dispatcher, mut rx) = NiriEventDispatcher::new(16);

        // Drop the dispatcher (and its sender)
        drop(dispatcher);

        // Receiver should return None
        let result = rx.recv().await;
        assert!(result.is_none());
    }
}
