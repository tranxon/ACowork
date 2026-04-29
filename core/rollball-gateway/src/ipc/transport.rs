//! IPC transport layer for Gateway (server-side)
//!
//! Provides `AsyncTransportServer` and `AsyncTransportConnection` implementations
//! for Unix Socket (Linux/macOS) and Named Pipe (Windows).
//!
//! Platform-specific code (`#[cfg(unix)]` / `#[cfg(windows)]`) is confined
//! to this file. Consumers (server.rs, client.rs) use only the trait.

use rollball_core::transport::{AsyncTransportConnection, AsyncTransportServer, TransportKind, classify_endpoint};
use rollball_core::protocol::Frame;
use rollball_core::error::RollballError;

// ── Unix Socket Transport (Linux/macOS) ──────────────────────────────────────

#[cfg(unix)]
pub mod unix_transport {
    use super::*;
    use tokio::net::UnixListener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    /// Server that listens on a Unix domain socket.
    pub struct UnixTransportServer {
        socket_path: String,
        listener: Option<UnixListener>,
    }

    impl UnixTransportServer {
        pub fn new(socket_path: &str) -> Self {
            Self {
                socket_path: socket_path.to_string(),
                listener: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl AsyncTransportServer for UnixTransportServer {
        async fn listen(&mut self) -> Result<(), RollballError> {
            // Remove stale socket file
            let _ = std::fs::remove_file(&self.socket_path);
            let listener = UnixListener::bind(&self.socket_path).map_err(|e| {
                RollballError::Ipc(format!(
                    "Failed to bind Unix socket '{}': {}",
                    self.socket_path, e
                ))
            })?;
            self.listener = Some(listener);
            Ok(())
        }

        async fn accept(&mut self) -> Result<Box<dyn AsyncTransportConnection>, RollballError> {
            let listener = self.listener.as_ref().ok_or_else(|| {
                RollballError::Ipc("Server not listening. Call listen() first.".to_string())
            })?;
            let (stream, _addr) = listener.accept().await.map_err(|e| {
                RollballError::Ipc(format!("Failed to accept Unix socket connection: {}", e))
            })?;
            Ok(Box::new(UnixTransportConnection::new(stream)))
        }

        fn endpoint_desc(&self) -> String {
            format!("unix:{}", self.socket_path)
        }
    }

    /// A single Unix socket connection.
    pub struct UnixTransportConnection {
        reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: tokio::net::unix::OwnedWriteHalf,
    }

    impl UnixTransportConnection {
        pub fn new(stream: tokio::net::UnixStream) -> Self {
            let (reader, writer) = stream.into_split();
            Self {
                reader: BufReader::new(reader),
                writer,
            }
        }
    }

    #[async_trait::async_trait]
    impl AsyncTransportConnection for UnixTransportConnection {
        async fn recv_frame(&mut self) -> Result<Option<Frame>, RollballError> {
            let mut header = [0u8; Frame::HEADER_SIZE];
            match self.reader.read_exact(&mut header).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(e) => {
                    return Err(RollballError::Ipc(format!(
                        "Failed to read frame header: {}",
                        e
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
            "unix:peer".to_string()
        }
    }
}

// ── Named Pipe Transport (Windows) ──────────────────────────────────────────

#[cfg(windows)]
pub mod windows_transport {
    use super::*;
    use tokio::net::windows::named_pipe::{ServerOptions, NamedPipeServer, NamedPipeClient, ClientOptions};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::io::split;

    /// Server that listens on a Windows Named Pipe.
    pub struct NamedPipeTransportServer {
        pipe_name: String,
        is_first_instance: bool,
    }

    impl NamedPipeTransportServer {
        pub fn new(pipe_name: &str) -> Self {
            Self {
                pipe_name: pipe_name.to_string(),
                is_first_instance: true,
            }
        }
    }

    #[async_trait::async_trait]
    impl AsyncTransportServer for NamedPipeTransportServer {
        async fn listen(&mut self) -> Result<(), RollballError> {
            // Named pipes on Windows don't need an explicit bind step.
            // The first `accept()` call creates the pipe instance.
            tracing::info!("Named Pipe server ready: {}", self.pipe_name);
            Ok(())
        }

        async fn accept(&mut self) -> Result<Box<dyn AsyncTransportConnection>, RollballError> {
            // Create a new pipe instance and wait for a client to connect.
            //
            // The first call must use first_pipe_instance(true) to create
            // the pipe; subsequent calls use false to create additional
            // instances under the same name.
            // max_instances is set to 255 to allow concurrent clients.
            let server = ServerOptions::new()
                .first_pipe_instance(self.is_first_instance)
                .max_instances(64)
                .create(&self.pipe_name)
                .map_err(|e| {
                    classify_pipe_server_error(&self.pipe_name, e)
                })?;

            self.is_first_instance = false;

            // Wait for client connection
            server.connect().await.map_err(|e| {
                RollballError::Ipc(format!(
                    "Failed to wait for Named Pipe connection on '{}': {}",
                    self.pipe_name, e
                ))
            })?;

            Ok(Box::new(NamedPipeConnection::from_server(server)))
        }

        fn endpoint_desc(&self) -> String {
            format!("pipe:{}", self.pipe_name)
        }
    }

    /// A single Named Pipe connection (server-side or client-side).
    ///
    /// Uses a generic `T: AsyncRead + AsyncWrite + Unpin + Send + Sync`
    /// internally, but since `NamedPipeServer` and `NamedPipeClient` have
    /// different types, we store the split halves directly.
    pub enum NamedPipeConnection {
        /// Server-side connection (accepted from a client)
        Server {
            reader: BufReader<tokio::io::ReadHalf<NamedPipeServer>>,
            writer: tokio::io::WriteHalf<NamedPipeServer>,
        },
        /// Client-side connection (for testing)
        Client {
            reader: BufReader<tokio::io::ReadHalf<NamedPipeClient>>,
            writer: tokio::io::WriteHalf<NamedPipeClient>,
        },
    }

    impl NamedPipeConnection {
        pub fn from_server(server: NamedPipeServer) -> Self {
            let (reader, writer) = split(server);
            Self::Server {
                reader: BufReader::new(reader),
                writer,
            }
        }

        /// Create a client-side connection (for testing).
        pub fn from_client(client: NamedPipeClient) -> Self {
            let (reader, writer) = split(client);
            Self::Client {
                reader: BufReader::new(reader),
                writer,
            }
        }
    }

    #[async_trait::async_trait]
    impl AsyncTransportConnection for NamedPipeConnection {
        async fn recv_frame(&mut self) -> Result<Option<Frame>, RollballError> {
            let mut header = [0u8; Frame::HEADER_SIZE];
            let read_result = match self {
                Self::Server { reader, .. } => reader.read_exact(&mut header).await,
                Self::Client { reader, .. } => reader.read_exact(&mut header).await,
            };

            match read_result {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    return Ok(None);
                }
                Err(e) => {
                    return Err(RollballError::Ipc(format!(
                        "Failed to read frame header from Named Pipe: {}",
                        e
                    )));
                }
            }

            let body_len =
                u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
            let msg_type = header[4];

            let mut body = vec![0u8; body_len];
            let body_result = match self {
                Self::Server { reader, .. } => reader.read_exact(&mut body).await,
                Self::Client { reader, .. } => reader.read_exact(&mut body).await,
            };
            body_result.map_err(|e| {
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
            let write_result = match self {
                Self::Server { writer, .. } => {
                    writer.write_all(&bytes).await?;
                    writer.flush().await
                }
                Self::Client { writer, .. } => {
                    writer.write_all(&bytes).await?;
                    writer.flush().await
                }
            };
            write_result.map_err(|e| {
                RollballError::Ipc(format!("Failed to write/flush frame to Named Pipe: {}", e))
            })?;
            Ok(())
        }

        fn peer_desc(&self) -> String {
            match self {
                Self::Server { .. } => "pipe:client".to_string(),
                Self::Client { .. } => "pipe:server".to_string(),
            }
        }
    }

    /// Connect as a client to a Named Pipe (for testing).
    ///
    /// Retries up to 3 times with 50ms delay when the pipe is busy or
    /// not yet available, since the server may still be initializing.
    pub async fn connect_client_async(pipe_name: &str) -> Result<Box<dyn AsyncTransportConnection>, RollballError> {
        let mut attempts = 0;
        let max_attempts = 10;
        loop {
            match ClientOptions::new().open(pipe_name) {
                Ok(client) => {
                    return Ok(Box::new(NamedPipeConnection::from_client(client)));
                }
                Err(e) => {
                    let is_retryable = matches!(e.kind(),
                        std::io::ErrorKind::NotFound
                    ) || e.raw_os_error() == Some(231); // ERROR_PIPE_BUSY
                    attempts += 1;
                    if is_retryable && attempts < max_attempts {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                    return Err(classify_pipe_connect_error(pipe_name, e));
                }
            }
        }
    }

    /// Synchronous connect — for simple test cases where async is not needed.
    pub fn connect_client(pipe_name: &str) -> Result<Box<dyn AsyncTransportConnection>, RollballError> {
        let mut attempts = 0;
        let max_attempts = 4;
        loop {
            match ClientOptions::new().open(pipe_name) {
                Ok(client) => {
                    return Ok(Box::new(NamedPipeConnection::from_client(client)));
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

    /// Classify a Named Pipe server-side error (pipe creation failure).
    fn classify_pipe_server_error(pipe_name: &str, e: std::io::Error) -> RollballError {
        match e.kind() {
            std::io::ErrorKind::PermissionDenied => RollballError::Ipc(format!(
                "Permission denied creating Named Pipe '{}' — check access rights",
                pipe_name
            )),
            _ => {
                if let Some(raw_os) = e.raw_os_error() {
                    // ERROR_ACCESS_DENIED (5)
                    if raw_os == 5 {
                        return RollballError::Ipc(format!(
                            "Permission denied creating Named Pipe '{}' — check access rights",
                            pipe_name
                        ));
                    }
                }
                RollballError::Ipc(format!(
                    "Failed to create Named Pipe '{}': {}",
                    pipe_name, e
                ))
            }
        }
    }

    /// Classify a Named Pipe client connection error into a human-readable message
    /// that distinguishes between "pipe not found" (Gateway not running),
    /// "permission denied", "pipe busy", and other errors.
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
                if let Some(raw_os) = e.raw_os_error() {
                    // ERROR_PIPE_BUSY (231)
                    if raw_os == 231 {
                        return RollballError::Ipc(format!(
                            "Named Pipe '{}' is busy — all pipe instances are in use",
                            pipe_name
                        ));
                    }
                    // ERROR_ACCESS_DENIED (5)
                    if raw_os == 5 {
                        return RollballError::Ipc(format!(
                            "Permission denied connecting to Named Pipe '{}' — check access rights",
                            pipe_name
                        ));
                    }
                    // ERROR_BAD_PIPE (230) — pipe state is invalid
                    if raw_os == 230 {
                        return RollballError::Ipc(format!(
                            "Named Pipe '{}' is in an invalid state — try reconnecting",
                            pipe_name
                        ));
                    }
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

/// Create the platform-appropriate server transport based on the endpoint.
pub fn create_server(endpoint: &str) -> Result<Box<dyn AsyncTransportServer>, RollballError> {
    match classify_endpoint(endpoint) {
        #[cfg(unix)]
        TransportKind::UnixSocket => {
            Ok(Box::new(unix_transport::UnixTransportServer::new(endpoint)))
        }
        #[cfg(windows)]
        TransportKind::NamedPipe => {
            Ok(Box::new(windows_transport::NamedPipeTransportServer::new(endpoint)))
        }
        TransportKind::LocalTcp => {
            Err(RollballError::Ipc("Local TCP transport not yet implemented".to_string()))
        }
        // Fallback for cross-compilation scenarios
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

/// Create a client-side connection (used by Gateway for test or future server-initiated connect).
/// Primarily used by Runtime, but kept here for symmetry.
pub fn create_connection() -> Result<Box<dyn AsyncTransportConnection>, RollballError> {
    // Client-side connection creation is in rollball-runtime's transport.
    // This is a placeholder for symmetry.
    Err(RollballError::Ipc(
        "Client connection creation is in rollball-runtime::ipc::transport".to_string()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_unix_socket() {
        assert_eq!(classify_endpoint("/tmp/gateway.sock"), TransportKind::UnixSocket);
        assert_eq!(classify_endpoint("/var/run/gateway.sock"), TransportKind::UnixSocket);
    }

    #[test]
    fn test_classify_named_pipe() {
        assert_eq!(classify_endpoint(r"\\.\pipe\rollball-gateway"), TransportKind::NamedPipe);
        assert_eq!(classify_endpoint("pipe://test"), TransportKind::NamedPipe);
    }

    #[test]
    fn test_classify_tcp() {
        assert_eq!(classify_endpoint("tcp://127.0.0.1:19876"), TransportKind::LocalTcp);
    }

    #[test]
    fn test_create_server_unix() {
        // On Unix, creating a Unix socket server should succeed
        #[cfg(unix)]
        {
            let result = create_server("/tmp/test-gateway.sock");
            assert!(result.is_ok());
        }
    }

    #[test]
    fn test_create_server_named_pipe() {
        // On Windows, creating a Named Pipe server should succeed
        #[cfg(windows)]
        {
            let result = create_server(r"\\.\pipe\rollball-test");
            assert!(result.is_ok());
        }
    }
}
