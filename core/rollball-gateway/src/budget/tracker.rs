//! Budget tracker — enforces budget limits and signals exceeded actions
//!
//! S4.3: Tracks cumulative usage per provider and enforces daily/monthly
//! limits. When a budget is exceeded, signals the appropriate action
//! (stop, fallback, warn).

use std::collections::HashMap;
use std::path::Path;

use crate::budget::store::BudgetStore;
use rollball_core::Budget;

/// Action to take when budget is exceeded
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExceededAction {
    /// Stop the agent immediately
    Stop,
    /// Fall back to a cheaper provider
    Fallback,
    /// Warn but allow the request
    Warn,
}

impl std::fmt::Display for ExceededAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExceededAction::Stop => write!(f, "stop"),
            ExceededAction::Fallback => write!(f, "fallback"),
            ExceededAction::Warn => write!(f, "warn"),
        }
    }
}

/// Snapshot of current budget status
#[derive(Debug, Clone)]
pub struct BudgetSnapshot {
    /// Provider name
    pub provider: String,
    /// Daily tokens used
    pub daily_tokens_used: u64,
    /// Daily token limit (if set)
    pub daily_token_limit: Option<u64>,
    /// Monthly tokens used
    pub monthly_tokens_used: u64,
    /// Monthly token limit (if set)
    pub monthly_token_limit: Option<u64>,
    /// Daily cost used (USD)
    pub daily_cost_used: f64,
    /// Daily cost limit (if set)
    pub daily_cost_limit: Option<f64>,
    /// Monthly cost used (USD)
    pub monthly_cost_used: f64,
    /// Monthly cost limit (if set)
    pub monthly_cost_limit: Option<f64>,
    /// Whether budget is exceeded
    pub exceeded: bool,
    /// Action to take if exceeded
    pub exceeded_action: ExceededAction,
}

/// Budget tracker — enforces budget limits per provider
///
/// S4.3.4: When budget is exceeded, sends the configured signal:
/// - stop: agent is stopped immediately
/// - fallback: request is routed to a cheaper provider
/// - warn: a warning is logged but the request proceeds
pub struct BudgetTracker {
    /// Budget store for persistent usage tracking
    store: BudgetStore,
    /// Budget limits per provider
    budgets: HashMap<String, Budget>,
}

impl BudgetTracker {
    /// Create a new BudgetTracker with the given data directory
    pub fn new(data_dir: &Path) -> Self {
        let mut store = BudgetStore::new(data_dir);
        store.load_from_disk();
        Self {
            store,
            budgets: HashMap::new(),
        }
    }

    /// Create an in-memory BudgetTracker (for testing)
    pub fn new_in_memory() -> Self {
        Self {
            store: BudgetStore::new_in_memory(),
            budgets: HashMap::new(),
        }
    }

    /// Set the budget for a provider
    pub fn set_budget(&mut self, provider: &str, budget: Budget) {
        self.budgets.insert(provider.to_string(), budget);
    }

    /// Remove the budget for a provider
    pub fn remove_budget(&mut self, provider: &str) {
        self.budgets.remove(provider);
    }

    /// Get the budget for a provider
    pub fn get_budget(&self, provider: &str) -> Option<&Budget> {
        self.budgets.get(provider)
    }

    /// Record usage for an agent+provider pair
    ///
    /// S4.3.2: Updates cumulative usage in the store
    pub fn record_usage(
        &mut self,
        agent_id: &str,
        provider: &str,
        tokens: u64,
        cost_usd: f64,
    ) {
        self.store.record_usage(agent_id, provider, tokens, cost_usd);
    }

    /// Check remaining tokens for a provider
    ///
    /// S4.3.3: Returns remaining daily tokens (or u64::MAX if no limit)
    pub fn remaining_tokens(&self, provider: &str) -> u64 {
        let budget = match self.budgets.get(provider) {
            Some(b) => b,
            None => return u64::MAX,
        };

        let daily_used = self.store.total_today_tokens(provider);
        let monthly_used = self.store.total_month_tokens(provider);

        let daily_remaining = budget.daily_tokens
            .map(|limit| limit.saturating_sub(daily_used))
            .unwrap_or(u64::MAX);

        let monthly_remaining = budget.monthly_tokens
            .map(|limit| limit.saturating_sub(monthly_used))
            .unwrap_or(u64::MAX);

        daily_remaining.min(monthly_remaining)
    }

    /// Check remaining cost for a provider
    ///
    /// S4.3.3: Returns remaining daily cost (or f64::MAX if no limit)
    pub fn remaining_cost_usd(&self, provider: &str) -> f64 {
        let budget = match self.budgets.get(provider) {
            Some(b) => b,
            None => return f64::MAX,
        };

        let daily_used = self.store.total_today_cost_usd(provider);
        let monthly_used = self.store.total_month_cost_usd(provider);

        let daily_remaining = budget.daily_cost_usd
            .map(|limit| (limit - daily_used).max(0.0))
            .unwrap_or(f64::MAX);

        let monthly_remaining = budget.monthly_cost_usd
            .map(|limit| (limit - monthly_used).max(0.0))
            .unwrap_or(f64::MAX);

        daily_remaining.min(monthly_remaining)
    }

    /// Check if budget is exceeded for a provider
    ///
    /// S4.3.4: Returns the exceeded action if budget is exceeded
    pub fn check_budget(&self, provider: &str) -> Option<ExceededAction> {
        let budget = self.budgets.get(provider)?;

        let daily_tokens_used = self.store.total_today_tokens(provider);
        let monthly_tokens_used = self.store.total_month_tokens(provider);
        let daily_cost_used = self.store.total_today_cost_usd(provider);
        let monthly_cost_used = self.store.total_month_cost_usd(provider);

        let exceeded = budget.daily_tokens.is_some_and(|l| daily_tokens_used >= l)
            || budget.monthly_tokens.is_some_and(|l| monthly_tokens_used >= l)
            || budget.daily_cost_usd.is_some_and(|l| daily_cost_used >= l)
            || budget.monthly_cost_usd.is_some_and(|l| monthly_cost_used >= l);

        if exceeded {
            Some(parse_exceeded_action(&budget.exceeded_action))
        } else {
            None
        }
    }

    /// Get a budget snapshot for a provider
    pub fn snapshot(&self, provider: &str) -> BudgetSnapshot {
        let budget = self.budgets.get(provider);
        let exceeded_action = budget
            .map(|b| parse_exceeded_action(&b.exceeded_action))
            .unwrap_or(ExceededAction::Stop);

        let daily_tokens_used = self.store.total_today_tokens(provider);
        let monthly_tokens_used = self.store.total_month_tokens(provider);
        let daily_cost_used = self.store.total_today_cost_usd(provider);
        let monthly_cost_used = self.store.total_month_cost_usd(provider);

        let exceeded = self.check_budget(provider).is_some();

        BudgetSnapshot {
            provider: provider.to_string(),
            daily_tokens_used,
            daily_token_limit: budget.and_then(|b| b.daily_tokens),
            monthly_tokens_used,
            monthly_token_limit: budget.and_then(|b| b.monthly_tokens),
            daily_cost_used,
            daily_cost_limit: budget.and_then(|b| b.daily_cost_usd),
            monthly_cost_used,
            monthly_cost_limit: budget.and_then(|b| b.monthly_cost_usd),
            exceeded,
            exceeded_action,
        }
    }
}

/// Parse exceeded action string
fn parse_exceeded_action(action: &str) -> ExceededAction {
    match action.to_lowercase().as_str() {
        "stop" | "deny" => ExceededAction::Stop,
        "fallback" => ExceededAction::Fallback,
        "warn" => ExceededAction::Warn,
        _ => ExceededAction::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_tracker_new_in_memory() {
        let tracker = BudgetTracker::new_in_memory();
        assert_eq!(tracker.remaining_tokens("openai"), u64::MAX);
    }

    #[test]
    fn test_budget_tracker_set_and_check() {
        let mut tracker = BudgetTracker::new_in_memory();
        let budget = Budget {
            daily_tokens: Some(1000),
            monthly_tokens: Some(10000),
            daily_cost_usd: Some(5.0),
            monthly_cost_usd: Some(50.0),
            exceeded_action: "stop".to_string(),
        };
        tracker.set_budget("openai", budget);

        // No usage yet
        assert_eq!(tracker.remaining_tokens("openai"), 1000);
        assert!((tracker.remaining_cost_usd("openai") - 5.0).abs() < 0.001);
        assert!(tracker.check_budget("openai").is_none());
    }

    #[test]
    fn test_budget_tracker_record_usage() {
        let mut tracker = BudgetTracker::new_in_memory();
        let budget = Budget {
            daily_tokens: Some(1000),
            monthly_tokens: Some(10000),
            daily_cost_usd: Some(5.0),
            monthly_cost_usd: Some(50.0),
            exceeded_action: "warn".to_string(),
        };
        tracker.set_budget("openai", budget);

        tracker.record_usage("com.test", "openai", 500, 2.0);
        assert_eq!(tracker.remaining_tokens("openai"), 500);
        assert!((tracker.remaining_cost_usd("openai") - 3.0).abs() < 0.001);
    }

    #[test]
    fn test_budget_tracker_exceeded_stop() {
        let mut tracker = BudgetTracker::new_in_memory();
        let budget = Budget {
            daily_tokens: Some(100),
            monthly_tokens: None,
            daily_cost_usd: None,
            monthly_cost_usd: None,
            exceeded_action: "stop".to_string(),
        };
        tracker.set_budget("openai", budget);

        tracker.record_usage("com.test", "openai", 150, 0.5);

        let exceeded = tracker.check_budget("openai");
        assert!(exceeded.is_some());
        assert_eq!(exceeded.unwrap(), ExceededAction::Stop);
    }

    #[test]
    fn test_budget_tracker_exceeded_fallback() {
        let mut tracker = BudgetTracker::new_in_memory();
        let budget = Budget {
            daily_tokens: None,
            monthly_tokens: None,
            daily_cost_usd: Some(1.0),
            monthly_cost_usd: None,
            exceeded_action: "fallback".to_string(),
        };
        tracker.set_budget("openai", budget);

        tracker.record_usage("com.test", "openai", 100, 2.0);

        let exceeded = tracker.check_budget("openai");
        assert!(exceeded.is_some());
        assert_eq!(exceeded.unwrap(), ExceededAction::Fallback);
    }

    #[test]
    fn test_budget_tracker_snapshot() {
        let mut tracker = BudgetTracker::new_in_memory();
        let budget = Budget {
            daily_tokens: Some(1000),
            monthly_tokens: Some(10000),
            daily_cost_usd: Some(5.0),
            monthly_cost_usd: Some(50.0),
            exceeded_action: "warn".to_string(),
        };
        tracker.set_budget("openai", budget);
        tracker.record_usage("com.test", "openai", 200, 1.0);

        let snapshot = tracker.snapshot("openai");
        assert_eq!(snapshot.daily_tokens_used, 200);
        assert_eq!(snapshot.daily_token_limit, Some(1000));
        assert!(!snapshot.exceeded);
    }

    #[test]
    fn test_parse_exceeded_action() {
        assert_eq!(parse_exceeded_action("stop"), ExceededAction::Stop);
        assert_eq!(parse_exceeded_action("deny"), ExceededAction::Stop);
        assert_eq!(parse_exceeded_action("fallback"), ExceededAction::Fallback);
        assert_eq!(parse_exceeded_action("warn"), ExceededAction::Warn);
        assert_eq!(parse_exceeded_action("unknown"), ExceededAction::Stop);
    }

    #[test]
    fn test_no_budget_unlimited() {
        let tracker = BudgetTracker::new_in_memory();
        assert_eq!(tracker.remaining_tokens("nonexistent"), u64::MAX);
        assert_eq!(tracker.remaining_cost_usd("nonexistent"), f64::MAX);
        assert!(tracker.check_budget("nonexistent").is_none());
    }
}
