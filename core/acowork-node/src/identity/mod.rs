//! Node identity — the dual-identity model (ADR-055 §6.12).
//!
//! A Node Agent has two identities persisted in
//! `{node_data_dir}/identity.json`:
//!
//! | identity        | format    | lifecycle        | used for                |
//! |-----------------|-----------|------------------|-------------------------|
//! | `machine_uid`   | UUID v4   | never changes    | machine fingerprint:    |
//! |                 |           | (except reinstall)| enrollment idempotency, |
//! |                 |           |                  | name-conflict detection |
//! | `node_id`       | slug      | renameable       | EVERY topic / client_id |
//! |                 |           |                  | / ACL / UI logical name |
//!
//! The identity must be finalized BEFORE the first MQTT CONNECT: the
//! LastWill topic (`acowork/nodes/{node_id}/status`) is part of the
//! CONNECT packet, so there is no "negotiate the name after connecting"
//! window. `load_or_create` is idempotent — re-running it with the same
//! home reuses the persisted identity (script-friendly).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::system_hostname;
use crate::error::{NodeError, Result};

/// Enrollment lifecycle of the node identity (ADR-055 §6.12).
///
/// Phase 2a: enrollment is token-free — `Enrolled` simply means the
/// identity has been persisted and the daemon has published its
/// identity to the Gateway. Phase 5a upgrades the state machine with
/// enrollment-token validation and Gateway-issued node tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentState {
    /// identity.json exists but the node has never connected to a
    /// Gateway.
    Created,
    /// The node has published its identity (info retained topic) to
    /// the Gateway at least once.
    Enrolled,
}

/// Persisted node identity (`{home}/identity.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    /// Logical node name (slug). Used in every topic and client_id.
    pub node_id: String,
    /// Machine fingerprint (UUID v4). Never changes.
    pub machine_uid: String,
    /// Gateway-issued long-term node token (Phase 5a; `None` until
    /// the Gateway signs one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_token: Option<String>,
    /// Last known Gateway address (`host:port` of the MQTT broker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_addr: Option<String>,
    /// Enrollment lifecycle state.
    #[serde(default = "default_enrollment_state")]
    pub enrollment: EnrollmentState,
    /// When this identity file was created.
    pub created_at: DateTime<Utc>,
    /// When this identity was last enrolled with a Gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrolled_at: Option<DateTime<Utc>>,
}

fn default_enrollment_state() -> EnrollmentState {
    EnrollmentState::Created
}

impl NodeIdentity {
    /// Path of the identity file inside a node home directory.
    pub fn path(home: &Path) -> PathBuf {
        home.join("identity.json")
    }

    /// Load the persisted identity, if the home directory has one.
    pub fn load(home: &Path) -> Result<Option<Self>> {
        let path = Self::path(home);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| NodeError::Identity(format!("Failed to read '{}': {}", path.display(), e)))?;
        let identity: Self = serde_json::from_str(&content).map_err(|e| {
            NodeError::Identity(format!(
                "Failed to parse identity file '{}': {}",
                path.display(),
                e
            ))
        })?;
        Self::validate(&identity)?;
        Ok(Some(identity))
    }

    /// Load the persisted identity or create + persist a new one.
    ///
    /// Idempotent: when identity.json already exists it is reused
    /// as-is (only `gateway_addr` is refreshed), so re-running
    /// `acowork-node start` / `enroll` never mints a new machine_uid.
    /// `--name` only takes effect at creation time; changing the name
    /// later is the `rename` command (Phase 3).
    pub fn load_or_create(
        home: &Path,
        explicit_name: Option<&str>,
        gateway_addr: Option<&str>,
    ) -> Result<Self> {
        if let Some(mut existing) = Self::load(home)? {
            if let Some(addr) = gateway_addr {
                existing.gateway_addr = Some(addr.to_string());
            }
            existing.save(home)?;
            return Ok(existing);
        }

        let node_id = match explicit_name {
            Some(name) => {
                if !acowork_core::node::node_id_is_valid(name) {
                    return Err(NodeError::Identity(format!(
                        "Invalid node name '{name}': must be 2-32 chars of [a-z0-9-], \
                         no leading/trailing hyphen"
                    )));
                }
                name.to_string()
            }
            None => acowork_core::node::node_id_from_hostname(&system_hostname()),
        };

        let identity = Self {
            node_id,
            machine_uid: Uuid::new_v4().to_string(),
            node_token: None,
            gateway_addr: gateway_addr.map(str::to_string),
            enrollment: EnrollmentState::Created,
            created_at: Utc::now(),
            enrolled_at: None,
        };
        identity.save(home)?;
        tracing::info!(
            node_id = %identity.node_id,
            machine_uid = %identity.machine_uid,
            "Created new node identity"
        );
        Ok(identity)
    }

    /// Persist the identity to `{home}/identity.json`.
    pub fn save(&self, home: &Path) -> Result<()> {
        std::fs::create_dir_all(home).map_err(|e| {
            NodeError::Identity(format!(
                "Failed to create node home '{}': {}",
                home.display(),
                e
            ))
        })?;
        let path = Self::path(home);
        let content = serde_json::to_string_pretty(self).map_err(|e| {
            NodeError::Identity(format!("Failed to serialize identity: {}", e))
        })?;
        std::fs::write(&path, content).map_err(|e| {
            NodeError::Identity(format!("Failed to write '{}': {}", path.display(), e))
        })
    }

    /// Mark the identity as enrolled with a Gateway (idempotent).
    pub fn mark_enrolled(&mut self, gateway_addr: &str) {
        if self.enrollment != EnrollmentState::Enrolled
            || self.enrolled_at.is_none()
            || self.gateway_addr.as_deref() != Some(gateway_addr)
        {
            self.enrollment = EnrollmentState::Enrolled;
            self.enrolled_at = Some(Utc::now());
            self.gateway_addr = Some(gateway_addr.to_string());
        }
    }

    /// Store a Gateway-issued node token (Phase 5a).
    #[allow(dead_code)]
    pub fn set_node_token(&mut self, token: Option<String>) {
        self.node_token = token;
    }

    fn validate(&self) -> Result<()> {
        if !acowork_core::node::node_id_is_valid(&self.node_id) {
            return Err(NodeError::Identity(format!(
                "Persisted node_id '{}' is not a valid slug — refusing to use it",
                self.node_id
            )));
        }
        if Uuid::parse_str(&self.machine_uid).is_err() {
            return Err(NodeError::Identity(format!(
                "Persisted machine_uid '{}' is not a valid UUID — refusing to use it",
                self.machine_uid
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_persists_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        let first = NodeIdentity::load_or_create(home, Some("gpu-server"), Some("10.0.0.1:19875"))
            .unwrap();
        assert_eq!(first.node_id, "gpu-server");
        assert_eq!(first.enrollment, EnrollmentState::Created);
        assert!(NodeIdentity::path(home).exists());

        // Re-run: same machine_uid, same node_id (idempotent).
        let second = NodeIdentity::load_or_create(home, Some("ignored-name"), None).unwrap();
        assert_eq!(second.node_id, "gpu-server");
        assert_eq!(second.machine_uid, first.machine_uid);

        // gateway_addr refresh still happens on re-run.
        let third = NodeIdentity::load_or_create(home, None, Some("10.0.0.2:19875")).unwrap();
        assert_eq!(third.gateway_addr.as_deref(), Some("10.0.0.2:19875"));
    }

    #[test]
    fn invalid_explicit_name_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let err = NodeIdentity::load_or_create(tmp.path(), Some("Bad_Name"), None).unwrap_err();
        assert!(err.to_string().contains("Invalid node name"));
        assert!(!NodeIdentity::path(tmp.path()).exists());
    }

    #[test]
    fn default_name_derives_from_hostname_slug_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let identity = NodeIdentity::load_or_create(tmp.path(), None, None).unwrap();
        assert!(acowork_core::node::node_id_is_valid(&identity.node_id));
    }

    #[test]
    fn corrupted_identity_file_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(NodeIdentity::path(tmp.path()), "{ not json").unwrap();
        let err = NodeIdentity::load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("parse identity file"));
    }

    #[test]
    fn tampered_node_id_is_rejected_on_load() {
        let tmp = tempfile::tempdir().unwrap();
        let mut identity =
            NodeIdentity::load_or_create(tmp.path(), Some("gpu-server"), None).unwrap();
        identity.node_id = "INVALID SLUG".to_string();
        identity.save(tmp.path()).unwrap();
        let err = NodeIdentity::load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not a valid slug"));
    }

    #[test]
    fn mark_enrolled_is_idempotent_but_stamps_first_time() {
        let tmp = tempfile::tempdir().unwrap();
        let mut identity =
            NodeIdentity::load_or_create(tmp.path(), Some("gpu-server"), None).unwrap();
        assert!(identity.enrolled_at.is_none());

        identity.mark_enrolled("10.0.0.1:19875");
        assert_eq!(identity.enrollment, EnrollmentState::Enrolled);
        let first = identity.enrolled_at.unwrap();

        identity.mark_enrolled("10.0.0.1:19875");
        assert_eq!(identity.enrolled_at, Some(first), "timestamp must be stable");
    }

    #[test]
    fn round_trip_serialization() {
        let tmp = tempfile::tempdir().unwrap();
        let identity =
            NodeIdentity::load_or_create(tmp.path(), Some("gpu-server"), Some("a:1")).unwrap();
        let loaded = NodeIdentity::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded.node_id, identity.node_id);
        assert_eq!(loaded.machine_uid, identity.machine_uid);
        assert_eq!(loaded.gateway_addr, Some("a:1".to_string()));
    }
}
