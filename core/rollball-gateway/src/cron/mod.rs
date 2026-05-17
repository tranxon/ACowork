//! Cron scheduler for time-based Intent triggers
//!
//! S4.5: Allows Agents to register cron schedules. When a schedule fires,
//! the Gateway pushes an IntentReceived to the registered Agent.
//!
//! Supports simplified 5-field cron expressions: `min hour day month weekday`
//!
//! Example schedules:
//! - `0 * * * *`     — every hour at minute 0
//! - `*/15 * * * *`  — every 15 minutes
//! - `0 9 * * 1-5`   — weekdays at 9:00 AM
//! - `0 0 1 * *`     — first day of every month at midnight

pub mod store;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use chrono::{Timelike, Datelike};

use crate::ipc::session::SessionManager;
pub use store::{CronStore, StoredCronEntry, CronStoreError};

/// A registered cron entry
#[derive(Debug, Clone)]
pub struct CronEntry {
    /// Unique ID for this cron entry
    pub id: String,
    /// Agent that owns this cron entry
    pub agent_id: String,
    /// Cron schedule expression (5-field)
    pub schedule: String,
    /// Action to fire when the schedule triggers
    pub action: String,
    /// Params to include in the IntentReceived
    pub params: serde_json::Value,
    /// Parsed schedule fields
    parsed: CronFields,
}

/// Parsed cron fields (min, hour, day, month, weekday)
#[derive(Debug, Clone)]
struct CronFields {
    minutes: Vec<u8>,
    hours: Vec<u8>,
    days: Vec<u8>,
    months: Vec<u8>,
    weekdays: Vec<u8>,
}

/// Cron scheduler — manages cron entries and fires triggers
#[derive(Debug, Clone, Default)]
pub struct CronScheduler {
    /// Cron entries by ID
    entries: HashMap<String, CronEntry>,
    /// Next ID counter
    next_id: u64,
}

impl CronScheduler {
    /// Create a new empty scheduler
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 1,
        }
    }

    /// Register a new cron entry
    ///
    /// Returns the cron entry ID on success, or an error message if the
    /// schedule expression is invalid.
    pub fn register(
        &mut self,
        agent_id: &str,
        schedule: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<String, String> {
        let parsed = parse_cron(schedule)?;
        let id = format!("cron-{}", self.next_id);
        self.next_id += 1;

        let entry = CronEntry {
            id: id.clone(),
            agent_id: agent_id.to_string(),
            schedule: schedule.to_string(),
            action: action.to_string(),
            params,
            parsed,
        };

        tracing::info!(
            "Cron registered: id={} agent={} schedule={} action={}",
            id, agent_id, schedule, action
        );
        self.entries.insert(id.clone(), entry);
        Ok(id)
    }

    /// Unregister a cron entry by ID
    pub fn unregister(&mut self, cron_id: &str) -> bool {
        if let Some(entry) = self.entries.remove(cron_id) {
            tracing::info!("Cron unregistered: id={} agent={}", cron_id, entry.agent_id);
            true
        } else {
            false
        }
    }

    /// Unregister all cron entries for an agent (called on agent stop)
    pub fn unregister_agent(&mut self, agent_id: &str) -> usize {
        let ids_to_remove: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.agent_id == agent_id)
            .map(|(id, _)| id.clone())
            .collect();

        let count = ids_to_remove.len();
        for id in ids_to_remove {
            self.entries.remove(&id);
        }
        if count > 0 {
            tracing::info!("Cron: unregistered {} entries for agent {}", count, agent_id);
        }
        count
    }

    /// Check which cron entries should fire at the given time
    ///
    /// Returns a list of (agent_id, action, params) tuples for entries
    /// whose schedule matches the given time.
    pub fn check(&self, time: &chrono::DateTime<chrono::Utc>) -> Vec<(&str, &str, &serde_json::Value)> {
        let minute = time.minute() as u8;
        let hour = time.hour() as u8;
        let day = time.day() as u8;
        let month = time.month() as u8;
        let weekday = time.weekday().num_days_from_sunday() as u8; // 0=Sun, 6=Sat

        self.entries
            .values()
            .filter(|e| {
                let p = &e.parsed;
                p.minutes.contains(&minute)
                    && p.hours.contains(&hour)
                    && p.days.contains(&day)
                    && p.months.contains(&month)
                    && p.weekdays.contains(&weekday)
            })
            .map(|e| (e.agent_id.as_str(), e.action.as_str(), &e.params))
            .collect()
    }

    /// Get the number of registered entries
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the scheduler is empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get all entries for a specific agent
    pub fn entries_for_agent(&self, agent_id: &str) -> Vec<&CronEntry> {
        self.entries
            .values()
            .filter(|e| e.agent_id == agent_id)
            .collect()
    }

    /// Load entries from a CronStore (used on Gateway restart)
    pub fn load_from_store(&mut self, store: &CronStore) -> Result<(), String> {
        let stored = store.list_all().map_err(|e| format!("Failed to load cron entries: {}", e))?;
        for entry in stored {
            if let Ok(parsed) = parse_cron(&entry.schedule) {
                let cron_entry = CronEntry {
                    id: entry.id.clone(),
                    agent_id: entry.agent_id.clone(),
                    schedule: entry.schedule.clone(),
                    action: entry.action.clone(),
                    params: serde_json::from_str(&entry.params)
                        .unwrap_or(serde_json::json!({})),
                    parsed,
                };
                self.entries.insert(entry.id.clone(), cron_entry);

                // Update next_id counter to avoid ID collisions
                if let Some(num) = entry.id.strip_prefix("cron-")
                    && let Ok(n) = num.parse::<u64>()
                    && n >= self.next_id {
                        self.next_id = n + 1;
                }
            } else {
                tracing::warn!(
                    "Skipping cron entry with invalid schedule: id={} schedule={}",
                    entry.id, entry.schedule
                );
            }
        }
        if !self.entries.is_empty() {
            tracing::info!("Loaded {} cron entries from store", self.entries.len());
        }
        Ok(())
    }
}

// ── Cron expression parser ──────────────────────────────────────────────────

/// Parse a 5-field cron expression into CronFields
fn parse_cron(expr: &str) -> Result<CronFields, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "Cron expression must have 5 fields (min hour day month weekday), got {}: '{}'",
            fields.len(),
            expr
        ));
    }

    Ok(CronFields {
        minutes: parse_field(fields[0], 0, 59, "minute")?,
        hours: parse_field(fields[1], 0, 23, "hour")?,
        days: parse_field(fields[2], 1, 31, "day")?,
        months: parse_field(fields[3], 1, 12, "month")?,
        weekdays: parse_field(fields[4], 0, 6, "weekday")?,
    })
}

/// Parse a single cron field (supports *, ranges, steps, and lists)
fn parse_field(field: &str, min: u8, max: u8, name: &str) -> Result<Vec<u8>, String> {
    let mut values = Vec::new();

    for part in field.split(',') {
        if part.contains('/') {
            // Step syntax: start/step or */step
            let parts: Vec<&str> = part.split('/').collect();
            if parts.len() != 2 {
                return Err(format!("Invalid step syntax in {} field: '{}'", name, part));
            }
            let step: u8 = parts[1]
                .parse()
                .map_err(|_| format!("Invalid step value in {} field: '{}'", name, parts[1]))?;
            if step == 0 {
                return Err(format!("Step value must be > 0 in {} field", name));
            }

            let start = if parts[0] == "*" {
                min
            } else {
                parts[0]
                    .parse()
                    .map_err(|_| format!("Invalid start value in {} field: '{}'", name, parts[0]))?
            };

            for v in (start..=max).step_by(step as usize) {
                if v >= min && v <= max && !values.contains(&v) {
                    values.push(v);
                }
            }
        } else if part.contains('-') {
            // Range syntax: start-end
            let parts: Vec<&str> = part.split('-').collect();
            if parts.len() != 2 {
                return Err(format!("Invalid range syntax in {} field: '{}'", name, part));
            }
            let start: u8 = parts[0]
                .parse()
                .map_err(|_| format!("Invalid range start in {} field: '{}'", name, parts[0]))?;
            let end: u8 = parts[1]
                .parse()
                .map_err(|_| format!("Invalid range end in {} field: '{}'", name, parts[1]))?;
            for v in start..=end {
                if v >= min && v <= max && !values.contains(&v) {
                    values.push(v);
                }
            }
        } else if part == "*" {
            // Wildcard: all values
            for v in min..=max {
                values.push(v);
            }
        } else {
            // Single value
            let v: u8 = part
                .parse()
                .map_err(|_| format!("Invalid value in {} field: '{}'", name, part))?;
            if v < min || v > max {
                return Err(format!(
                    "Value {} out of range [{},{}] in {} field",
                    v, min, max, name
                ));
            }
            values.push(v);
        }
    }

    values.sort();
    Ok(values)
}

/// Run the cron scheduler loop as a background task.
///
/// Checks every minute for entries that should fire, and pushes
/// IntentReceived messages to the target Agent's IPC session.
///
/// If the target Agent is not running, attempts to start it first
/// (via LifecycleManager), then pushes the Intent.
pub async fn run_cron_scheduler(
    scheduler: Arc<Mutex<CronScheduler>>,
    session_mgr: Arc<Mutex<SessionManager>>,
    gateway_state: crate::ipc::server::SharedState,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    // Skip the first immediate tick
    interval.tick().await;

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();

        let triggers: Vec<(String, String, serde_json::Value)> = {
            let sched = scheduler.lock().await;
            sched
                .check(&now)
                .into_iter()
                .map(|(agent_id, action, params)| {
                    (agent_id.to_string(), action.to_string(), params.clone())
                })
                .collect()
        };

        for (agent_id, action, params) in triggers {
            tracing::info!("Cron fired: agent={} action={}", agent_id, action);

            // Check if agent is running; if not, try to start it
            let is_running = {
                let gw = gateway_state.read().await;
                gw.is_running(&agent_id)
            };

            if !is_running {
                tracing::info!("Cron: agent {} not running, attempting to start", agent_id);
                let mut gw = gateway_state.write().await;
                if gw.is_installed(&agent_id) {
                    // Start the agent process
                    let grpc_addr = crate::grpc::server::default_grpc_addr();
                    let gateway_grpc_endpoint = format!("http://{}", grpc_addr);
                    let log_file_size_mb = gw.config.as_ref().map(|c| c.log_file_size_mb).unwrap_or(10);
                    let mut lifecycle = crate::lifecycle::manager::LifecycleManager::new(0, gateway_grpc_endpoint, log_file_size_mb);
                    match lifecycle.start_agent(&agent_id, &mut gw, false).await {
                        Ok(()) => {
                            tracing::info!("Cron: started agent {} for scheduled trigger", agent_id);
                        }
                        Err(e) => {
                            tracing::error!("Cron: failed to start agent {}: {}", agent_id, e);
                            continue;
                        }
                    }
                } else {
                    tracing::warn!("Cron: agent {} not installed, skipping trigger", agent_id);
                    continue;
                }
            }

            // Find the agent's session and push IntentReceived
            let pushed = {
                let mgr = session_mgr.lock().await;
                if let Some((_conn_id, session)) = mgr.find_by_agent_id(&agent_id) {
                    let intent_msg = rollball_core::protocol::GatewayResponse::IntentReceived {
                        from: format!("cron:{}", agent_id),
                        action: action.clone(),
                        params: params.clone(),
                        command: None,
                    };
                    let _ = session;
                    drop(mgr);

                    // Re-acquire just for pushing
                    let mgr = session_mgr.lock().await;
                    if let Some((_, session)) = mgr.find_by_agent_id(&agent_id) {
                        session.push_message(intent_msg).await
                    } else {
                        false
                    }
                } else {
                    tracing::warn!(
                        "Cron trigger skipped: agent {} not connected (session not found)",
                        agent_id
                    );
                    false
                }
            };

            if !pushed {
                tracing::warn!(
                    "Cron trigger failed to push: agent={} action={}",
                    agent_id,
                    action
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_wildcard() {
        let fields = parse_cron("* * * * *").unwrap();
        assert_eq!(fields.minutes.len(), 60);
        assert_eq!(fields.hours.len(), 24);
        assert_eq!(fields.days.len(), 31);
        assert_eq!(fields.months.len(), 12);
        assert_eq!(fields.weekdays.len(), 7);
    }

    #[test]
    fn test_parse_specific_values() {
        let fields = parse_cron("0 9 * * 1-5").unwrap();
        assert_eq!(fields.minutes, vec![0]);
        assert_eq!(fields.hours, vec![9]);
        assert_eq!(fields.weekdays, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_parse_step() {
        let fields = parse_cron("*/15 * * * *").unwrap();
        assert_eq!(fields.minutes, vec![0, 15, 30, 45]);
    }

    #[test]
    fn test_parse_list() {
        let fields = parse_cron("0,30 9,17 * * *").unwrap();
        assert_eq!(fields.minutes, vec![0, 30]);
        assert_eq!(fields.hours, vec![9, 17]);
    }

    #[test]
    fn test_parse_invalid_field_count() {
        let result = parse_cron("* * *");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("5 fields"));
    }

    #[test]
    fn test_parse_out_of_range() {
        let result = parse_cron("60 * * * *");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("out of range"));
    }

    #[test]
    fn test_scheduler_register() {
        let mut scheduler = CronScheduler::new();
        let id = scheduler
            .register("com.example.weather", "0 * * * *", "hourly_check", serde_json::json!({}))
            .unwrap();
        assert!(id.starts_with("cron-"));
        assert_eq!(scheduler.len(), 1);
    }

    #[test]
    fn test_scheduler_unregister() {
        let mut scheduler = CronScheduler::new();
        let id = scheduler
            .register("com.example.weather", "0 * * * *", "hourly_check", serde_json::json!({}))
            .unwrap();
        assert!(scheduler.unregister(&id));
        assert!(scheduler.is_empty());
    }

    #[test]
    fn test_scheduler_unregister_agent() {
        let mut scheduler = CronScheduler::new();
        scheduler
            .register("com.example.weather", "0 * * * *", "hourly_check", serde_json::json!({}))
            .unwrap();
        scheduler
            .register("com.example.weather", "0 9 * * *", "morning_check", serde_json::json!({}))
            .unwrap();
        scheduler
            .register("com.example.calendar", "0 0 * * *", "daily_check", serde_json::json!({}))
            .unwrap();
        assert_eq!(scheduler.len(), 3);

        let count = scheduler.unregister_agent("com.example.weather");
        assert_eq!(count, 2);
        assert_eq!(scheduler.len(), 1);
    }

    #[test]
    fn test_scheduler_check() {
        let mut scheduler = CronScheduler::new();
        scheduler
            .register("com.example.weather", "30 9 * * *", "morning_report", serde_json::json!({"type": "daily"}))
            .unwrap();

        // 9:30 AM on any day should match
        let time = chrono::DateTime::parse_from_rfc3339("2026-04-24T09:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let matches = scheduler.check(&time);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "com.example.weather");
        assert_eq!(matches[0].1, "morning_report");

        // 9:31 AM should NOT match
        let time2 = chrono::DateTime::parse_from_rfc3339("2026-04-24T09:31:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let matches2 = scheduler.check(&time2);
        assert!(matches2.is_empty());
    }

    #[test]
    fn test_scheduler_check_step() {
        let mut scheduler = CronScheduler::new();
        scheduler
            .register("com.example.monitor", "*/15 * * * *", "health_check", serde_json::json!({}))
            .unwrap();

        // Should match at minute 0, 15, 30, 45
        for minute in [0, 15, 30, 45] {
            let time = chrono::DateTime::parse_from_rfc3339(&format!(
                "2026-04-24T09:{:02}:00Z",
                minute
            ))
            .unwrap()
            .with_timezone(&chrono::Utc);
            let matches = scheduler.check(&time);
            assert_eq!(matches.len(), 1, "Should match at minute {}", minute);
        }

        // Should NOT match at minute 7
        let time = chrono::DateTime::parse_from_rfc3339("2026-04-24T09:07:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let matches = scheduler.check(&time);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_entries_for_agent() {
        let mut scheduler = CronScheduler::new();
        scheduler
            .register("com.example.weather", "0 * * * *", "hourly", serde_json::json!({}))
            .unwrap();
        scheduler
            .register("com.example.weather", "0 9 * * *", "morning", serde_json::json!({}))
            .unwrap();
        scheduler
            .register("com.example.calendar", "0 0 * * *", "daily", serde_json::json!({}))
            .unwrap();

        let weather_entries = scheduler.entries_for_agent("com.example.weather");
        assert_eq!(weather_entries.len(), 2);

        let calendar_entries = scheduler.entries_for_agent("com.example.calendar");
        assert_eq!(calendar_entries.len(), 1);
    }
}
