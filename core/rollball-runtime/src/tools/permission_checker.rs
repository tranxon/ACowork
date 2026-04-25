//! Runtime-side permission checker
//!
//! On startup, the Runtime fetches its granted permissions from the Gateway
//! and caches them locally. Before each tool execution, the checker verifies
//! the cached grants cover the required permission.
//!
//! The cache can be invalidated when the Gateway notifies the Runtime
//! that permissions have been revoked (S1.7).

use std::collections::HashMap;
use parking_lot::RwLock;
use rollball_core::permission::{Permission, PermissionGrant, PermissionPolicy};

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
    pub fn add_grant(&self, grant: PermissionGrant) {
        let mut cache = self.cache.write();
        let cat = grant.permission.category().to_string();
        cache.by_category.entry(cat).or_default().push(grant);
        cache.generation += 1;
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
}
