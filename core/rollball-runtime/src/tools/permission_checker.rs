//! Runtime-side permission checker
//!
//! On startup, the Runtime fetches its granted permissions from the Gateway
//! and caches them locally. Before each tool execution, the checker verifies
//! the cached grants cover the required permission.
//!
//! S2.3: When the cache miss occurs and the policy requires user interaction
//! (AskAlways/Default), the checker can send a PermissionRequest via IPC
//! to the Gateway, wait for the response (with 60s timeout), and cache
//! the result if granted.

use std::collections::HashMap;
use parking_lot::RwLock;
use rollball_core::permission::{Permission, PermissionGrant, PermissionPolicy};
use crate::ipc::client::GatewayClient;

/// Result of a permission check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// Permission is granted (either by cache or policy)
    Granted,
    /// Permission is denied (not in cache and not auto-approved)
    Denied(String),
    /// Permission requires runtime request to Gateway (AskAlways/Default policy)
    NeedsRequest(Permission),
}

/// Cached permission checker for the Runtime side.
///
/// On construction, the cache is populated with grants fetched from Gateway.
/// The cache supports:
/// - O(1) permission lookup via category-based index
/// - Invalidation on revocation notification
/// - Policy-based auto-approval for low-risk permissions
pub struct PermissionChecker {
    /// Cached grants, indexed by permission category for fast lookup
    cache: RwLock<PermissionCache>,
    /// The agent_id this checker belongs to
    agent_id: String,
}

/// In-memory cache of granted permissions.
struct PermissionCache {
    /// Granted permissions grouped by category
    by_category: HashMap<String, Vec<PermissionGrant>>,
    /// Generation counter — incremented on every invalidation
    generation: u64,
}

impl PermissionCache {
    fn new() -> Self {
        Self {
            by_category: HashMap::new(),
            generation: 0,
        }
    }

    fn from_grants(grants: Vec<PermissionGrant>) -> Self {
        let mut cache = Self::new();
        for grant in grants {
            if grant.is_expired() {
                continue;
            }
            let cat = grant.permission.category().to_string();
            cache.by_category.entry(cat).or_default().push(grant);
        }
        cache.generation = 1;
        cache
    }
}

impl PermissionChecker {
    /// Create a new checker with initial grants (fetched from Gateway on startup).
    pub fn new(agent_id: &str, grants: Vec<PermissionGrant>) -> Self {
        let cache = PermissionCache::from_grants(grants);
        Self {
            cache: RwLock::new(cache),
            agent_id: agent_id.to_string(),
        }
    }

    /// Create an empty checker (no grants yet).
    pub fn empty(agent_id: &str) -> Self {
        Self {
            cache: RwLock::new(PermissionCache::new()),
            agent_id: agent_id.to_string(),
        }
    }

    /// Check if a permission is granted.
    ///
    /// Returns:
    /// - `Granted` if the permission is covered by a cached grant or auto-approved by policy
    /// - `Denied` if no grant covers it and policy is Deny
    /// - `NeedsRequest` if no grant covers it and policy requires user interaction
    pub fn check(&self, requested: &Permission) -> CheckResult {
        // 1. Check cache first
        if self.check_cache(requested) {
            return CheckResult::Granted;
        }

        // 2. Check policy for auto-approval
        let policy = PermissionPolicy::for_permission(requested);
        match policy {
            PermissionPolicy::Allow => CheckResult::Granted,
            PermissionPolicy::Deny => {
                CheckResult::Denied(format!(
                    "Permission '{}' denied by policy",
                    requested.to_permission_string()
                ))
            }
            PermissionPolicy::AskAlways | PermissionPolicy::Default => {
                CheckResult::NeedsRequest(requested.clone())
            }
        }
    }

    /// Quick check: is the permission granted? (no request needed)
    pub fn is_granted(&self, requested: &Permission) -> bool {
        matches!(self.check(requested), CheckResult::Granted)
    }

    /// Add a newly granted permission to the cache.
    /// Used when a runtime permission request is approved by the user.
    ///
    /// P1-7 fix: Checks for duplicates before adding, since tools
    /// can run concurrently and multiple add_grant calls may race.
    pub fn add_grant(&self, grant: PermissionGrant) {
        let mut cache = self.cache.write();
        let cat = grant.permission.category().to_string();
        let entry = cache.by_category.entry(cat.clone());
        match entry {
            std::collections::hash_map::Entry::Occupied(mut occupied) => {
                let grants = occupied.get_mut();
                // Check if an equivalent grant already exists
                let already_exists = grants.iter().any(|g| {
                    g.permission == grant.permission && g.authorized_by == grant.authorized_by
                });
                if !already_exists {
                    grants.push(grant);
                    cache.generation += 1;
                }
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(vec![grant]);
                cache.generation += 1;
            }
        }
    }

    /// Invalidate the entire cache. Called when Gateway notifies revocation.
    pub fn invalidate_all(&self) {
        let mut cache = self.cache.write();
        cache.by_category.clear();
        cache.generation += 1;
    }

    /// Refresh the cache with new grants from Gateway.
    pub fn refresh(&self, grants: Vec<PermissionGrant>) {
        let new_cache = PermissionCache::from_grants(grants);
        let mut cache = self.cache.write();
        *cache = new_cache;
    }

    /// Get the current cache generation (for detecting staleness).
    pub fn generation(&self) -> u64 {
        self.cache.read().generation
    }

    /// Get the agent_id.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Check the local cache for a matching grant.
    fn check_cache(&self, requested: &Permission) -> bool {
        let cache = self.cache.read();
        let cat = requested.category();
        if let Some(grants) = cache.by_category.get(cat) {
            return grants.iter().any(|g| g.matches_request(requested));
        }
        false
    }

    /// S2.3: Check permission and request via IPC if needed.
    ///
    /// This is the main entry point for runtime permission checking:
    /// 1. Check the local cache first
    /// 2. If cache hit → Granted
    /// 3. If cache miss → check policy:
    ///    - Allow → auto-grant
    ///    - Deny → denied
    ///    - AskAlways/Default → send PermissionRequest via IPC Client
    ///      - Wait up to 60s for response
    ///      - If granted → cache the grant and return Granted
    ///      - If denied or timeout → return Denied
    ///
    /// Returns (granted, reason) tuple. If IPC client is not available
    /// (None), falls back to local-only check (NeedsRequest becomes Denied).
    pub async fn check_and_request(
        &self,
        requested: &Permission,
        ipc_client: Option<&mut GatewayClient>,
    ) -> (bool, Option<String>) {
        // 1. Check local cache first
        if self.check_cache(requested) {
            return (true, None);
        }

        // 2. Check policy
        let policy = PermissionPolicy::for_permission(requested);
        match policy {
            PermissionPolicy::Allow => (true, None),
            PermissionPolicy::Deny => {
                (false, Some(format!(
                    "Permission '{}' denied by policy",
                    requested.to_permission_string()
                )))
            }
            PermissionPolicy::AskAlways | PermissionPolicy::Default => {
                // 3. Send IPC request if client is available
                match ipc_client {
                    Some(client) => {
                        let perm_str = requested.to_permission_string();
                        match client.request_permission(&perm_str, "Runtime tool execution").await {
                            Ok((granted, reason)) => {
                                if granted {
                                    // Cache the grant for future checks
                                    let grant = PermissionGrant::new(
                                        &self.agent_id,
                                        requested.clone(),
                                        "ipc_approval",
                                    );
                                    self.add_grant(grant);
                                }
                                (granted, reason)
                            }
                            Err(e) => {
                                // IPC error (timeout, disconnected, etc.) — deny
                                (false, Some(format!(
                                    "Permission request failed: {}",
                                    e
                                )))
                            }
                        }
                    }
                    None => {
                        // No IPC client available — deny (can't ask user)
                        (false, Some(format!(
                            "Permission '{}' requires approval but IPC not available",
                            requested.to_permission_string()
                        )))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_granted_from_cache() {
        let grants = vec![
            PermissionGrant::new("com.example.agent", Permission::Shell, "user"),
        ];
        let checker = PermissionChecker::new("com.example.agent", grants);

        assert_eq!(checker.check(&Permission::Shell), CheckResult::Granted);
    }

    #[test]
    fn test_check_broad_grant_covers_narrow() {
        let grants = vec![
            PermissionGrant::new("com.example.agent", Permission::Network(None), "user"),
        ];
        let checker = PermissionChecker::new("com.example.agent", grants);

        assert_eq!(
            checker.check(&Permission::Network(Some("https://api.weather.com".into()))),
            CheckResult::Granted
        );
    }

    #[test]
    fn test_check_auto_approved() {
        let checker = PermissionChecker::empty("com.example.agent");

        // MemoryRead is auto-approved by policy
        assert_eq!(checker.check(&Permission::MemoryRead), CheckResult::Granted);
    }

    #[test]
    fn test_check_needs_request() {
        let checker = PermissionChecker::empty("com.example.agent");

        // Shell is AskAlways policy — needs request
        assert!(matches!(
            checker.check(&Permission::Shell),
            CheckResult::NeedsRequest(_)
        ));
    }

    #[test]
    fn test_check_denied_not_in_cache() {
        let checker = PermissionChecker::empty("com.example.agent");

        // Network is Default policy — needs request, not denied
        assert!(matches!(
            checker.check(&Permission::Network(None)),
            CheckResult::NeedsRequest(_)
        ));
    }

    #[test]
    fn test_is_granted() {
        let grants = vec![
            PermissionGrant::new("com.example.agent", Permission::Shell, "user"),
        ];
        let checker = PermissionChecker::new("com.example.agent", grants);

        assert!(checker.is_granted(&Permission::Shell));
        assert!(!checker.is_granted(&Permission::Wasm));
    }

    #[test]
    fn test_add_grant_at_runtime() {
        let checker = PermissionChecker::empty("com.example.agent");

        // Initially not granted
        assert!(!checker.is_granted(&Permission::Shell));

        // Add grant
        checker.add_grant(PermissionGrant::new("com.example.agent", Permission::Shell, "user"));

        // Now granted
        assert!(checker.is_granted(&Permission::Shell));
    }

    #[test]
    fn test_invalidate_all() {
        let grants = vec![
            PermissionGrant::new("com.example.agent", Permission::Shell, "user"),
        ];
        let checker = PermissionChecker::new("com.example.agent", grants);

        assert!(checker.is_granted(&Permission::Shell));

        let gen_before = checker.generation();
        checker.invalidate_all();

        assert_eq!(checker.generation(), gen_before + 1);
        // After invalidation, Shell needs request (not auto-approved)
        assert!(matches!(
            checker.check(&Permission::Shell),
            CheckResult::NeedsRequest(_)
        ));
    }

    #[test]
    fn test_refresh_cache() {
        let checker = PermissionChecker::empty("com.example.agent");

        // Refresh with new grants
        let new_grants = vec![
            PermissionGrant::new("com.example.agent", Permission::Network(None), "user"),
        ];
        checker.refresh(new_grants);

        assert!(checker.is_granted(&Permission::Network(None)));
        assert!(checker.is_granted(&Permission::Network(Some("https://example.com".into()))));
    }

    #[test]
    fn test_expired_grant_not_honored() {
        let past = chrono::Utc::now().timestamp_millis() - 10000;
        let expired = PermissionGrant::with_expiry(
            "com.example.agent",
            Permission::Shell,
            "user",
            past,
        );
        let checker = PermissionChecker::new("com.example.agent", vec![expired]);

        // Expired grant should not be honored
        assert!(matches!(
            checker.check(&Permission::Shell),
            CheckResult::NeedsRequest(_)
        ));
    }

    // ── RagQuery permission checker tests (S4.6) ─────────────────────────

    #[test]
    fn test_rag_query_broad_grant_covers_scoped() {
        let grants = vec![
            PermissionGrant::new(
                "com.example.rag",
                Permission::RagQuery(None),
                "user",
            ),
        ];
        let checker = PermissionChecker::new("com.example.rag", grants);

        // Broad RagQuery(None) covers scoped RagQuery(Some(endpoint))
        assert_eq!(
            checker.check(&Permission::RagQuery(Some("https://rag.corp.example.com".into()))),
            CheckResult::Granted
        );
    }

    #[test]
    fn test_rag_query_scoped_grant_does_not_cover_broad() {
        let grants = vec![
            PermissionGrant::new(
                "com.example.rag",
                Permission::RagQuery(Some("https://rag.corp.example.com".into())),
                "user",
            ),
        ];
        let checker = PermissionChecker::new("com.example.rag", grants);

        // Scoped grant does NOT cover broad request
        assert!(matches!(
            checker.check(&Permission::RagQuery(None)),
            CheckResult::NeedsRequest(_)
        ));

        // Scoped grant covers same endpoint
        assert_eq!(
            checker.check(&Permission::RagQuery(Some("https://rag.corp.example.com".into()))),
            CheckResult::Granted
        );

        // Scoped grant does NOT cover different endpoint
        assert!(matches!(
            checker.check(&Permission::RagQuery(Some("https://other-rag.example.com".into()))),
            CheckResult::NeedsRequest(_)
        ));
    }

    #[test]
    fn test_rag_query_default_policy_needs_request() {
        let checker = PermissionChecker::empty("com.example.rag");

        // RagQuery is Default policy — needs request when not in cache
        assert!(matches!(
            checker.check(&Permission::RagQuery(None)),
            CheckResult::NeedsRequest(_)
        ));
    }
}
