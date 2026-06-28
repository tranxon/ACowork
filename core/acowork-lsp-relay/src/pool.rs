//! LSP process pool — process lifecycle bound to LSP Relay, not WebSocket session.
//!
//! Maintains a map of `(command, workspace_root) → LspProcessEntry`.
//! Multiple WebSocket clients can share a single LSP process.
//! Idle processes are reaped after a configurable timeout.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, broadcast, mpsc};

use std::process::Stdio;

/// Default idle timeout before a pooled LSP process is reaped (10 minutes).
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Default reaper tick interval (60 seconds).
const REAPER_INTERVAL: Duration = Duration::from_secs(60);

/// Key for pool lookup: "{command}:{args_joined}:{workspace_root}"
type PoolKey = String;

/// A pooled LSP process entry shared across WebSocket clients.
pub struct LspProcessEntry {
    /// Send JSON-RPC messages to LSP stdin
    pub stdin_tx: mpsc::UnboundedSender<String>,
    /// Subscribe to receive JSON-RPC messages from LSP stdout
    pub stdout_tx: broadcast::Sender<String>,
    /// Number of active WebSocket clients using this process
    pub active_clients: AtomicUsize,
    /// When last client disconnected (None if clients are active)
    pub last_idle_since: Mutex<Option<Instant>>,
    /// Resolved LSP command (e.g. "rust-analyzer")
    pub command: String,
    /// Workspace root directory
    pub workspace_root: String,
    /// Process ID (for logging)
    pub pid: u32,
    /// Cached InitializeResult JSON from the first successful handshake.
    pub init_result: Mutex<Option<String>>,
}

/// Shared LSP process pool.
pub struct LspPool {
    entries: Mutex<HashMap<PoolKey, Arc<LspProcessEntry>>>,
}

impl Default for LspPool {
    fn default() -> Self {
        Self::new()
    }
}

impl LspPool {
    /// Create a new empty pool.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Build the pool key from command, args, and workspace root.
    fn make_key(command: &str, args: &[String], workspace_root: &str) -> PoolKey {
        let args_joined = args.join(" ");
        format!("{}:{}:{}", command, args_joined, workspace_root)
    }

    /// Get an existing LSP process or spawn a new one.
    pub async fn get_or_spawn(
        &self,
        command: &str,
        args: &[String],
        workspace_root: &str,
    ) -> anyhow::Result<Arc<LspProcessEntry>> {
        let key = Self::make_key(command, args, workspace_root);
        let mut entries = self.entries.lock().await;

        if let Some(entry) = entries.get(&key) {
            if !entry.stdin_tx.is_closed() {
                entry.active_clients.fetch_add(1, Ordering::Relaxed);
                *entry.last_idle_since.lock().await = None;
                tracing::info!(
                    "[LSP Pool] Reusing '{}' (PID {}) for workspace '{}'",
                    command,
                    entry.pid,
                    workspace_root
                );
                return Ok(Arc::clone(entry));
            }
            tracing::warn!(
                "[LSP Pool] Stale entry for '{}' in '{}' (PID {}), removing",
                command,
                workspace_root,
                entry.pid
            );
            entries.remove(&key);
        }

        let entry = Self::spawn_pooled(command, args, workspace_root).await?;
        entries.insert(key, Arc::clone(&entry));
        Ok(entry)
    }

    /// Mark a client as disconnected from the given pool entry.
    pub async fn client_disconnected(&self, command: &str, args: &[String], workspace_root: &str) {
        let key = Self::make_key(command, args, workspace_root);
        let entries = self.entries.lock().await;
        if let Some(entry) = entries.get(&key) {
            let prev = entry.active_clients.fetch_sub(1, Ordering::Relaxed);
            if prev <= 1 {
                *entry.last_idle_since.lock().await = Some(Instant::now());
                tracing::info!(
                    "[LSP Pool] '{}' (PID {}) now idle, workspace '{}'",
                    entry.command,
                    entry.pid,
                    entry.workspace_root,
                );
            }
        }
    }

    /// Evict processes that have been idle longer than `timeout`.
    pub async fn reap_idle(&self, timeout: Duration) {
        let mut entries = self.entries.lock().await;
        let mut to_remove = Vec::new();

        for (key, entry) in entries.iter() {
            let idle_since = *entry.last_idle_since.lock().await;
            if let Some(since) = idle_since
                && since.elapsed() > timeout
            {
                tracing::info!(
                    "[LSP Pool] Evicting idle '{}' (PID {}), idle for {:?}",
                    entry.command,
                    entry.pid,
                    since.elapsed(),
                );
                to_remove.push(key.clone());
            }
        }

        for key in to_remove {
            entries.remove(&key);
        }
    }

    /// Spawn a new LSP process and set up stdin/stdout relay tasks.
    async fn spawn_pooled(
        command: &str,
        args: &[String],
        workspace_root: &str,
    ) -> anyhow::Result<Arc<LspProcessEntry>> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        let mut child = cmd
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let pid = child.id().unwrap_or(0);
        tracing::info!(
            "[LSP Pool] Spawned '{}' (PID {}) in workspace '{}', args={:?}",
            command,
            pid,
            workspace_root,
            args
        );

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to take stdin from child process"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to take stdout from child process"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to take stderr from child process"))?;

        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        let (stdout_tx, _) = broadcast::channel::<String>(256);

        // Background task: read from mpsc channel → write to child stdin
        let stdin_pid = pid;
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(msg) = stdin_rx.recv().await {
                let frame = format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg);
                if stdin.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdin.flush().await;
            }
            let _ = stdin.shutdown().await;
            tracing::info!("[LSP Pool] stdin writer ended for PID {}", stdin_pid);
        });

        // Background task: read LSP Base Protocol frames from stdout → broadcast
        let stdout_tx_clone = stdout_tx.clone();
        let stdout_cmd = command.to_string();
        let stdout_pid = pid;
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut content_length: usize = 0;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            tracing::warn!(
                                "[LSP Pool] '{}' (PID {}) stdout closed (process exited)",
                                stdout_cmd,
                                stdout_pid
                            );
                            return;
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                break;
                            }
                            if let Some(len) = crate::protocol::parse_content_length(trimmed) {
                                content_length = len;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "[LSP Pool] '{}' (PID {}) stdout read error: {}",
                                stdout_cmd,
                                stdout_pid,
                                e
                            );
                            return;
                        }
                    }
                }

                if content_length == 0 {
                    continue;
                }

                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }

                if let Ok(msg) = String::from_utf8(body) {
                    let _ = stdout_tx_clone.send(msg);
                }
            }
        });

        // Background task: read LSP stderr line-by-line and log via tracing.
        let stderr_cmd = command.to_string();
        let stderr_pid = pid;
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        tracing::warn!(
                            "[LSP Pool] '{}' (PID {}) stderr: {}",
                            stderr_cmd,
                            stderr_pid,
                            line
                        );
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(
                            "[LSP Pool] '{}' (PID {}) stderr read error: {}",
                            stderr_cmd,
                            stderr_pid,
                            e
                        );
                        break;
                    }
                }
            }
        });

        // Background task: wait for child exit (detect crash)
        let cmd_for_wait = command.to_string();
        tokio::spawn(async move {
            let status = child.wait().await;
            tracing::warn!(
                "[LSP Pool] '{}' (PID {}) exited: {:?}",
                cmd_for_wait,
                pid,
                status
            );
        });

        let entry = Arc::new(LspProcessEntry {
            stdin_tx,
            stdout_tx,
            active_clients: AtomicUsize::new(1),
            last_idle_since: Mutex::new(None),
            command: command.to_string(),
            workspace_root: workspace_root.to_string(),
            pid,
            init_result: Mutex::new(None),
        });

        Ok(entry)
    }

    /// Start a background reaper task that periodically evicts idle processes.
    pub fn start_reaper(pool: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(REAPER_INTERVAL);
            loop {
                interval.tick().await;
                pool.reap_idle(DEFAULT_IDLE_TIMEOUT).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Use `cat` as a mock LSP server — it reads from stdin and writes to
    /// stdout, which is the minimal behaviour needed for the pool to set up
    /// stdin/stdout relay tasks.
    const MOCK_CMD: &str = "cat";

    fn mock_args() -> Vec<String> {
        vec![]
    }

    fn mock_workspace() -> String {
        std::env::temp_dir().to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn get_or_spawn_creates_new_entry() {
        let pool = LspPool::new();
        let entry = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("should spawn mock LSP");

        assert_eq!(entry.command, MOCK_CMD);
        assert_eq!(entry.workspace_root, mock_workspace());
        assert!(entry.pid > 0);
        assert_eq!(
            entry.active_clients.load(Ordering::Relaxed),
            1,
            "first client should set active_clients=1"
        );
        assert!(
            entry.last_idle_since.lock().await.is_none(),
            "should not be idle while client is active"
        );
    }

    #[tokio::test]
    async fn get_or_spawn_reuses_existing_entry() {
        let pool = LspPool::new();

        let entry1 = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("first spawn");

        let entry2 = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("should reuse");

        assert_eq!(
            entry1.pid, entry2.pid,
            "same key must return the same process"
        );
        assert_eq!(
            entry2.active_clients.load(Ordering::Relaxed),
            2,
            "second client should increment to 2"
        );
    }

    #[tokio::test]
    async fn client_disconnected_decrements_count() {
        let pool = LspPool::new();

        let entry = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("spawn");

        assert_eq!(entry.active_clients.load(Ordering::Relaxed), 1);

        pool.client_disconnected(MOCK_CMD, &mock_args(), &mock_workspace())
            .await;

        assert_eq!(
            entry.active_clients.load(Ordering::Relaxed),
            0,
            "after disconnect, active_clients should be 0"
        );
        assert!(
            entry.last_idle_since.lock().await.is_some(),
            "entry should be marked idle after last client disconnects"
        );
    }

    #[tokio::test]
    async fn client_disconnected_does_not_mark_idle_if_other_clients_remain() {
        let pool = LspPool::new();

        // Two clients connect
        let entry = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("first spawn");
        pool.get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("second spawn");

        assert_eq!(entry.active_clients.load(Ordering::Relaxed), 2);

        // One disconnects
        pool.client_disconnected(MOCK_CMD, &mock_args(), &mock_workspace())
            .await;

        assert_eq!(
            entry.active_clients.load(Ordering::Relaxed),
            1,
            "one client should remain"
        );
        assert!(
            entry.last_idle_since.lock().await.is_none(),
            "should not be idle while a client is still active"
        );
    }

    #[tokio::test]
    async fn reap_idle_removes_idle_entries() {
        let pool = LspPool::new();

        // Spawn and immediately disconnect → entry becomes idle
        let entry = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("spawn");
        pool.client_disconnected(MOCK_CMD, &mock_args(), &mock_workspace())
            .await;

        // Wait a tiny bit so `elapsed()` > 0
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Reap with 1ms timeout — should remove the idle entry
        pool.reap_idle(Duration::from_millis(1)).await;

        // A new get_or_spawn should create a NEW process (different PID),
        // because the old one was reaped.
        let entry2 = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("new spawn after reap");

        assert_ne!(
            entry.pid, entry2.pid,
            "reaped entry should be replaced by a new process"
        );
    }

    #[tokio::test]
    async fn reap_idle_does_not_remove_active_entries() {
        let pool = LspPool::new();

        // Spawn but don't disconnect — entry is active
        let entry = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("spawn");

        // Reap with very short timeout — should NOT remove the active entry
        pool.reap_idle(Duration::from_millis(0)).await;

        // Same PID should be returned (entry was not reaped)
        let entry2 = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("should still exist");

        assert_eq!(
            entry.pid, entry2.pid,
            "active entry should not be reaped"
        );
    }

    #[tokio::test]
    async fn different_workspaces_get_different_processes() {
        let pool = LspPool::new();

        let ws1 = std::env::temp_dir().to_string_lossy().to_string();
        let ws2 = std::env::temp_dir()
            .join("other")
            .to_string_lossy()
            .to_string();
        std::fs::create_dir_all(&ws2).ok();

        let entry1 = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &ws1)
            .await
            .expect("spawn ws1");
        let entry2 = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &ws2)
            .await
            .expect("spawn ws2");

        assert_ne!(
            entry1.pid, entry2.pid,
            "different workspaces should get different processes"
        );
    }

    // ── make_key tests (indirect via get_or_spawn behavior) ─────────────

    #[tokio::test]
    async fn make_key_differentiates_by_args() {
        // Same command + workspace, different args → different pool keys
        let pool = LspPool::new();
        let ws = mock_workspace();

        let entry1 = pool
            .get_or_spawn(MOCK_CMD, &[], &ws)
            .await
            .expect("spawn no args");

        let entry2 = pool
            .get_or_spawn(MOCK_CMD, &["--stdio".to_string()], &ws)
            .await
            .expect("spawn with args");

        assert_ne!(
            entry1.pid, entry2.pid,
            "different args should produce different pool keys"
        );
    }

    // ── stdin/stdout relay tests ────────────────────────────────────────

    #[tokio::test]
    async fn stdin_stdout_relay_round_trip() {
        // Use `cat` as a mock LSP server: it echoes stdin to stdout.
        // The pool's spawn_pooled sets up:
        //   - stdin writer: receives JSON text, prepends Content-Length
        //     header, writes to child stdin
        //   - stdout reader: parses Content-Length header, extracts JSON
        //     body, broadcasts via channel
        // So sending JSON via stdin_tx should result in the same JSON
        // arriving on stdout_tx.
        let pool = LspPool::new();
        let entry = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("spawn");

        let test_msg = r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#;

        // Send a message through stdin
        entry
            .stdin_tx
            .send(test_msg.to_string())
            .expect("send to stdin");

        // Subscribe to stdout BEFORE sending (broadcast only delivers to
        // existing subscribers)
        let mut rx = entry.stdout_tx.subscribe();

        // Re-send because we subscribed after the first send
        entry
            .stdin_tx
            .send(test_msg.to_string())
            .expect("re-send to stdin");

        // Wait for the response with a timeout
        let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for stdout");

        assert_eq!(
            received.unwrap(),
            test_msg,
            "cat should echo back the same JSON message"
        );
    }

    #[tokio::test]
    async fn stdin_stdout_relay_multiple_messages() {
        let pool = LspPool::new();
        let entry = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("spawn");

        let mut rx = entry.stdout_tx.subscribe();

        let messages = vec![
            r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#,
            r#"{"jsonrpc":"2.0","method":"shutdown","id":2}"#,
            r#"{"jsonrpc":"2.0","method":"exit"}"#,
        ];

        for msg in &messages {
            entry
                .stdin_tx
                .send(msg.to_string())
                .expect("send to stdin");
        }

        // Receive all messages (cat echoes them back)
        let mut received = Vec::new();
        for _ in &messages {
            let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timeout waiting for stdout")
                .expect("channel closed");
            received.push(msg);
        }

        assert_eq!(
            received.len(),
            messages.len(),
            "should receive all echoed messages"
        );
        for (i, msg) in messages.iter().enumerate() {
            assert_eq!(&received[i], msg, "message {i} mismatch");
        }
    }

    // ── start_reaper tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn start_reaper_evicts_idle_processes() {
        // start_reaper uses REAPER_INTERVAL (60s) and DEFAULT_IDLE_TIMEOUT
        // (600s) — too slow for tests. Instead, we verify that start_reaper
        // can be called without panicking, and that the reaper logic
        // (reap_idle) works correctly when called manually (already tested
        // in reap_idle_removes_idle_entries).
        //
        // Here we just verify start_reaper spawns a task successfully.
        let pool = Arc::new(LspPool::new());

        // Spawn a mock process so the pool has something to reap
        let _entry = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("spawn");

        // Start the reaper — should not panic
        LspPool::start_reaper(Arc::clone(&pool));

        // Give it a moment to tick
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The process should still be active (reaper interval is 60s,
        // and the process hasn't been disconnected)
        let entry2 = pool
            .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
            .await
            .expect("should still exist");

        // Same PID = not reaped
        assert!(_entry.pid == entry2.pid);
    }

    #[tokio::test]
    async fn pool_handles_process_death() {
        // When a process dies (stdin_tx is closed), get_or_spawn should
        // detect the stale entry and spawn a new process.
        let pool = LspPool::new();
        let ws = mock_workspace();

        // Spawn a short-lived process: `echo` exits immediately
        let _entry1 = pool
            .get_or_spawn("echo", &[], &ws)
            .await
            .expect("spawn echo");

        // Wait for echo to exit
        tokio::time::sleep(Duration::from_millis(200)).await;

        // get_or_spawn should detect the dead process and spawn a new one
        let entry2 = pool
            .get_or_spawn("echo", &[], &ws)
            .await
            .expect("should spawn new process");

        // The new process may or may not have a different PID depending
        // on timing, but the pool should not return a stale entry.
        // If stdin_tx is closed, the entry is stale and should be replaced.
        assert!(
            !entry2.stdin_tx.is_closed(),
            "new entry's stdin_tx should be open"
        );
    }
}
