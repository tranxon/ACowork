//! Provider compatibility cache — learns (provider, model) quirks once, reuses forever.
//!
//! ## Background
//!
//! Many OpenAI-compatible providers (volcano-engine, deepseek, etc.) reject
//! requests with `400 Bad Request` when the client sends fields the provider's
//! schema does not understand (e.g. `thinking.type`, `parallel_tool_calls`,
//! `temperature`, etc.). Without a memory layer, our runtime previously ran
//! a multi-step fallback chain on **every** turn — every request paid 2–3x
//! the HTTP cost and latency, and the user still saw `400` until the chain
//! converged.
//!
//! ## Design (ADR-NNN — Provider Compatibility Cache)
//!
//! 1. **First request (cold start)**: runtime runs the legacy fallback chain.
//!    When a stripped variant succeeds, we persist *which* fields were
//!    stripped to `provider_compat.json` under the agent's `work_dir/config/`.
//! 2. **Subsequent requests**: we apply the cached `StripProfile` *before*
//!    the HTTP call, eliminating the fallback chain entirely.
//! 3. **Self-healing**: if a cached profile fails again (provider changed
//!    schema, hot update, etc.), we `invalidate()` the entry and re-probe.
//!
//! ## Persistence
//!
//! Path: `{work_dir}/config/provider_compat.json`
//!
//! Atomic write via `*.tmp` → `rename` so a crash mid-write cannot corrupt
//! the cache.  Writes are async (`tokio::spawn`) so they never block the
//! streaming response.
//!
//! ## Key format
//!
//! `"{provider_id}::{model_id}"` — keeps different vendors with the same
//! underlying model (e.g. `glm-5.2` on volcano-engine vs. a self-hosted
//! OpenAI-compatible wrapper) isolated.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Describes which fields to strip from a request before sending it.
///
/// One entry per `(provider_id, model_id)` pair.  A `true` flag means
/// "do not send this field at all" — the runtime will set it to `None`
/// (or omit it from the JSON body) before serializing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StripProfile {
    /// Drop the `stream_options` object (some providers reject `include_usage`).
    pub strip_stream_options: bool,
    /// Drop the `reasoning_effort` field (o-series / MiniMax only — incompatible with non-OpenAI).
    pub strip_reasoning_effort: bool,
    /// Drop the `thinking.type` field (GLM 5.x custom field, often rejected by older deployments).
    pub strip_thinking: bool,
    /// Drop the `temperature` field (some providers require it absent rather than `null`).
    pub strip_temperature: bool,
    /// Drop the entire `tools` array (last resort — disables function calling).
    pub strip_tools: bool,
    /// Cap `max_tokens` to this value if originally larger (`None` = no cap).
    pub max_tokens_cap: Option<u32>,
    /// Which fallback generation succeeded (1=FB1, 2=FB2, …).  Diagnostic only.
    pub fallback_generation: u32,
    /// Unix timestamp (seconds) of the last successful use of this profile.
    pub last_success_unix_ts: u64,
}

impl StripProfile {
    /// Empty profile — nothing stripped.  Useful as a default.
    pub fn empty() -> Self {
        Self {
            strip_stream_options: false,
            strip_reasoning_effort: false,
            strip_thinking: false,
            strip_temperature: false,
            strip_tools: false,
            max_tokens_cap: None,
            fallback_generation: 0,
            last_success_unix_ts: now_unix_ts(),
        }
    }
}

/// In-memory + on-disk cache of `StripProfile` keyed by `"provider_id::model_id"`.
///
/// Thread-safe via `parking_lot::RwLock`.  Hot-path reads (`get`) never block
/// on the lock for long — at most a single `HashMap::get`.
pub struct CompatCache {
    inner: RwLock<HashMap<String, StripProfile>>,
    persist_path: PathBuf,
    /// Serializes concurrent `write_atomic` calls so that two spawned
    /// persist tasks never race on the same `*.tmp` file.
    persist_lock: Arc<tokio::sync::Mutex<()>>,
}

impl CompatCache {
    /// Load cache from disk.  Missing file → empty cache.
    /// Malformed file → log warning, empty cache (does not panic).
    pub fn load(persist_path: PathBuf) -> Arc<Self> {
        let entries = match fs::read_to_string(&persist_path) {
            Ok(contents) => match serde_json::from_str::<HashMap<String, StripProfile>>(&contents) {
                Ok(map) => map,
                Err(e) => {
                    warn!(
                        path = %persist_path.display(),
                        error = %e,
                        "provider_compat.json is malformed — starting with empty cache",
                    );
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                debug!(
                    path = %persist_path.display(),
                    "provider_compat.json not found — starting with empty cache",
                );
                HashMap::new()
            }
            Err(e) => {
                warn!(
                    path = %persist_path.display(),
                    error = %e,
                    "failed to read provider_compat.json — starting with empty cache",
                );
                HashMap::new()
            }
        };

        debug!(
            path = %persist_path.display(),
            entries = entries.len(),
            "CompatCache loaded",
        );

        Arc::new(Self {
            inner: RwLock::new(entries),
            persist_path,
            persist_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Hot-path read.  Returns `None` if no profile recorded for this key.
    ///
    /// Cheap: a single `RwLock::read` + `HashMap::get`.  No await.
    pub fn get(&self, key: &str) -> Option<StripProfile> {
        self.inner.read().get(key).cloned()
    }

    /// Record a successful fallback.  Updates memory synchronously, then
    /// spawns an async task to persist.
    pub fn record_success(&self, key: String, profile: StripProfile) {
        let mut profile = profile;
        profile.last_success_unix_ts = now_unix_ts();
        {
            let mut guard = self.inner.write();
            guard.insert(key.clone(), profile.clone());
        }
        debug!(
            key = %key,
            fallback_generation = profile.fallback_generation,
            "CompatCache: recorded successful fallback",
        );
        self.persist_async();
    }

    /// Drop a cached entry.  Next request will re-probe via the fallback chain.
    pub fn invalidate(&self, key: &str) {
        {
            let mut guard = self.inner.write();
            if guard.remove(key).is_some() {
                warn!(
                    key = %key,
                    "CompatCache: invalidated cached profile — provider behavior changed",
                );
            }
        }
        self.persist_async();
    }

    /// Snapshot the in-memory cache.  Used by tests and async persist.
    fn snapshot(&self) -> HashMap<String, StripProfile> {
        self.inner.read().clone()
    }

    /// Spawn an async task to write the cache to disk.  Atomic via tmp+rename.
    /// A `Mutex` serializes concurrent writes so two spawned tasks never
    /// race on the same `*.tmp` file.
    fn persist_async(&self) {
        let path = self.persist_path.clone();
        let snapshot = self.snapshot();
        let lock = self.persist_lock.clone();
        tokio::spawn(async move {
            // Serialize concurrent persist tasks.  The lock is held for
            // the duration of the write+rename so a later task always
            // sees a clean tmp file.
            let _guard = lock.lock().await;
            if let Err(e) = write_atomic(&path, &snapshot).await {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "CompatCache: failed to persist to disk",
                );
            }
        });
    }
}

/// Atomic write helper — write to `*.tmp`, then `rename` over target.
async fn write_atomic(
    path: &Path,
    entries: &HashMap<String, StripProfile>,
) -> io::Result<()> {
    // Serialize synchronously — JSON is small (~few KB at most).
    let serialized = serde_json::to_string_pretty(entries)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let tmp_path = path.with_extension("json.tmp");

    // Off-thread blocking I/O to avoid stalling the runtime.
    let path_owned = path.to_path_buf();
    let tmp_owned = tmp_path.clone();
    let result: io::Result<()> = tokio::task::spawn_blocking(move || -> io::Result<()> {
        if let Some(parent) = path_owned.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&tmp_owned, serialized.as_bytes())?;
        fs::rename(&tmp_owned, &path_owned)?;
        Ok(())
    })
    .await
    .map_err(|e| io::Error::other(format!("join error: {e}")))?;

    result
}

fn now_unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = env::temp_dir();
        p.push(format!(
            "acowork_compat_test_{}_{}_{}.json",
            name,
            std::process::id(),
            now_unix_ts()
        ));
        let _ = fs::remove_file(&p);
        p
    }

    fn sample_profile() -> StripProfile {
        StripProfile {
            strip_stream_options: true,
            strip_reasoning_effort: false,
            strip_thinking: true,
            strip_temperature: false,
            strip_tools: false,
            max_tokens_cap: Some(8192),
            fallback_generation: 3,
            last_success_unix_ts: 1234567890,
        }
    }

    #[tokio::test]
    async fn load_missing_file_yields_empty_cache() {
        let path = tmp_path("missing");
        let cache = CompatCache::load(path.clone());
        assert!(cache.get("volcano-engine-coding::glm-5.2").is_none());
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn record_then_get_roundtrip() {
        let path = tmp_path("roundtrip");
        let cache = CompatCache::load(path.clone());
        let key = "volcano-engine-coding::glm-5.2".to_string();
        cache.record_success(key.clone(), sample_profile());
        // Synchronous in-memory read should hit immediately.
        let got = cache.get(&key).expect("profile should be in cache");
        assert!(got.strip_stream_options);
        assert!(got.strip_thinking);
        assert_eq!(got.max_tokens_cap, Some(8192));
        assert_eq!(got.fallback_generation, 3);
        // last_success_unix_ts should have been refreshed.
        assert!(got.last_success_unix_ts > 1234567890);
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn invalidate_removes_entry() {
        let path = tmp_path("invalidate");
        let cache = CompatCache::load(path.clone());
        let key = "volcano-engine-coding::glm-5.2".to_string();
        cache.record_success(key.clone(), sample_profile());
        assert!(cache.get(&key).is_some());
        cache.invalidate(&key);
        assert!(cache.get(&key).is_none());
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn persist_then_load_roundtrip() {
        let path = tmp_path("persist");
        // Persist a known snapshot directly (bypasses record_success's
        // async spawn to avoid racing the test's own write_atomic).
        let mut snap = HashMap::new();
        snap.insert(
            "volcano-engine-coding::glm-5.2".to_string(),
            sample_profile(),
        );
        write_atomic(&path, &snap).await.unwrap();

        // Second cache: load from disk.
        let cache2 = CompatCache::load(path.clone());
        let got = cache2
            .get("volcano-engine-coding::glm-5.2")
            .expect("profile should be reloaded");
        assert!(got.strip_stream_options);
        assert!(got.strip_thinking);
        assert_eq!(got.max_tokens_cap, Some(8192));
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn malformed_file_yields_empty_cache() {
        let path = tmp_path("malformed");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, "{ not valid json").unwrap();
        let cache = CompatCache::load(path.clone());
        assert!(cache.get("any::key").is_none());
        let _ = fs::remove_file(&path);
    }
}