//! Storage primitives — atomic I/O + library-index persistence + tree scan.
//!
//! Layering (ADR-040-style):
//! - `atomic.rs` — atomic write (tmp + rename) + atomic read; the only path
//!   that touches `library.json` files on disk for the read-modify-write
//!   cycle (design §3.3 "write: file lock + atomic replace").
//! - `library.rs` — `LibraryStore` per-directory read/write; owns the
//!   mapping `dir_id → disk path → LibraryIndex`.
//! - `tree.rs` — filesystem walk + startup reconciliation (design §3.3
//!   "filename is authoritative; reconcile on startup").
//! - `trash.rs` — `.trash/` slot storage (content file + `.meta.json`
//!   sidecar) for soft-deleted documents.
//!
//! Service-layer traits in `crate::service::*` are the only callers of
//! this module — the HTTP layer must not import `store` directly.

pub mod atomic;
pub mod library;
pub mod trash;
pub mod tree;
