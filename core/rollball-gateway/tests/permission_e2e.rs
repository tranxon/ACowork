//! End-to-end integration test: Permission framework (Phase 3 S1.8)
//!
//! Validates the complete permission lifecycle on the Gateway side:
//! 1. Install agent with declared permissions → review → user approves
//! 2. Runtime permission request → policy check → grant/deny
//! 3. Permission revocation via store
//! 4. Permission reset (clear all)
//! 5. Permission upgrade diff on agent upgrade
//! 6. Full lifecycle: install → approve → revoke → re-approve

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use rollball_core::permission::{Permission, PermissionGrant, PermissionPolicy};
use rollball_gateway::gateway::state::{AgentInfo, GatewayState};
use rollball_gateway::ipc::server::SharedState;
use rollball_gateway::package_manager::permission_diff::PermissionDiff;
use rollball_gateway::package_manager::permission_review::PermissionReview;
use rollball_gateway::permission_store::PermissionStore;

fn temp_dir(name: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rollball-perm-e2e-{}-{}", name, id));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().to_string()
}

/// Serialize a Permission to TOML inline table format
fn perm_to_toml(p: &Permission) -> String {
    match p.type_value() {
        Some(v) => format!("{{type = \"{}\", value = \"{}\"}}", p.type_name(), v),
        None => format!("{{type = \"{}\"}}", p.type_name()),
    }
}

fn make_manifest(agent_id: &str, permissions: &[Permission]) -> rollball_core::AgentManifest {
    let perms_toml = permissions
        .iter()
        .map(|p| perm_to_toml(p))
        .collect::<Vec<_>>()
        .join(", ");
    let toml_str = format!(
        r#"agent_id = "{}"
version = "1.0.0"
name = "Test"
description = "test"
author = "test"
runtime_version = "0.1.0"
permissions = [{}]
[llm]
provider = "openai"
model = "gpt-4"
"#,
        agent_id, perms_toml
    );
    rollball_core::AgentManifest::from_toml(&toml_str).unwrap()
}

/// E2E Test 1: Install-time permission review → user approval → grants persisted
#[tokio::test]
async fn test_e2e_permission_install_review() {
    let dir = temp_dir("install_review");
    let state: SharedState = std::sync::Arc::new(tokio::sync::RwLock::new(GatewayState::new(&dir)));
    let perm_store = PermissionStore::open_in_memory().unwrap();

    let agent_id = "com.example.weather";
    let declared = vec![
        Permission::Network(Some("https://api.weather.com".to_string())),
        Permission::MemoryRead,
        Permission::Shell,
    ];

    // Step 1: Permission review — categorize declared permissions
    let review = PermissionReview::review(&declared, &perm_store, agent_id).unwrap();

    // MemoryRead should be auto-approved by policy
    assert!(
        review.auto_approved.contains(&Permission::MemoryRead),
        "MemoryRead should be auto-approved"
    );

    // Network and Shell should require user approval
    assert_eq!(review.new_permissions.len(), 2, "Network + Shell need approval");

    // Step 2: User approves all new permissions
    for perm in &review.new_permissions {
        let grant = PermissionGrant::new(agent_id, perm.clone(), "user");
        perm_store.grant(&grant).unwrap();
    }

    // Auto-approved permissions should also be persisted
    for perm in &review.auto_approved {
        let grant = PermissionGrant::new(agent_id, perm.clone(), "auto");
        perm_store.grant(&grant).unwrap();
    }

    // Step 3: Verify all grants are persisted and queryable
    let grants = perm_store.query_grants(agent_id).unwrap();
    assert_eq!(grants.len(), 3, "Should have 3 grants (network + memory + shell)");

    // Step 4: Install the agent
    {
        let mut guard = state.write().await;
        guard.add_installed(AgentInfo {
            agent_id: agent_id.to_string(),
            version: "1.0.0".to_string(),
            name: "Weather".to_string(),
            install_path: "/tmp/weather".to_string(),
            manifest: make_manifest(agent_id, &[
                Permission::Network(Some("https://api.weather.com".to_string())),
                Permission::MemoryRead,
                Permission::Shell,
            ]),
        });
    }

    let guard = state.read().await;
    assert!(guard.is_installed(agent_id));
}

/// E2E Test 2: Runtime permission request → auto-approve by policy
#[test]
fn test_e2e_runtime_permission_auto_approve() {
    let perm_store = PermissionStore::open_in_memory().unwrap();
    let agent_id = "com.example.test";

    // MemoryRead should be auto-approved by policy
    let policy = PermissionPolicy::for_permission(&Permission::MemoryRead);
    assert_eq!(policy, PermissionPolicy::Allow);

    // Simulate Gateway-side processing of a runtime permission request
    let perm = Permission::MemoryRead;
    match perm_store.has_permission(agent_id, &perm) {
        Ok(false) => {
            let policy = PermissionPolicy::for_permission(&perm);
            if policy == PermissionPolicy::Allow {
                let grant = PermissionGrant::new(agent_id, perm.clone(), "auto");
                perm_store.grant(&grant).unwrap();
            }
        }
        Ok(true) => { /* already granted */ }
        Err(e) => panic!("Store error: {}", e),
    }

    // Verify the grant was persisted
    assert!(perm_store.has_permission(agent_id, &Permission::MemoryRead).unwrap());
}

/// E2E Test 3: Runtime permission request → user approval flow
#[test]
fn test_e2e_runtime_permission_user_approval() {
    let perm_store = PermissionStore::open_in_memory().unwrap();
    let agent_id = "com.example.agent";

    // Shell permission requires user approval
    let policy = PermissionPolicy::for_permission(&Permission::Shell);
    assert_ne!(policy, PermissionPolicy::Allow, "Shell should not be auto-approved");

    // Not yet granted
    assert!(!perm_store.has_permission(agent_id, &Permission::Shell).unwrap());

    // Simulate user approval → persist grant
    let grant = PermissionGrant::new(agent_id, Permission::Shell, "user");
    perm_store.grant(&grant).unwrap();

    // Now granted
    assert!(perm_store.has_permission(agent_id, &Permission::Shell).unwrap());

    // Grant metadata is correct
    let grants = perm_store.query_grants(agent_id).unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].authorized_by, "user");
}

/// E2E Test 4: Permission revocation
#[test]
fn test_e2e_permission_revocation() {
    let perm_store = PermissionStore::open_in_memory().unwrap();
    let agent_id = "com.example.agent";

    // Grant shell permission
    let grant = PermissionGrant::new(agent_id, Permission::Shell, "user");
    perm_store.grant(&grant).unwrap();
    assert!(perm_store.has_permission(agent_id, &Permission::Shell).unwrap());

    // Revoke the permission
    let revoked = perm_store.revoke(agent_id, Some(&Permission::Shell)).unwrap();
    assert_eq!(revoked, 1, "Should revoke 1 grant");

    // No longer granted
    assert!(!perm_store.has_permission(agent_id, &Permission::Shell).unwrap());
}

/// E2E Test 5: Permission reset clears all grants
#[test]
fn test_e2e_permission_reset() {
    let perm_store = PermissionStore::open_in_memory().unwrap();
    let agent_id = "com.example.agent";

    // Grant multiple permissions
    for perm in [
        Permission::Shell,
        Permission::Network(None),
        Permission::MemoryWrite,
    ] {
        let grant = PermissionGrant::new(agent_id, perm, "user");
        perm_store.grant(&grant).unwrap();
    }

    // Verify all 3 grants exist
    let grants = perm_store.query_grants(agent_id).unwrap();
    assert_eq!(grants.len(), 3);

    // Reset all permissions
    let count = perm_store.reset(agent_id).unwrap();
    assert_eq!(count, 3);

    // Verify no grants remain
    let grants = perm_store.query_grants(agent_id).unwrap();
    assert!(grants.is_empty());
}

/// E2E Test 6: Permission upgrade on agent upgrade — diff detection
#[test]
fn test_e2e_permission_upgrade_diff() {
    let old_manifest = make_manifest("com.example.agent", &[Permission::Network(Some("https://api.example.com".to_string()))]);
    let new_manifest = make_manifest(
        "com.example.agent",
        &[Permission::Network(Some("https://api.example.com".to_string())), Permission::Shell],
    );

    let diff = PermissionDiff::from_manifests(&old_manifest, &new_manifest);

    // Network permission should be unchanged
    assert!(
        diff.unchanged.iter().any(|p| matches!(p, Permission::Network(Some(_)))),
        "Network permission should be unchanged"
    );

    // Shell should be added
    assert!(
        diff.added.contains(&Permission::Shell),
        "Shell should be detected as added"
    );

    // Nothing should be removed
    assert!(diff.removed.is_empty(), "No permissions should be removed");
}

/// E2E Test 7: Full lifecycle — install → approve → revoke → re-approve → verify
#[test]
fn test_e2e_permission_full_lifecycle() {
    let perm_store = PermissionStore::open_in_memory().unwrap();
    let agent_id = "com.example.lifecycle";

    // Phase 1: Install with declared permissions → review
    let declared = vec![Permission::Network(Some("https://api.example.com".to_string()))];
    let review = PermissionReview::review(&declared, &perm_store, agent_id).unwrap();

    // Approve the network permission
    for perm in &review.new_permissions {
        let grant = PermissionGrant::new(agent_id, perm.clone(), "user");
        perm_store.grant(&grant).unwrap();
    }
    assert!(perm_store.has_permission(agent_id, &Permission::Network(Some("https://api.example.com".to_string()))).unwrap());

    // Phase 2: Runtime requests additional permission (filesystem:read)
    let fs_perm = Permission::FilesystemRead(Some("/tmp/data".to_string()));
    assert!(!perm_store.has_permission(agent_id, &fs_perm).unwrap());

    // User approves the request
    let grant = PermissionGrant::new(agent_id, fs_perm.clone(), "user");
    perm_store.grant(&grant).unwrap();
    assert!(perm_store.has_permission(agent_id, &fs_perm).unwrap());

    // Phase 3: Revoke filesystem permission
    perm_store.revoke(agent_id, Some(&fs_perm)).unwrap();
    assert!(!perm_store.has_permission(agent_id, &fs_perm).unwrap());

    // Network still works
    assert!(perm_store.has_permission(agent_id, &Permission::Network(Some("https://api.example.com".to_string()))).unwrap());

    // Phase 4: Upgrade adds shell permission
    let old_manifest = make_manifest(agent_id, &[Permission::Network(Some("https://api.example.com".to_string()))]);
    let new_manifest = make_manifest(agent_id, &[Permission::Network(Some("https://api.example.com".to_string())), Permission::Shell]);
    let diff = PermissionDiff::from_manifests(&old_manifest, &new_manifest);
    assert!(diff.added.contains(&Permission::Shell));

    // Review the new permission
    let review = PermissionReview::review(&diff.added, &perm_store, agent_id).unwrap();
    for perm in &review.new_permissions {
        let grant = PermissionGrant::new(agent_id, perm.clone(), "user");
        perm_store.grant(&grant).unwrap();
    }

    // All 3 permissions now granted
    let grants = perm_store.query_grants(agent_id).unwrap();
    assert_eq!(grants.len(), 2, "network + shell (filesystem was revoked)");

    // Phase 5: Reset everything
    perm_store.reset(agent_id).unwrap();
    let grants = perm_store.query_grants(agent_id).unwrap();
    assert!(grants.is_empty(), "All permissions should be cleared after reset");
}
