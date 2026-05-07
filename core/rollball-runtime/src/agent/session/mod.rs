//! Session management module for multi-session concurrency.
//!
//! Provides `SessionTask` (per-session execution actor), `SessionHandle`
//! (external interaction handle), and `SessionManager` (lifecycle manager
//! for multiple concurrent sessions).

pub(crate) mod session_handle;
pub mod session_manager;
pub mod session_task;

pub use session_manager::{SessionManager, SessionManagerConfig};
pub use session_task::SessionMessage;
