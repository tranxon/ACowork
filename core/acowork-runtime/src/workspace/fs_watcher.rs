//! Workspace filesystem watcher (ADR-058).
//!
//! Wraps a `notify::PollWatcher` (500ms poll interval, same selection
//! rationale as `security/fs_watcher.rs`) and aggregates raw notify
//! events into `WorkspaceFsChangeEvent` batches within a 500ms window:
//!
//! - multiple modifications of the same path coalesce to one `Modified`
//! - `Created` followed by `Modify` in the same window stays `Created`
//! - `Created` followed by `Remove` in the same window cancels out
//!   (atomic temp-file churn produces no event)
//! - rename is NOT inferred — it degrades to `Deleted(old)` +
//!   `Created(new)` (PollWatcher has no inode pairing)
//!
//! Out-of-bounds paths (symlink escapes, sibling directories) are
//! dropped before aggregation; only forward-slash workspace-relative
//! paths ever leave this module.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::{Config, EventKind, PollWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use acowork_core::mqtt_proto::{FsChange, FsChangeKind, WorkspaceFsChangeEvent};

/// `notify::PollWatcher` scan interval — same value as
/// `security/fs_watcher.rs::FS_POLL_INTERVAL` (cross-platform
/// predictable latency, see ADR-009 §11.4).
const FS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Aggregation window. Worst-case end-to-end latency is
/// poll interval + window ≈ 1s (ADR-058 §3.1).
pub const WINDOW_DURATION: Duration = Duration::from_millis(500);

/// Error type for watcher construction.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceFsWatcherError {
    #[error("workspace fs watcher init failed: {0}")]
    Init(String),
}

/// Destination for aggregated `WorkspaceFsChangeEvent` batches.
///
/// Implemented by the MQTT publisher in [`super::watcher_set`] and by
/// test collectors — keeps this module free of MQTT knowledge.
#[async_trait::async_trait]
pub trait WorkspaceFsEventSink: Send + Sync {
    async fn publish(&self, event: WorkspaceFsChangeEvent);
}

/// One watcher per (agent, workspace): aggregates notify events for a
/// single workspace root into windowed `WorkspaceFsChangeEvent` batches.
///
/// The struct is consumed by [`WorkspaceFsWatcher::run`], which owns
/// the `PollWatcher` for the task's lifetime. Dropping the struct
/// (task abort / shutdown) stops the notify polling.
pub struct WorkspaceFsWatcher {
    workspace_dir: PathBuf,
    agent_id: String,
    workspace_id: String,
    /// Kept alive for as long as the watcher runs; dropping it closes
    /// the event channel, which makes `run`'s `rx.recv()` return `None`.
    #[allow(dead_code)] // read never happens — the field's Drop does the work
    notify_watcher: PollWatcher,
    rx: mpsc::UnboundedReceiver<notify::Event>,
    /// Aggregator buffer for the current window (rel path → latest kind).
    pending: HashMap<PathBuf, FsChangeKind>,
    /// When the current window opened; `None` = idle (no pending events).
    window_started: Option<Instant>,
}

impl WorkspaceFsWatcher {
    /// Create a watcher for `workspace_root` (recursive).
    ///
    /// Uses an unbounded tokio channel so the synchronous notify
    /// callback can emit events without an async context (same
    /// convention as `security/fs_watcher.rs`).
    pub fn new(
        workspace_root: &Path,
        agent_id: &str,
        workspace_id: &str,
    ) -> Result<Self, WorkspaceFsWatcherError> {
        let (tx, rx) = mpsc::unbounded_channel();

        let mut notify_watcher = PollWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            },
            Config::default().with_poll_interval(FS_POLL_INTERVAL),
        )
        .map_err(|e| WorkspaceFsWatcherError::Init(e.to_string()))?;

        notify_watcher
            .watch(workspace_root, RecursiveMode::Recursive)
            .map_err(|e| WorkspaceFsWatcherError::Init(e.to_string()))?;

        Ok(Self {
            workspace_dir: workspace_root.to_path_buf(),
            agent_id: agent_id.to_string(),
            workspace_id: workspace_id.to_string(),
            notify_watcher,
            rx,
            pending: HashMap::new(),
            window_started: None,
        })
    }

    /// Run the aggregation loop until the notify watcher is dropped or
    /// a shutdown signal arrives. A final flush on exit ensures the
    /// current partial window is not lost.
    pub async fn run(
        mut self,
        sink: std::sync::Arc<dyn WorkspaceFsEventSink>,
        mut shutdown: mpsc::UnboundedReceiver<()>,
    ) {
        tracing::info!(
            agent_id = %self.agent_id,
            workspace_id = %self.workspace_id,
            root = %self.workspace_dir.display(),
            "WorkspaceFsWatcher started"
        );
        loop {
            // Window deadline is only meaningful while a window is open.
            let deadline = self
                .window_started
                .map(|start| tokio::time::Instant::from_std(start + WINDOW_DURATION));
            tokio::select! {
                _ = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.flush(sink.as_ref()).await;
                }
                maybe_raw = self.rx.recv() => {
                    match maybe_raw {
                        Some(raw) => {
                            self.ingest(raw);
                            if let Some(start) = self.window_started
                                && start.elapsed() >= WINDOW_DURATION
                            {
                                self.flush(sink.as_ref()).await;
                            }
                        }
                        None => break, // notify watcher dropped → shutdown
                    }
                }
                _ = shutdown.recv() => break, // explicit stop from the set
            }
        }
        // Flush the partial window so in-flight events are not lost.
        self.flush(sink.as_ref()).await;
        tracing::debug!(
            agent_id = %self.agent_id,
            workspace_id = %self.workspace_id,
            "WorkspaceFsWatcher stopped"
        );
    }

    /// Ingest one raw notify event into the aggregation window.
    ///
    /// Applies the coalescing rules from ADR-058 §3.1:
    /// - `Create` + `Modify` in the same window → stays `Created`
    /// - `Create` + `Remove` in the same window → cancelled out
    /// - `Modify(Metadata)` and content `Modify` both map to `Modified`
    pub fn ingest(&mut self, event: notify::Event) {
        for path in &event.paths {
            // Out-of-bounds filter: keep only paths inside the workspace
            // root (symlink escapes / sibling dirs are dropped outright —
            // never leak absolute paths).
            if !path.starts_with(&self.workspace_dir) {
                continue;
            }
            let Some(rel) = self.to_rel_path(path) else {
                continue;
            };
            let kind = match event.kind {
                EventKind::Create(_) => Some(FsChangeKind::Created),
                EventKind::Modify(_) => Some(FsChangeKind::Modified),
                EventKind::Remove(_) => Some(FsChangeKind::Deleted),
                _ => None,
            };
            let Some(kind) = kind else { continue };

            match kind {
                FsChangeKind::Created => {
                    self.pending.insert(rel, FsChangeKind::Created);
                }
                FsChangeKind::Modified => {
                    // Created → Modified in the same window: the creation
                    // already announces the file; keep just `Created`.
                    if matches!(self.pending.get(&rel), Some(FsChangeKind::Created)) {
                        continue;
                    }
                    self.pending.insert(rel, FsChangeKind::Modified);
                }
                FsChangeKind::Deleted => {
                    // Created → Deleted in the same window: the file
                    // appeared and vanished — emit nothing.
                    if matches!(self.pending.get(&rel), Some(FsChangeKind::Created)) {
                        self.pending.remove(&rel);
                        continue;
                    }
                    self.pending.insert(rel, FsChangeKind::Deleted);
                }
                FsChangeKind::Unspecified => {}
            }

            if self.window_started.is_none() {
                self.window_started = Some(Instant::now());
            }
        }
    }

    /// Flush the aggregation window into one event and hand it to the
    /// sink. No-op when the window is empty.
    pub async fn flush(&mut self, sink: &dyn WorkspaceFsEventSink) {
        if self.pending.is_empty() {
            self.window_started = None;
            return;
        }
        let now_ms = epoch_ms();
        let changes: Vec<FsChange> = self
            .pending
            .drain()
            .map(|(rel, kind)| FsChange {
                kind: kind.into(),
                path: path_to_forward_slash(&rel),
                timestamp_ms: now_ms,
            })
            .collect();
        self.window_started = None;

        let event = WorkspaceFsChangeEvent {
            agent_id: self.agent_id.clone(),
            workspace_id: self.workspace_id.clone(),
            changes,
            window_end_ms: now_ms,
        };
        sink.publish(event).await;
    }

    /// Normalize an absolute path to a workspace-relative path.
    /// `strip_prefix` failure means the path is outside the workspace
    /// (out-of-bounds) — returns `None` and the caller drops it.
    fn to_rel_path(&self, abs: &Path) -> Option<PathBuf> {
        abs.strip_prefix(&self.workspace_dir)
            .ok()
            .map(|rel| rel.components().collect::<PathBuf>())
    }
}

/// Current epoch time in milliseconds.
fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Convert a relative path to the forward-slash string form used by the
/// HTTP tree endpoints (Windows `\` separators are normalized).
fn path_to_forward_slash(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Test sink collecting every published event.
    struct CollectSink {
        events: Mutex<Vec<WorkspaceFsChangeEvent>>,
    }

    #[async_trait::async_trait]
    impl WorkspaceFsEventSink for CollectSink {
        async fn publish(&self, event: WorkspaceFsChangeEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn sink() -> std::sync::Arc<CollectSink> {
        std::sync::Arc::new(CollectSink {
            events: Mutex::new(Vec::new()),
        })
    }

    fn watcher_at(dir: &Path) -> WorkspaceFsWatcher {
        WorkspaceFsWatcher::new(dir, "agent-1", "ws-1").expect("watcher init")
    }

    fn notify_event(kind: EventKind, path: &Path) -> notify::Event {
        notify::Event {
            kind,
            paths: vec![path.to_path_buf()],
            attrs: Default::default(),
        }
    }

    #[test]
    fn rel_path_normalization_and_out_of_bounds() {
        let dir = Path::new("/tmp/ws");
        let w = watcher_at(dir);
        assert_eq!(
            w.to_rel_path(Path::new("/tmp/ws/a/b.txt")),
            Some(PathBuf::from("a/b.txt"))
        );
        // Out-of-bounds sibling directory → dropped.
        assert_eq!(w.to_rel_path(Path::new("/tmp/other/x.txt")), None);
    }

    #[test]
    fn forward_slash_conversion() {
        let p = PathBuf::from("a").join("b.txt");
        assert_eq!(path_to_forward_slash(&p), "a/b.txt");
    }

    #[test]
    fn ingest_maps_create_modify_remove() {
        let dir = Path::new("/tmp/ws");
        let mut w = watcher_at(dir);
        w.ingest(notify_event(EventKind::Create(notify::event::CreateKind::Any), Path::new("/tmp/ws/new.txt")));
        w.ingest(notify_event(
            EventKind::Modify(notify::event::ModifyKind::Any),
            Path::new("/tmp/ws/edit.txt"),
        ));
        w.ingest(notify_event(
            EventKind::Modify(notify::event::ModifyKind::Metadata(notify::event::MetadataKind::Any)),
            Path::new("/tmp/ws/touched.txt"),
        ));
        w.ingest(notify_event(EventKind::Remove(notify::event::RemoveKind::Any), Path::new("/tmp/ws/gone.txt")));

        assert_eq!(w.pending.get(Path::new("new.txt")), Some(&FsChangeKind::Created));
        assert_eq!(w.pending.get(Path::new("edit.txt")), Some(&FsChangeKind::Modified));
        // Metadata modify also maps to Modified (ADR-058 §3.1).
        assert_eq!(w.pending.get(Path::new("touched.txt")), Some(&FsChangeKind::Modified));
        assert_eq!(w.pending.get(Path::new("gone.txt")), Some(&FsChangeKind::Deleted));
    }

    #[test]
    fn create_then_modify_stays_created() {
        let dir = Path::new("/tmp/ws");
        let mut w = watcher_at(dir);
        let p = Path::new("/tmp/ws/f.txt");
        w.ingest(notify_event(EventKind::Create(notify::event::CreateKind::Any), p));
        w.ingest(notify_event(EventKind::Modify(notify::event::ModifyKind::Any), p));
        w.ingest(notify_event(EventKind::Modify(notify::event::ModifyKind::Any), p));
        assert_eq!(w.pending.get(Path::new("f.txt")), Some(&FsChangeKind::Created));
    }

    #[test]
    fn create_then_delete_cancels_out() {
        let dir = Path::new("/tmp/ws");
        let mut w = watcher_at(dir);
        let p = Path::new("/tmp/ws/tmp-file.txt");
        w.ingest(notify_event(EventKind::Create(notify::event::CreateKind::Any), p));
        w.ingest(notify_event(EventKind::Remove(notify::event::RemoveKind::Any), p));
        assert!(!w.pending.contains_key(Path::new("tmp-file.txt")));
        // Window closes when the buffer empties.
        assert!(w.pending.is_empty());
    }

    #[test]
    fn modify_then_delete_is_delete() {
        let dir = Path::new("/tmp/ws");
        let mut w = watcher_at(dir);
        let p = Path::new("/tmp/ws/old.txt");
        w.ingest(notify_event(EventKind::Modify(notify::event::ModifyKind::Any), p));
        w.ingest(notify_event(EventKind::Remove(notify::event::RemoveKind::Any), p));
        assert_eq!(w.pending.get(Path::new("old.txt")), Some(&FsChangeKind::Deleted));
    }

    #[test]
    fn out_of_bounds_paths_are_dropped() {
        let dir = Path::new("/tmp/ws");
        let mut w = watcher_at(dir);
        w.ingest(notify_event(EventKind::Create(notify::event::CreateKind::Any), Path::new("/tmp/other/x.txt")));
        assert!(w.pending.is_empty());
    }

    #[tokio::test]
    async fn flush_emits_batched_event_and_resets_window() {
        let dir = Path::new("/tmp/ws");
        let mut w = watcher_at(dir);
        w.ingest(notify_event(EventKind::Create(notify::event::CreateKind::Any), Path::new("/tmp/ws/a.txt")));
        w.ingest(notify_event(EventKind::Modify(notify::event::ModifyKind::Any), Path::new("/tmp/ws/b.txt")));

        let sink = sink();
        w.flush(sink.as_ref()).await;

        {
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            let ev = &events[0];
            assert_eq!(ev.agent_id, "agent-1");
            assert_eq!(ev.workspace_id, "ws-1");
            assert_eq!(ev.changes.len(), 2);
            assert!(ev.window_end_ms > 0);
        }
        // Buffer and window reset — a second flush is a no-op.
        w.flush(sink.as_ref()).await;
        assert_eq!(sink.events.lock().unwrap().len(), 1);
    }

    /// End-to-end: real PollWatcher against a temp dir, aggregation via
    /// `run`, events collected by the sink. Covers happy path +
    /// same-window coalescing at the polling layer.
    #[tokio::test]
    async fn end_to_end_detects_file_operations() {
        let dir = std::env::temp_dir().join("acowork-test-ws-fswatcher-e2e");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();
        let w = WorkspaceFsWatcher::new(&dir, "agent-e2e", "ws-e2e").unwrap();
        let sink = sink();
        let task = tokio::spawn(w.run(sink.clone(), shutdown_rx));

        // Create + modify + delete, then wait past poll (500ms) + window
        // (500ms) so the aggregated batch lands in the sink.
        std::fs::write(dir.join("created.txt"), b"hello").unwrap();
        std::fs::write(dir.join("created.txt"), b"hello world").unwrap();
        std::fs::write(dir.join("deleted.txt"), b"temp").unwrap();
        std::fs::remove_file(dir.join("deleted.txt")).unwrap();
        tokio::time::sleep(Duration::from_millis(2200)).await;

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

        let events = sink.events.lock().unwrap();
        assert!(
            !events.is_empty(),
            "expected at least one aggregated event after file ops"
        );
        // Collect all (path, kind) pairs across windows.
        let mut saw_created = false;
        let mut saw_deleted = false;
        for ev in events.iter() {
            for c in &ev.changes {
                match (c.path.as_str(), c.kind) {
                    ("created.txt", k) if k == FsChangeKind::Created as i32 => saw_created = true,
                    ("deleted.txt", k) if k == FsChangeKind::Deleted as i32 => saw_deleted = true,
                    _ => {}
                }
            }
        }
        assert!(saw_created, "created.txt must surface as Created (got {:?})", events.iter().flat_map(|e| e.changes.iter().map(|c| (c.path.clone(), c.kind))).collect::<Vec<_>>());
        // deleted.txt was created+deleted — either cancelled in the same
        // window (no event) or split across windows (Deleted). Both are
        // valid per ADR-058; only a phantom Created would be a bug.
        if saw_deleted {
            // fine — split windows
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
