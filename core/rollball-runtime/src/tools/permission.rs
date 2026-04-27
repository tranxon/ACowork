//! Tool permission validation
//!
//! Validates tool calls against the Agent's declared manifest permissions.
//! 16 built-in tools and their required permissions:
//!
//! | Tool | Required Permission |
//! |------|-------------------|
//! | memory_recall | memory:read |
//! | memory_store | memory:write |
//! | http_request | network:<url> |
//! | web_fetch | network:<url> |
//! | web_search | search:web → Network |
//! | shell | filesystem:exec → Shell |
//! | file_read | filesystem:read:<path> |
//! | file_write | filesystem:write:<path> |
//! | file_edit | filesystem:write:<path> |
//! | glob_search | filesystem:read:<path> |
//! | content_search | filesystem:read:<path> |
//! | intent_send | intent:send:<target> |
//! | identity_store | identity:write |
//! | identity_query | identity:read |
//! | identity_observe | identity:read |
//! | rag_query | rag:query + network:<rag_url> (dual permission, S4.6) |

use rollball_core::permission::Permission;
use rollball_core::AgentManifest;

/// Map a tool name to the Permission it requires.
/// Returns None if the tool doesn't require a specific permission.
pub fn tool_required_permission(tool_name: &str) -> Option<Permission> {
    match tool_name {
        "memory_recall" => Some(Permission::MemoryRead),
        "memory_store" => Some(Permission::MemoryWrite),
        "http_request" | "web_fetch" => Some(Permission::Network(None)),
        "web_search" => Some(Permission::Network(None)), // design doc says search:web, mapped to Network for now
        "shell" => Some(Permission::Shell),
        "file_read" | "glob_search" | "content_search" => Some(Permission::FilesystemRead(None)),
        "file_write" | "file_edit" => Some(Permission::FilesystemWrite(None)),
        "intent_send" => Some(Permission::IntentSend(None)),
        "identity_store" => Some(Permission::IdentityWrite),
        "identity_query" | "identity_observe" => Some(Permission::IdentityRead),
        // RAG tool: base permission is RagQuery; validate_permission also checks
        // network whitelist for the RAG endpoint (dual permission, S4.6)
        "rag_query" => Some(Permission::RagQuery(None)),
        _ => None,
    }
}

/// Validate tool call against manifest permissions
///
/// Returns Ok(()) if the tool is allowed, Err with reason if not.
/// Tools that don't require permissions are always allowed.
///
/// For `rag_query`, this performs **dual permission validation** (S4.6):
/// 1. Check `rag:query` (or `rag:query:<endpoint>`) is declared
/// 2. Check `network:<endpoint>` is declared (network whitelist)
/// The endpoint URL is extracted from `manifest.rag_config()`.
pub fn validate_permission(manifest: &AgentManifest, tool_name: &str) -> Result<(), String> {
    let required = match tool_required_permission(tool_name) {
        Some(p) => p,
        None => return Ok(()), // No permission needed
    };

    // Also check that the tool is declared in manifest.tools
    //
    // Special case for rag_query: the manifest declares RAG tools with a custom
    // name (e.g., "enterprise_knowledge") and type="rag". The built-in tool's
    // runtime name is always "rag_query", so we check manifest.has_rag() instead.
    if !manifest.tools.is_empty() {
        let tool_declared = if tool_name == "rag_query" {
            manifest.has_rag()
        } else {
            manifest.has_tool(tool_name)
        };
        if !tool_declared {
            return Err(format!(
                "Tool '{}' not declared in manifest. Available tools: [{}]",
                tool_name,
                manifest.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>().join(", ")
            ));
        }
    }

    // Check primary permission
    //
    // S4.6: For rag_query, the tool_required_permission returns RagQuery(None),
    // but if the manifest has a scoped RagQuery permission (RagQuery(Some(endpoint))),
    // the broad→narrow matching won't cover it. So we upgrade the required permission
    // to include the specific endpoint from manifest.rag_config().
    // A broad RagQuery(None) in manifest covers any endpoint.
    // A scoped RagQuery(Some(endpoint)) covers that specific endpoint.
    let effective_required = if tool_name == "rag_query" {
        if let Some((_, rag_config)) = manifest.rag_config() {
            Permission::RagQuery(Some(rag_config.endpoint.clone()))
        } else {
            required
        }
    } else {
        required
    };

    if !manifest.has_permission(&effective_required) {
        return Err(format!(
            "Permission '{}' required for tool '{}' but not declared in manifest",
            effective_required.to_permission_string(),
            tool_name
        ));
    }

    // S4.6: RAG dual permission — also check network whitelist for the endpoint
    if tool_name == "rag_query" {
        if let Err(e) = validate_rag_network_whitelist(manifest) {
            return Err(e);
        }
    }

    Ok(())
}

/// Validate RAG network whitelist (S4.6)
///
/// RAG queries send HTTP requests to the configured endpoint, so the agent
/// must also have `network:<endpoint_url>` permission. This function:
///
/// 1. Extracts the RAG endpoint from `manifest.rag_config()`
/// 2. Checks that the manifest declares a `Network` permission covering the endpoint
///
/// Broad `Network(None)` covers any endpoint.
/// Narrow `Network(Some("https://rag.corp.example.com"))` only covers that URL.
pub fn validate_rag_network_whitelist(manifest: &AgentManifest) -> Result<(), String> {
    let (_tool_name, rag_config) = match manifest.rag_config() {
        Some(config) => config,
        None => {
            // No RAG config in manifest — nothing to validate
            return Ok(());
        }
    };

    let endpoint = &rag_config.endpoint;

    // Security: RAG requests carry auth credentials (Bearer/API key).
    // HTTP endpoints transmit credentials in cleartext — reject them.
    if !endpoint.starts_with("https://") {
        return Err(format!(
            "RAG endpoint must use HTTPS: '{}' (HTTP would expose auth credentials)",
            endpoint
        ));
    }

    let network_perm = Permission::Network(Some(endpoint.clone()));

    if manifest.has_permission(&network_perm) {
        Ok(())
    } else {
        Err(format!(
            "RAG endpoint '{}' requires network permission (e.g., 'network:{}' or 'network' for full access) but not declared in manifest",
            endpoint, endpoint
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rollball_core::manifest::AgentManifest;

    fn full_manifest() -> AgentManifest {
        let toml_str = r#"
            agent_id = "com.test.agent"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[permissions]]
            type = "Shell"

            [[permissions]]
            type = "Network"

            [[permissions]]
            type = "MemoryRead"

            [[permissions]]
            type = "MemoryWrite"

            [[permissions]]
            type = "FilesystemRead"

            [[permissions]]
            type = "FilesystemWrite"

            [[permissions]]
            type = "IntentSend"

            [[permissions]]
            type = "IdentityWrite"

            [[tools]]
            name = "shell"

            [[tools]]
            name = "http_request"

            [[tools]]
            name = "web_fetch"

            [[tools]]
            name = "web_search"

            [[tools]]
            name = "memory_store"

            [[tools]]
            name = "memory_recall"

            [[tools]]
            name = "file_read"

            [[tools]]
            name = "file_write"

            [[tools]]
            name = "file_edit"

            [[tools]]
            name = "glob_search"

            [[tools]]
            name = "content_search"

            [[tools]]
            name = "intent_send"

            [[tools]]
            name = "identity_store"
        "#;
        AgentManifest::from_toml(toml_str).unwrap()
    }

    #[test]
    fn test_tool_required_permission_memory_recall() {
        let perm = tool_required_permission("memory_recall").unwrap();
        assert_eq!(perm, Permission::MemoryRead);
    }

    #[test]
    fn test_tool_required_permission_memory_store() {
        let perm = tool_required_permission("memory_store").unwrap();
        assert_eq!(perm, Permission::MemoryWrite);
    }

    #[test]
    fn test_tool_required_permission_http_request() {
        let perm = tool_required_permission("http_request").unwrap();
        assert!(matches!(perm, Permission::Network(None)));
    }

    #[test]
    fn test_tool_required_permission_shell() {
        let perm = tool_required_permission("shell").unwrap();
        assert_eq!(perm, Permission::Shell);
    }

    #[test]
    fn test_tool_required_permission_intent_send() {
        let perm = tool_required_permission("intent_send").unwrap();
        assert!(matches!(perm, Permission::IntentSend(None)));
    }

    #[test]
    fn test_tool_required_permission_identity_store() {
        let perm = tool_required_permission("identity_store").unwrap();
        assert!(matches!(perm, Permission::IdentityWrite));
    }

    #[test]
    fn test_tool_required_permission_unknown() {
        assert!(tool_required_permission("weather").is_none());
        assert!(tool_required_permission("calculator").is_none());
        assert!(tool_required_permission("http_get").is_none());
        assert!(tool_required_permission("http_post").is_none());
    }

    #[test]
    fn test_validate_permission_all_allowed() {
        let manifest = full_manifest();
        assert!(validate_permission(&manifest, "shell").is_ok());
        assert!(validate_permission(&manifest, "http_request").is_ok());
        assert!(validate_permission(&manifest, "web_fetch").is_ok());
        assert!(validate_permission(&manifest, "web_search").is_ok());
        assert!(validate_permission(&manifest, "memory_store").is_ok());
        assert!(validate_permission(&manifest, "memory_recall").is_ok());
        assert!(validate_permission(&manifest, "file_read").is_ok());
        assert!(validate_permission(&manifest, "file_write").is_ok());
        assert!(validate_permission(&manifest, "file_edit").is_ok());
        assert!(validate_permission(&manifest, "glob_search").is_ok());
        assert!(validate_permission(&manifest, "content_search").is_ok());
        assert!(validate_permission(&manifest, "intent_send").is_ok());
        assert!(validate_permission(&manifest, "identity_store").is_ok());
    }

    #[test]
    fn test_validate_permission_missing_tool_declaration() {
        let manifest = full_manifest();
        // "unknown_tool" has no required permission, so it's allowed by default
        // (permission check is skipped when tool_required_permission returns None)
        // To enforce tool declarations, we need a different approach
        // For now, unknown tools with no permission mapping pass through
        let result = validate_permission(&manifest, "unknown_tool");
        // This succeeds because unknown_tool has no required permission
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_permission_missing_permission() {
        let toml_str = r#"
            agent_id = "com.test.agent"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[tools]]
            name = "shell"
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        let result = validate_permission(&manifest, "shell");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Permission"));
    }

    #[test]
    fn test_validate_permission_empty_tools_any_allowed() {
        let toml_str = r#"
            agent_id = "com.test.agent"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[permissions]]
            type = "Shell"
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        assert!(validate_permission(&manifest, "shell").is_ok());
    }

    // ── RAG permission tests (S4.6) ─────────────────────────────────────

    #[test]
    fn test_tool_required_permission_rag_query() {
        let perm = tool_required_permission("rag_query").unwrap();
        assert!(matches!(perm, Permission::RagQuery(None)));
    }

    /// Helper: manifest with RAG tool + rag:query + network permissions
    fn rag_manifest_full() -> AgentManifest {
        let toml_str = r#"
            agent_id = "com.test.rag"
            version = "1.0.0"
            name = "RAG Test Agent"
            description = "Test RAG permissions"
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
            max_results = 5
            score_threshold = 0.7
        "#;
        AgentManifest::from_toml(toml_str).unwrap()
    }

    /// Helper: manifest with RAG tool + rag:query but NO network permission
    fn rag_manifest_no_network() -> AgentManifest {
        let toml_str = r#"
            agent_id = "com.test.rag"
            version = "1.0.0"
            name = "RAG Test Agent"
            description = "Test RAG permissions"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[permissions]]
            type = "RagQuery"

            [[tools]]
            type = "rag"
            name = "enterprise_knowledge"

            [tools.rag]
            endpoint = "https://rag.corp.example.com/v1/query"
            collection = "product_docs"
            max_results = 5
            score_threshold = 0.7
        "#;
        AgentManifest::from_toml(toml_str).unwrap()
    }

    /// Helper: manifest with RAG tool + network permission but NO rag:query
    fn rag_manifest_no_rag_perm() -> AgentManifest {
        let toml_str = r#"
            agent_id = "com.test.rag"
            version = "1.0.0"
            name = "RAG Test Agent"
            description = "Test RAG permissions"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[permissions]]
            type = "Network"

            [[tools]]
            type = "rag"
            name = "enterprise_knowledge"

            [tools.rag]
            endpoint = "https://rag.corp.example.com/v1/query"
            collection = "product_docs"
            max_results = 5
            score_threshold = 0.7
        "#;
        AgentManifest::from_toml(toml_str).unwrap()
    }

    /// Helper: manifest with RAG tool + scoped rag:query + scoped network
    fn rag_manifest_scoped() -> AgentManifest {
        let toml_str = r#"
            agent_id = "com.test.rag"
            version = "1.0.0"
            name = "RAG Test Agent"
            description = "Test RAG permissions"
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
            collection = "product_docs"
            max_results = 5
            score_threshold = 0.7
        "#;
        AgentManifest::from_toml(toml_str).unwrap()
    }

    /// Helper: manifest with RAG tool + scoped network that does NOT match endpoint
    fn rag_manifest_wrong_network_scope() -> AgentManifest {
        let toml_str = r#"
            agent_id = "com.test.rag"
            version = "1.0.0"
            name = "RAG Test Agent"
            description = "Test RAG permissions"
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
            collection = "product_docs"
            max_results = 5
            score_threshold = 0.7
        "#;
        AgentManifest::from_toml(toml_str).unwrap()
    }

    #[test]
    fn test_rag_query_both_permissions_ok() {
        let manifest = rag_manifest_full();
        assert!(validate_permission(&manifest, "rag_query").is_ok());
    }

    #[test]
    fn test_rag_query_missing_rag_query_permission() {
        let manifest = rag_manifest_no_rag_perm();
        let result = validate_permission(&manifest, "rag_query");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("rag:query"), "Error should mention rag:query: {err}");
    }

    #[test]
    fn test_rag_query_missing_network_permission() {
        let manifest = rag_manifest_no_network();
        let result = validate_permission(&manifest, "rag_query");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("network permission"), "Error should mention network permission: {err}");
        assert!(err.contains("https://rag.corp.example.com/v1/query"), "Error should mention endpoint: {err}");
    }

    #[test]
    fn test_rag_query_scoped_permissions_ok() {
        let manifest = rag_manifest_scoped();
        assert!(validate_permission(&manifest, "rag_query").is_ok());
    }

    #[test]
    fn test_rag_query_wrong_network_scope_denied() {
        let manifest = rag_manifest_wrong_network_scope();
        let result = validate_permission(&manifest, "rag_query");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("network permission"), "Error should mention network permission: {err}");
    }

    #[test]
    fn test_validate_rag_network_whitelist_broad_network() {
        // Broad Network(None) covers any RAG endpoint
        let manifest = rag_manifest_full();
        assert!(validate_rag_network_whitelist(&manifest).is_ok());
    }

    #[test]
    fn test_validate_rag_network_whitelist_no_rag_config() {
        // No RAG config → nothing to validate
        let toml_str = r#"
            agent_id = "com.test.agent"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        assert!(validate_rag_network_whitelist(&manifest).is_ok());
    }

    #[test]
    fn test_validate_rag_network_whitelist_scoped_match() {
        let manifest = rag_manifest_scoped();
        assert!(validate_rag_network_whitelist(&manifest).is_ok());
    }

    #[test]
    fn test_validate_rag_network_whitelist_scoped_mismatch() {
        let manifest = rag_manifest_wrong_network_scope();
        assert!(validate_rag_network_whitelist(&manifest).is_err());
    }

    #[test]
    fn test_rag_endpoint_must_be_https() {
        // HTTP endpoint should be rejected — credentials would be exposed in cleartext
        let toml_str = r#"
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
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        let result = validate_rag_network_whitelist(&manifest);
        assert!(result.is_err(), "HTTP endpoint should be rejected");
        let err = result.unwrap_err();
        assert!(err.contains("HTTPS"), "Error should mention HTTPS: {err}");
        assert!(err.contains("http://insecure-rag.internal"), "Error should mention endpoint: {err}");
    }
}
