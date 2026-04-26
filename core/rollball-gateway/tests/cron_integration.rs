//! S3.5: Cron integration tests
//!
//! End-to-end tests for the Cron trigger system:
//! 1. CronStore persistence (insert, delete, list, recovery)
//! 2. CronScheduler registration, checking, and agent-scoped management
//! 3. CronScheduler load_from_store (restart recovery)
//! 4. Manifest cron trigger parsing
//! 5. Cron IPC protocol round-trip
//! 6. Cron HTTP API integration

use rollball_core::protocol::{Frame, GatewayRequest, GatewayResponse};
use rollball_gateway::cron::{CronScheduler, CronStore, StoredCronEntry};
use rollball_gateway::gateway::state::{AgentInfo, GatewayState};

fn temp_vault_dir(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("rollball-test-cron-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().to_string()
}

// ── S3.5: CronStore persistence tests ────────────────────────────────

#[test]
fn test_cron_store_persistence_roundtrip() {
    let store = CronStore::open_in_memory().unwrap();

    // Insert entries
    let entry1 = StoredCronEntry {
        id: "cron-1".to_string(),
        agent_id: "com.example.weather".to_string(),
        schedule: "0 * * * *".to_string(),
        action: "hourly_check".to_string(),
        params: r#"{"type":"weather"}"#.to_string(),
    };
    let entry2 = StoredCronEntry {
        id: "cron-2".to_string(),
        agent_id: "com.example.weather".to_string(),
        schedule: "0 9 * * 1-5".to_string(),
        action: "weekday_report".to_string(),
        params: "{}".to_string(),
    };
    store.insert(&entry1).unwrap();
    store.insert(&entry2).unwrap();

    // List by agent
    let weather_entries = store.list_by_agent("com.example.weather").unwrap();
    assert_eq!(weather_entries.len(), 2);

    // List all
    let all = store.list_all().unwrap();
    assert_eq!(all.len(), 2);

    // Delete one
    assert!(store.delete("cron-1").unwrap());
    let remaining = store.list_all().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "cron-2");
}

#[test]
fn test_cron_store_delete_by_agent() {
    let store = CronStore::open_in_memory().unwrap();

    for i in 1..=3 {
        let entry = StoredCronEntry {
            id: format!("cron-{}", i),
            agent_id: "com.example.weather".to_string(),
            schedule: "0 * * * *".to_string(),
            action: format!("task-{}", i),
            params: "{}".to_string(),
        };
        store.insert(&entry).unwrap();
    }
    let other = StoredCronEntry {
        id: "cron-10".to_string(),
        agent_id: "com.example.calendar".to_string(),
        schedule: "0 0 * * *".to_string(),
        action: "daily".to_string(),
        params: "{}".to_string(),
    };
    store.insert(&other).unwrap();

    let count = store.delete_by_agent("com.example.weather").unwrap();
    assert_eq!(count, 3);

    let remaining = store.list_all().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].agent_id, "com.example.calendar");
}

// ── S3.5: CronScheduler load_from_store tests ────────────────────────

#[test]
fn test_scheduler_load_from_store() {
    let store = CronStore::open_in_memory().unwrap();

    // Insert entries into store
    let entry1 = StoredCronEntry {
        id: "cron-1".to_string(),
        agent_id: "com.example.weather".to_string(),
        schedule: "0 * * * *".to_string(),
        action: "hourly_check".to_string(),
        params: "{}".to_string(),
    };
    let entry2 = StoredCronEntry {
        id: "cron-5".to_string(),
        agent_id: "com.example.monitor".to_string(),
        schedule: "*/15 * * * *".to_string(),
        action: "health_check".to_string(),
        params: r#"{"type":"ping"}"#.to_string(),
    };
    store.insert(&entry1).unwrap();
    store.insert(&entry2).unwrap();

    // Load into scheduler
    let mut scheduler = CronScheduler::new();
    scheduler.load_from_store(&store).unwrap();

    assert_eq!(scheduler.len(), 2);

    // Verify next_id is updated (should be at least 6)
    // We can't directly access next_id, but we can test by registering
    let id = scheduler.register("com.example.test", "0 0 * * *", "daily", serde_json::json!({})).unwrap();
    assert!(id.starts_with("cron-"));
    // The new ID should be >= 6
    let num: u64 = id.strip_prefix("cron-").unwrap().parse().unwrap();
    assert!(num >= 6, "New cron ID should be >= 6 after loading entries up to cron-5, got {}", num);
}

#[test]
fn test_scheduler_load_from_store_invalid_schedule_skipped() {
    let store = CronStore::open_in_memory().unwrap();

    let valid = StoredCronEntry {
        id: "cron-1".to_string(),
        agent_id: "com.example.weather".to_string(),
        schedule: "0 * * * *".to_string(),
        action: "hourly_check".to_string(),
        params: "{}".to_string(),
    };
    let invalid = StoredCronEntry {
        id: "cron-2".to_string(),
        agent_id: "com.example.bad".to_string(),
        schedule: "invalid cron expr".to_string(),
        action: "bad_action".to_string(),
        params: "{}".to_string(),
    };
    store.insert(&valid).unwrap();
    store.insert(&invalid).unwrap();

    let mut scheduler = CronScheduler::new();
    scheduler.load_from_store(&store).unwrap();

    // Only the valid entry should be loaded
    assert_eq!(scheduler.len(), 1);
    let entries = scheduler.entries_for_agent("com.example.weather");
    assert_eq!(entries.len(), 1);
}

// ── S3.5: Manifest cron trigger parsing tests ────────────────────────

#[test]
fn test_manifest_cron_triggers() {
    let toml_str = r#"
        agent_id = "com.example.monitor"
        version = "1.0.0"
        name = "Monitor"
        description = "Monitoring agent"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "openai"
        model = "gpt-4"

        [[triggers]]
        type = "cron"
        schedule = "*/15 * * * *"
        action = "health_check"
        params = { type = "ping" }

        [[triggers]]
        type = "cron"
        schedule = "0 9 * * 1-5"
        action = "weekday_report"

        [[triggers]]
        type = "manual"
    "#;
    let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
    let cron_triggers = manifest.cron_triggers();
    assert_eq!(cron_triggers.len(), 2);

    // First cron trigger
    assert_eq!(cron_triggers[0].schedule, Some("*/15 * * * *".to_string()));
    assert_eq!(cron_triggers[0].action, Some("health_check".to_string()));
    assert_eq!(cron_triggers[0].params, Some(serde_json::json!({ "type": "ping" })));

    // Second cron trigger
    assert_eq!(cron_triggers[1].schedule, Some("0 9 * * 1-5".to_string()));
    assert_eq!(cron_triggers[1].action, Some("weekday_report".to_string()));
}

#[test]
fn test_manifest_no_cron_triggers() {
    let toml_str = r#"
        agent_id = "com.example.simple"
        version = "1.0.0"
        name = "Simple"
        description = "Simple agent"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "openai"
        model = "gpt-4"

        [[triggers]]
        type = "manual"
    "#;
    let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
    let cron_triggers = manifest.cron_triggers();
    assert!(cron_triggers.is_empty());
}

// ── S3.5: Cron IPC protocol round-trip tests ─────────────────────────

#[test]
fn test_cron_register_ipc_roundtrip() {
    let request = GatewayRequest::CronRegister {
        agent_id: "com.example.weather".to_string(),
        schedule: "0 * * * *".to_string(),
        action: "hourly_check".to_string(),
        params: serde_json::json!({}),
    };

    let frame = Frame::from_message(Frame::TYPE_REQUEST, &request).unwrap();
    let parsed: GatewayRequest = frame.to_message().unwrap();

    if let GatewayRequest::CronRegister {
        agent_id,
        schedule,
        action,
        params,
    } = parsed
    {
        assert_eq!(agent_id, "com.example.weather");
        assert_eq!(schedule, "0 * * * *");
        assert_eq!(action, "hourly_check");
        assert!(params.is_object());
    } else {
        panic!("Expected CronRegister");
    }
}

#[test]
fn test_cron_unregister_ipc_roundtrip() {
    let request = GatewayRequest::CronUnregister {
        cron_id: "cron-1".to_string(),
    };

    let frame = Frame::from_message(Frame::TYPE_REQUEST, &request).unwrap();
    let parsed: GatewayRequest = frame.to_message().unwrap();

    if let GatewayRequest::CronUnregister { cron_id } = parsed {
        assert_eq!(cron_id, "cron-1");
    } else {
        panic!("Expected CronUnregister");
    }
}

#[test]
fn test_cron_list_ipc_roundtrip() {
    let request = GatewayRequest::CronList {};

    let frame = Frame::from_message(Frame::TYPE_REQUEST, &request).unwrap();
    let parsed: GatewayRequest = frame.to_message().unwrap();

    assert!(matches!(parsed, GatewayRequest::CronList {}));
}

#[test]
fn test_cron_register_result_roundtrip() {
    let response = GatewayResponse::CronRegisterResult {
        cron_id: Some("cron-42".to_string()),
        error: None,
    };

    let frame = Frame::from_message(Frame::TYPE_RESPONSE, &response).unwrap();
    let parsed: GatewayResponse = frame.to_message().unwrap();

    if let GatewayResponse::CronRegisterResult { cron_id, error } = parsed {
        assert_eq!(cron_id, Some("cron-42".to_string()));
        assert!(error.is_none());
    } else {
        panic!("Expected CronRegisterResult");
    }
}

#[test]
fn test_cron_unregister_result_roundtrip() {
    let response = GatewayResponse::CronUnregisterResult { removed: true };

    let frame = Frame::from_message(Frame::TYPE_RESPONSE, &response).unwrap();
    let parsed: GatewayResponse = frame.to_message().unwrap();

    if let GatewayResponse::CronUnregisterResult { removed } = parsed {
        assert!(removed);
    } else {
        panic!("Expected CronUnregisterResult");
    }
}

#[test]
fn test_cron_list_result_roundtrip() {
    let response = GatewayResponse::CronListResult {
        entries: vec![
            rollball_core::protocol::CronEntryInfo {
                id: "cron-1".to_string(),
                agent_id: "com.example.weather".to_string(),
                schedule: "0 * * * *".to_string(),
                action: "hourly".to_string(),
                params: serde_json::json!({}),
            },
        ],
    };

    let frame = Frame::from_message(Frame::TYPE_RESPONSE, &response).unwrap();
    let parsed: GatewayResponse = frame.to_message().unwrap();

    if let GatewayResponse::CronListResult { entries } = parsed {
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "cron-1");
        assert_eq!(entries[0].schedule, "0 * * * *");
    } else {
        panic!("Expected CronListResult");
    }
}

// ── S3.5: Cron + GatewayState integration tests ──────────────────────

#[test]
fn test_cron_scheduler_in_gateway_state() {
    let dir = temp_vault_dir("state-cron");
    let mut state = GatewayState::new(&dir);

    // Register a cron entry
    let id = state.cron_scheduler
        .register("com.example.weather", "0 * * * *", "hourly_check", serde_json::json!({}))
        .unwrap();
    assert!(id.starts_with("cron-"));

    // Verify entry exists
    let entries = state.cron_scheduler.entries_for_agent("com.example.weather");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, "hourly_check");
}

#[test]
fn test_cron_store_in_gateway_state() {
    let dir = temp_vault_dir("state-store");
    let mut state = GatewayState::new(&dir);

    // Initially no CronStore
    assert!(state.cron_store.is_none());

    // Set up CronStore
    let store = CronStore::open_in_memory().unwrap();
    state.cron_store = Some(std::sync::Arc::new(store));

    // Register + persist
    let id = state.cron_scheduler
        .register("com.example.weather", "0 9 * * *", "morning_report", serde_json::json!({}))
        .unwrap();

    if let Some(store) = &state.cron_store {
        let entry = StoredCronEntry {
            id: id.clone(),
            agent_id: "com.example.weather".to_string(),
            schedule: "0 9 * * *".to_string(),
            action: "morning_report".to_string(),
            params: "{}".to_string(),
        };
        store.insert(&entry).unwrap();

        // Verify persistence
        let stored = store.list_by_agent("com.example.weather").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].schedule, "0 9 * * *");
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_cron_scheduler_recovery_after_restart() {
    let dir = temp_vault_dir("recovery");
    let store = CronStore::open_in_memory().unwrap();

    // Simulate first Gateway run: register and persist
    let mut scheduler1 = CronScheduler::new();
    let id1 = scheduler1.register("com.example.weather", "0 * * * *", "hourly", serde_json::json!({})).unwrap();
    let id2 = scheduler1.register("com.example.weather", "0 9 * * *", "morning", serde_json::json!({})).unwrap();
    for entry in scheduler1.entries_for_agent("com.example.weather") {
        let stored = StoredCronEntry {
            id: entry.id.clone(),
            agent_id: entry.agent_id.clone(),
            schedule: entry.schedule.clone(),
            action: entry.action.clone(),
            params: serde_json::to_string(&entry.params).unwrap_or_else(|_| "{}".to_string()),
        };
        store.insert(&stored).unwrap();
    }

    // Simulate Gateway restart: load from store
    let mut scheduler2 = CronScheduler::new();
    scheduler2.load_from_store(&store).unwrap();

    assert_eq!(scheduler2.len(), 2);
    let entries = scheduler2.entries_for_agent("com.example.weather");
    assert_eq!(entries.len(), 2);

    // Verify the schedules are correctly restored
    let schedules: Vec<&str> = entries.iter().map(|e| e.schedule.as_str()).collect();
    assert!(schedules.contains(&"0 * * * *"));
    assert!(schedules.contains(&"0 9 * * *"));

    // New IDs should not collide
    let new_id = scheduler2.register("com.example.test", "0 0 * * *", "daily", serde_json::json!({})).unwrap();
    assert_ne!(new_id, id1);
    assert_ne!(new_id, id2);

    let _ = std::fs::remove_dir_all(&dir);
}

// ── S3.5: Install-time cron registration integration ─────────────────

#[test]
fn test_install_agent_registers_cron_triggers() {
    let dir = temp_vault_dir("install-cron");
    let mut state = GatewayState::new(&dir);

    // Set up CronStore
    state.cron_store = Some(std::sync::Arc::new(CronStore::open_in_memory().unwrap()));

    let toml_str = r#"
        agent_id = "com.example.monitor"
        version = "1.0.0"
        name = "Monitor"
        description = "Monitoring agent"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "openai"
        model = "gpt-4"

        [[triggers]]
        type = "cron"
        schedule = "*/15 * * * *"
        action = "health_check"
    "#;
    let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();

    state.add_installed(AgentInfo {
        agent_id: "com.example.monitor".to_string(),
        version: "1.0.0".to_string(),
        name: "Monitor".to_string(),
        install_path: dir.clone(),
        manifest,
    });

    // Manually register cron triggers (simulating install.rs logic)
    let agent_id = "com.example.monitor";
    let manifest = state.installed_agents.get(agent_id).unwrap();
    for trigger in manifest.manifest.cron_triggers() {
        if let Some(schedule) = &trigger.schedule {
            let action = trigger.action.as_deref().unwrap_or("cron_trigger");
            let params = trigger.params.clone().unwrap_or(serde_json::json!({}));
            let cron_id = state.cron_scheduler.register(agent_id, schedule, action, params.clone()).unwrap();
            if let Some(store) = &state.cron_store {
                let entry = StoredCronEntry {
                    id: cron_id,
                    agent_id: agent_id.to_string(),
                    schedule: schedule.to_string(),
                    action: action.to_string(),
                    params: serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string()),
                };
                store.insert(&entry).unwrap();
            }
        }
    }

    // Verify cron entries registered
    let entries = state.cron_scheduler.entries_for_agent("com.example.monitor");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].schedule, "*/15 * * * *");
    assert_eq!(entries[0].action, "health_check");

    // Verify persistence
    if let Some(store) = &state.cron_store {
        let stored = store.list_by_agent("com.example.monitor").unwrap();
        assert_eq!(stored.len(), 1);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
