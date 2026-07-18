//! Shared MQTT session lifecycle module (ADR-039 Phase 2).
//!
//! Exports:
//! - [`MqttSession`] – session handle bundling state + reconnect
//! - [`BootstrapAction`] – five-step idempotent bootstrap contract
//! - [`ErrClass`] – 6-type error classifier
//! - [`ErrorDescriptor`] – version-agnostic error description
//! - [`classify`] – maps `ErrorDescriptor` to `ErrClass`
//! - [`SessionState`] – state machine for MQTT connection lifecycle
//! - [`ReconnectPolicy`] – exponential backoff with jitter
//! - [`SessionStateTx`] / [`SessionStateRx`] – `tokio::sync::watch` broadcast

mod bootstrap;
mod err_class;
mod reconnect;
mod session;
mod session_state;

pub use bootstrap::BootstrapAction;
pub use err_class::{classify, ErrorDescriptor, ErrorKind, ErrClass, RefusedReason};
pub use reconnect::{Backoff, ReconnectPolicy};
pub use session::MqttSession;
pub use session_state::{SessionState, SessionStateRx, SessionStateTx};
