//! Niri IPC client implementation
//!
//! This module provides the `NiriClient` for communicating with the niri compositor.
//! The client handles socket discovery, connection management, and the JSON protocol.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::sleep;
use tracing::warn;

use super::NiriError;

/// Default number of connection retry attempts
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Initial delay between retry attempts (100ms)
const INITIAL_RETRY_DELAY_MS: u64 = 100;

/// Maximum delay between retry attempts (1 second)
const MAX_RETRY_DELAY_MS: u64 = 1000;

/// Environment variable name for the niri socket path
const NIRI_SOCKET_ENV: &str = "NIRI_SOCKET";

/// Discover the niri IPC socket path from the environment
///
/// Reads the `NIRI_SOCKET` environment variable and validates that
/// the path exists. This is the standard way niri exposes its socket.
///
/// # Errors
///
/// Returns `NiriError::SocketNotSet` if `$NIRI_SOCKET` is not set.
/// Returns `NiriError::SocketNotFound` if the path doesn't exist.
///
/// # Example
///
/// ```ignore
/// let socket_path = get_socket_path()?;
/// println!("Niri socket at: {}", socket_path.display());
/// ```
pub fn get_socket_path() -> Result<PathBuf, NiriError> {
    // Read NIRI_SOCKET environment variable
    let socket_path_str = std::env::var(NIRI_SOCKET_ENV).map_err(|_| NiriError::SocketNotSet)?;

    let socket_path = PathBuf::from(&socket_path_str);

    // Validate the path exists
    if !socket_path.exists() {
        return Err(NiriError::SocketNotFound { path: socket_path });
    }

    Ok(socket_path)
}

/// Client for communicating with the niri compositor via IPC
///
/// The client connects to niri's Unix domain socket and provides methods
/// for querying compositor state and subscribing to events.
///
/// # Example
///
/// ```ignore
/// let client = NiriClient::connect().await?;
/// let focused = client.get_focused_window().await?;
/// ```
#[derive(Debug)]
pub struct NiriClient {
    /// The Unix socket connection to niri
    socket: UnixStream,
    /// The socket path (stored for error messages and reconnection)
    #[allow(dead_code)]
    socket_path: PathBuf,
}

impl NiriClient {
    /// Attempt to connect to the niri compositor's IPC socket
    ///
    /// This discovers the socket path from `$NIRI_SOCKET` and establishes
    /// a connection. Returns an error if niri is not running or the
    /// socket is inaccessible.
    ///
    /// # Errors
    ///
    /// Returns `NiriError::SocketNotSet` if `$NIRI_SOCKET` is not set.
    /// Returns `NiriError::SocketNotFound` if the socket path doesn't exist.
    /// Returns `NiriError::ConnectionFailed` if the connection fails.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let client = NiriClient::connect().await?;
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

        Ok(Self {
            socket,
            socket_path,
        })
    }

    /// Get a reference to the underlying socket
    ///
    /// This is useful for implementing read/write operations in later tasks.
    #[allow(dead_code)]
    pub(crate) fn socket(&self) -> &UnixStream {
        &self.socket
    }

    /// Get a mutable reference to the underlying socket
    ///
    /// This is useful for implementing read/write operations in later tasks.
    #[allow(dead_code)]
    pub(crate) fn socket_mut(&mut self) -> &mut UnixStream {
        &mut self.socket
    }

    /// Attempt to connect with retry logic and exponential backoff
    ///
    /// Retries connection up to `max_retries` times with exponential backoff.
    /// Useful for handling transient socket unavailability during niri startup.
    ///
    /// # Backoff Strategy
    ///
    /// - Initial delay: 100ms
    /// - Each retry: delay *= 2
    /// - Maximum delay: 1 second (capped)
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
    /// let client = NiriClient::connect_with_retry(5).await?;
    /// ```
    pub async fn connect_with_retry(max_retries: u32) -> Result<Self, NiriError> {
        let mut attempt = 0;
        let mut delay_ms = INITIAL_RETRY_DELAY_MS;
        let mut last_error: Option<NiriError> = None;

        loop {
            attempt += 1;

            match Self::connect().await {
                Ok(client) => return Ok(client),
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
                        "Niri IPC connection failed, retrying..."
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
            "Failed to connect to niri after all retry attempts"
        );

        Err(NiriError::MaxRetriesExceeded { attempts: attempt })
    }

    /// Attempt to connect with default retry settings
    ///
    /// Uses the default maximum retries (3 attempts) with exponential backoff.
    /// This is a convenience method for the common case.
    ///
    /// # Errors
    ///
    /// Returns `NiriError::MaxRetriesExceeded` if all attempts fail.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let client = NiriClient::connect_with_default_retry().await?;
    /// ```
    pub async fn connect_with_default_retry() -> Result<Self, NiriError> {
        Self::connect_with_retry(DEFAULT_MAX_RETRIES).await
    }

    /// Send a request to niri and receive a response
    ///
    /// This is the core IPC method. It serializes the request to JSON,
    /// writes it to the socket with a newline, reads the response line,
    /// and deserializes the reply.
    ///
    /// # Protocol
    ///
    /// Niri uses a JSON-over-newline protocol:
    /// 1. Client writes: `"Request"` + newline (e.g., `"Version"\n`)
    /// 2. Server responds: JSON `{"Ok":...}` or `{"Err":"..."}` + newline
    ///
    /// # Errors
    ///
    /// Returns `NiriError::SerializeFailed` if the request cannot be serialized.
    /// Returns `NiriError::SendFailed` if writing to the socket fails.
    /// Returns `NiriError::ReceiveFailed` if reading from the socket fails.
    /// Returns `NiriError::ConnectionClosed` if the socket closes unexpectedly.
    /// Returns `NiriError::DeserializeFailed` if the response cannot be parsed.
    /// Returns `NiriError::NiriError` if niri returns an error response.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use niri_ipc::{Request, Response};
    ///
    /// let mut client = NiriClient::connect().await?;
    /// let response = client.send_request(Request::Version).await?;
    /// if let Response::Version(version) = response {
    ///     println!("Niri version: {}", version);
    /// }
    /// ```
    pub async fn send_request(
        &mut self,
        request: niri_ipc::Request,
    ) -> Result<niri_ipc::Response, NiriError> {
        // Serialize the request to JSON
        let request_json =
            serde_json::to_string(&request).map_err(NiriError::SerializeFailed)?;

        // Write the request to the socket with a newline
        self.socket
            .write_all(request_json.as_bytes())
            .await
            .map_err(NiriError::SendFailed)?;
        self.socket
            .write_all(b"\n")
            .await
            .map_err(NiriError::SendFailed)?;
        self.socket.flush().await.map_err(NiriError::SendFailed)?;

        // Read the response line
        // We need to split the socket for reading since we need a BufReader
        let (read_half, _write_half) = self.socket.split();
        let mut reader = BufReader::new(read_half);
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

        // Extract the response or convert the error
        reply.map_err(|message| NiriError::NiriError { message })
    }

    /// Query the niri compositor version
    ///
    /// This is a convenience method that sends a `Request::Version` and
    /// extracts the version string from the response.
    ///
    /// # Errors
    ///
    /// Returns any error from `send_request()`, or panics if the response
    /// is not `Response::Version` (which should not happen with a valid niri).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut client = NiriClient::connect().await?;
    /// let version = client.get_version().await?;
    /// println!("Niri version: {}", version);
    /// ```
    pub async fn get_version(&mut self) -> Result<String, NiriError> {
        let response = self.send_request(niri_ipc::Request::Version).await?;

        match response {
            niri_ipc::Response::Version(version) => Ok(version),
            other => {
                // This should never happen with a well-behaved niri
                panic!(
                    "Unexpected response to Version request: {:?}",
                    std::mem::discriminant(&other)
                );
            }
        }
    }

    /// Query the currently focused window
    ///
    /// This is a convenience method that sends a `Request::FocusedWindow` and
    /// extracts the window information from the response.
    ///
    /// # Returns
    ///
    /// Returns `Ok(Some(FocusedWindow))` if a window is focused, containing
    /// the window's `app_id`, `title`, and `id`.
    ///
    /// Returns `Ok(None)` if no window is currently focused (e.g., when the
    /// desktop background is focused). This is not an error condition.
    ///
    /// # Errors
    ///
    /// Returns any error from `send_request()`, or panics if the response
    /// is not `Response::FocusedWindow` (which should not happen with a valid niri).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut client = NiriClient::connect().await?;
    /// match client.get_focused_window().await? {
    ///     Some(window) => println!("Focused: {} ({})", window.app_id, window.title),
    ///     None => println!("No window focused"),
    /// }
    /// ```
    pub async fn get_focused_window(
        &mut self,
    ) -> Result<Option<super::types::FocusedWindow>, NiriError> {
        let response = self.send_request(niri_ipc::Request::FocusedWindow).await?;

        match response {
            niri_ipc::Response::FocusedWindow(maybe_window) => {
                Ok(maybe_window.map(super::types::FocusedWindow::from))
            }
            other => {
                // This should never happen with a well-behaved niri
                panic!(
                    "Unexpected response to FocusedWindow request: {:?}",
                    std::mem::discriminant(&other)
                );
            }
        }
    }

    /// Query the list of workspaces from niri
    ///
    /// This is a convenience method that sends a `Request::Workspaces` and
    /// converts the response to a list of internal `WorkspaceInfo` types.
    ///
    /// # Returns
    ///
    /// Returns a vector of `WorkspaceInfo` containing all workspaces across
    /// all outputs. Each workspace includes its `id`, `name`, `is_active`,
    /// `is_focused`, and `output` fields.
    ///
    /// # Errors
    ///
    /// Returns any error from `send_request()`, or panics if the response
    /// is not `Response::Workspaces` (which should not happen with a valid niri).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut client = NiriClient::connect().await?;
    /// let workspaces = client.get_workspaces().await?;
    /// for ws in workspaces {
    ///     println!("Workspace {}: active={}, focused={}", ws.id, ws.is_active, ws.is_focused);
    /// }
    /// ```
    pub async fn get_workspaces(&mut self) -> Result<Vec<super::types::WorkspaceInfo>, NiriError> {
        let response = self.send_request(niri_ipc::Request::Workspaces).await?;

        match response {
            niri_ipc::Response::Workspaces(workspaces) => {
                Ok(workspaces
                    .into_iter()
                    .map(super::types::WorkspaceInfo::from)
                    .collect())
            }
            other => {
                // This should never happen with a well-behaved niri
                panic!(
                    "Unexpected response to Workspaces request: {:?}",
                    std::mem::discriminant(&other)
                );
            }
        }
    }

    /// Query the list of all windows from niri
    ///
    /// This is a convenience method that sends a `Request::Windows` and
    /// converts the response to a list of internal `Window` types.
    ///
    /// # Returns
    ///
    /// Returns a vector of `Window` containing all windows across all workspaces.
    /// Each window includes its `id`, `app_id`, `title`, and `workspace_id`.
    ///
    /// # Errors
    ///
    /// Returns any error from `send_request()`, or panics if the response
    /// is not `Response::Windows` (which should not happen with a valid niri).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut client = NiriClient::connect().await?;
    /// let windows = client.get_windows().await?;
    /// for window in windows {
    ///     println!("Window {}: {} (workspace {:?})", window.id, window.app_id, window.workspace_id);
    /// }
    /// ```
    pub async fn get_windows(&mut self) -> Result<Vec<super::types::Window>, NiriError> {
        let response = self.send_request(niri_ipc::Request::Windows).await?;

        match response {
            niri_ipc::Response::Windows(windows) => {
                Ok(windows.into_iter().map(super::types::Window::from).collect())
            }
            other => {
                // This should never happen with a well-behaved niri
                panic!(
                    "Unexpected response to Windows request: {:?}",
                    std::mem::discriminant(&other)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::Mutex;

    // Mutex to serialize tests that modify the NIRI_SOCKET environment variable.
    // Environment variables are global state, so tests modifying them must not run in parallel.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Test that missing NIRI_SOCKET environment variable produces SocketNotSet error
    #[test]
    fn test_socket_not_set_error() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // Save original value to restore after test (test isolation)
        let original = env::var(NIRI_SOCKET_ENV).ok();

        // Ensure NIRI_SOCKET is not set for this test
        env::remove_var(NIRI_SOCKET_ENV);

        let result = get_socket_path();

        // Restore original value
        if let Some(val) = original {
            env::set_var(NIRI_SOCKET_ENV, val);
        }

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, NiriError::SocketNotSet),
            "Expected SocketNotSet error, got: {:?}",
            err
        );

        // Verify error message is clear
        let error_message = format!("{}", err);
        assert!(
            error_message.contains("NIRI_SOCKET"),
            "Error message should mention NIRI_SOCKET: {}",
            error_message
        );
    }

    /// Test that non-existent socket path produces SocketNotFound error
    #[test]
    fn test_socket_not_found_error() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // Save original value to restore after test (test isolation)
        let original = env::var(NIRI_SOCKET_ENV).ok();
        let fake_path = "/tmp/nonexistent-niri-socket-12345";

        // Set NIRI_SOCKET to a path that doesn't exist
        env::set_var(NIRI_SOCKET_ENV, fake_path);

        let result = get_socket_path();

        // Restore original value
        if let Some(val) = original {
            env::set_var(NIRI_SOCKET_ENV, val);
        } else {
            env::remove_var(NIRI_SOCKET_ENV);
        }

        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            NiriError::SocketNotFound { path } => {
                assert_eq!(
                    path.to_str().unwrap(),
                    fake_path,
                    "Error should contain the invalid path"
                );
            }
            other => panic!("Expected SocketNotFound error, got: {:?}", other),
        }

        // Verify error message includes the path
        let error_message = format!("{}", err);
        assert!(
            error_message.contains(fake_path),
            "Error message should include the socket path: {}",
            error_message
        );
    }

    /// Test that valid socket path format is accepted (path exists)
    #[test]
    fn test_valid_socket_path_format() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // Save original value to restore after test (test isolation)
        let original = env::var(NIRI_SOCKET_ENV).ok();

        // Use /tmp as a valid existing path (it's a directory, not a socket,
        // but get_socket_path() only checks existence, not socket type)
        let valid_path = "/tmp";

        env::set_var(NIRI_SOCKET_ENV, valid_path);

        let result = get_socket_path();

        // Restore original value
        if let Some(val) = original {
            env::set_var(NIRI_SOCKET_ENV, val);
        } else {
            env::remove_var(NIRI_SOCKET_ENV);
        }

        assert!(
            result.is_ok(),
            "Valid existing path should be accepted: {:?}",
            result
        );
        assert_eq!(result.unwrap(), PathBuf::from(valid_path));
    }

    /// Test that empty socket path is rejected
    #[test]
    fn test_empty_socket_path_rejected() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // Save original value to restore after test (test isolation)
        let original = env::var(NIRI_SOCKET_ENV).ok();
        let empty_path = "";

        env::set_var(NIRI_SOCKET_ENV, empty_path);

        let result = get_socket_path();

        // Restore original value
        if let Some(val) = original {
            env::set_var(NIRI_SOCKET_ENV, val);
        } else {
            env::remove_var(NIRI_SOCKET_ENV);
        }

        // Empty path doesn't exist, so should return SocketNotFound
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), NiriError::SocketNotFound { .. }),
            "Empty path should fail with SocketNotFound"
        );
    }

    /// Test that connection errors are well-typed
    #[tokio::test]
    async fn test_connection_to_nonexistent_socket() {
        use std::fs;
        use tempfile::tempdir;

        let _guard = ENV_MUTEX.lock().unwrap();
        // Save original value to restore after test (test isolation)
        let original = env::var(NIRI_SOCKET_ENV).ok();

        // Create a temporary directory and a path within it
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let socket_path = temp_dir.path().join("test-niri.sock");

        // Create an empty file (not a socket) to test connection failure
        fs::write(&socket_path, "").expect("Failed to create dummy file");

        env::set_var(NIRI_SOCKET_ENV, socket_path.to_str().unwrap());

        let result = NiriClient::connect().await;

        // Restore original value
        if let Some(val) = original {
            env::set_var(NIRI_SOCKET_ENV, val);
        } else {
            env::remove_var(NIRI_SOCKET_ENV);
        }

        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            NiriError::ConnectionFailed { path, source } => {
                assert_eq!(path, &socket_path);
                // Source should be an IO error (connection refused or not a socket)
                assert!(
                    !source.to_string().is_empty(),
                    "Source error should have a message"
                );
            }
            other => panic!("Expected ConnectionFailed error, got: {:?}", other),
        }

        // Verify error message is descriptive
        let error_message = format!("{}", err);
        assert!(
            error_message.contains("Failed to connect"),
            "Error should mention connection failure: {}",
            error_message
        );
    }

    /// Test that retry logic immediately fails for SocketNotSet
    #[tokio::test]
    async fn test_retry_fails_immediately_for_socket_not_set() {
        let _guard = ENV_MUTEX.lock().unwrap();
        // Save original value to restore after test (test isolation)
        let original = env::var(NIRI_SOCKET_ENV).ok();

        // Ensure NIRI_SOCKET is not set
        env::remove_var(NIRI_SOCKET_ENV);

        // This should fail immediately without retrying
        let start = std::time::Instant::now();
        let result = NiriClient::connect_with_retry(5).await;
        let elapsed = start.elapsed();

        // Restore original value
        if let Some(val) = original {
            env::set_var(NIRI_SOCKET_ENV, val);
        }

        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), NiriError::SocketNotSet),
            "Should fail with SocketNotSet without retrying"
        );

        // Should complete quickly (no retries for SocketNotSet)
        assert!(
            elapsed.as_millis() < 100,
            "Should fail immediately without retry delays: {:?}",
            elapsed
        );
    }

    /// Test that MaxRetriesExceeded error contains attempt count
    #[tokio::test]
    async fn test_max_retries_exceeded_error() {
        use tempfile::tempdir;

        let _guard = ENV_MUTEX.lock().unwrap();
        // Save original value to restore after test (test isolation)
        let original = env::var(NIRI_SOCKET_ENV).ok();

        // Create a temp dir with a non-existent socket path
        let temp_dir = tempdir().expect("Failed to create temp dir");
        let socket_path = temp_dir.path().join("niri.sock");

        // Create the path but as a regular file (not a socket)
        std::fs::write(&socket_path, "").expect("Failed to create dummy file");

        env::set_var(NIRI_SOCKET_ENV, socket_path.to_str().unwrap());

        // Use 0 retries to speed up the test
        let result = NiriClient::connect_with_retry(0).await;

        // Restore original value
        if let Some(val) = original {
            env::set_var(NIRI_SOCKET_ENV, val);
        } else {
            env::remove_var(NIRI_SOCKET_ENV);
        }

        // With 0 retries, we try once then give up
        // But first attempt will fail with ConnectionFailed, which is retryable,
        // but since max_retries=0, we exceed retries after attempt 1
        assert!(result.is_err());
        match result.unwrap_err() {
            NiriError::MaxRetriesExceeded { attempts } => {
                assert_eq!(attempts, 1, "Should have attempted exactly once");
            }
            NiriError::ConnectionFailed { .. } => {
                // This is also acceptable - connection failed on first try
            }
            other => panic!(
                "Expected MaxRetriesExceeded or ConnectionFailed, got: {:?}",
                other
            ),
        }
    }

    // =========================================================================
    // Window/Workspace Query Smoke Tests
    // =========================================================================
    //
    // These tests verify the type conversion paths used by the query methods:
    // - get_focused_window() -> Option<FocusedWindow>
    // - get_workspaces() -> Vec<WorkspaceInfo>
    // - get_windows() -> Vec<Window>
    //
    // Since the actual methods require a live niri connection, we test the
    // type conversions that would be performed on niri responses.
    //
    // ## Manual Integration Test Procedure
    //
    // To verify full integration with a running niri compositor:
    //
    // 1. Prerequisites:
    //    - Running niri compositor session
    //    - $NIRI_SOCKET environment variable set
    //    - At least one window open
    //
    // 2. Build and run smoke test:
    //    ```bash
    //    cargo build -p niri-mapper-daemon
    //    # In a niri session:
    //    cargo test -p niri-mapper-daemon -- --ignored niri_integration
    //    ```
    //
    // 3. Manual verification with niri msg:
    //    ```bash
    //    # Compare our parsing with niri's own tool:
    //    niri msg focused-window
    //    niri msg workspaces
    //    niri msg windows
    //    ```
    //
    // 4. Expected behaviors:
    //    - get_focused_window(): Returns Some(window) when focused, None when desktop
    //    - get_workspaces(): Returns non-empty list, exactly one is_focused=true
    //    - get_windows(): Returns list of all windows (may be empty)
    //
    // =========================================================================

    /// Helper to create a niri_ipc::Window for testing
    fn make_test_window(
        id: u64,
        app_id: Option<&str>,
        title: Option<&str>,
        workspace_id: Option<u64>,
    ) -> niri_ipc::Window {
        niri_ipc::Window {
            id,
            title: title.map(String::from),
            app_id: app_id.map(String::from),
            pid: Some(12345),
            workspace_id,
            is_focused: true,
            is_urgent: false,
            is_floating: false,
            focus_timestamp: None,
            layout: niri_ipc::WindowLayout {
                pos_in_scrolling_layout: None,
                tile_size: (800.0, 600.0),
                window_size: (800, 600),
                tile_pos_in_workspace_view: Some((100.0, 100.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
        }
    }

    /// Helper to create a niri_ipc::Workspace for testing
    fn make_test_workspace(
        id: u64,
        name: Option<&str>,
        output: Option<&str>,
        is_active: bool,
        is_focused: bool,
    ) -> niri_ipc::Workspace {
        niri_ipc::Workspace {
            id,
            idx: 1,
            name: name.map(String::from),
            output: output.map(String::from),
            is_urgent: false,
            is_active,
            is_focused,
            active_window_id: Some(42),
        }
    }

    // -------------------------------------------------------------------------
    // FocusedWindow type conversion tests (simulates get_focused_window path)
    // -------------------------------------------------------------------------

    #[test]
    fn smoke_focused_window_with_all_fields() {
        // Simulates: Response::FocusedWindow(Some(window)) with all fields set
        let niri_window = make_test_window(42, Some("firefox"), Some("Mozilla Firefox"), Some(1));

        let focused: super::super::types::FocusedWindow = niri_window.into();

        assert_eq!(focused.id, 42);
        assert_eq!(focused.app_id, "firefox");
        assert_eq!(focused.title, "Mozilla Firefox");
    }

    #[test]
    fn smoke_focused_window_with_none_app_id() {
        // Some Wayland clients don't set app_id
        let niri_window = make_test_window(1, None, Some("Window Title"), Some(1));

        let focused: super::super::types::FocusedWindow = niri_window.into();

        assert_eq!(focused.id, 1);
        assert_eq!(focused.app_id, ""); // Defaults to empty string
        assert_eq!(focused.title, "Window Title");
    }

    #[test]
    fn smoke_focused_window_with_none_title() {
        // Some windows don't set a title
        let niri_window = make_test_window(2, Some("my-app"), None, Some(1));

        let focused: super::super::types::FocusedWindow = niri_window.into();

        assert_eq!(focused.id, 2);
        assert_eq!(focused.app_id, "my-app");
        assert_eq!(focused.title, ""); // Defaults to empty string
    }

    #[test]
    fn smoke_focused_window_with_all_none_optionals() {
        // Edge case: window with no app_id or title
        let niri_window = make_test_window(3, None, None, None);

        let focused: super::super::types::FocusedWindow = niri_window.into();

        assert_eq!(focused.id, 3);
        assert_eq!(focused.app_id, "");
        assert_eq!(focused.title, "");
    }

    #[test]
    fn smoke_focused_window_with_empty_strings() {
        // Some apps set empty strings instead of None
        let niri_window = make_test_window(4, Some(""), Some(""), Some(1));

        let focused: super::super::types::FocusedWindow = niri_window.into();

        assert_eq!(focused.id, 4);
        assert_eq!(focused.app_id, "");
        assert_eq!(focused.title, "");
    }

    // -------------------------------------------------------------------------
    // WorkspaceInfo type conversion tests (simulates get_workspaces path)
    // -------------------------------------------------------------------------

    #[test]
    fn smoke_workspace_with_all_fields() {
        // Simulates: Response::Workspaces with a fully-populated workspace
        let niri_ws = make_test_workspace(1, Some("main"), Some("DP-1"), true, true);

        let ws: super::super::types::WorkspaceInfo = niri_ws.into();

        assert_eq!(ws.id, 1);
        assert_eq!(ws.name, Some("main".to_string()));
        assert_eq!(ws.output, "DP-1");
        assert!(ws.is_active);
        assert!(ws.is_focused);
    }

    #[test]
    fn smoke_workspace_without_name() {
        // Workspaces often don't have names
        let niri_ws = make_test_workspace(2, None, Some("HDMI-1"), true, false);

        let ws: super::super::types::WorkspaceInfo = niri_ws.into();

        assert_eq!(ws.id, 2);
        assert_eq!(ws.name, None);
        assert_eq!(ws.output, "HDMI-1");
        assert!(ws.is_active);
        assert!(!ws.is_focused);
    }

    #[test]
    fn smoke_workspace_without_output() {
        // Unusual but handle gracefully: workspace with no output
        let niri_ws = make_test_workspace(3, Some("orphan"), None, false, false);

        let ws: super::super::types::WorkspaceInfo = niri_ws.into();

        assert_eq!(ws.id, 3);
        assert_eq!(ws.name, Some("orphan".to_string()));
        assert_eq!(ws.output, ""); // Defaults to empty string
        assert!(!ws.is_active);
        assert!(!ws.is_focused);
    }

    #[test]
    fn smoke_workspace_active_not_focused() {
        // Multi-monitor: workspace visible but not focused
        let niri_ws = make_test_workspace(4, Some("secondary"), Some("DP-2"), true, false);

        let ws: super::super::types::WorkspaceInfo = niri_ws.into();

        assert!(ws.is_active, "Should be active (visible on output)");
        assert!(!ws.is_focused, "Should not be focused (different output has focus)");
    }

    // -------------------------------------------------------------------------
    // Window type conversion tests (simulates get_windows path)
    // -------------------------------------------------------------------------

    #[test]
    fn smoke_window_with_all_fields() {
        // Simulates: Response::Windows with a fully-populated window
        let niri_window = make_test_window(100, Some("alacritty"), Some("Terminal"), Some(1));

        let window: super::super::types::Window = niri_window.into();

        assert_eq!(window.id, 100);
        assert_eq!(window.app_id, "alacritty");
        assert_eq!(window.title, "Terminal");
        assert_eq!(window.workspace_id, Some(1));
    }

    #[test]
    fn smoke_window_without_workspace() {
        // Window not assigned to a workspace (floating/special)
        let niri_window = make_test_window(200, Some("popup"), Some("Popup Window"), None);

        let window: super::super::types::Window = niri_window.into();

        assert_eq!(window.id, 200);
        assert_eq!(window.workspace_id, None);
    }

    #[test]
    fn smoke_window_with_none_optionals() {
        // Window with minimal information
        let niri_window = make_test_window(300, None, None, None);

        let window: super::super::types::Window = niri_window.into();

        assert_eq!(window.id, 300);
        assert_eq!(window.app_id, "");
        assert_eq!(window.title, "");
        assert_eq!(window.workspace_id, None);
    }

    // -------------------------------------------------------------------------
    // Empty response handling tests
    // -------------------------------------------------------------------------

    #[test]
    fn smoke_empty_windows_response() {
        // Simulates: Response::Windows(vec![]) - no windows open
        let niri_windows: Vec<niri_ipc::Window> = vec![];

        let windows: Vec<super::super::types::Window> =
            niri_windows.into_iter().map(Into::into).collect();

        assert!(windows.is_empty());
    }

    #[test]
    fn smoke_empty_workspaces_response() {
        // Simulates: Response::Workspaces(vec![]) - unusual but possible
        let niri_workspaces: Vec<niri_ipc::Workspace> = vec![];

        let workspaces: Vec<super::super::types::WorkspaceInfo> =
            niri_workspaces.into_iter().map(Into::into).collect();

        assert!(workspaces.is_empty());
    }

    // -------------------------------------------------------------------------
    // Multiple items response tests
    // -------------------------------------------------------------------------

    #[test]
    fn smoke_multiple_windows_response() {
        // Simulates: Response::Windows with multiple windows
        let niri_windows = vec![
            make_test_window(1, Some("firefox"), Some("Browser"), Some(1)),
            make_test_window(2, Some("alacritty"), Some("Terminal"), Some(1)),
            make_test_window(3, Some("code"), Some("Editor"), Some(2)),
        ];

        let windows: Vec<super::super::types::Window> =
            niri_windows.into_iter().map(Into::into).collect();

        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].id, 1);
        assert_eq!(windows[0].app_id, "firefox");
        assert_eq!(windows[1].id, 2);
        assert_eq!(windows[1].app_id, "alacritty");
        assert_eq!(windows[2].id, 3);
        assert_eq!(windows[2].app_id, "code");
    }

    #[test]
    fn smoke_multiple_workspaces_response() {
        // Simulates: Response::Workspaces with multiple workspaces
        let niri_workspaces = vec![
            make_test_workspace(1, Some("main"), Some("DP-1"), true, true),
            make_test_workspace(2, Some("secondary"), Some("DP-1"), false, false),
            make_test_workspace(3, None, Some("HDMI-1"), true, false),
        ];

        let workspaces: Vec<super::super::types::WorkspaceInfo> =
            niri_workspaces.into_iter().map(Into::into).collect();

        assert_eq!(workspaces.len(), 3);

        // Verify exactly one workspace is focused
        let focused_count = workspaces.iter().filter(|ws| ws.is_focused).count();
        assert_eq!(focused_count, 1, "Exactly one workspace should be focused");

        // Verify the focused one
        let focused = workspaces.iter().find(|ws| ws.is_focused).unwrap();
        assert_eq!(focused.id, 1);
        assert_eq!(focused.name, Some("main".to_string()));
    }

    // -------------------------------------------------------------------------
    // Return type structure tests
    // -------------------------------------------------------------------------

    #[test]
    fn smoke_focused_window_has_expected_fields() {
        // Verify FocusedWindow type has all required fields with correct types
        let focused = super::super::types::FocusedWindow {
            id: 1,
            app_id: "test".to_string(),
            title: "Test Window".to_string(),
        };

        // Type assertions via assignment
        let _id: u64 = focused.id;
        let _app_id: String = focused.app_id;
        let _title: String = focused.title;
    }

    #[test]
    fn smoke_workspace_info_has_expected_fields() {
        // Verify WorkspaceInfo type has all required fields with correct types
        let ws = super::super::types::WorkspaceInfo {
            id: 1,
            name: Some("test".to_string()),
            is_active: true,
            is_focused: true,
            output: "DP-1".to_string(),
        };

        // Type assertions via assignment
        let _id: u64 = ws.id;
        let _name: Option<String> = ws.name;
        let _is_active: bool = ws.is_active;
        let _is_focused: bool = ws.is_focused;
        let _output: String = ws.output;
    }

    #[test]
    fn smoke_window_has_expected_fields() {
        // Verify Window type has all required fields with correct types
        let window = super::super::types::Window {
            id: 1,
            app_id: "test".to_string(),
            title: "Test Window".to_string(),
            workspace_id: Some(1),
        };

        // Type assertions via assignment
        let _id: u64 = window.id;
        let _app_id: String = window.app_id;
        let _title: String = window.title;
        let _workspace_id: Option<u64> = window.workspace_id;
    }
}
