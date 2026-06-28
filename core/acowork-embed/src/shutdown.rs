//! Graceful shutdown handling — re-exported from acowork-core.
//!
//! This module is kept as a thin re-export so existing `use crate::shutdown::Shutdown`
//! paths continue to work. New code should use `acowork_core::shutdown` directly.

pub use acowork_core::shutdown::*;
