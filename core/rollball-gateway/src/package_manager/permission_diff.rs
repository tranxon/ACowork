//! Permission upgrade detection
//!
//! When an Agent is upgraded, its new manifest may declare additional
//! permissions not present in the old version. This module detects
//! the difference and returns the new permissions for user review.

use rollball_core::permission::Permission;
use rollball_core::AgentManifest;

/// Result of comparing permissions between old and new manifest versions.
#[derive(Debug)]
pub struct PermissionDiff {
    /// Permissions present in both versions (unchanged)
    pub unchanged: Vec<Permission>,
    /// Permissions only in the new version (added)
    pub added: Vec<Permission>,
    /// Permissions only in the old version (removed)
    pub removed: Vec<Permission>,
}

impl PermissionDiff {
    /// Compute the permission diff between old and new manifests.
    pub fn from_manifests(old: &AgentManifest, new: &AgentManifest) -> Self {
        let old_perms: Vec<&Permission> = old.permissions.iter().collect();
        let new_perms: Vec<&Permission> = new.permissions.iter().collect();

        let mut unchanged = Vec::new();
        let mut added = Vec::new();
        let mut removed = Vec::new();

        // Find unchanged and removed (in old but not in new)
        for old_perm in &old_perms {
            let exists_in_new = new_perms.iter().any(|np| np.matches(old_perm));
            if exists_in_new {
                unchanged.push((*old_perm).clone());
            } else {
                removed.push((*old_perm).clone());
            }
        }

        // Find added (in new but not in old)
        for new_perm in &new_perms {
            let exists_in_old = old_perms.iter().any(|op| op.matches(new_perm));
            if !exists_in_old {
                added.push((*new_perm).clone());
            }
        }

        Self {
            unchanged,
            added,
            removed,
        }
    }

    /// Whether the upgrade introduces new permissions
    pub fn has_new_permissions(&self) -> bool {
        !self.added.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manifest(permissions: Vec<Permission>) -> AgentManifest {
        let perm_toml = permissions
            .iter()
            .map(|p| {
                let (t, v) = match p {
                    Permission::Network(v) => ("Network", v.as_deref()),
                    Permission::FilesystemRead(v) => ("FilesystemRead", v.as_deref()),
                    Permission::FilesystemWrite(v) => ("FilesystemWrite", v.as_deref()),
                    Permission::MemoryRead => ("MemoryRead", None),
                    Permission::MemoryWrite => ("MemoryWrite", None),
                    Permission::IntentSend(v) => ("IntentSend", v.as_deref()),
                    Permission::IntentReceive(v) => ("IntentReceive", v.as_deref()),
                    Permission::IdentityRead => ("IdentityRead", None),
                    Permission::IdentityWrite => ("IdentityWrite", None),
                    Permission::Shell => ("Shell", None),
                    Permission::Wasm => ("Wasm", None),
                };
                if let Some(val) = v {
                    format!("[[permissions]]\ntype = \"{}\"\nvalue = \"{}\"", t, val)
                } else {
                    format!("[[permissions]]\ntype = \"{}\"", t)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let toml = format!(
            r#"
            agent_id = "com.test.agent"
            version = "1.0.0"
            name = "Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            {}
            "#,
            perm_toml
        );
        AgentManifest::from_toml(&toml).unwrap()
    }

    #[test]
    fn test_no_permission_change() {
        let old = make_manifest(vec![Permission::MemoryRead, Permission::Shell]);
        let new = make_manifest(vec![Permission::Shell, Permission::MemoryRead]);

        let diff = PermissionDiff::from_manifests(&old, &new);
        assert!(!diff.has_new_permissions());
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.unchanged.len(), 2);
    }

    #[test]
    fn test_new_permissions_added() {
        let old = make_manifest(vec![Permission::MemoryRead]);
        let new = make_manifest(vec![Permission::MemoryRead, Permission::Shell]);

        let diff = PermissionDiff::from_manifests(&old, &new);
        assert!(diff.has_new_permissions());
        assert!(diff.added.contains(&Permission::Shell));
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn test_permissions_removed() {
        let old = make_manifest(vec![Permission::MemoryRead, Permission::Shell]);
        let new = make_manifest(vec![Permission::MemoryRead]);

        let diff = PermissionDiff::from_manifests(&old, &new);
        assert!(!diff.has_new_permissions());
        assert!(diff.removed.contains(&Permission::Shell));
    }

    #[test]
    fn test_broad_to_narrow_not_added() {
        // Old has broad Network(None), new has specific Network(Some(url))
        let old = make_manifest(vec![Permission::Network(None)]);
        let new = make_manifest(vec![Permission::Network(Some("https://api.example.com".into()))]);

        let diff = PermissionDiff::from_manifests(&old, &new);
        // The narrow permission is covered by the broad one, so it's unchanged
        assert!(!diff.has_new_permissions());
    }

    #[test]
    fn test_narrow_to_broad_is_added() {
        // Old has specific, new has broad — this IS a new (broader) permission
        let old = make_manifest(vec![Permission::Network(Some("https://api.example.com".into()))]);
        let new = make_manifest(vec![Permission::Network(None)]);

        let diff = PermissionDiff::from_manifests(&old, &new);
        assert!(diff.has_new_permissions());
        assert!(diff.added.contains(&Permission::Network(None)));
    }
}
