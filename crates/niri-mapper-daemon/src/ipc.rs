//! IPC server for daemon communication
//!
//! Provides a Unix domain socket for CLI and external tools to communicate
//! with the running daemon. Used for profile switching, status queries, etc.

use std::path::PathBuf;

use anyhow::{Context, Result};
use nix::libc;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

// ============================================================================
// IPC Message Types
// ============================================================================

/// Request messages sent from CLI/external tools to the daemon
///
/// These messages are serialized as JSON with a `type` field for discrimination:
/// - `{"type": "profile_switch", "device": "...", "profile": "..."}`
/// - `{"type": "profile_list", "device": "..."}`
/// - `{"type": "status"}`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Switch a device to a specific profile
    ProfileSwitch {
        /// Name of the device to switch
        device: String,
        /// Name of the profile to activate
        profile: String,
    },
    /// List available profiles for a device
    ProfileList {
        /// Name of the device to query
        device: String,
    },
    /// Query overall daemon status
    Status,
}

/// Response messages sent from the daemon back to CLI/external tools
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponse {
    /// Operation completed successfully
    Success {
        /// Optional message with additional details
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// List of profiles for a device
    ProfileList {
        /// Available profile names
        profiles: Vec<String>,
        /// Currently active profile name
        active: String,
    },
    /// Daemon status information
    Status {
        /// Status of each grabbed device
        devices: Vec<DeviceStatus>,
    },
    /// Error occurred while processing request
    Error {
        /// Error description
        message: String,
    },
}

/// Status information for a single grabbed device
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceStatus {
    /// Device name (as configured)
    pub name: String,
    /// Device path (e.g., /dev/input/event5)
    pub path: PathBuf,
    /// Currently active profile name
    pub active_profile: String,
    /// List of available profile names
    pub available_profiles: Vec<String>,
}

// ============================================================================
// IPC Server
// ============================================================================

/// IPC server for daemon communication via Unix domain socket
///
/// The socket is created at `$XDG_RUNTIME_DIR/niri-mapper.sock` if available,
/// or falls back to `/tmp/niri-mapper-$UID.sock` if XDG_RUNTIME_DIR is not set.
///
/// The socket file is automatically removed when the server is dropped.
pub struct IpcServer {
    listener: UnixListener,
    socket_path: PathBuf,
}

impl IpcServer {
    /// Create a new IPC server
    ///
    /// This will:
    /// 1. Determine the socket path (XDG_RUNTIME_DIR or fallback)
    /// 2. Remove any existing socket file (stale from previous run)
    /// 3. Create and bind the UnixListener
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The socket path cannot be determined
    /// - An existing socket file cannot be removed
    /// - The socket cannot be created or bound
    pub fn new() -> Result<Self> {
        let socket_path = Self::determine_socket_path();

        tracing::info!("IPC socket path: {}", socket_path.display());

        // Remove existing socket file if present (stale from previous run)
        if socket_path.exists() {
            tracing::debug!(
                "Removing stale socket file: {}",
                socket_path.display()
            );
            std::fs::remove_file(&socket_path).with_context(|| {
                format!(
                    "Failed to remove stale socket file: {}",
                    socket_path.display()
                )
            })?;
        }

        // Create and bind the Unix listener
        let listener = UnixListener::bind(&socket_path).with_context(|| {
            format!(
                "Failed to create IPC socket at {}",
                socket_path.display()
            )
        })?;

        tracing::info!("IPC server listening on {}", socket_path.display());

        Ok(Self {
            listener,
            socket_path,
        })
    }

    /// Accept an incoming connection
    ///
    /// This method asynchronously waits for a client to connect to the socket.
    ///
    /// # Errors
    ///
    /// Returns an error if accepting the connection fails.
    pub async fn accept(&self) -> Result<UnixStream> {
        let (stream, _addr) = self
            .listener
            .accept()
            .await
            .context("Failed to accept IPC connection")?;

        tracing::debug!("Accepted IPC connection");

        Ok(stream)
    }

    /// Get the socket path
    ///
    /// Returns the path where the IPC socket is bound.
    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Determine the socket path based on environment
    ///
    /// Prefers `$XDG_RUNTIME_DIR/niri-mapper.sock` if the environment variable
    /// is set, otherwise falls back to `/tmp/niri-mapper-$UID.sock`.
    fn determine_socket_path() -> PathBuf {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(runtime_dir).join("niri-mapper.sock")
        } else {
            tracing::warn!(
                "XDG_RUNTIME_DIR not set, using fallback socket path in /tmp"
            );
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/niri-mapper-{}.sock", uid))
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        // Clean up socket file on shutdown
        if self.socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                tracing::warn!(
                    "Failed to remove IPC socket file on shutdown: {}",
                    e
                );
            } else {
                tracing::debug!(
                    "Removed IPC socket file: {}",
                    self.socket_path.display()
                );
            }
        }
    }
}

// ============================================================================
// IPC Connection Handler
// ============================================================================

/// Handle an incoming IPC connection.
///
/// This function:
/// 1. Reads a line of JSON from the stream
/// 2. Parses it as an `IpcRequest`
/// 3. Executes the request via the provided handler
/// 4. Sends the `IpcResponse` back as JSON
///
/// # Arguments
///
/// * `stream` - The Unix stream from an accepted connection
/// * `handler` - A closure that processes `IpcRequest` and returns `IpcResponse`
///
/// # Errors
///
/// Returns an error if:
/// - Reading from the stream fails
/// - Writing to the stream fails
/// - JSON parsing/serialization fails
pub async fn handle_ipc_connection<F>(mut stream: UnixStream, handler: F) -> Result<()>
where
    F: FnOnce(IpcRequest) -> IpcResponse,
{
    // Split into read/write halves
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);

    // Read a line (JSON message terminated by newline)
    let mut line = String::new();
    let bytes_read = reader
        .read_line(&mut line)
        .await
        .context("Failed to read IPC request")?;

    if bytes_read == 0 {
        // Connection closed without sending data
        tracing::debug!("IPC connection closed without data");
        return Ok(());
    }

    // Trim whitespace (including trailing newline)
    let line = line.trim();

    tracing::debug!("Received IPC request: {}", line);

    // Parse the request
    let response = match serde_json::from_str::<IpcRequest>(line) {
        Ok(request) => {
            tracing::debug!("Parsed IPC request: {:?}", request);
            handler(request)
        }
        Err(e) => {
            tracing::warn!("Failed to parse IPC request: {}", e);
            IpcResponse::Error {
                message: format!("Invalid request: {}", e),
            }
        }
    };

    // Serialize and send the response
    let response_json = serde_json::to_string(&response)
        .context("Failed to serialize IPC response")?;

    tracing::debug!("Sending IPC response: {}", response_json);

    writer
        .write_all(response_json.as_bytes())
        .await
        .context("Failed to write IPC response")?;

    writer
        .write_all(b"\n")
        .await
        .context("Failed to write newline")?;

    writer.flush().await.context("Failed to flush IPC response")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use tempfile::TempDir;

    // ========================================================================
    // IPC Message Serialization Tests
    // ========================================================================

    #[test]
    fn test_request_profile_switch_serialization() {
        let request = IpcRequest::ProfileSwitch {
            device: "Keychron K3 Pro".to_string(),
            profile: "gaming".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"type":"profile_switch","device":"Keychron K3 Pro","profile":"gaming"}"#
        );

        // Round-trip
        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn test_request_profile_list_serialization() {
        let request = IpcRequest::ProfileList {
            device: "Keychron K3 Pro".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"type":"profile_list","device":"Keychron K3 Pro"}"#
        );

        // Round-trip
        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn test_request_status_serialization() {
        let request = IpcRequest::Status;
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"type":"status"}"#);

        // Round-trip
        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn test_response_success_serialization() {
        // Without message
        let response = IpcResponse::Success { message: None };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"type":"success"}"#);

        // With message
        let response = IpcResponse::Success {
            message: Some("Profile switched".to_string()),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"type":"success","message":"Profile switched"}"#);

        // Round-trip
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn test_response_profile_list_serialization() {
        let response = IpcResponse::ProfileList {
            profiles: vec!["default".to_string(), "gaming".to_string()],
            active: "default".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            json,
            r#"{"type":"profile_list","profiles":["default","gaming"],"active":"default"}"#
        );

        // Round-trip
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn test_response_status_serialization() {
        let response = IpcResponse::Status {
            devices: vec![DeviceStatus {
                name: "Keychron K3 Pro".to_string(),
                path: PathBuf::from("/dev/input/event5"),
                active_profile: "default".to_string(),
                available_profiles: vec!["default".to_string(), "gaming".to_string()],
            }],
        };
        let json = serde_json::to_string(&response).unwrap();
        // Verify it contains expected structure
        assert!(json.contains(r#""type":"status""#));
        assert!(json.contains(r#""name":"Keychron K3 Pro""#));
        assert!(json.contains(r#""path":"/dev/input/event5""#));
        assert!(json.contains(r#""active_profile":"default""#));

        // Round-trip
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn test_response_error_serialization() {
        let response = IpcResponse::Error {
            message: "Device not found".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"type":"error","message":"Device not found"}"#);

        // Round-trip
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, response);
    }

    #[test]
    fn test_request_deserialization_from_spec_format() {
        // Verify we can parse the exact JSON formats from the spec
        let json = r#"{"type": "profile_switch", "device": "Keychron K3 Pro", "profile": "gaming"}"#;
        let request: IpcRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(request, IpcRequest::ProfileSwitch { .. }));

        let json = r#"{"type": "profile_list", "device": "Keychron K3 Pro"}"#;
        let request: IpcRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(request, IpcRequest::ProfileList { .. }));

        let json = r#"{"type": "status"}"#;
        let request: IpcRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(request, IpcRequest::Status));
    }

    // ========================================================================
    // IPC Server Tests
    // ========================================================================

    #[tokio::test]
    async fn test_ipc_server_creation_and_cleanup() {
        // Use a temp directory for the socket
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_RUNTIME_DIR", temp_dir.path());

        let socket_path = temp_dir.path().join("niri-mapper.sock");

        // Create server
        let server = IpcServer::new().unwrap();
        assert_eq!(server.socket_path(), &socket_path);
        assert!(socket_path.exists());

        // Drop server and verify cleanup
        drop(server);
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    async fn test_ipc_server_removes_stale_socket() {
        // Use a temp directory for the socket
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_RUNTIME_DIR", temp_dir.path());

        let socket_path = temp_dir.path().join("niri-mapper.sock");

        // Create a stale socket file
        std::fs::write(&socket_path, "stale").unwrap();
        assert!(socket_path.exists());

        // Create server - should remove stale file and bind successfully
        let server = IpcServer::new().unwrap();
        assert!(socket_path.exists());

        drop(server);
    }

    #[tokio::test]
    async fn test_ipc_server_accept_connection() {
        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_RUNTIME_DIR", temp_dir.path());

        let server = IpcServer::new().unwrap();
        let socket_path = server.socket_path().clone();

        // Spawn a task to accept connections
        let accept_handle = tokio::spawn(async move {
            server.accept().await.unwrap()
        });

        // Connect from client side
        let _client = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap();

        // Server should have accepted the connection
        let _server_stream = accept_handle.await.unwrap();
    }

    // ========================================================================
    // CLI Profile Switching Integration Tests (Task 030-3.7.2)
    // ========================================================================
    //
    // These tests verify that the CLI can communicate with the daemon for
    // profile switching operations. They simulate the full request/response
    // flow using real Unix sockets.
    // ========================================================================

    #[tokio::test]
    async fn test_cli_profile_switch_request_to_daemon() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_RUNTIME_DIR", temp_dir.path());

        let server = IpcServer::new().unwrap();
        let socket_path = server.socket_path().clone();

        // Spawn a mock daemon handler
        let handler_task = tokio::spawn(async move {
            let stream = server.accept().await.unwrap();

            // Handle the IPC connection with a simple profile switch handler
            handle_ipc_connection(stream, |request| {
                match request {
                    IpcRequest::ProfileSwitch { device, profile } => {
                        IpcResponse::Success {
                            message: Some(format!(
                                "Switched device '{}' to profile '{}'",
                                device, profile
                            )),
                        }
                    }
                    _ => IpcResponse::Error {
                        message: "Unexpected request".to_string(),
                    },
                }
            })
            .await
            .unwrap();
        });

        // Simulate CLI sending a profile switch request
        let mut client = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap();

        let request = IpcRequest::ProfileSwitch {
            device: "Keychron K3 Pro".to_string(),
            profile: "gaming".to_string(),
        };
        let request_json = serde_json::to_string(&request).unwrap();
        client.write_all(request_json.as_bytes()).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        client.flush().await.unwrap();

        // Read the response
        let (reader, _writer) = client.split();
        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();

        // Parse and verify the response
        let response: IpcResponse = serde_json::from_str(response_line.trim()).unwrap();
        match response {
            IpcResponse::Success { message } => {
                assert!(message.is_some());
                let msg = message.unwrap();
                assert!(msg.contains("Keychron K3 Pro"));
                assert!(msg.contains("gaming"));
            }
            _ => panic!("Expected Success response"),
        }

        handler_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_cli_profile_list_request_to_daemon() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_RUNTIME_DIR", temp_dir.path());

        let server = IpcServer::new().unwrap();
        let socket_path = server.socket_path().clone();

        // Spawn a mock daemon handler
        let handler_task = tokio::spawn(async move {
            let stream = server.accept().await.unwrap();

            // Handle the IPC connection with a profile list handler
            handle_ipc_connection(stream, |request| {
                match request {
                    IpcRequest::ProfileList { device } => {
                        if device == "Keychron K3 Pro" {
                            IpcResponse::ProfileList {
                                profiles: vec![
                                    "default".to_string(),
                                    "gaming".to_string(),
                                    "productivity".to_string(),
                                ],
                                active: "default".to_string(),
                            }
                        } else {
                            IpcResponse::Error {
                                message: format!("Device '{}' not found", device),
                            }
                        }
                    }
                    _ => IpcResponse::Error {
                        message: "Unexpected request".to_string(),
                    },
                }
            })
            .await
            .unwrap();
        });

        // Simulate CLI sending a profile list request
        let mut client = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap();

        let request = IpcRequest::ProfileList {
            device: "Keychron K3 Pro".to_string(),
        };
        let request_json = serde_json::to_string(&request).unwrap();
        client.write_all(request_json.as_bytes()).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        client.flush().await.unwrap();

        // Read the response
        let (reader, _writer) = client.split();
        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();

        // Parse and verify the response
        let response: IpcResponse = serde_json::from_str(response_line.trim()).unwrap();
        match response {
            IpcResponse::ProfileList { profiles, active } => {
                assert_eq!(profiles.len(), 3);
                assert!(profiles.contains(&"default".to_string()));
                assert!(profiles.contains(&"gaming".to_string()));
                assert!(profiles.contains(&"productivity".to_string()));
                assert_eq!(active, "default");
            }
            _ => panic!("Expected ProfileList response"),
        }

        handler_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_cli_device_not_found_error() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_RUNTIME_DIR", temp_dir.path());

        let server = IpcServer::new().unwrap();
        let socket_path = server.socket_path().clone();

        // Spawn a mock daemon handler that returns device not found
        let handler_task = tokio::spawn(async move {
            let stream = server.accept().await.unwrap();

            handle_ipc_connection(stream, |request| {
                match request {
                    IpcRequest::ProfileSwitch { device, .. } => {
                        IpcResponse::Error {
                            message: format!(
                                "Device '{}' not found. Available devices: Keychron K3 Pro, Logitech G502",
                                device
                            ),
                        }
                    }
                    _ => IpcResponse::Error {
                        message: "Unexpected request".to_string(),
                    },
                }
            })
            .await
            .unwrap();
        });

        // Simulate CLI sending a request for a non-existent device
        let mut client = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap();

        let request = IpcRequest::ProfileSwitch {
            device: "NonExistent Device".to_string(),
            profile: "gaming".to_string(),
        };
        let request_json = serde_json::to_string(&request).unwrap();
        client.write_all(request_json.as_bytes()).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        client.flush().await.unwrap();

        // Read the response
        let (reader, _writer) = client.split();
        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();

        // Parse and verify the error response
        let response: IpcResponse = serde_json::from_str(response_line.trim()).unwrap();
        match response {
            IpcResponse::Error { message } => {
                assert!(message.contains("NonExistent Device"));
                assert!(message.contains("not found"));
            }
            _ => panic!("Expected Error response"),
        }

        handler_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_cli_profile_not_found_error() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_RUNTIME_DIR", temp_dir.path());

        let server = IpcServer::new().unwrap();
        let socket_path = server.socket_path().clone();

        // Spawn a mock daemon handler that returns profile not found
        let handler_task = tokio::spawn(async move {
            let stream = server.accept().await.unwrap();

            handle_ipc_connection(stream, |request| {
                match request {
                    IpcRequest::ProfileSwitch { device, profile } => {
                        IpcResponse::Error {
                            message: format!(
                                "Profile '{}' not found for device '{}'. Available profiles: default, gaming",
                                profile, device
                            ),
                        }
                    }
                    _ => IpcResponse::Error {
                        message: "Unexpected request".to_string(),
                    },
                }
            })
            .await
            .unwrap();
        });

        // Simulate CLI sending a request for a non-existent profile
        let mut client = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap();

        let request = IpcRequest::ProfileSwitch {
            device: "Keychron K3 Pro".to_string(),
            profile: "nonexistent".to_string(),
        };
        let request_json = serde_json::to_string(&request).unwrap();
        client.write_all(request_json.as_bytes()).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        client.flush().await.unwrap();

        // Read the response
        let (reader, _writer) = client.split();
        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();

        // Parse and verify the error response
        let response: IpcResponse = serde_json::from_str(response_line.trim()).unwrap();
        match response {
            IpcResponse::Error { message } => {
                assert!(message.contains("nonexistent"));
                assert!(message.contains("not found"));
                assert!(message.contains("Keychron K3 Pro"));
            }
            _ => panic!("Expected Error response"),
        }

        handler_task.await.unwrap();
    }

    #[test]
    fn test_cli_connection_to_nonexistent_socket() {
        // This test verifies that the CLI handles the case when the daemon
        // socket doesn't exist (daemon not running)
        use std::os::unix::net::UnixStream;

        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("niri-mapper.sock");

        // Attempt to connect to a socket that doesn't exist
        let result = UnixStream::connect(&socket_path);
        assert!(result.is_err());

        let err = result.unwrap_err();
        // On Linux, connecting to a non-existent socket returns ENOENT (No such file or directory)
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn test_cli_daemon_status_request() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_RUNTIME_DIR", temp_dir.path());

        let server = IpcServer::new().unwrap();
        let socket_path = server.socket_path().clone();

        // Spawn a mock daemon handler that returns status info
        let handler_task = tokio::spawn(async move {
            let stream = server.accept().await.unwrap();

            handle_ipc_connection(stream, |request| {
                match request {
                    IpcRequest::Status => {
                        IpcResponse::Status {
                            devices: vec![
                                DeviceStatus {
                                    name: "Keychron K3 Pro".to_string(),
                                    path: PathBuf::from("/dev/input/event5"),
                                    active_profile: "default".to_string(),
                                    available_profiles: vec![
                                        "default".to_string(),
                                        "gaming".to_string(),
                                    ],
                                },
                                DeviceStatus {
                                    name: "Logitech G502".to_string(),
                                    path: PathBuf::from("/dev/input/event7"),
                                    active_profile: "gaming".to_string(),
                                    available_profiles: vec![
                                        "default".to_string(),
                                        "gaming".to_string(),
                                        "fps".to_string(),
                                    ],
                                },
                            ],
                        }
                    }
                    _ => IpcResponse::Error {
                        message: "Unexpected request".to_string(),
                    },
                }
            })
            .await
            .unwrap();
        });

        // Simulate CLI sending a status request
        let mut client = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap();

        let request = IpcRequest::Status;
        let request_json = serde_json::to_string(&request).unwrap();
        client.write_all(request_json.as_bytes()).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        client.flush().await.unwrap();

        // Read the response
        let (reader, _writer) = client.split();
        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();

        // Parse and verify the status response
        let response: IpcResponse = serde_json::from_str(response_line.trim()).unwrap();
        match response {
            IpcResponse::Status { devices } => {
                assert_eq!(devices.len(), 2);

                let keychron = devices.iter().find(|d| d.name == "Keychron K3 Pro").unwrap();
                assert_eq!(keychron.active_profile, "default");
                assert_eq!(keychron.path, PathBuf::from("/dev/input/event5"));
                assert_eq!(keychron.available_profiles.len(), 2);

                let logitech = devices.iter().find(|d| d.name == "Logitech G502").unwrap();
                assert_eq!(logitech.active_profile, "gaming");
                assert_eq!(logitech.path, PathBuf::from("/dev/input/event7"));
                assert_eq!(logitech.available_profiles.len(), 3);
            }
            _ => panic!("Expected Status response"),
        }

        handler_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_cli_invalid_json_request_handling() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let temp_dir = TempDir::new().unwrap();
        env::set_var("XDG_RUNTIME_DIR", temp_dir.path());

        let server = IpcServer::new().unwrap();
        let socket_path = server.socket_path().clone();

        // Spawn a mock daemon handler
        let handler_task = tokio::spawn(async move {
            let stream = server.accept().await.unwrap();

            // The handle_ipc_connection function should handle invalid JSON gracefully
            handle_ipc_connection(stream, |_request| {
                // This shouldn't be called for invalid JSON
                IpcResponse::Success { message: None }
            })
            .await
            .unwrap();
        });

        // Send invalid JSON to the daemon
        let mut client = tokio::net::UnixStream::connect(&socket_path)
            .await
            .unwrap();

        client
            .write_all(b"{ invalid json garbage }\n")
            .await
            .unwrap();
        client.flush().await.unwrap();

        // Read the error response
        let (reader, _writer) = client.split();
        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();

        // Parse and verify the error response
        let response: IpcResponse = serde_json::from_str(response_line.trim()).unwrap();
        match response {
            IpcResponse::Error { message } => {
                assert!(message.contains("Invalid request"));
            }
            _ => panic!("Expected Error response for invalid JSON"),
        }

        handler_task.await.unwrap();
    }
}
