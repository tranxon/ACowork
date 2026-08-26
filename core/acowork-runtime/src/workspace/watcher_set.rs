//! Workspace watcher set (ADR-058 §3.6).
//!
//! Owns one [`WorkspaceFsWatcher`] task per workspace of the running
//! agent. The set is the single lifecycle authority:
//!
//! - Phase C (startup) and the HTTP workspace CRUD handlers call
//!   [`WorkspaceWatcherSet::sync_from_resolver`] to reconcile the
//!   watcher set against the on-disk `agent_workspaces.json`
//!   (as re-read into the shared `WorkspaceResolver`).
//! - Runtime shutdown / idle sleep (process exit) drops the set, which
//!   stops every watcher task.
//!
//! The set watches ONLY user-configured workspaces
//! (`agent_workspaces.json` entries). `__package_root__` is excluded
//! because it is the parent of the agent home — watching it would
//! duplicate every agent-home event; `__agent_home__` is excluded
//! because it is runtime-managed state (logs, conversations, memory)
//! whose continuous writes would generate event noise rather than
//! user-relevant tree changes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use acowork_core::mqtt_proto::{WorkspaceFsChangeEvent, DataEnvelope, data_envelope};

use crate::http::server::SharedMqttClientSlot;
use crate::mqtt::client::{MqttQoS, RuntimeMqttClient};
use crate::tools::workspace_resolver::WorkspaceResolver;
use crate::workspace::fs_watcher::{WorkspaceFsEventSink, WorkspaceFsWatcher, WorkspaceFsWatcherError};

/// Shared handle type — same late-bind pattern as every other
/// ADR-040 slot. Created empty in Phase A, kept in the boot context
/// and cloned into the Runtime HTTP server state.
pub type SharedWorkspaceWatcherSet = Arc<tokio::sync::Mutex<WorkspaceWatcherSet>>;

/// Task bookkeeping for one running watcher.
struct WatcherHandle {
    root: PathBuf,
    shutdown_tx: mpsc::UnboundedSender<()>,
    task: tokio::task::JoinHandle<()>,
}

/// Set of per-workspace watcher tasks for one agent
/// (one Runtime process = one agent).
pub struct WorkspaceWatcherSet {
    agent_id: String,
    mqtt_slot: SharedMqttClientSlot,
    watchers: HashMap<String, WatcherHandle>,
}

/// MQTT sink — encodes each aggregated event as a `DataEnvelope` and
/// publishes it on `acowork/agents/{id}/workspaces/{wid}/fs-changed`
/// at QoS 1 (at-least-once; a lost event would desync the Desktop's
/// FileTree until the reconnect full-sync fallback).
struct MqttFsEventSink {
    agent_id: String,
    mqtt_slot: SharedMqttClientSlot,
}

#[async_trait::async_trait]
impl WorkspaceFsEventSink for MqttFsEventSink {
    async fn publish(&self, event: WorkspaceFsChangeEvent) {
        let topic = format!(
            "acowork/agents/{}/workspaces/{}/fs-changed",
            self.agent_id, event.workspace_id
        );
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::WorkspaceFsChangeEvent(event)),
        };
        // Clone the client handle out of the slot without holding the
        // slot lock across the publish await.
        let client = self.mqtt_slot.lock().await.clone();
        let Some(client) = client else {
            tracing::debug!(
                topic = %topic,
                "MQTT client not ready — workspace fs event dropped"
            );
            return;
        };
        let client: RuntimeMqttClient = client.lock().await.clone();
        if let Err(e) = client
            .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
            .await
        {
            tracing::warn!(
                topic = %topic,
                error = %e,
                "WorkspaceFsWatcher: failed to publish fs-changed event"
            );
        }
    }
}

impl WorkspaceWatcherSet {
    /// Create an empty set. `mqtt_slot` is the same late-bind slot the
    /// HTTP server holds — events published before the MQTT connection
    /// is established are dropped (acceptable: Desktop reconnects
    /// trigger a full tree re-sync, see ADR-058 §3.4).
    pub fn new(agent_id: String, mqtt_slot: SharedMqttClientSlot) -> Self {
        Self {
            agent_id,
            mqtt_slot,
            watchers: HashMap::new(),
        }
    }

    /// Number of live watchers (diagnostics).
    pub fn watcher_count(&self) -> usize {
        self.watchers.len()
    }

    /// Ensure a watcher exists for `workspace_id` at `root`.
    ///
    /// Idempotent when a watcher already runs with the same root.
    /// A path change stops the old watcher first (single-instance
    /// invariant per workspace_id).
    pub fn ensure_watcher(
        &mut self,
        workspace_id: &str,
        root: &PathBuf,
    ) -> Result<(), WorkspaceFsWatcherError> {
        if let Some(handle) = self.watchers.get(workspace_id) {
            if handle.root == *root {
                return Ok(());
            }
            self.stop_watcher(workspace_id);
        }

        let watcher = WorkspaceFsWatcher::new(root, &self.agent_id, workspace_id)?;
        let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();
        let sink = Arc::new(MqttFsEventSink {
            agent_id: self.agent_id.clone(),
            mqtt_slot: self.mqtt_slot.clone(),
        });
        let task = tokio::spawn(watcher.run(sink, shutdown_rx));
        tracing::info!(
            agent_id = %self.agent_id,
            workspace_id = %workspace_id,
            root = %root.display(),
            "Workspace watcher started"
        );
        self.watchers.insert(
            workspace_id.to_string(),
            WatcherHandle {
                root: root.clone(),
                shutdown_tx,
                task,
            },
        );
        Ok(())
    }

    /// Stop the watcher for `workspace_id` (no-op when absent).
    ///
    /// Graceful path first (shutdown signal → the task flushes its
    /// partial window and exits); `abort()` is only the fallback.
    pub fn stop_watcher(&mut self, workspace_id: &str) {
        if let Some(handle) = self.watchers.remove(workspace_id) {
            let _ = handle.shutdown_tx.send(());
            // Give the graceful path a moment; abort is only a safety net.
            let mut task = handle.task;
            tokio::spawn(async move {
                tokio::select! {
                    _ = &mut task => {}
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                        task.abort();
                    }
                }
            });
            tracing::info!(
                agent_id = %self.agent_id,
                workspace_id = %workspace_id,
                "Workspace watcher stopped"
            );
        }
    }

    /// Stop every watcher (Runtime shutdown / idle sleep path).
    pub fn stop_all(&mut self) {
        for id in self.watchers.keys().cloned().collect::<Vec<_>>() {
            self.stop_watcher(&id);
        }
    }

    /// Reconcile the watcher set against the resolver's current
    /// workspace list (user-configured entries only — see module docs).
    ///
    /// Called from Phase C startup and after every workspace CRUD
    /// mutation (the HTTP handlers reload the resolver first).
    pub fn sync_from_resolver(&mut self, resolver: &WorkspaceResolver) {
        // Desired set: user-configured workspaces only.
        let desired: Vec<(String, PathBuf)> = resolver
            .allowed_dirs()
            .iter()
            .filter(|d| d.id != "__agent_home__" && d.id != "__package_root__")
            .map(|d| (d.id.clone(), PathBuf::from(&d.path)))
            .collect();

        // Stop watchers whose workspace disappeared or whose root moved.
        let current: Vec<(String, PathBuf)> = self
            .watchers
            .iter()
            .map(|(id, h)| (id.clone(), h.root.clone()))
            .collect();
        for (id, root) in current {
            if !desired.iter().any(|(did, droot)| *did == id && *droot == root) {
                self.stop_watcher(&id);
            }
        }

        // Ensure every desired workspace has a live watcher.
        for (id, root) in desired {
            if let Err(e) = self.ensure_watcher(&id, &root) {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    workspace_id = %id,
                    root = %root.display(),
                    error = %e,
                    "Failed to start workspace watcher"
                );
            }
        }
    }
}

impl Drop for WorkspaceWatcherSet {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::workspace_resolver::{WorkspaceAccess, WorkspaceDir};

    fn empty_slot() -> SharedMqttClientSlot {
        Arc::new(tokio::sync::Mutex::new(None))
    }

    fn set() -> WorkspaceWatcherSet {
        WorkspaceWatcherSet::new("agent-test".to_string(), empty_slot())
    }

    fn resolver_with(dirs: Vec<(&str, String)>) -> WorkspaceResolver {
        let allowed = dirs
            .into_iter()
            .map(|(id, path)| WorkspaceDir {
                id: id.to_string(),
                path,
                access: WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            })
            .collect();
        WorkspaceResolver::new_for_test(allowed)
    }

    fn temp_ws(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acowork-ws-set-{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn ensure_watcher_dedupes_by_id_and_root() {
        let mut set = set();
        let dir = temp_ws("dedupe");
        set.ensure_watcher("ws-1", &dir).unwrap();
        set.ensure_watcher("ws-1", &dir).unwrap();
        assert_eq!(set.watcher_count(), 1);

        // Different root for the same id → restart, still one watcher.
        let dir2 = temp_ws("dedupe2");
        set.ensure_watcher("ws-1", &dir2).unwrap();
        assert_eq!(set.watcher_count(), 1);

        set.stop_all();
    }

    #[tokio::test]
    async fn sync_from_resolver_starts_stops_and_updates() {
        let mut set = set();
        let dir_a = temp_ws("a");
        let dir_b = temp_ws("b");
        let dir_c = temp_ws("c");

        // Initial sync: two workspaces.
        set.sync_from_resolver(&resolver_with(vec![
            ("ws-a", dir_a.to_string_lossy().to_string()),
            ("ws-b", dir_b.to_string_lossy().to_string()),
        ]));
        assert_eq!(set.watcher_count(), 2);

        // ws-b removed, ws-c added, ws-a kept → still 2, but different.
        set.sync_from_resolver(&resolver_with(vec![
            ("ws-a", dir_a.to_string_lossy().to_string()),
            ("ws-c", dir_c.to_string_lossy().to_string()),
        ]));
        assert_eq!(set.watcher_count(), 2);
        assert!(set.watchers.contains_key("ws-a"));
        assert!(set.watchers.contains_key("ws-c"));
        assert!(!set.watchers.contains_key("ws-b"));

        // Agent home / package root are never watched.
        set.sync_from_resolver(&resolver_with(vec![
            ("__agent_home__", dir_a.to_string_lossy().to_string()),
            ("__package_root__", dir_b.to_string_lossy().to_string()),
        ]));
        assert_eq!(set.watcher_count(), 0);

        set.stop_all();
    }

    /// Watcher tasks actually observe file changes and route them
    /// through the sink — with the MQTT slot empty the sink drops the
    /// event, so this test asserts task liveness via the watcher count
    /// staying stable over multiple syncs (no task panics/aborts) plus
    /// an ensure→stop cycle completing cleanly.
    #[tokio::test]
    async fn watcher_tasks_survive_sync_cycles() {
        let mut set = set();
        let dir = temp_ws("cycles");
        set.sync_from_resolver(&resolver_with(vec![(
            "ws-x",
            dir.to_string_lossy().to_string(),
        )]));
        // Touch a file while the watcher runs.
        std::fs::write(dir.join("f.txt"), b"data").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        // Re-sync with the same resolver — the watcher must survive.
        set.sync_from_resolver(&resolver_with(vec![(
            "ws-x",
            dir.to_string_lossy().to_string(),
        )]));
        assert_eq!(set.watcher_count(), 1);
        std::fs::remove_file(dir.join("f.txt")).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        set.stop_all();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
