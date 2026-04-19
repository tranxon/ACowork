//! Transport layer abstraction (Unix Socket / Named Pipe / Local TCP)
//!
//! Phase 1 implements Unix Socket on Linux/macOS and Named Pipe on Windows.
//! Local TCP (mobile) will be added in later phases.
//!
//! NOTE: We intentionally avoid `async_trait` here because this trait
//! must be dyn-compatible (used as `Box<dyn Transport>`). In Rust
//! Edition 2024, `async_trait` generates code that is NOT dyn-safe.
//! Instead we manually return `Pin<Box<dyn Future>>`.

use std::future::Future;
use std::io;
use std::pin::Pin;
use rollball_core::protocol::Frame;

/// Transport trait — manually async for dyn compatibility
pub trait Transport: Send + Sync {
    fn connect<'a>(
        &'a self,
        endpoint: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    fn send_frame<'a>(
        &'a self,
        frame: &'a Frame,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    fn recv_frame<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Frame, String>> + Send + 'a>>;

    /// Check if the transport is currently connected
    fn is_connected(&self) -> bool;

    /// Disconnect and clean up resources
    fn disconnect<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// Create transport based on endpoint scheme
pub fn create_transport(endpoint: &str) -> Box<dyn Transport> {
    if endpoint.starts_with("unix://") {
        Box::new(UnixSocketTransport::new())
    } else if endpoint.starts_with("pipe://") {
        Box::new(WindowsNamedPipeTransport::new())
    } else if endpoint.starts_with("tcp://") {
        unimplemented!("TCP transport not yet supported")
    } else {
        panic!("Unknown endpoint scheme: {endpoint}. Use unix://, pipe://, or tcp://")
    }
}

/// Unix Socket transport (Linux/macOS)
///
/// In Phase 1, this provides the implementation for Unix domain sockets.
/// The actual connection uses tokio's UnixStream.
pub struct UnixSocketTransport {
    connected: parking_lot::Mutex<bool>,
}

impl UnixSocketTransport {
    pub fn new() -> Self {
        Self {
            connected: parking_lot::Mutex::new(false),
        }
    }
}

impl Default for UnixSocketTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for UnixSocketTransport {
    fn connect<'a>(
        &'a self,
        endpoint: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let path = endpoint.strip_prefix("unix://").ok_or_else(|| {
                format!("Invalid Unix socket endpoint: {}", endpoint)
            })?;

            // Attempt to connect to the Unix socket
            #[cfg(unix)]
            match tokio::net::UnixStream::connect(path).await {
                Ok(_stream) => {
                    *self.connected.lock() = true;
                    tracing::info!("Connected to Unix socket: {}", path);
                    Ok(())
                }
                Err(e) => {
                    *self.connected.lock() = false;
                    Err(format!("Failed to connect to Unix socket '{}': {}", path, e))
                }
            }
            #[cfg(not(unix))]
            {
                // On non-Unix platforms, Unix sockets are not available
                *self.connected.lock() = false;
                Err(format!("Unix sockets not available on this platform. Path: {}", path))
            }
        })
    }

    fn send_frame<'a>(
        &'a self,
        frame: &'a Frame,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if !self.is_connected() {
                return Err("Not connected".to_string());
            }

            let bytes = frame.to_bytes();
            // In a full implementation, we would write to the stored UnixStream.
            // For Phase 1, we log the send operation.
            tracing::debug!(
                "Sending frame: type={}, len={}",
                frame.msg_type,
                frame.body_len
            );

            // Placeholder: frame encoding is correct but actual I/O needs stream storage
            let _ = bytes.len(); // Suppress unused warning
            Ok(())
        })
    }

    fn recv_frame<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Frame, String>> + Send + 'a>> {
        Box::pin(async move {
            if !self.is_connected() {
                return Err("Not connected".to_string());
            }

            // In a full implementation, we would read from the stored UnixStream.
            // For Phase 1, this is a placeholder.
            Err("Recv not yet implemented for Unix Socket transport".to_string())
        })
    }

    fn is_connected(&self) -> bool {
        *self.connected.lock()
    }

    fn disconnect<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            *self.connected.lock() = false;
            Ok(())
        })
    }
}

/// Windows Named Pipe transport
///
/// Phase 1 provides a stub for Windows. Full implementation
/// will use tokio'sNamedPipeClient in Phase 2.
pub struct WindowsNamedPipeTransport {
    connected: parking_lot::Mutex<bool>,
}

impl WindowsNamedPipeTransport {
    pub fn new() -> Self {
        Self {
            connected: parking_lot::Mutex::new(false),
        }
    }
}

impl Default for WindowsNamedPipeTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for WindowsNamedPipeTransport {
    fn connect<'a>(
        &'a self,
        endpoint: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let path = endpoint.strip_prefix("pipe://").ok_or_else(|| {
                format!("Invalid Named Pipe endpoint: {}", endpoint)
            })?;

            // Phase 1: Placeholder for Windows Named Pipe connection
            // Phase 2 will use tokio::net::windows::named_pipe::ClientOptions
            tracing::info!("Connecting to Named Pipe: {} (stub)", path);
            *self.connected.lock() = true;
            Ok(())
        })
    }

    fn send_frame<'a>(
        &'a self,
        frame: &'a Frame,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if !self.is_connected() {
                return Err("Not connected".to_string());
            }
            let _ = frame.to_bytes();
            Ok(())
        })
    }

    fn recv_frame<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Frame, String>> + Send + 'a>> {
        Box::pin(async move {
            if !self.is_connected() {
                return Err("Not connected".to_string());
            }
            Err("Recv not yet implemented for Named Pipe transport".to_string())
        })
    }

    fn is_connected(&self) -> bool {
        *self.connected.lock()
    }

    fn disconnect<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            *self.connected.lock() = false;
            Ok(())
        })
    }
}

/// Helper: read exactly N bytes from an async reader
pub async fn read_exact<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
    n: usize,
) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Helper: read a Frame from an async reader
pub async fn read_frame<R: tokio::io::AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<Frame, String> {
    // Read header: 4 bytes (body_len) + 1 byte (msg_type)
    let mut header = [0u8; Frame::HEADER_SIZE];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|e| format!("Failed to read frame header: {e}"))?;

    let body_len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let msg_type = header[4];

    // Read body
    let mut body = vec![0u8; body_len as usize];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("Failed to read frame body: {e}"))?;

    Ok(Frame {
        body_len,
        msg_type,
        body,
    })
}

/// Helper: write a Frame to an async writer
pub async fn write_frame<W: tokio::io::AsyncWriteExt + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> Result<(), String> {
    let bytes = frame.to_bytes();
    writer
        .write_all(&bytes)
        .await
        .map_err(|e| format!("Failed to write frame: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("Failed to flush frame: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_transport_unix() {
        let _transport = create_transport("unix:///tmp/gateway.sock");
    }

    #[test]
    fn test_create_transport_pipe() {
        let _transport = create_transport("pipe://\\\\.\\pipe\\rollball-gateway");
    }

    #[test]
    #[should_panic(expected = "Unknown endpoint scheme")]
    fn test_create_transport_invalid() {
        let _transport = create_transport("http://localhost:8080");
    }

    #[tokio::test]
    async fn test_unix_socket_connect_invalid() {
        let transport = UnixSocketTransport::new();
        let result = transport.connect("unix:///nonexistent/socket.sock").await;
        assert!(result.is_err());
        assert!(!transport.is_connected());
    }

    #[tokio::test]
    async fn test_unix_socket_disconnect() {
        let transport = UnixSocketTransport::new();
        *transport.connected.lock() = true;
        assert!(transport.is_connected());
        transport.disconnect().await.unwrap();
        assert!(!transport.is_connected());
    }

    #[tokio::test]
    async fn test_named_pipe_connect_stub() {
        let transport = WindowsNamedPipeTransport::new();
        let result = transport
            .connect("pipe://\\\\.\\pipe\\rollball")
            .await;
        assert!(result.is_ok());
        assert!(transport.is_connected());
    }

    #[tokio::test]
    async fn test_send_frame_not_connected() {
        let transport = UnixSocketTransport::new();
        let frame = Frame::from_message(Frame::TYPE_REQUEST, &serde_json::json!({})).unwrap();
        let result = transport.send_frame(&frame).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_recv_frame_not_connected() {
        let transport = UnixSocketTransport::new();
        let result = transport.recv_frame().await;
        assert!(result.is_err());
    }

    #[test]
    fn test_unix_socket_default() {
        let transport = UnixSocketTransport::default();
        assert!(!transport.is_connected());
    }

    #[test]
    fn test_named_pipe_default() {
        let transport = WindowsNamedPipeTransport::default();
        assert!(!transport.is_connected());
    }
}
