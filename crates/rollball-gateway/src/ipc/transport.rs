//! IPC transport layer for Gateway
//!
//! Server-side transport: accepts connections from Agent Runtime processes.
//! Uses Unix Socket on Linux/macOS and Named Pipe on Windows.

use rollball_core::protocol::Frame;
use crate::error::GatewayError;

/// Transport trait for Gateway IPC server
pub trait Transport: Send + Sync {
    /// Start listening for connections
    fn listen(&self) -> Result<(), GatewayError>;
    
    /// Accept next incoming connection (blocking)
    fn accept(&self) -> Result<Box<dyn TransportConnection>, GatewayError>;
}

/// A single connection from an Agent Runtime
pub trait TransportConnection: Send + Sync {
    /// Read a frame from the connection
    fn recv_frame(&mut self) -> Result<Option<Frame>, GatewayError>;
    
    /// Send a frame to the connection
    fn send_frame(&mut self, frame: &Frame) -> Result<(), GatewayError>;
    
    /// Close the connection
    fn close(&mut self) -> Result<(), GatewayError>;
    
    /// Get peer description (for logging)
    fn peer_desc(&self) -> String;
}

// ── Unix Socket Transport (Linux/macOS) ──────────────────────────────────────

/// Unix Socket transport for Gateway
pub struct UnixSocketTransport {
    #[allow(dead_code)]
    socket_path: String,
}

impl UnixSocketTransport {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }
}

impl Transport for UnixSocketTransport {
    fn listen(&self) -> Result<(), GatewayError> {
        #[cfg(unix)]
        {
            // Remove stale socket file
            let _ = std::fs::remove_file(&self.socket_path);
            // Actual binding happens in accept() loop via tokio
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(GatewayError::Ipc("Unix sockets not available on this platform".to_string()))
        }
    }

    fn accept(&self) -> Result<Box<dyn TransportConnection>, GatewayError> {
        #[cfg(unix)]
        {
            // Synchronous accept for simplicity in Phase 1
            let listener = std::os::unix::net::UnixListener::bind(&self.socket_path)
                .map_err(|e| GatewayError::Ipc(format!("Failed to bind Unix socket '{}': {}", self.socket_path, e)))?;
            let (stream, _addr) = listener.accept()
                .map_err(|e| GatewayError::Ipc(format!("Failed to accept connection: {}", e)))?;
            Ok(Box::new(UnixSocketConnection { stream }))
        }
        #[cfg(not(unix))]
        {
            Err(GatewayError::Ipc("Unix sockets not available on this platform".to_string()))
        }
    }
}

/// Unix Socket connection
#[cfg(unix)]
struct UnixSocketConnection {
    stream: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl TransportConnection for UnixSocketConnection {
    fn recv_frame(&mut self) -> Result<Option<Frame>, GatewayError> {
        use std::io::{Read, Write};
        
        // Read header: 4 bytes body_len + 1 byte msg_type
        let mut header = [0u8; Frame::HEADER_SIZE];
        match self.stream.read_exact(&mut header) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(GatewayError::Ipc(format!("Failed to read frame header: {}", e))),
        }
        
        let body_len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let msg_type = header[4];
        
        // Read body
        let mut body = vec![0u8; body_len];
        self.stream.read_exact(&mut body)
            .map_err(|e| GatewayError::Ipc(format!("Failed to read frame body: {}", e)))?;
        
        Ok(Some(Frame {
            body_len: body_len as u32,
            msg_type,
            body,
        }))
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<(), GatewayError> {
        use std::io::Write;
        let bytes = frame.to_bytes();
        self.stream.write_all(&bytes)
            .map_err(|e| GatewayError::Ipc(format!("Failed to send frame: {}", e)))?;
        self.stream.flush()
            .map_err(|e| GatewayError::Ipc(format!("Failed to flush frame: {}", e)))?;
        Ok(())
    }

    fn close(&mut self) -> Result<(), GatewayError> {
        use std::io::Write;
        self.stream.flush().ok();
        Ok(())
    }

    fn peer_desc(&self) -> String {
        format!("unix:{}", self.stream.peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "unknown".to_string()))
    }
}

// ── Named Pipe Transport (Windows) ──────────────────────────────────────────

/// Named Pipe transport for Windows
pub struct NamedPipeTransport {
    #[allow(dead_code)]
    pipe_name: String,
}

impl NamedPipeTransport {
    pub fn new(pipe_name: &str) -> Self {
        Self { pipe_name: pipe_name.to_string() }
    }
}

impl Transport for NamedPipeTransport {
    fn listen(&self) -> Result<(), GatewayError> {
        // Named pipes on Windows are created on first accept
        Ok(())
    }

    fn accept(&self) -> Result<Box<dyn TransportConnection>, GatewayError> {
        // Phase 1: stub — actual Named Pipe implementation requires tokio + windows-sys
        Err(GatewayError::Ipc("Named Pipe transport not yet implemented (Windows). Use Unix Socket on Linux/macOS.".to_string()))
    }
}

/// Create appropriate transport based on socket_path
pub fn create_transport(socket_path: &str) -> Result<Box<dyn Transport>, GatewayError> {
    if socket_path.contains('/') || socket_path.contains('\\') || socket_path.ends_with(".sock") {
        Ok(Box::new(UnixSocketTransport::new(socket_path)))
    } else if socket_path.starts_with(r"\\.\pipe\") {
        Ok(Box::new(NamedPipeTransport::new(socket_path)))
    } else {
        // Default: treat as Unix socket path
        Ok(Box::new(UnixSocketTransport::new(socket_path)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_transport_unix() {
        let transport = create_transport("/tmp/gateway.sock");
        assert!(transport.is_ok());
    }

    #[test]
    fn test_create_transport_named_pipe() {
        let transport = create_transport(r"\\.\pipe\rollball-gateway");
        assert!(transport.is_ok());
    }

    #[test]
    fn test_unix_socket_transport_new() {
        let transport = UnixSocketTransport::new("/tmp/test.sock");
        assert_eq!(transport.socket_path, "/tmp/test.sock");
    }

    #[test]
    fn test_named_pipe_transport_new() {
        let transport = NamedPipeTransport::new(r"\\.\pipe\test");
        assert_eq!(transport.pipe_name, r"\\.\pipe\test");
    }
}
