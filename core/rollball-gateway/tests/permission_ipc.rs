//! S2.6: Permission IPC integration tests
//!
//! End-to-end tests for the Permission IPC flow:
//! 1. Permission request/response protocol round-trip
//! 2. Permission HTTP API integration
//! 3. Intent permission validation via PermissionStore

use rollball_core::permission::{Permission, PermissionGrant};

// ── S2.6: Permission protocol round-trip tests ───────────────────────

#[test]
fn test_permission_request_response_roundtrip() {
    use rollball_core::protocol::{GatewayRequest, GatewayResponse};

    // Test request roundtrip via JSON serialization
    let request = GatewayRequest::PermissionRequest {
        request_id: "perm-rt-001".to_string(),
        permission: "shell".to_string(),
        reason: "Execute build script".to_string(),
        timeout_ms: rollball_core::protocol::PERMISSION_REQUEST_TIMEOUT_MS,
    };

    let json = serde_json::to_string(&request).unwrap();
    let parsed: GatewayRequest = serde_json::from_str(&json).unwrap();

    if let GatewayRequest::PermissionRequest {
        request_id,
        permission,
        reason,
        timeout_ms,
    } = parsed
    {
        assert_eq!(request_id, "perm-rt-001");
        assert_eq!(permission, "shell");
        assert_eq!(reason, "Execute build script");
        assert_eq!(timeout_ms, 60_000);
    } else {
        panic!("Expected PermissionRequest");
    }

    // Test response roundtrip
    let response = GatewayResponse::PermissionResult {
        request_id: "perm-rt-001".to_string(),
        granted: true,
        reason: None,
    };

    let json = serde_json::to_string(&response).unwrap();
    let parsed: GatewayResponse = serde_json::from_str(&json).unwrap();

    if let GatewayResponse::PermissionResult {
        request_id,
        granted,
        reason,
    } = parsed
    {
        assert_eq!(request_id, "perm-rt-001");
        assert!(granted);
        assert!(reason.is_none());
    } else {
        panic!("Expected PermissionResult");
    }
}

// ── S2.6: Intent permission validation integration ────────────────────

#[test]
fn test_intent_permission_validation() {
    use rollball_gateway::permission_store::PermissionStore;

    let perm_store = PermissionStore::open_in_memory().unwrap();

    // Agent without intent:send permission
    let has_perm = perm_store.has_permission(
        "com.example.sender",
        &Permission::IntentSend(Some("com.example.target".to_string())),
    ).unwrap();
    assert!(!has_perm, "Should not have intent:send permission");

    // Grant broad intent:send permission
    let grant = PermissionGrant::new(
        "com.example.sender",
        Permission::IntentSend(None),
        "user",
    );
    perm_store.grant(&grant).unwrap();

    // Now should have intent:send for any target
    let has_perm = perm_store.has_permission(
        "com.example.sender",
        &Permission::IntentSend(Some("com.example.target".to_string())),
    ).unwrap();
    assert!(has_perm, "Should have broad intent:send permission");

    // Also check narrow grant
    let narrow_grant = PermissionGrant::new(
        "com.example.sender2",
        Permission::IntentSend(Some("com.example.calendar".to_string())),
        "user",
    );
    perm_store.grant(&narrow_grant).unwrap();

    let has_narrow = perm_store.has_permission(
        "com.example.sender2",
        &Permission::IntentSend(Some("com.example.calendar".to_string())),
    ).unwrap();
    assert!(has_narrow, "Should have narrow intent:send for calendar");

    let has_other = perm_store.has_permission(
        "com.example.sender2",
        &Permission::IntentSend(Some("com.example.weather".to_string())),
    ).unwrap();
    assert!(!has_other, "Should not have intent:send for weather");
}
