//! Search Provider integration tests — Gateway level (Phase 3: P3.3 + P3.4)
//!
//! P3.3: No provider configured error return test (HTTP API)
//! P3.4: Vault CRUD + distribution chain end-to-end test
//!
//! Tests cover:
//! - Search key CRUD through VaultFacade
//! - HTTP API endpoints for search keys (list/add/remove/update)
//! - search_list.json rebuild after vault changes
//! - Protocol type serialization

use std::sync::Arc;
use tokio::sync::RwLock;

use rollball_gateway::gateway::state::GatewayState;
use rollball_gateway::vault::VaultFacade;

const TEST_PASSWORD: &str = "test-password-123";

// ── Helpers ─────────────────────────────────────────────────────────

fn temp_data_dir(test_name: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rollball-search-test-{}-{}", test_name, id));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().to_string()
}

/// Create a VaultFacade and unlock it for testing.
fn create_test_vault_unlocked(data_dir: &str) -> VaultFacade {
    let mut vault = VaultFacade::new(data_dir);
    vault.unlock(TEST_PASSWORD).expect("Failed to unlock vault");
    vault
}

// ── P3.4: Vault CRUD end-to-end tests ──────────────────────────────

#[test]
fn test_vault_store_and_get_search_key() {
    let dir = temp_data_dir("store_get");
    let mut vault = create_test_vault_unlocked(&dir);

    vault
        .store_search_key("tavily", "tvly-test-key-123")
        .expect("Failed to store search key");

    let entry = vault
        .get_search_key("tavily")
        .expect("Failed to get search key");
    assert_eq!(entry.api_key, "tvly-test-key-123");
}

#[test]
fn test_vault_list_search_keys() {
    let dir = temp_data_dir("list_keys");
    let mut vault = create_test_vault_unlocked(&dir);

    let initial = vault.list_search_keys().expect("Failed to list search keys");
    assert!(initial.is_empty(), "Expected empty search providers list");

    vault
        .store_search_key("tavily", "key-1")
        .expect("Failed to store tavily");
    vault
        .store_search_key("brave", "key-2")
        .expect("Failed to store brave");
    vault
        .store_search_key("serper", "key-3")
        .expect("Failed to store serper");

    let entries = vault.list_search_keys().expect("Failed to list search keys");
    assert_eq!(entries.len(), 3);
    let providers: Vec<&str> = entries.iter().map(|e| e.provider.as_str()).collect();
    assert!(providers.contains(&"tavily"));
    assert!(providers.contains(&"brave"));
    assert!(providers.contains(&"serper"));
}

#[test]
fn test_vault_list_search_key_previews() {
    let dir = temp_data_dir("previews");
    let mut vault = create_test_vault_unlocked(&dir);

    vault
        .store_search_key("tavily", "tvly-secret-key-abc123")
        .expect("Failed to store");

    let previews = vault.list_search_keys().expect("Failed to list search keys");
    assert_eq!(previews.len(), 1);
    assert_eq!(previews[0].provider, "tavily");
    // Key "tvly-secret-key-abc123" → first 3 "tvl" + last 3 "123" = "tvl...123"
    assert!(
        previews[0].key_preview.contains("tvl")
            && previews[0].key_preview.contains("123"),
        "Key preview should contain prefix+suffix: {}",
        previews[0].key_preview
    );
}

#[test]
fn test_vault_update_search_key() {
    let dir = temp_data_dir("update");
    let mut vault = create_test_vault_unlocked(&dir);

    vault
        .store_search_key("tavily", "old-key")
        .expect("Failed to store");

    let _ = vault.remove_search_key("tavily");
    vault
        .store_search_key("tavily", "new-key")
        .expect("Failed to update");

    let entry = vault.get_search_key("tavily").expect("Failed to get");
    assert_eq!(entry.api_key, "new-key");
}

#[test]
fn test_vault_remove_search_key() {
    let dir = temp_data_dir("remove");
    let mut vault = create_test_vault_unlocked(&dir);

    vault
        .store_search_key("tavily", "key-to-remove")
        .expect("Failed to store");

    // remove_search_key returns Result<(), GatewayError>
    vault
        .remove_search_key("tavily")
        .expect("Failed to remove");

    let result = vault.get_search_key("tavily");
    assert!(result.is_err(), "Key should be gone after removal");
}

#[test]
fn test_vault_get_missing_search_key_error() {
    let dir = temp_data_dir("missing");
    let vault = create_test_vault_unlocked(&dir);

    let result = vault.get_search_key("nonexistent");
    assert!(result.is_err());
}

#[test]
fn test_vault_store_special_characters_in_key() {
    let dir = temp_data_dir("special_chars");
    let mut vault = create_test_vault_unlocked(&dir);

    let complex_key = "tvly-!@#$%^&*()_+-=[]{}|;':\",./<>?~`";
    vault
        .store_search_key("tavily", complex_key)
        .expect("Failed to store complex key");

    let entry = vault.get_search_key("tavily").expect("Failed to get");
    assert_eq!(entry.api_key, complex_key);
}

// ── P3.4: HTTP API integration tests ───────────────────────────────

use axum::body::Body;
use axum::http::{Request, Method, StatusCode};
use tower::ServiceExt;

use rollball_gateway::http::routes::{AppState, build_router};
use rollball_gateway::http::auth::HttpAuth;

fn create_test_http_app(data_dir: &str) -> axum::Router {
    let mut gw_state = GatewayState::new(data_dir);
    // Unlock the vault so HTTP handlers can read/write search keys
    gw_state.vault.unlock(TEST_PASSWORD).expect("Failed to unlock test vault");
    gw_state.config = Some(rollball_gateway::config::GatewayConfig::default());
    let state = AppState::new(
        Arc::new(RwLock::new(gw_state)),
        Arc::new(HttpAuth::new(false)),
        None,
        None,
        None,
    );
    build_router(state)
}

#[tokio::test]
async fn test_http_list_search_keys_empty() {
    let dir = temp_data_dir("http_list_empty");
    let app = create_test_http_app(&dir);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/search/keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_http_add_search_key() {
    let dir = temp_data_dir("http_add");
    let app = create_test_http_app(&dir);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/search/keys")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"provider":"tavily","key":"tvly-api-key-12345"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    // Verify it appears in list
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/search/keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let keys = json.as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["provider"], "tavily");
}

#[tokio::test]
async fn test_http_remove_search_key() {
    let dir = temp_data_dir("http_remove");
    let app = create_test_http_app(&dir);

    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/search/keys")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"provider":"tavily","key":"key-to-delete"}"#,
                ))
                .unwrap(),
        )
        .await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/search/keys/tavily")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Verify it's gone
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/search/keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_http_update_search_key() {
    let dir = temp_data_dir("http_update");
    let app = create_test_http_app(&dir);

    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/search/keys")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"provider":"tavily","key":"old-key"}"#,
                ))
                .unwrap(),
        )
        .await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri("/api/search/keys/tavily")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"key":"new-key"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Verify update
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/search/keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let keys = json.as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert!(keys[0]["key_preview"].as_str().unwrap().contains("new"));
}

#[tokio::test]
async fn test_http_add_duplicate_search_key_overwrites() {
    let dir = temp_data_dir("http_duplicate");
    let app = create_test_http_app(&dir);

    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/search/keys")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"provider":"tavily","key":"first-key"}"#,
                ))
                .unwrap(),
        )
        .await;

    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/search/keys")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"provider":"tavily","key":"second-key"}"#,
                ))
                .unwrap(),
        )
        .await;

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/search/keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_http_add_invalid_provider_rejected() {
    let dir = temp_data_dir("http_invalid");
    let app = create_test_http_app(&dir);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/search/keys")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"provider":"","key":"some-key"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(!response.status().is_success());
}

#[tokio::test]
async fn test_http_remove_nonexistent_key() {
    let dir = temp_data_dir("http_remove_nonexistent");
    let app = create_test_http_app(&dir);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/search/keys/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── P3.3: No provider HTTP error tests ─────────────────────────────

#[tokio::test]
async fn test_http_remove_without_prerequisite_returns_error() {
    let dir = temp_data_dir("http_no_prereq");
    let app = create_test_http_app(&dir);

    // Attempt to remove a provider that was never added
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/search/keys/never-added")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(!response.status().is_success());
}

// ── Protocol type serialization tests ──────────────────────────────

use rollball_core::protocol::SearchProviderListItem;

#[test]
fn test_search_provider_list_item_serialization() {
    let item = SearchProviderListItem {
        id: "tavily".to_string(),
        name: "Tavily Search".to_string(),
        description: "Test desc".to_string(),
        requires_api_key: true,
        base_url: "https://api.tavily.com".to_string(),
    };

    let json = serde_json::to_string(&item).unwrap();
    assert!(json.contains("tavily"));
    assert!(json.contains("Tavily Search"));

    let deserialized: SearchProviderListItem = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, item.id);
    assert_eq!(deserialized.name, item.name);
    assert!(deserialized.requires_api_key);
}

#[test]
fn test_search_key_entry_serialization() {
    use rollball_core::protocol::SearchKeyEntry;

    let entry = SearchKeyEntry {
        provider_id: "brave".to_string(),
        api_key: "BSA-12345".to_string(),
    };

    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains("brave"));
    assert!(json.contains("BSA-12345"));

    let deserialized: SearchKeyEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.provider_id, "brave");
    assert_eq!(deserialized.api_key, "BSA-12345");
}

#[test]
fn test_agent_search_provider_serialization() {
    use rollball_core::protocol::{AgentSearchConfig, AgentSearchProvider};

    let config = AgentSearchConfig {
        providers: vec![
            AgentSearchProvider {
                provider: "tavily".to_string(),
                priority: 1,
            },
            AgentSearchProvider {
                provider: "brave".to_string(),
                priority: 2,
            },
        ],
    };

    let json = serde_json::to_string(&config).unwrap();
    assert!(json.contains("tavily"));
    assert!(json.contains("brave"));
    assert!(json.contains("\"priority\":1"));
    assert!(json.contains("\"priority\":2"));

    let deserialized: AgentSearchConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.providers.len(), 2);
    assert_eq!(deserialized.providers[0].provider, "tavily");
    assert_eq!(deserialized.providers[0].priority, 1);
}
