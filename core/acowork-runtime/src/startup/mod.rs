//! Agent startup sequence — structured phase orchestration.
//!
//! This module breaks the monolithic `async_main` into four clearly-defined
//! phases, each with a single responsibility:
//!
//! ```text
//! Phase A  — per-agent init  (package, gateway, provider, tools, embedding)
//! Phase B  — per-session init (conversation, AgentCore, SessionManager)
//! Phase C  — spawn subsystems (chunk_relay, MCP auto-connect, DevMode)
//! Phase D  — announce ready + run gateway loop
//! ```
//!
//! Each phase function is in its own sub-module and carries a tracing
//! `info_span` so startup duration is visible in structured logs.

pub(crate) mod agent_init;
pub(crate) mod context;
pub(crate) mod gateway_loop;
pub(crate) mod session_init;
pub(crate) mod subsystems;

// Re-export the phase entry points for convenience.
pub(crate) use agent_init::phase_a_init_agent;
pub(crate) use gateway_loop::phase_d_run;
pub(crate) use session_init::phase_b_init_session;
pub(crate) use subsystems::phase_c_spawn_subsystems;

// Re-export helpers from `cli.rs` that Phase A needs.
// These are free functions defined at the crate root of `cli.rs` and are
// not part of any `impl` block, so we alias them here for clarity.
pub(crate) mod super_mod {
    pub(crate) use crate::cli::{
        connect_gateway_client, read_resource_cache, resolve_skill_mode, save_resource_cache,
        RuntimeResourceCache,
    };
}
