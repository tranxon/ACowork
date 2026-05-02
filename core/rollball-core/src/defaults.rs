//! Default constants for Gateway configuration.
//!
//! All crates should reference these constants instead of hardcoding
//! host, port, or URL values for the Gateway HTTP API.

/// Default Gateway HTTP listen port
pub const GATEWAY_HTTP_PORT: u16 = 19876;

/// Default Gateway HTTP listen host (localhost only)
pub const GATEWAY_HTTP_HOST: &str = "127.0.0.1";

/// Default Gateway HTTP base URL (composed from host and port)
pub const GATEWAY_HTTP_URL: &str = "http://127.0.0.1:19876";

/// Default maximum port when auto-incrementing on conflict
pub const GATEWAY_HTTP_PORT_MAX: u16 = 19878;
