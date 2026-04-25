//! Installation-time permission review
//!
//! Before an Agent is installed or upgraded, its declared permissions
//! are reviewed against already-granted permissions. New permissions
//! require explicit user approval.

use rollball_core::permission::{Permission, PermissionGrant, PermissionPolicy};
use crate::permission_store::PermissionStore;

/// Result of a permission review during installation.
#[derive(Debug)]
pub struct PermissionReview {
    /// Permissions already granted (no action needed)
    pub already_granted: Vec<Permission>,
    /// New permissions requiring user approval
    pub new_permissions: Vec<Permission>,
    /// Auto-approved permissions (per policy)
    pub auto_approved: Vec<Permission>,
}

impl PermissionReview {
    /// Review an Agent's declared permissions against existing grants.
    ///
    /// Returns a `PermissionReview` categorizing each declared permission:
    /// - already_granted: covered by an existing grant
    /// - auto_approved: policy says Allow (e.g., memory:read)
    /// - new_permissions: needs user approval
    pub fn review(
        declared: &[Permission],
        store: &PermissionStore,
        agent_id: &str,
    ) -> Result<Self, crate::permission_store::PermissionStoreError> {
        let existing_grants = store.query_grants(agent_id)?;

        let mut already_granted = Vec::new();
        let mut auto_approved = Vec::new();
        let mut new_permissions = Vec::new();

        for perm in declared {
            // Check if already covered by an existing grant
            let covered = existing_grants.iter().any(|g| g.matches_request(perm));
            if covered {
                already_granted.push(perm.clone());
                continue;
            }

            // Check if auto-approved by policy
            let policy = PermissionPolicy::for_permission(perm);
            match policy {
                PermissionPolicy::Allow => {
                    auto_approved.push(perm.clone());
                }
                PermissionPolicy::Deny => {
                    // Denied by policy — still list as "new" so the caller
                    // can inform the user and reject the install
                    new_permissions.push(perm.clone());
                }
                PermissionPolicy::AskAlways | PermissionPolicy::Default => {
                    new_permissions.push(perm.clone());
                }
            }
        }

        Ok(Self {
            already_granted,
            new_permissions,
            auto_approved,
        })
    }

    /// Whether any permissions require user approval
    pub fn needs_approval(&self) -> bool {
        !self.new_permissions.is_empty()
    }

    /// Apply the review result: grant auto-approved and user-approved permissions.
    /// `user_approved` is the subset of `new_permissions` that the user accepted.
    pub fn apply(
        &self,
        store: &PermissionStore,
        agent_id: &str,
        user_approved: &[Permission],
    ) -> Result<ApplyResult, crate::permission_store::PermissionStoreError> {
        let mut granted_count = 0u32;

        // Grant auto-approved permissions
        for perm in &self.auto_approved {
            let grant = PermissionGrant::new(agent_id, perm.clone(), "auto");
            store.grant(&grant)?;
            granted_count += 1;
        }

        // Grant user-approved permissions
        for perm in user_approved {
            let grant = PermissionGrant::new(agent_id, perm.clone(), "user");
            store.grant(&grant)?;
            granted_count += 1;
        }

        // Already-granted permissions: update the timestamp (re-affirm)
        for perm in &self.already_granted {
            let grant = PermissionGrant::new(agent_id, perm.clone(), "user");
            store.grant(&grant)?;
            granted_count += 1;
        }

        // Compute denied permissions (new_permissions not in user_approved)
        let denied: Vec<Permission> = self
            .new_permissions
            .iter()
            .filter(|p| !user_approved.iter().any(|u| u.matches(p)))
            .cloned()
            .collect();

        Ok(ApplyResult {
            granted_count,
            denied,
        })
    }
}

/// Result of applying a permission review.
#[derive(Debug)]
pub struct ApplyResult {
    /// Total number of grants written to the store
    pub granted_count: u32,
    /// Permissions that were requested but denied by the user
    pub denied: Vec<Permission>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> PermissionStore {
        PermissionStore::open_in_memory().unwrap()
    }

    #[test]
    fn test_review_new_agent_all_need_approval() {
        let store = make_store();

        let declared = vec![
            Permission::Network(None),
            Permission::Shell,
            Permission::MemoryRead,
        ];

        let review = PermissionReview::review(&declared, &store, "com.example.new").unwrap();

        assert!(review.already_granted.is_empty());
        assert!(review.auto_approved.contains(&Permission::MemoryRead));
        assert!(review.new_permissions.contains(&Permission::Shell));
        assert!(review.new_permissions.contains(&Permission::Network(None)));
        assert!(review.needs_approval());
    }

    #[test]
    fn test_review_existing_grants_covered() {
        let store = make_store();
        // Pre-grant Shell
        store.grant(&PermissionGrant::new("com.example.agent", Permission::Shell, "user")).unwrap();

        let declared = vec![
            Permission::Shell,
            Permission::MemoryRead,
        ];

        let review = PermissionReview::review(&declared, &store, "com.example.agent").unwrap();

        assert!(review.already_granted.contains(&Permission::Shell));
        assert!(review.auto_approved.contains(&Permission::MemoryRead));
        assert!(!review.needs_approval());
    }

    #[test]
    fn test_apply_user_approves_all() {
        let store = make_store();

        let declared = vec![
            Permission::Network(None),
            Permission::Shell,
            Permission::MemoryRead,
        ];

        let review = PermissionReview::review(&declared, &store, "com.example.agent").unwrap();

        // User approves all new permissions
        let result = review.apply(&store, "com.example.agent", &review.new_permissions).unwrap();

        assert_eq!(result.granted_count, 3); // auto(1) + user(2) + already(0)
        assert!(result.denied.is_empty());

        // Verify store has the grants
        let grants = store.query_grants("com.example.agent").unwrap();
        assert_eq!(grants.len(), 3);
    }

    #[test]
    fn test_apply_user_denies_shell() {
        let store = make_store();

        let declared = vec![
            Permission::Shell,
            Permission::MemoryRead,
        ];

        let review = PermissionReview::review(&declared, &store, "com.example.agent").unwrap();

        // User only approves MemoryRead (not Shell)
        let user_approved = vec![Permission::MemoryRead];
        let result = review.apply(&store, "com.example.agent", &user_approved).unwrap();

        assert!(result.denied.contains(&Permission::Shell));

        // Shell should NOT be in the store
        assert!(!store.has_permission("com.example.agent", &Permission::Shell).unwrap());
        // MemoryRead should be
        assert!(store.has_permission("com.example.agent", &Permission::MemoryRead).unwrap());
    }

    #[test]
    fn test_review_no_permissions_declared() {
        let store = make_store();

        let declared: Vec<Permission> = vec![];
        let review = PermissionReview::review(&declared, &store, "com.example.agent").unwrap();

        assert!(!review.needs_approval());
        assert!(review.already_granted.is_empty());
        assert!(review.auto_approved.is_empty());
        assert!(review.new_permissions.is_empty());
    }

    #[test]
    fn test_broad_grant_covers_narrow() {
        let store = make_store();
        // Pre-grant broad Network(None)
        store.grant(&PermissionGrant::new("com.example.agent", Permission::Network(None), "user")).unwrap();

        let declared = vec![
            Permission::Network(Some("https://api.weather.com".into())),
        ];

        let review = PermissionReview::review(&declared, &store, "com.example.agent").unwrap();

        // Narrow request is covered by broad grant
        assert!(review.already_granted.contains(&Permission::Network(Some("https://api.weather.com".into()))));
        assert!(!review.needs_approval());
    }
}
