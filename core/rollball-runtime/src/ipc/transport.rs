//! Transport layer for Agent Runtime (client-side)
//!
//! Provides `AsyncTransportConnection` implementations for connecting to the
//! Gateway process via Unix Socket (Linux/macOS) or Named Pipe (Windows).
//!
//! Unlike the Gateway's transport (which also has a server trait), the Runtime
//! only needs the client side — it connects, then sends/receives frames.

use rollball_core::transport::{AsyncTransportConnection, TransportKind, classify_endpoint};
use rollball_core::protocol::Frame;
use rollball_core::error::RollballError;

// ── Unix Socket client transport (Linux/macOS) ──────────────────────────────

#[cfg(unix)]
mod unix_client {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncBufReadExt, BufReader};

    /// Client connection over Unix domain socket.
    pub struct UnixClientConnection {
        reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: tokio::net::unix::OwnedWriteHalf,
    }

    impl UnixClientConnection {
        /// Connect to a Unix socket at the given path.
        pub async fn connect(socket_path: &str) -> Result<Self, RollballError> {
            let stream = tokio::net::UnixStream::connect(socket_path).await
                .map_err(|e| {
                    RollballError::Ipc(format!(
                        "Failed to connect to Unix socket '{}': {}",
                        socket_path, e
                    ))
                })?;

            let (reader, writer) = stream.into_split();
            Ok(Self {
                reader: BufReader::new(reader),
                writer,
            })
        }
    }

    #[async_trait::async_trait]
    impl AsyncTransportConnection for UnixClientConnection {
        async fn recv_frame(&mut self) -> Result<Option<Frame>, RollballError> {
            let mut header = [0u8; Frame::HEADER_SIZE];
            match self.reader.read_exact(&mut header).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(e) => {
                    return Err(RollballError::Ipc(format!(
                        "Failed to read frame header: {}", e
                    )));
                }
            }

            let body_len =
                u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let msg_type = header[4];

            let mut body = vec![0u8; body_len];
            self.reader.read_exact(&mut body).await.map_err(|e| {
                RollballError::Ipc(format!("Failed to read frame body: {}", e))
            })?;

            Ok(Some(Frame {
                body_len: body_len as u32,
                msg_type,
                body,
            }))
        }

        async fn send_frame(&mut self, frame: &Frame) -> Result<(), RollballError> {
            let bytes = frame.to_bytes();
            self.writer.write_all(&bytes).await.map_err(|e| {
                RollballError::Ipc(format!("Failed to write frame: {}", e))
            })?;
            self.writer.flush().await.map_err(|e| {
                RollballError::Ipc(format!("Failed to flush frame: {}", e))
            })?;
            Ok(())
        }

        fn peer_desc(&self) -> String {
            "unix:gateway".to_string()
        }
    }
}

// ── Named Pipe client transport (Windows) ──────────────────────────────────

#[cfg(windows)]
mod windows_client {
    use super::*;
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::io::split;

    /// Client connection over Windows Named Pipe.
    pub struct NamedPipeClientConnection {
        reader: BufReader<tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>>,
        writer: tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    }

    impl NamedPipeClientConnection {
        /// Connect to a Named Pipe at the given path.
        ///
        /// Retries up to 3 times with 50ms delay when the pipe is busy
        /// or not yet available, since the server may not have created
        /// enough instances yet.
        pub fn connect(pipe_name: &str) -> Result<Self, RollballError> {
            let mut attempts = 0;
            let max_attempts = 10;
            loop {
                match ClientOptions::new().open(pipe_name) {
                    Ok(client) => {
                        let (reader, writer) = split(client);
                        return Ok(Self {
                            reader: BufReader::new(reader),
                            writer,
                        });
                    }
                    Err(e) => {
                        let is_retryable = matches!(e.kind(),
                            std::io::ErrorKind::NotFound
                        ) || e.raw_os_error() == Some(231); // ERROR_PIPE_BUSY
                        attempts += 1;
                        if is_retryable && attempts < max_attempts {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            continue;
                        }
                        return Err(classify_pipe_connect_error(pipe_name, e));
                    }
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl AsyncTransportConnection for NamedPipeClientConnection {
        async fn recv_frame(&mut self) -> Result<Option<Frame>, RollballError> {
            let mut header = [0u8; Frame::HEADER_SIZE];
            match self.reader.read_exact(&mut header).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    return Ok(None);
                }
                Err(e) => {
                    return Err(RollballError::Ipc(format!(
                        "Failed to read frame header from Named Pipe: {}", e
                    )));
                }
            }

            let body_len =
                u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let msg_type = header[4];

            let mut body = vec![0u8; body_len];
            self.reader.read_exact(&mut body).await.map_err(|e| {
                RollballError::Ipc(format!("Failed to read frame body from Named Pipe: {}", e))
            })?;

            Ok(Some(Frame {
                body_len: body_len as u32,
                msg_type,
                body,
            }))
        }

        async fn send_frame(&mut self, frame: &Frame) -> Result<(), RollballError> {
            let bytes = frame.to_bytes();
            self.writer.write_all(&bytes).await.map_err(|e| {
                RollballError::Ipc(format!("Failed to write frame to Named Pipe: {}", e))
            })?;
            self.writer.flush().await.map_err(|e| {
                RollballError::Ipc(format!("Failed to flush frame to Named Pipe: {}", e))
            })?;
            Ok(())
        }

        fn peer_desc(&self) -> String {
            "pipe:gateway".to_string()
        }
    }

    /// Classify a Named Pipe connection error into a human-readable message
    /// that distinguishes between "pipe not found" (Gateway not running),
    /// "permission denied", and other errors.
    fn classify_pipe_connect_error(pipe_name: &str, e: std::io::Error) -> RollballError {
        match e.kind() {
            std::io::ErrorKind::NotFound => RollballError::Ipc(format!(
                "Named Pipe '{}' does not exist — is the Gateway running?",
                pipe_name
            )),
            std::io::ErrorKind::PermissionDenied => RollballError::Ipc(format!(
                "Permission denied connecting to Named Pipe '{}' — check access rights",
                pipe_name
            )),
            _ => {
                // Windows named pipe errors often come as raw OS error codes.
                // Check for ERROR_PIPE_BUSY (231 = 0xE7) specifically.
                if let Some(raw_os) = e.raw_os_error()
                    && raw_os == 231
                {
                    return RollballError::Ipc(format!(
                        "Named Pipe '{}' is busy — all pipe instances are in use",
                        pipe_name
                    ));
                }
                RollballError::Ipc(format!(
                    "Failed to connect to Named Pipe '{}': {}",
                    pipe_name, e
                ))
            }
        }
    }
}

// ── Platform dispatch ────────────────────────────────────────────────────────

/// Connect to the Gateway at the given endpoint.
///
/// Returns a platform-appropriate `AsyncTransportConnection`.
pub async fn connect(endpoint: &str) -> Result<Box<dyn AsyncTransportConnection>, RollballError> {
    match classify_endpoint(endpoint) {
        #[cfg(unix)]
        TransportKind::UnixSocket => {
            let path = endpoint
                .strip_prefix("unix://")
                .unwrap_or(endpoint);
            let conn = unix_client::UnixClientConnection::connect(path).await?;
            Ok(Box::new(conn))
        }
        #[cfg(windows)]
        TransportKind::NamedPipe => {
            let pipe_name = endpoint
                .strip_prefix("pipe://")
                .unwrap_or(endpoint);
            let conn = windows_client::NamedPipeClientConnection::connect(pipe_name)?;
            Ok(Box::new(conn))
        }
        TransportKind::LocalTcp => {
            Err(RollballError::Ipc("Local TCP transport not yet implemented".to_string()))
        }
        #[allow(unreachable_patterns)]
        _ => {
            Err(RollballError::Ipc(format!(
                "Transport kind {:?} not available on this platform for endpoint '{}'",
                classify_endpoint(endpoint),
                endpoint
            )))
        }
    }
}

/// Determine the endpoint URI from a raw path/name.
///
/// Normalizes the endpoint to include the scheme prefix:
/// - Unix: `unix:///path/to/socket`
/// - Named Pipe: `pipe://\\.\pipe\name`
pub fn normalize_endpoint(raw: &str) -> String {
    if raw.starts_with("unix://") || raw.starts_with("pipe://") || raw.starts_with("tcp://") {
        raw.to_string()
    } else if raw.starts_with(r"\\.\pipe\") {
        format!("pipe://{}", raw)
    } else {
        format!("unix://{}", raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_unix_endpoint() {
        assert_eq!(normalize_endpoint("/tmp/gateway.sock"), "unix:///tmp/gateway.sock");
        assert_eq!(normalize_endpoint("unix:///tmp/gateway.sock"), "unix:///tmp/gateway.sock");
    }

    #[test]
    fn test_normalize_pipe_endpoint() {
        assert_eq!(
            normalize_endpoint(r"\\.\pipe\rollball-gateway"),
            r"pipe://\\.\pipe\rollball-gateway"
        );
        assert_eq!(
            normalize_endpoint("pipe://test"),
            "pipe://test"
        );
    }

    #[test]
    fn test_classify_unix_socket() {
        assert_eq!(classify_endpoint("/tmp/gateway.sock"), TransportKind::UnixSocket);
        assert_eq!(classify_endpoint("unix:///tmp/gateway.sock"), TransportKind::UnixSocket);
    }

    #[test]
    fn test_classify_named_pipe() {
        assert_eq!(classify_endpoint(r"\\.\pipe\rollball"), TransportKind::NamedPipe);
        assert_eq!(classify_endpoint("pipe://test"), TransportKind::NamedPipe);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_connect_invalid_unix_socket() {
        let result = connect("/nonexistent/path/gateway.sock").await;
        assert!(result.is_err());
    }
}
