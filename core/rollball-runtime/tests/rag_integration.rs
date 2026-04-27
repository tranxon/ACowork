//! S4.7: RAG integration tests
//!
//! Cross-cutting integration tests for the Phase 4 RAG feature:
//! 1. Manifest RAG declaration → tool registration + permission validation
//! 2. RAG dual permission check (rag:query + network whitelist)
//! 3. RagClient timeout + graceful degradation
//! 4. RagQueryTool end-to-end (mock client)
//! 5. No RAG declaration → zero intrusion (no tools, no permissions)

use std::sync::Arc;
use std::time::Duration;

use rollball_core::permission::Permission;
use rollball_core::tools::traits::Tool;
use rollball_core::AgentManifest;
use rollball_runtime::tools::builtin::rag_query::RagQueryTool;
use rollball_runtime::tools::permission::{
    validate_permission, validate_rag_network_whitelist,
};
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

fn rag_manifest_no_permissions() -> AgentManifest {
    let toml = r#"
        agent_id = "com.test.rag"
        version = "1.0.0"
        name = "RAG Agent No Perms"
        description = "Test"
        author = "test"
        runtime_version = "0.1.0"

        [llm]
        provider = "openai"
        model = "gpt-4"

        [[tools]]
        type = "rag"
        name = "enterprise_knowledge"

        [tools.rag]
        endpoint = "https://rag.corp.example.com/v1/query"
        collection = "product_docs"
        max_results = 5
        score_threshold = 0.7
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

// ── Test 2: RAG dual permission validation ────────────────────────────────

#[test]
fn test_rag_dual_permission_both_granted() {
    let manifest = rag_manifest();
    assert!(validate_permission(&manifest, "rag_query").is_ok(),
        "rag_query should pass with both rag:query + network permissions");
}

#[test]
fn test_rag_dual_permission_missing_both() {
    let manifest = rag_manifest_no_permissions();
    let result = validate_permission(&manifest, "rag_query");
    assert!(result.is_err(), "rag_query should fail without permissions");
}

#[test]
fn test_rag_network_whitelist_broad() {
    let manifest = rag_manifest();
    assert!(validate_rag_network_whitelist(&manifest).is_ok(),
        "Broad Network(None) should cover any RAG endpoint");
}

#[test]
fn test_rag_network_whitelist_no_rag_config() {
    let manifest = no_rag_manifest();
    assert!(validate_rag_network_whitelist(&manifest).is_ok(),
        "No RAG config → nothing to validate, should pass");
}

#[test]
fn test_rag_network_whitelist_missing_network_perm() {
    let manifest = rag_manifest_no_permissions();
    let result = validate_rag_network_whitelist(&manifest);
    assert!(result.is_err(), "Should fail without network permission");
    let err = result.unwrap_err();
    assert!(err.contains("network permission"), "Error should mention network: {err}");
    assert!(err.contains("https://rag.corp.example.com/v1/query"), "Error should mention endpoint: {err}");
}

// ── Test 3: RAG scoped permission ─────────────────────────────────────────

#[test]
fn test_rag_scoped_permission_matches_endpoint() {
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
        value = "https://rag.corp.example.com/v1/query"

        [[permissions]]
        type = "Network"
        value = "https://rag.corp.example.com/v1/query"

        [[tools]]
        type = "rag"
        name = "enterprise_knowledge"

        [tools.rag]
        endpoint = "https://rag.corp.example.com/v1/query"
        max_results = 5
        score_threshold = 0.7
    "#;
    let manifest = AgentManifest::from_toml(toml).unwrap();
    assert!(validate_permission(&manifest, "rag_query").is_ok(),
        "Scoped rag:query + scoped network matching endpoint should pass");
}

#[test]
fn test_rag_scoped_network_mismatch_denied() {
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
        value = "https://other-api.example.com"

        [[tools]]
        type = "rag"
        name = "enterprise_knowledge"

        [tools.rag]
        endpoint = "https://rag.corp.example.com/v1/query"
        max_results = 5
        score_threshold = 0.7
    "#;
    let manifest = AgentManifest::from_toml(toml).unwrap();
    let result = validate_permission(&manifest, "rag_query");
    assert!(result.is_err(), "Network scope mismatch should deny rag_query");
    assert!(result.unwrap_err().contains("network permission"));
}

// ── Test 3.5: RAG endpoint HTTPS enforcement (P0-2 from review) ──────────

#[test]
fn test_rag_http_endpoint_rejected() {
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
        endpoint = "http://insecure-rag.internal/v1/query"
        max_results = 5
        score_threshold = 0.7
    "#;
    let manifest = AgentManifest::from_toml(toml).unwrap();
    let result = validate_permission(&manifest, "rag_query");
    assert!(result.is_err(), "HTTP endpoint should be rejected");
    let err = result.unwrap_err();
    assert!(err.contains("HTTPS"), "Error should mention HTTPS: {err}");
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
