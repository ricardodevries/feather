//! Versioned request and response messages for the local `featherd` socket.

use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::error::FeatherError;
use crate::error::Result;
use crate::types::IPC_PROTOCOL_VERSION;

/// Maximum encoded size of one newline-delimited IPC message.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// A request sent to the local daemon socket.
pub struct Request {
    /// IPC protocol version understood by the client.
    pub protocol_version: u32,
    /// Client-generated identifier echoed by the daemon.
    pub request_id: String,
    /// Operation for the daemon to perform.
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "command", content = "args", rename_all = "kebab-case")]
/// Operations accepted by the local daemon.
pub enum Command {
    /// Return the current daemon, sensor, and output status.
    Status,
    /// Return devices known to the running daemon.
    Devices,
    /// Validate and activate the configuration file again.
    ConfigReload,
    /// List configured profiles and the active profile.
    ProfileList,
    /// Activate a configured profile.
    ProfileSet {
        /// Profile name from the configuration file.
        profile: String,
        /// Save the profile as the startup selection.
        #[serde(default)]
        persist: bool,
    },
    /// Set a temporary fan duty override.
    OverrideFan {
        /// Output alias to override.
        output: String,
        /// Requested fan duty from 0 through 100.
        percent: u8,
        /// Override lifetime in milliseconds.
        duration_ms: u64,
        /// Permit a value below the output's configured minimum.
        #[serde(default)]
        allow_below_minimum: bool,
    },
    /// Set a temporary RGB override.
    OverrideRgb {
        /// Output alias to override.
        output: String,
        /// Red, green, and blue channel values.
        rgb: [u8; 3],
        /// Override lifetime in milliseconds.
        duration_ms: u64,
    },
    /// Clear temporary overrides.
    OverrideClear {
        /// Output alias, or all outputs when omitted.
        output: Option<String>,
    },
    /// Return one output or physical device to its hardware policy.
    OutputRelease {
        /// Output alias, or all outputs when omitted.
        output: Option<String>,
    },
    /// Resume `featherd` control of one output or physical device.
    OutputResume {
        /// Output alias, or all outputs when omitted.
        output: Option<String>,
    },
    /// Read raw diagnostic data from a configured Corsair HID device.
    DebugHidDump {
        /// Configured device alias.
        device: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// A response returned by the local daemon.
pub struct Response {
    /// IPC protocol version used by the daemon.
    pub protocol_version: u32,
    /// Identifier copied from the request.
    pub request_id: String,
    /// Whether the command completed successfully.
    pub ok: bool,
    /// Command result when `ok` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Structured error when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// An error returned across the local IPC protocol.
pub struct ApiError {
    /// Machine-readable error category.
    pub code: ErrorCode,
    /// Error message intended for the CLI user.
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Machine-readable categories for IPC errors.
pub enum ErrorCode {
    /// The request body or command arguments are invalid.
    InvalidRequest,
    /// The client and daemon use different IPC protocol versions.
    IncompatibleProtocol,
    /// The configuration file is invalid.
    InvalidConfig,
    /// A named profile, output, or device does not exist.
    UnknownResource,
    /// A hardware operation failed.
    HardwareFailure,
    /// The daemon encountered an error outside the other categories.
    Internal,
}

impl Request {
    /// Creates a request with the current protocol version and a unique process-local ID.
    pub fn new(command: Command) -> Self {
        let sequence = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: format!("{}-{sequence}", std::process::id()),
            command,
        }
    }
}

impl Response {
    /// Creates a successful response containing `data`.
    pub fn success(request_id: String, data: Value) -> Self {
        Self {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// Creates a failed response with a structured error.
    pub fn failure(request_id: String, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id,
            ok: false,
            data: None,
            error: Some(ApiError {
                code,
                message: message.into(),
            }),
        }
    }

    /// Checks the response envelope and returns its data.
    ///
    /// # Errors
    ///
    /// Returns an error when the protocol version differs, the envelope is
    /// incomplete, or the daemon reported a failed command.
    pub fn into_data(self) -> Result<Value> {
        if self.protocol_version != IPC_PROTOCOL_VERSION {
            return Err(FeatherError::Protocol(format!(
                "daemon uses protocol {}, client uses {IPC_PROTOCOL_VERSION}",
                self.protocol_version
            )));
        }
        if self.ok {
            self.data.ok_or_else(|| {
                FeatherError::Protocol("successful daemon response contained no data".into())
            })
        } else {
            let error = self.error.ok_or_else(|| {
                FeatherError::Protocol("failed daemon response contained no error".into())
            })?;
            Err(match error.code {
                ErrorCode::InvalidConfig => FeatherError::Config(error.message),
                ErrorCode::InvalidRequest => FeatherError::Protocol(error.message),
                ErrorCode::HardwareFailure => FeatherError::Driver(error.message),
                ErrorCode::IncompatibleProtocol
                | ErrorCode::UnknownResource
                | ErrorCode::Internal => FeatherError::Daemon(error.message),
            })
        }
    }
}

/// Sends one command to the daemon and returns its JSON result.
///
/// # Errors
///
/// Returns an error when the socket cannot be reached, a timeout expires, an
/// IPC message is invalid, or the daemon rejects the command.
pub async fn send_request(socket: &Path, command: Command) -> Result<Value> {
    let request = Request::new(command);
    let request_id = request.request_id.clone();
    let stream = timeout(CLIENT_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|_| FeatherError::Daemon(format!("timed out connecting to {}", socket.display())))?
        .map_err(|error| {
            FeatherError::Daemon(format!(
                "could not connect to {}: {error}",
                socket.display()
            ))
        })?;
    let (reader, mut writer) = stream.into_split();
    let mut payload = serde_json::to_vec(&request)?;
    if payload.len() > MAX_MESSAGE_BYTES {
        return Err(FeatherError::Protocol("request is too large".into()));
    }
    payload.push(b'\n');
    timeout(CLIENT_TIMEOUT, writer.write_all(&payload))
        .await
        .map_err(|_| FeatherError::Daemon("timed out writing to daemon".into()))??;
    writer.shutdown().await?;

    let mut line = String::new();
    let mut reader = BufReader::new(reader).take((MAX_MESSAGE_BYTES + 1) as u64);
    timeout(CLIENT_TIMEOUT, reader.read_line(&mut line))
        .await
        .map_err(|_| FeatherError::Daemon("timed out waiting for daemon response".into()))??;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(FeatherError::Protocol(
            "daemon response is too large".into(),
        ));
    }
    if line.is_empty() {
        return Err(FeatherError::Protocol(
            "daemon closed the socket without a response".into(),
        ));
    }
    let response: Response = serde_json::from_str(&line)?;
    if response.request_id != request_id {
        return Err(FeatherError::Protocol(format!(
            "response request ID '{}' did not match '{request_id}'",
            response.request_id
        )));
    }
    response.into_data()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn command_wire_format_is_stable() -> anyhow::Result<()> {
        let request = Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: "test-1".into(),
            command: Command::OverrideFan {
                output: "case".into(),
                percent: 55,
                duration_ms: 10_000,
                allow_below_minimum: false,
            },
        };
        assert_eq!(
            serde_json::to_value(request)?,
            json!({
                "protocol_version": IPC_PROTOCOL_VERSION,
                "request_id": "test-1",
                "command": "override-fan",
                "args": {
                    "output": "case",
                    "percent": 55,
                    "duration_ms": 10_000,
                    "allow_below_minimum": false
                }
            })
        );
        Ok(())
    }

    #[test]
    fn response_error_maps_to_domain_error() {
        let response = Response::failure("test".into(), ErrorCode::HardwareFailure, "denied");
        assert!(matches!(response.into_data(), Err(FeatherError::Driver(_))));
    }

    #[tokio::test]
    async fn client_and_server_round_trip() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("round-trip.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let request: Request = serde_json::from_str(&line)?;
            let response = Response::success(request.request_id, json!({ "answer": 42 }));
            let mut payload = serde_json::to_vec(&response)?;
            payload.push(b'\n');
            writer.write_all(&payload).await?;
            writer.shutdown().await?;
            Result::Ok(())
        });

        let response = send_request(&socket, Command::Status).await?;
        assert_eq!(response, json!({ "answer": 42 }));
        server.await??;
        Ok(())
    }
}
