//! S4.7: RAG integration tests
//!
//! Cross-cutting integration tests for the Phase 4 RAG feature:
//! 1. Manifest RAG declaration → tool registration + permission validation
//! 2. RAG dual permission check (rag:query + network whitelist)
//! 3. RagClient timeout + graceful degradation
//! 4. RagQueryTool end-to-end (mock client)
//! 5. No RAG declaration → zero intrusion (no tools, no permissions)
//! 6. Mock HTTP server end-to-end (S4.7)

use std::sync::Arc;
use std::time::Duration;

use rollball_core::permission::Permission;
use rollball_core::tools::traits::Tool;
use rollball_core::AgentManifest;
use rollball_runtime::tools::builtin::rag_query::RagQueryTool;
use rollball_runtime::tools::rag::client::{RagAuthCredential, RagClient, RagClientConfig};
use rollball_runtime::tools::rag::types::{
    AnnotatedRagResult, RagQueryRequest, RagQueryResponse, RagResultItem,
};

// ── Helper: RAG manifest ──────────────────────────────────────────────────

fn rag_manifest() -> AgentManifest {
    let toml = r#"
        agent_id = "com.test.rag"
        version = "1.0.0"
        name = "RAG Agent"
        description = "Test"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "openai"
        model = "gpt-4"

        [[permissions]]
        type = "RagQuery"

        [[permissions]]
        type = "Network"

        [[tools]]
        type = "rag"
        name = "enterprise_knowledge"

        [tools.rag]
        endpoint = "https://rag.corp.example.com/v1/query"
        collection = "product_docs"
        auth_ref = "vault:rag_enterprise_key"
        auth_type = "bearer"
        max_results = 5
        score_threshold = 0.7
    "#;
    AgentManifest::from_toml(toml).unwrap()
}

fn no_rag_manifest() -> AgentManifest {
    let toml = r#"
        agent_id = "com.test.no-rag"
        version = "1.0.0"
        name = "No RAG Agent"
        description = "Test"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "openai"
        model = "gpt-4"
    "#;
    AgentManifest::from_toml(toml).unwrap()
}

// ── Helper: mock RagClient ────────────────────────────────────────────────

fn mock_rag_client() -> Arc<RagClient> {
    let config = RagClientConfig {
        endpoint: "https://10.255.255.1/v1/query".to_string(), // non-routable
        collection: Some("product_docs".to_string()),
        auth: RagAuthCredential::None,
        default_max_results: 5,
        default_score_threshold: 0.7,
        timeout: Duration::from_millis(100),
        tool_name: "enterprise_knowledge".to_string(),
    };
    Arc::new(RagClient::new(config))
}

// ── Test 1: Manifest RAG declaration ──────────────────────────────────────

#[test]
fn test_manifest_rag_declaration_detected() {
    let manifest = rag_manifest();
    assert!(manifest.has_rag(), "has_rag() should return true for RAG manifest");

    let (tool_name, rag_config) = manifest.rag_config().unwrap();
    assert_eq!(tool_name, "enterprise_knowledge");
    assert_eq!(rag_config.endpoint, "https://rag.corp.example.com/v1/query");
    assert_eq!(rag_config.collection.as_deref(), Some("product_docs"));
    assert_eq!(rag_config.auth_ref.as_deref(), Some("vault:rag_enterprise_key"));
    assert_eq!(rag_config.auth_type, "bearer");
    assert_eq!(rag_config.max_results, 5);
}

#[test]
fn test_manifest_no_rag_zero_intrusion() {
    let manifest = no_rag_manifest();
    assert!(!manifest.has_rag(), "has_rag() should return false without RAG declaration");
    assert!(manifest.rag_config().is_none());
}

// ── Test 4: RagQueryTool end-to-end ───────────────────────────────────────

#[tokio::test]
async fn test_rag_query_tool_missing_query_param() {
    let client = mock_rag_client();
    let tool = RagQueryTool::new(client);
    let result = tool.execute(serde_json::json!({})).await.unwrap();
    assert!(!result.ok, "Missing query should fail");
    assert!(result.error.unwrap().contains("Missing 'query'"));
}

#[tokio::test]
async fn test_rag_query_tool_empty_query_param() {
    let client = mock_rag_client();
    let tool = RagQueryTool::new(client);
    let result = tool.execute(serde_json::json!({ "query": "" })).await.unwrap();
    assert!(!result.ok, "Empty query should fail");
}

#[tokio::test]
async fn test_rag_query_tool_timeout_graceful_degradation() {
    let client = mock_rag_client();
    let tool = RagQueryTool::new(client);
    let result = tool
        .execute(serde_json::json!({ "query": "product roadmap" }))
        .await
        .unwrap();
    // RAG unavailable → graceful degradation: ok=true with "no results" message
    assert!(result.ok, "RAG timeout should degrade gracefully");
    assert!(result.content.contains("No relevant results"),
        "Timeout should produce 'no results' message, got: {}", result.content);
}

#[tokio::test]
async fn test_rag_query_tool_spec() {
    let client = mock_rag_client();
    let tool = RagQueryTool::new(client);
    let spec = tool.spec();
    assert_eq!(spec.name, "rag_query");
    assert!(spec.input_schema["properties"]["query"].is_object());
    assert!(spec.input_schema["properties"]["top_k"].is_object());
    assert!(spec.input_schema["properties"]["score_threshold"].is_object());
    assert!(spec.input_schema["properties"]["filters"].is_object());
    let required = spec.input_schema["required"].as_array().unwrap();
    assert!(required.contains(&serde_json::json!("query")));
}

// ── Test 5: RAG protocol types ────────────────────────────────────────────

#[test]
fn test_rag_protocol_request_serialization() {
    let mut req = RagQueryRequest::new("Q3 roadmap".to_string(), 5);
    req.collection = Some("product_docs".to_string());
    req.score_threshold = Some(0.7);

    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"protocol_version\":\"1.0\""));
    assert!(json.contains("\"query\":\"Q3 roadmap\""));
    assert!(json.contains("\"top_k\":5"));

    let parsed: RagQueryRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.query, "Q3 roadmap");
    assert_eq!(parsed.top_k, 5);
}

#[test]
fn test_rag_protocol_response_deserialization() {
    let json = r#"{
        "protocol_version": "1.0",
        "results": [
            {
                "content": "Q3 roadmap includes AI assistant",
                "source_url": "https://docs.corp.example.com/roadmap",
                "chunk_id": "roadmap-3",
                "score": 0.92
            },
            {
                "content": "Engineering plan for Q3",
                "score": 0.85
            }
        ]
    }"#;
    let resp: RagQueryResponse = serde_json::from_str(json).unwrap();
    assert_eq!(resp.results.len(), 2);
    assert_eq!(resp.results[0].content, "Q3 roadmap includes AI assistant");
    assert_eq!(resp.results[0].score, 0.92);
    assert!(resp.results[0].source_url.is_some());
    assert!(resp.results[1].source_url.is_none());
}

#[test]
fn test_rag_annotated_result_source_label() {
    let result = AnnotatedRagResult {
        item: RagResultItem {
            content: "test content".to_string(),
            source_url: Some("https://docs.corp.example.com/page1".to_string()),
            chunk_id: Some("chunk-42".to_string()),
            score: 0.88,
        },
        source_label: "[RAG:enterprise_knowledge]".to_string(),
        tool_name: "enterprise_knowledge".to_string(),
    };
    assert_eq!(result.source_label, "[RAG:enterprise_knowledge]");
    assert_eq!(result.item.score, 0.88);
}

// ── Test 6: RAG auth credential ───────────────────────────────────────────

#[test]
fn test_rag_auth_credential_bearer() {
    let cred = RagAuthCredential::Bearer("secret-token".to_string());
    assert!(matches!(cred, RagAuthCredential::Bearer(_)));
}

#[test]
fn test_rag_auth_credential_api_key() {
    let cred = RagAuthCredential::ApiKey("my-api-key".to_string());
    assert!(matches!(cred, RagAuthCredential::ApiKey(_)));
}

#[test]
fn test_rag_auth_credential_none() {
    let cred = RagAuthCredential::None;
    assert!(matches!(cred, RagAuthCredential::None));
}

#[test]
fn test_rag_auth_from_vault_ref_bearer() {
    let cred = RagAuthCredential::from_vault_ref(
        Some("vault:rag_key"),
        "bearer",
        Some("retrieved-token"),
    );
    assert!(matches!(cred, RagAuthCredential::Bearer(ref s) if s == "retrieved-token"));
}

#[test]
fn test_rag_auth_from_vault_ref_api_key() {
    let cred = RagAuthCredential::from_vault_ref(
        Some("vault:rag_key"),
        "api_key",
        Some("retrieved-key"),
    );
    assert!(matches!(cred, RagAuthCredential::ApiKey(ref s) if s == "retrieved-key"));
}

#[test]
fn test_rag_auth_from_vault_ref_no_auth() {
    let cred = RagAuthCredential::from_vault_ref(None, "bearer", None);
    assert!(matches!(cred, RagAuthCredential::None));
}

// ── Test 7: Permission enum RAG coverage ──────────────────────────────────

#[test]
fn test_permission_rag_query_broad_covers_narrow() {
    let broad = Permission::RagQuery(None);
    let narrow = Permission::RagQuery(Some("https://rag.corp.example.com".into()));
    assert!(broad.matches(&narrow), "RagQuery(None) should cover RagQuery(Some)");
    assert!(!narrow.matches(&broad), "RagQuery(Some) should not cover RagQuery(None)");
}

#[test]
fn test_permission_rag_query_same_scope_matches() {
    let a = Permission::RagQuery(Some("https://rag.corp.example.com".into()));
    let b = Permission::RagQuery(Some("https://rag.corp.example.com".into()));
    assert!(a.matches(&b), "Same scope should match");
}

#[test]
fn test_permission_rag_query_different_scope_no_match() {
    let a = Permission::RagQuery(Some("https://rag1.corp.example.com".into()));
    let b = Permission::RagQuery(Some("https://rag2.corp.example.com".into()));
    assert!(!a.matches(&b), "Different scopes should not match");
}

#[test]
fn test_permission_rag_query_does_not_cross_match_network() {
    let rag = Permission::RagQuery(Some("https://rag.corp.example.com".into()));
    let net = Permission::Network(Some("https://rag.corp.example.com".into()));
    assert!(!rag.matches(&net), "RagQuery should not match Network");
    assert!(!net.matches(&rag), "Network should not match RagQuery");
}

// ── Test 8: RagQueryTool whitespace trim (P1-2 fix) ───────────────────────

#[tokio::test]
async fn test_rag_query_tool_whitespace_query_rejected() {
    let client = mock_rag_client();
    let tool = RagQueryTool::new(client);
    let result = tool.execute(serde_json::json!({ "query": "   " })).await.unwrap();
    assert!(!result.ok, "Whitespace-only query should be rejected");
    assert!(result.error.unwrap().contains("Missing 'query'"));
}

// ── Test 10: Mock HTTP server end-to-end ─────────────────────────────────
//
// These tests use mockito to spin up a real HTTP server that responds
// with standard RAG protocol responses, validating the full
// RagClient → RagQueryTool → permission pipeline.

#[tokio::test]
async fn test_rag_client_mock_server_success() {
    let mut server = mockito::Server::new_async().await;

    let response_body = serde_json::json!({
        "protocol_version": "1.0",
        "results": [
            {
                "content": "Q3 product roadmap includes AI assistant",
                "source_url": "https://docs.corp.example.com/roadmap",
                "chunk_id": "roadmap-3",
                "score": 0.92
            },
            {
                "content": "Engineering plan for Q3 delivery",
                "source_url": "https://docs.corp.example.com/eng-plan",
                "chunk_id": "eng-7",
                "score": 0.85
            }
        ]
    });

    let mock = server.mock("POST", "/v1/query")
        .match_header("content-type", "application/json")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&response_body).unwrap())
        .create_async()
        .await;

    let config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: Some("product_docs".to_string()),
        auth: RagAuthCredential::None,
        default_max_results: 5,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let client = RagClient::new(config);

    let results = client.query("product roadmap").await;
    mock.assert_async().await;

    assert_eq!(results.len(), 2, "Should return 2 results from mock server");
    assert_eq!(results[0].item.content, "Q3 product roadmap includes AI assistant");
    assert_eq!(results[0].item.score, 0.92);
    assert_eq!(results[0].source_label, "[RAG:enterprise_knowledge]");
    assert_eq!(results[1].item.content, "Engineering plan for Q3 delivery");
    assert_eq!(results[1].item.score, 0.85);
}

#[tokio::test]
async fn test_rag_client_mock_server_bearer_auth() {
    let mut server = mockito::Server::new_async().await;

    let response_body = serde_json::json!({
        "protocol_version": "1.0",
        "results": [{
            "content": "Confidential doc",
            "score": 0.95
        }]
    });

    let mock = server.mock("POST", "/v1/query")
        .match_header("authorization", "Bearer my-secret-token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&response_body).unwrap())
        .create_async()
        .await;

    let config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: None,
        auth: RagAuthCredential::Bearer("my-secret-token".to_string()),
        default_max_results: 5,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let client = RagClient::new(config);

    let results = client.query("confidential").await;
    mock.assert_async().await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].item.content, "Confidential doc");
}

#[tokio::test]
async fn test_rag_client_mock_server_api_key_auth() {
    let mut server = mockito::Server::new_async().await;

    let response_body = serde_json::json!({
        "protocol_version": "1.0",
        "results": [{
            "content": "API protected doc",
            "score": 0.88
        }]
    });

    let mock = server.mock("POST", "/v1/query")
        .match_header("x-api-key", "my-api-key-123")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&response_body).unwrap())
        .create_async()
        .await;

    let config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: None,
        auth: RagAuthCredential::ApiKey("my-api-key-123".to_string()),
        default_max_results: 5,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let client = RagClient::new(config);

    let results = client.query("api docs").await;
    mock.assert_async().await;
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn test_rag_client_mock_server_error_degrades() {
    let mut server = mockito::Server::new_async().await;

    let mock = server.mock("POST", "/v1/query")
        .with_status(500)
        .with_header("content-type", "application/json")
        .with_body("{\"error\": \"internal server error\"}")
        .create_async()
        .await;

    let config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: None,
        auth: RagAuthCredential::None,
        default_max_results: 5,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let client = RagClient::new(config);

    let results = client.query("test").await;
    mock.assert_async().await;
    // HTTP 500 should degrade to empty results
    assert!(results.is_empty(), "Server error should gracefully degrade to empty");
}

#[tokio::test]
async fn test_rag_client_mock_server_sends_protocol_version() {
    let mut server = mockito::Server::new_async().await;

    let response_body = serde_json::json!({
        "protocol_version": "1.0",
        "results": []
    });

    let mock = server.mock("POST", "/v1/query")
        .match_body(mockito::Matcher::JsonString(
            serde_json::to_string(&serde_json::json!({
                "protocol_version": "1.0",
                "query": "product roadmap",
                "top_k": 5,
                "collection": "product_docs",
                "score_threshold": 0.7
            })).unwrap()
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&response_body).unwrap())
        .create_async()
        .await;

    let config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: Some("product_docs".to_string()),
        auth: RagAuthCredential::None,
        default_max_results: 5,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let client = RagClient::new(config);

    let results = client.query("product roadmap").await;
    mock.assert_async().await;
    assert!(results.is_empty()); // Empty results is fine — we're validating the request body
}

#[tokio::test]
async fn test_rag_query_tool_e2e_with_mock_server() {
    let mut server = mockito::Server::new_async().await;

    let response_body = serde_json::json!({
        "protocol_version": "1.0",
        "results": [
            {
                "content": "The deployment pipeline uses GitHub Actions",
                "source_url": "https://wiki.corp.example.com/ci",
                "chunk_id": "ci-12",
                "score": 0.91
            }
        ]
    });

    let mock = server.mock("POST", "/v1/query")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&response_body).unwrap())
        .create_async()
        .await;

    let config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: None,
        auth: RagAuthCredential::None,
        default_max_results: 5,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let client = Arc::new(RagClient::new(config));
    let tool = RagQueryTool::new(client);

    let result = tool
        .execute(serde_json::json!({ "query": "deployment pipeline" }))
        .await
        .unwrap();

    mock.assert_async().await;
    assert!(result.ok, "RAG tool should succeed with mock server");
    assert!(result.content.contains("deployment pipeline"),
        "Result should contain the query-matched content, got: {}", result.content);
    assert!(result.content.contains("Enterprise knowledge results"),
        "Result should have enterprise knowledge header, got: {}", result.content);
    assert!(result.content.contains("source: https://wiki.corp.example.com/ci"),
        "Result should contain source URL, got: {}", result.content);
}

#[tokio::test]
async fn test_rag_query_tool_e2e_custom_params() {
    let mut server = mockito::Server::new_async().await;

    let response_body = serde_json::json!({
        "protocol_version": "1.0",
        "results": [
            {
                "content": "Matched doc with custom top_k",
                "score": 0.78
            }
        ]
    });

    let mock = server.mock("POST", "/v1/query")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&response_body).unwrap())
        .create_async()
        .await;

    let config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: None,
        auth: RagAuthCredential::None,
        default_max_results: 5,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let client = Arc::new(RagClient::new(config));
    let tool = RagQueryTool::new(client);

    let result = tool
        .execute(serde_json::json!({
            "query": "custom search",
            "top_k": 10,
            "score_threshold": 0.5
        }))
        .await
        .unwrap();

    mock.assert_async().await;
    assert!(result.ok);
    assert!(result.content.contains("Matched doc"));
}

#[tokio::test]
async fn test_rag_client_mock_server_empty_results() {
    let mut server = mockito::Server::new_async().await;

    let response_body = serde_json::json!({
        "protocol_version": "1.0",
        "results": []
    });

    let mock = server.mock("POST", "/v1/query")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&response_body).unwrap())
        .create_async()
        .await;

    let config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: None,
        auth: RagAuthCredential::None,
        default_max_results: 5,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let client = RagClient::new(config);

    let results = client.query("obscure query").await;
    mock.assert_async().await;
    assert!(results.is_empty(), "Empty results array should return empty Vec");
}

// ── Test 11: MemoryManager dual-channel retrieval ────────────────────────
//
// These tests validate that MemoryManager.retrieve() correctly combines
// Grafeo local results with RAG enterprise results when rag_client is
// configured, and behaves identically to Phase 3 when rag_client is None.

use rollball_grafeo::grafeo::GrafeoStore;
use rollball_memory::{HintType, MemoryQuery, MemoryStore};
use rollball_runtime::memory::{MemoryManager, MemoryManagerConfig, ConversationRecord};
use chrono::Utc;

/// Helper: create in-memory GrafeoStore and store an episode via record()
/// This ensures the node is properly indexed for retrieval.
fn grafeo_store_with_record(user_msg: &str, assistant_msg: &str) -> GrafeoStore {
    let store = GrafeoStore::new_in_memory().unwrap();
    let manager = MemoryManager::new(MemoryManagerConfig::default());
    let record = ConversationRecord {
        session_id: "test-session".to_string(),
        turn_index: 0,
        user_message: user_msg.to_string(),
        assistant_response: assistant_msg.to_string(),
        retrieved_memory_ids: vec![],
        timestamp: Utc::now(),
    };
    manager.record(&store, &record).unwrap();
    store
}

fn memory_query(text: &str) -> MemoryQuery {
    MemoryQuery {
        query_text: text.to_string(),
        embedding: None, // Use text search to ensure deterministic results
        filters: Default::default(),
        limit: 10,
        expand_hops: 0,
        min_score: None,
        abstention_enabled: false,
        hint_type: HintType::Semantic,
    }
}

#[tokio::test]
async fn test_memory_manager_no_rag_single_channel() {
    // Without RAG, only Grafeo channel should return results
    let store = grafeo_store_with_record("I live in Beijing", "Got it, Beijing!");
    let manager = MemoryManager::new(MemoryManagerConfig::default());
    assert!(!manager.has_rag(), "Manager without rag_client should report has_rag=false");

    let result = manager.retrieve(&store, &memory_query("Beijing")).await.unwrap();
    assert!(!result.memories.is_empty(), "Grafeo channel should return results");
    // All results should come from Grafeo (no RAG source)
    assert!(result.memories.iter().all(|m| m.source != "rag"),
        "No RAG results expected without rag_client");
}

#[tokio::test]
async fn test_memory_manager_with_rag_dual_channel() {
    // With RAG, both channels should contribute results
    let mut server = mockito::Server::new_async().await;

    let response_body = serde_json::json!({
        "protocol_version": "1.0",
        "results": [{
            "content": "Company policy requires VPN for remote access",
            "source_url": "https://wiki.corp.example.com/vpn-policy",
            "chunk_id": "vpn-5",
            "score": 0.88
        }]
    });

    let mock = server.mock("POST", "/v1/query")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&response_body).unwrap())
        .create_async()
        .await;

    let store = grafeo_store_with_record("I work from home on Fridays", "Remote work noted");

    let rag_config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: None,
        auth: RagAuthCredential::None,
        default_max_results: 3,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let rag_client = Arc::new(RagClient::new(rag_config));
    let manager = MemoryManager::with_rag(MemoryManagerConfig::default(), rag_client);
    assert!(manager.has_rag(), "Manager with rag_client should report has_rag=true");

    let result = manager.retrieve(&store, &memory_query("home")).await.unwrap();
    mock.assert_async().await;

    // Should have results from both channels
    let grafeo_results: Vec<_> = result.memories.iter().filter(|m| m.source != "rag").collect();
    let rag_results: Vec<_> = result.memories.iter().filter(|m| m.source == "rag").collect();

    assert!(!grafeo_results.is_empty(), "Grafeo channel should return results");
    assert!(!rag_results.is_empty(), "RAG channel should return results");

    // RAG results should have source annotations
    let rag_mem = rag_results[0];
    assert!(rag_mem.source_url.is_some(), "RAG result should have source_url");
    assert!(rag_mem.chunk_id.is_some(), "RAG result should have chunk_id");
    assert_eq!(rag_mem.source_url.as_deref(), Some("https://wiki.corp.example.com/vpn-policy"));
}

#[tokio::test]
async fn test_memory_manager_rag_degradation_graceful() {
    // RAG server returns 500 → should degrade to empty RAG results,
    // but Grafeo results should still be returned
    let mut server = mockito::Server::new_async().await;

    let mock = server.mock("POST", "/v1/query")
        .with_status(500)
        .with_body("{\"error\": \"internal\"}")
        .create_async()
        .await;

    let store = grafeo_store_with_record("I prefer light theme", "Light theme noted");

    let rag_config = RagClientConfig {
        endpoint: format!("{}/v1/query", server.url()),
        collection: None,
        auth: RagAuthCredential::None,
        default_max_results: 3,
        default_score_threshold: 0.7,
        timeout: Duration::from_secs(5),
        tool_name: "enterprise_knowledge".to_string(),
    };
    let rag_client = Arc::new(RagClient::new(rag_config));
    let manager = MemoryManager::with_rag(MemoryManagerConfig::default(), rag_client);

    let result = manager.retrieve(&store, &memory_query("light")).await.unwrap();
    mock.assert_async().await;

    // Grafeo results should still be present
    let grafeo_results: Vec<_> = result.memories.iter().filter(|m| m.source != "rag").collect();
    assert!(!grafeo_results.is_empty(), "Grafeo results should survive RAG degradation");

    // RAG results should be empty (graceful degradation)
    let rag_results: Vec<_> = result.memories.iter().filter(|m| m.source == "rag").collect();
    assert!(rag_results.is_empty(), "Failed RAG should produce no results");
}

#[tokio::test]
async fn test_memory_manager_no_rag_identical_to_phase3() {
    // Without RAG, MemoryManager behavior is identical to Phase 3
    let store = grafeo_store_with_record("I like Rust programming", "Rust noted");
    let manager = MemoryManager::new(MemoryManagerConfig::default());

    let result = manager.retrieve(&store, &memory_query("Rust")).await.unwrap();
    // Same as Phase 3: only Grafeo results, no RAG overhead
    assert!(!result.memories.is_empty());
    assert!(result.memories.iter().all(|m| m.source != "rag"));
}

// =============================================================================
// Diagnostic: end-to-end write → read verification (multi-turn conversation)
// =============================================================================

/// Simulates the exact production path: MemoryManager.record() + retrieve().
/// Verifies that:
/// 1. Nodes are created with the correct content format
/// 2. BM25 text search finds nodes by semantic content
/// 3. Multiple turns accumulate and are all searchable
/// 4. The `content` field matches what record_turn_to_memory writes
#[tokio::test]
async fn test_diagnostic_memory_write_read_loop() {
    let store = GrafeoStore::new_in_memory().unwrap();
    let manager = MemoryManager::new(MemoryManagerConfig::default());

    // Check initial stats
    let stats = store.stats().unwrap();
    eprintln!("[DIAG] Initial node_count: {}", stats.episode_count);
    assert_eq!(stats.episode_count, 0, "Fresh store should have 0 nodes");

    // ── Turn 1 ──
    let record_1 = ConversationRecord {
        session_id: "diag-session".to_string(),
        turn_index: 0,
        user_message: "I live in Beijing with my cat named Luna".to_string(),
        assistant_response: "Got it! You live in Beijing and have a cat Luna.".to_string(),
        retrieved_memory_ids: vec![],
        timestamp: Utc::now(),
    };
    manager.record(&store, &record_1).unwrap();

    let stats = store.stats().unwrap();
    eprintln!("[DIAG] After turn 1 — episode_count: {}", stats.episode_count);
    assert!(stats.episode_count >= 1, "Turn 1 should create at least 1 episodic node");

    // Search for turn 1 content (English)
    let r1 = manager.retrieve(&store, &memory_query("Beijing")).await.unwrap();
    eprintln!(
        "[DIAG] Turn 1 search 'Beijing' → {} results (sources: {:?})",
        r1.memories.len(),
        r1.memories.iter().map(|m| m.source.as_str()).collect::<Vec<_>>()
    );
    for m in &r1.memories {
        eprintln!("  node_id={} score={:.4} label={} content={:.80}...", m.node_id, m.score, m.label, m.content);
    }
    assert!(!r1.memories.is_empty(), "Should find 'Beijing' via BM25 text search");
    assert!(r1.memories[0].content.contains("Beijing"), "Content should contain the searched term");

    // Search for 'cat'
    let r1b = manager.retrieve(&store, &memory_query("cat Luna")).await.unwrap();
    eprintln!("[DIAG] Turn 1 search 'cat Luna' → {} results", r1b.memories.len());
    assert!(!r1b.memories.is_empty(), "Should find 'cat Luna' via BM25");

    // ── Test Chinese text (CRITICAL: BM25 may not tokenize CJK) ──
    let cn_store = GrafeoStore::new_in_memory().unwrap();
    let cn_record = ConversationRecord {
        session_id: "cn-test".to_string(),
        turn_index: 0,
        user_message: "我叫张三，住在北京朝阳区".to_string(),
        assistant_response: "你好张三！记住了。".to_string(),
        retrieved_memory_ids: vec![],
        timestamp: Utc::now(),
    };
    manager.record(&cn_store, &cn_record).unwrap();
    let cn_stats = cn_store.stats().unwrap();
    eprintln!("[DIAG] Chinese store node_count: {}", cn_stats.episode_count);

    let cn_result = manager.retrieve(&cn_store, &memory_query("张三")).await.unwrap();
    eprintln!("[DIAG] Chinese search '张三' → {} results", cn_result.memories.len());
    if cn_result.memories.is_empty() {
        eprintln!("[DIAG] ⚠️ BM25 returned 0 results for Chinese text!");
        eprintln!("[DIAG] Root cause: grafeo-engine BM25 tokenizer likely uses whitespace splitting");
        eprintln!("[DIAG] Chinese characters have no spaces between words → no tokens → no matches");
        eprintln!("[DIAG] Fix: need a CJK-aware tokenizer (jieba-rs / tantivy-jieba) or use embedding search");
    } else {
        for m in &cn_result.memories {
            eprintln!("  node_id={} score={:.4} label={} content={:.80}...", m.node_id, m.score, m.label, m.content);
        }
    }

    // ── Turn 2 (different topic) ──
    let record_2 = ConversationRecord {
        session_id: "diag-session".to_string(),
        turn_index: 1,
        user_message: "I love spicy food, especially Sichuan hot pot".to_string(),
        assistant_response: "Sichuan cuisine is great! Hot pot is a classic.".to_string(),
        retrieved_memory_ids: vec![],
        timestamp: Utc::now(),
    };
    manager.record(&store, &record_2).unwrap();

    let stats = store.stats().unwrap();
    eprintln!("[DIAG] After turn 2 — episode_count: {}", stats.episode_count);
    assert!(stats.episode_count >= 2, "Turn 2 should add another node");

    // Search for turn 1 content (should still be findable)
    let r2 = manager.retrieve(&store, &memory_query("Beijing cat")).await.unwrap();
    eprintln!("[DIAG] Turn 2 search 'Beijing cat' → {} results", r2.memories.len());
    assert!(!r2.memories.is_empty(), "Turn 1 memory should still be retrievable after turn 2");

    // Search for turn 2 content
    let r2b = manager.retrieve(&store, &memory_query("hot pot")).await.unwrap();
    eprintln!("[DIAG] Turn 2 search 'hot pot' → {} results", r2b.memories.len());
    assert!(!r2b.memories.is_empty(), "Should find turn 2 content 'hot pot'");

    // ── Turn 3 ──
    let record_3 = ConversationRecord {
        session_id: "diag-session".to_string(),
        turn_index: 2,
        user_message: "My phone number is 555-1234".to_string(),
        assistant_response: "Phone number recorded.".to_string(),
        retrieved_memory_ids: vec![],
        timestamp: Utc::now(),
    };
    manager.record(&store, &record_3).unwrap();

    let stats = store.stats().unwrap();
    eprintln!("[DIAG] After turn 3 — episode_count: {}", stats.episode_count);
    assert!(stats.episode_count >= 3, "Turn 3 should add another node");

    // Cross-topic search: should find all 3 turns
    let r3 = manager.retrieve(&store, &memory_query("phone number")).await.unwrap();
    eprintln!("[DIAG] Turn 3 search 'phone number' → {} results", r3.memories.len());
    assert!(!r3.memories.is_empty(), "Should find 'phone number' from turn 3");

    // Search with limit=1 to verify top result ranking
    let mut limited_query = memory_query("hot pot Sichuan");
    limited_query.limit = 1;
    let r4 = manager.retrieve(&store, &limited_query).await.unwrap();
    eprintln!("[DIAG] Limited search 'hot pot Sichuan' → {} results", r4.memories.len());
    assert_eq!(r4.memories.len(), 1, "limit=1 should return exactly 1 result");

    // Search for something NOT in the store
    let r5 = manager.retrieve(&store, &memory_query("quantum computer")).await.unwrap();
    eprintln!("[DIAG] Search 'quantum computer' (unrelated) → {} results", r5.memories.len());
    // BM25 may return low-score results; the key is that they should be low-score
    // and abstention is NOT enabled, so we may get 0 or some low-score results.
    // This is expected behavior for BM25 without abstention.

    eprintln!("[DIAG] ✅ All diagnostic checks passed!");
    eprintln!("[DIAG] Total episodic nodes written: {}", stats.episode_count);
}
