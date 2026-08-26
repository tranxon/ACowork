//! Node-local sidecar hosting (LSP relay supervisor) — Phase 4.
//!
//! **Migration landing point (ADR-055 §6.7)**: the LSP relay moves
//! from the Gateway to each Node (`node-local` scope) because the LSP
//! server must share the filesystem with the workspace
//! (`root_uri = file://{workspace_root}`). The supervisor code is
//! cloned from the Gateway's `lifecycle/lsp_relay_supervisor.rs`
//! (the tested template — 5 unit tests, SSE heartbeat, crash
//! recovery), with the Gateway-health probe retargeted to Node
//! health. Status topics move to
//! `acowork/nodes/{node_id}/sidecars/{kind}/status` (retained).
