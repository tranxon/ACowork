//! rollball-grafeo — Grafeo graph database engine
//!
//! Phase 2: Full graph database implementation with:
//! - Three-layer five-type biomimetic memory
//! - Forgetting mechanism (decay)
//! - Associative diffusion retrieval
//! - Privacy level filtering

pub mod decay;
pub mod error;
pub mod grafeo;
pub mod graph;
pub mod retrieval;

pub use decay::DecayConfig;
pub use error::{GrafeoError, Result};
pub use grafeo::GrafeoStore;
