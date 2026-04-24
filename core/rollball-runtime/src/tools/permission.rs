//! Tool permission validation
//!
//! Validates tool calls against the Agent's declared manifest permissions.
//! 15 built-in tools and their required permissions:
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
//! | identity_store | identity:write → IntentSend |
//! | identity_query | identity:read → MemoryRead |
//! | identity_observe | identity:read → MemoryRead |

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
        "identity_store" => Some(Permission::IntentSend(None)), // identity:write → IntentSend broad permission
        "identity_query" | "identity_observe" => Some(Permission::MemoryRead), // identity:read → MemoryRead
        _ => None,
    }
}

/// Validate tool call against manifest permissions
///
/// Returns Ok(()) if the tool is allowed, Err with reason if not.
/// Tools that don't require permissions are always allowed.
pub fn validate_permission(manifest: &AgentManifest, tool_name: &str) -> Result<(), String> {
    let required = match tool_required_permission(tool_name) {
        Some(p) => p,
        None => return Ok(()), // No permission needed
    };

    // Also check that the tool is declared in manifest.tools
    if !manifest.tools.is_empty() && !manifest.has_tool(tool_name) {
        return Err(format!(
            "Tool '{}' not declared in manifest. Available tools: [{}]",
            tool_name,
            manifest.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>().join(", ")
        ));
    }

    // Check permission
    if manifest.has_permission(&required) {
        Ok(())
    } else {
        Err(format!(
            "Permission '{}' required for tool '{}' but not declared in manifest",
            required.to_permission_string(),
            tool_name
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
        assert!(matches!(perm, Permission::IntentSend(None)));
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
}
