//! Budget tracking module
//!
//! Tracks token and cost usage per agent/provider with daily and monthly
//! accumulation. Supports budget limits with configurable exceeded actions.

pub mod store;
pub mod tracker;

pub use store::BudgetStore;
pub use tracker::{BudgetTracker, ExceededAction, BudgetSnapshot};
