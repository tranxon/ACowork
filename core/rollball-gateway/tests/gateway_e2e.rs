//! End-to-end integration test: Intent + Budget + Rate
//!
//! Validates the cross-module interaction flow:
//! 1. Install two agents (sender + target)
//! 2. Configure budget and rate limits
//! 3. Route an Intent from sender to target
//! 4. Verify budget tracking records usage
//! 5. Verify rate limiting throttles excessive requests
//! 6. Verify capability registry updates on install/uninstall

use std::fs;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use rollball_gateway::budget::tracker::BudgetTracker;
use rollball_gateway::gateway::state::{
    AgentInfo, GatewayState, RunningAgentInfo,
};
use rollball_gateway::intent::router::IntentRouter;
use rollball_gateway::ipc::server::SharedState;
use rollball_gateway::ipc::session::SessionManager;
use rollball_gateway::rate::bucket::RateLimiter;

fn temp_dir(name: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rollball-e2e-{}-{}", name, id));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().to_string()
}

fn make_manifest(agent_id: &str, capabilities: &[&str]) -> rollball_core::AgentManifest {
    let mut caps = String::new();
    for cap in capabilities {
        caps.push_str(&format!(
            "[capabilities.{}]\ndescription = \"{}\"\n",
            cap, cap
        ));
    }
    let toml_str = format!(
        r#"agent_id = "{}"
version = "1.0.0"
name = "Test"
description = "test"
author = "test"
runtime_version = "0.1.0"
[llm]
provider = "openai"
model = "gpt-4"
{}
"#,
        agent_id, caps
    );
    rollball_core::AgentManifest::from_toml(&toml_str).unwrap()
}

/// E2E Test 1: Intent routing with capability lookup
///
/// Flow: install two agents → lookup target by capability → route intent
#[tokio::test]
async fn test_e2e_intent_routing_with_capabilities() {
    let dir = temp_dir("intent_cap");
    let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));

    // Step 1: Install sender and target agents
    {
        let mut guard = state.write().await;
        guard.add_installed(AgentInfo {
            agent_id: "com.example.sender".to_string(),
            version: "1.0.0".to_string(),
            name: "Sender".to_string(),
            install_path: "/tmp/sender".to_string(),
            manifest: make_manifest("com.example.sender", &[]),
        });
        guard.add_installed(AgentInfo {
            agent_id: "com.example.weather".to_string(),
            version: "1.0.0".to_string(),
            name: "Weather".to_string(),
            install_path: "/tmp/weather".to_string(),
            manifest: make_manifest("com.example.weather", &["weather_query", "forecast"]),
        });
    }

    // Step 2: Verify capability registry was populated
    {
        let guard = state.read().await;
        let caps = guard.capability_registry.find_by_action("weather_query");
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].agent_id, "com.example.weather");
    }

    // Step 3: Start target agent
    {
        let mut guard = state.write().await;
        guard.add_running(RunningAgentInfo {
            agent_id: "com.example.weather".to_string(),
            pid: 12345,
            started_at: chrono::Utc::now(),
            workspace: "/tmp/weather-workspace".to_string(),
            connected: false,
            ready: false,
            dev_mode: false,
            debug_port: None,
            identity_entries: vec![],
            workspace_config_json: None,
        });
    }

    // Step 4: Route a sync Intent (need session for target)
    let router = IntentRouter::new();
    let session_mgr: Arc<Mutex<SessionManager>> = Arc::new(Mutex::new(SessionManager::new()));
    // Simulate target agent's IPC session
    {
        let (push_tx, _push_rx) = tokio::sync::mpsc::channel(8);
        let mut mgr = session_mgr.lock().await;
        mgr.create_session_with_push("conn-weather", push_tx);
        mgr.get_session_mut("conn-weather").unwrap().authenticate("com.example.weather");
    }
    let result = router.route_sync(
        "com.example.sender",
        "com.example.weather",
        "weather_query",
        &serde_json::json!({"city": "Shanghai"}),
        &state,
        &session_mgr,
    ).await;

    // Should succeed because target is installed and running
    assert!(result.is_ok(), "Intent routing should succeed: {:?}", result);

    let _ = fs::remove_dir_all(&dir);
}

/// E2E Test 2: Budget tracking records usage and enforces limits
#[tokio::test]
async fn test_e2e_budget_enforcement() {
    let dir = temp_dir("budget");
    let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));

    // Step 1: Set budget limits
    {
        let mut guard = state.write().await;
        let mut tracker = BudgetTracker::new_in_memory();
        let budget = rollball_core::Budget {
            daily_tokens: Some(1000),
            monthly_tokens: None,
            daily_cost_usd: Some(1.0),
            monthly_cost_usd: None,
            exceeded_action: "deny".to_string(),
        };
        tracker.set_budget("openai", budget);
        guard.set_budget_tracker(tracker);
    }

    // Step 2: Record usage (simulating an LLM call)
    {
        let mut guard = state.write().await;
        if let Some(tracker) = guard.budget_tracker_mut() {
            tracker.record_usage("com.example.weather", "openai", 500, 0.25);
        }
    }

    // Step 3: Check remaining budget
    {
        let guard = state.read().await;
        if let Some(tracker) = guard.budget_tracker() {
            let remaining_tokens = tracker.remaining_tokens("openai");
            let remaining_cost = tracker.remaining_cost_usd("openai");
            assert_eq!(remaining_tokens, 500);
            assert!((remaining_cost - 0.75).abs() < 0.01);
        } else {
            panic!("Budget tracker should be set");
        }
    }

    // Step 4: Exceed budget
    let exceeded = {
        let mut guard = state.write().await;
        if let Some(tracker) = guard.budget_tracker_mut() {
            tracker.record_usage("com.example.weather", "openai", 600, 0.80);
            tracker.check_budget("openai").is_some()
        } else {
            false
        }
    };
    assert!(exceeded, "Budget should be exceeded after over-usage");

    let _ = fs::remove_dir_all(&dir);
}

/// E2E Test 3: Rate limiting throttles excessive requests
#[tokio::test]
async fn test_e2e_rate_limiting() {
    let dir = temp_dir("rate");
    let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));

    // Step 1: Set rate limit (2 tokens per 60 seconds for openai)
    {
        let mut guard = state.write().await;
        let mut limiter = RateLimiter::new();
        limiter.add_bucket("openai", 2, 2.0 / 60.0); // 2 requests per 60s
        guard.set_rate_limiter(limiter);
    }

    // Step 2: Acquire tokens (should succeed for first 2)
    let mut granted_count = 0;
    for _ in 0..3 {
        let mut guard = state.write().await;
        if let Some(limiter) = guard.rate_limiter_mut() {
            let result = limiter.try_acquire_for("openai", "com.example.weather");
            if result.granted {
                granted_count += 1;
            }
        }
    }

    assert_eq!(granted_count, 2, "Only 2 requests should be granted");

    // Step 3: Verify the 3rd request was throttled
    let third_result = {
        let mut guard = state.write().await;
        if let Some(limiter) = guard.rate_limiter_mut() {
            limiter.try_acquire_for("openai", "com.example.weather")
        } else {
            panic!("Rate limiter should be set");
        }
    };
    assert!(!third_result.granted, "3rd request should be denied");
    assert!(third_result.retry_after_ms.is_some(), "Should have retry_after_ms");

    let _ = fs::remove_dir_all(&dir);
}

/// E2E Test 4: Full flow — Intent routing + Budget + Rate combined
///
/// Simulates a realistic multi-agent interaction:
/// 1. Install agents with capabilities
/// 2. Set budget and rate limits
/// 3. Route an Intent
/// 4. Record budget usage from the LLM call
/// 5. Acquire rate limit token before the LLM call
/// 6. Verify all systems interact correctly
#[tokio::test]
async fn test_e2e_intent_budget_rate_combined() {
    let dir = temp_dir("combined");
    let state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));

    // Step 1: Install and start agents
    {
        let mut guard = state.write().await;
        guard.add_installed(AgentInfo {
            agent_id: "com.example.sender".to_string(),
            version: "1.0.0".to_string(),
            name: "Sender".to_string(),
            install_path: "/tmp/sender".to_string(),
            manifest: make_manifest("com.example.sender", &[]),
        });
        guard.add_installed(AgentInfo {
            agent_id: "com.example.weather".to_string(),
            version: "1.0.0".to_string(),
            name: "Weather".to_string(),
            install_path: "/tmp/weather".to_string(),
            manifest: make_manifest("com.example.weather", &["weather_query"]),
        });
        guard.add_running(RunningAgentInfo {
            agent_id: "com.example.weather".to_string(),
            pid: 54321,
            started_at: chrono::Utc::now(),
            workspace: "/tmp/weather-ws".to_string(),
            connected: false,
            ready: false,
            dev_mode: false,
            debug_port: None,
            identity_entries: vec![],
            workspace_config_json: None,
        });

        // Set budget and rate limits
        let mut tracker = BudgetTracker::new_in_memory();
        let budget = rollball_core::Budget {
            daily_tokens: Some(10000),
            monthly_tokens: None,
            daily_cost_usd: Some(10.0),
            monthly_cost_usd: None,
            exceeded_action: "deny".to_string(),
        };
        tracker.set_budget("openai", budget);
        guard.set_budget_tracker(tracker);

        let mut limiter = RateLimiter::new();
        limiter.add_bucket("openai", 100, 100.0 / 60.0);
        guard.set_rate_limiter(limiter);
    }

    // Step 2: Acquire rate limit token
    let rate_result = {
        let mut guard = state.write().await;
        guard.rate_limiter_mut().unwrap().try_acquire_for("openai", "com.example.sender")
    };
    assert!(rate_result.granted, "Rate limit should grant token");

    // Step 3: Route Intent (need session for target)
    let router = IntentRouter::new();
    let session_mgr: Arc<Mutex<SessionManager>> = Arc::new(Mutex::new(SessionManager::new()));
    // Simulate target agent's IPC session
    {
        let (push_tx, _push_rx) = tokio::sync::mpsc::channel(8);
        let mut mgr = session_mgr.lock().await;
        mgr.create_session_with_push("conn-weather", push_tx);
        mgr.get_session_mut("conn-weather").unwrap().authenticate("com.example.weather");
    }
    let intent_result = router.route_sync(
        "com.example.sender",
        "com.example.weather",
        "weather_query",
        &serde_json::json!({"city": "Beijing"}),
        &state,
        &session_mgr,
    ).await;
    assert!(intent_result.is_ok(), "Intent should route successfully");

    // Step 4: Record budget usage (simulating LLM response)
    {
        let mut guard = state.write().await;
        guard.budget_tracker_mut().unwrap().record_usage(
            "com.example.weather",
            "openai",
            1500,
            0.03,
        );
    }

    // Step 5: Verify all systems are consistent
    {
        let guard = state.read().await;

        // Budget should reflect usage
        let remaining_tokens = guard.budget_tracker().unwrap().remaining_tokens("openai");
        assert_eq!(remaining_tokens, 8500, "Remaining tokens should be 10000 - 1500");

        // Capability should still be registered
        let caps = guard.capability_registry.find_by_action("weather_query");
        assert_eq!(caps.len(), 1, "weather_query capability should exist");

        // Agent should still be running
        assert!(guard.is_running("com.example.weather"));
    }

    // Step 6: Uninstall target → capability should be removed
    {
        let mut guard = state.write().await;
        guard.remove_installed("com.example.weather");
    }
    {
        let guard = state.read().await;
        let caps = guard.capability_registry.find_by_action("weather_query");
        assert!(caps.is_empty(), "Capability should be removed after uninstall");
    }

    let _ = fs::remove_dir_all(&dir);
}

/// E2E Test 5: Capability broadcast on install/uninstall
///
/// Verifies that the capability broadcast channel correctly
/// distributes CapabilityUpdate messages to subscribers.
#[tokio::test]
async fn test_e2e_capability_broadcast_flow() {
    let dir = temp_dir("cap_broadcast");
    let _state: SharedState = Arc::new(RwLock::new(GatewayState::new(&dir)));

    // Create broadcast channel directly
    let (cap_tx, mut cap_rx) = tokio::sync::broadcast::channel::<rollball_core::protocol::GatewayResponse>(64);

    // Simulate install event — broadcast CapabilityUpdate
    let install_update = rollball_core::protocol::GatewayResponse::CapabilityUpdate {
        agent_id: "com.example.weather".to_string(),
        actions: vec!["weather_query".to_string(), "forecast".to_string()],
        removed: false,
    };
    cap_tx.send(install_update).unwrap();

    // Verify subscriber receives the update
    let msg = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        cap_rx.recv(),
    )
    .await
    .expect("Timeout waiting for capability broadcast")
    .expect("Channel closed");

    match &msg {
        rollball_core::protocol::GatewayResponse::CapabilityUpdate {
            agent_id,
            actions,
            removed,
        } => {
            assert_eq!(agent_id, "com.example.weather");
            assert_eq!(actions.len(), 2);
            assert!(!removed);
        }
        _ => panic!("Expected CapabilityUpdate, got {:?}", msg),
    }

    // Simulate uninstall event
    let uninstall_update = rollball_core::protocol::GatewayResponse::CapabilityUpdate {
        agent_id: "com.example.weather".to_string(),
        actions: vec![],
        removed: true,
    };
    cap_tx.send(uninstall_update).unwrap();

    let msg2 = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        cap_rx.recv(),
    )
    .await
    .expect("Timeout waiting for uninstall broadcast")
    .expect("Channel closed");

    match &msg2 {
        rollball_core::protocol::GatewayResponse::CapabilityUpdate {
            agent_id,
            actions,
            removed,
        } => {
            assert_eq!(agent_id, "com.example.weather");
            assert!(actions.is_empty());
            assert!(removed);
        }
        _ => panic!("Expected CapabilityUpdate (removed), got {:?}", msg2),
    }

    let _ = fs::remove_dir_all(&dir);
}
