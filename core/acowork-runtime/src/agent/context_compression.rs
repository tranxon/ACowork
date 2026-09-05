//! Tool compression was retired from the runtime.
//!
//! The two builtin tools (`context_retrieve`, `context_abandon`) that
//! used to live on this contract are no longer registered with the LLM;
//! their source files survive in `crate::tools::builtin` as dead code
//! for future reference.
//!
//! The `RetrieveQueue` type alias is kept solely so the surviving tool
//! source files continue to compile. Nothing constructs or drains this
//! queue at runtime.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[allow(dead_code)]
pub type RetrieveQueue = Arc<Mutex<VecDeque<(String, String)>>>;
