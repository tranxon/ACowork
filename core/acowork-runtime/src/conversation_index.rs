//! Conversation vector index (ADR-081 §4.2, P1-2).
//!
//! Reuses `grafeo-engine` via [`GrafeoStore`] in a dedicated store file
//! `{work_dir}/conversation_index/` — one node per indexed message with
//! `session_id`, `message_index` (the JSONL line number), `role`,
//! `content`, `embedding`. Physically isolated from the memory store:
//! the directory can be deleted and rebuilt from the JSONL history at
//! any time (ADR-081 "索引目录独立，可删重建；降级关键词").
//!
//! The JSONL conversation log is append-only (compaction appends a
//! `kind="compaction"` marker — never truncates), so the JSONL line
//! number is a stable message index and a per-session watermark is all
//! the incremental state we need. [`ConversationIndexer`] tails the
//! files: cold-start scan on first run, then a periodic delta scan.
//! Only `user`/`assistant` messages are indexed — tool calls, thoughts,
//! system nudges and compaction summaries are dialogue noise for search.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use grafeo_common::types::Value;

use acowork_grafeo::grafeo::GrafeoStore;
use acowork_grafeo::types::GrafeoConfig;

use crate::conversation::ConversationEntry;
use crate::error::Result;

/// Label for every indexed conversation-message node.
const LABEL: &str = "ConversationMessage";
/// Content is truncated to this many chars before embedding, bounding
/// embedding cost for very long messages (the stored snippet is the
/// truncated form, matching what was embedded).
const MAX_INDEX_CONTENT: usize = 4_000;

/// One ranked conversation hit.
#[derive(Debug, Clone)]
pub struct ConversationHit {
    pub session_id: String,
    /// JSONL line number — also the session message index.
    pub message_index: usize,
    pub role: String,
    pub content: String,
    pub score: f64,
}

/// The conversation vector index + per-session watermark.
pub struct ConversationIndex {
    store: GrafeoStore,
    /// `{work_dir}/conversations` — the JSONL + meta source of truth the
    /// indexer tails and `/search` reads session titles from.
    conversations_dir: PathBuf,
    /// Embedding dimension the store (and its HNSW index) was opened with.
    /// Must match the live provider's dimension or vector writes panic —
    /// the indexer guards against a mismatch using this value.
    embedding_dim: usize,
    /// Per-session watermark: next JSONL line index to index.
    watermark: Mutex<HashMap<String, usize>>,
    /// True while the index is not yet caught up with the JSONL history.
    /// Surfaced by `/search` as the ADR-081 `indexing` flag.
    indexing: AtomicBool,
}

impl ConversationIndex {
    /// Open (or create) the conversation index under `work_dir`, sized for
    /// `embedding_dim` (the live provider's dimension — hardcoding the
    /// default 384 made every vector write mismatch a 512-dim provider).
    ///
    /// ponytail: if the provider's dimension later changes, the persisted
    /// HNSW index keeps the old dim and writes will mismatch. The store dir
    /// is self-healing (delete + restart, or POST /conversation/index/rebuild);
    /// an in-place dimension migration is not implemented since no store
    /// pre-dates this change.
    pub fn open(work_dir: &Path, embedding_dim: usize) -> Result<Self> {
        let store = GrafeoStore::open(&GrafeoConfig {
            db_path: work_dir.join("conversation_index"),
            embedding_dim,
        })
        .map_err(|e| crate::error::RuntimeError::Memory(e.to_string()))?;
        // The engine's `init_schema` only creates indexes for the four
        // memory labels — `ConversationMessage` needs its own BM25 +
        // HNSW indexes or every search comes back empty. Idempotent:
        // re-opening an existing store restores its indexes as-is.
        let hnsw = store.hnsw_config().clone();
        let _ = store.db().create_text_index(LABEL, "content");
        let _ = store.db().create_vector_index(
            LABEL,
            "embedding",
            Some(hnsw.dim),
            Some(acowork_grafeo::index_config::VECTOR_METRIC),
            Some(hnsw.m),
            Some(hnsw.ef_construction),
            None,
        );
        let index = Self {
            store,
            conversations_dir: work_dir.join("conversations"),
            embedding_dim,
            watermark: Mutex::new(HashMap::new()),
            indexing: AtomicBool::new(true),
        };
        // The node store is persistent but the watermark is in-memory, so a
        // Runtime restart would otherwise reset every session to line 0 and
        // re-index the whole JSONL on top of the surviving nodes — duplicate
        // rows, an index that grows without bound across restarts.
        // Reconcile from the store: rebuild the watermark from the highest
        // indexed line per session and drop the duplicates an earlier
        // restart already wrote.
        index.recover_watermarks();
        Ok(index)
    }

    /// Rebuild `watermark` from the persisted nodes and purge duplicate
    /// per-`(session_id, message_index)` rows. Called once from [`open`].
    fn recover_watermarks(&self) {
        let ids = self.store.db().graph_store().nodes_by_label(LABEL);
        let mut max_line: HashMap<String, usize> = HashMap::new();
        let mut seen: std::collections::HashSet<(String, i64)> = std::collections::HashSet::new();
        let mut purged = 0usize;
        for id in ids {
            let Some(n) = self.store.get_node(id) else {
                continue;
            };
            let (Some(sid), Some(line)) = (
                n.get_property("session_id")
                    .and_then(|v| v.as_str().map(str::to_string)),
                n.get_property("message_index").and_then(|v| v.as_int64()),
            ) else {
                // Node without the identifying props: unsearchable garbage.
                if self.store.delete_node(id).unwrap_or(false) {
                    purged += 1;
                }
                continue;
            };
            // Keep the first occurrence of a line, drop later duplicates.
            if !seen.insert((sid.clone(), line)) {
                if self.store.delete_node(id).unwrap_or(false) {
                    purged += 1;
                }
                continue;
            }
            let next = line.max(0) as usize + 1;
            let entry = max_line.entry(sid).or_insert(0);
            *entry = (*entry).max(next);
        }
        let sessions = max_line.len();
        *self.watermark.lock().unwrap() = max_line;
        if purged > 0 || sessions > 0 {
            tracing::info!(sessions, purged, "conversation index: watermarks recovered");
        }
    }

    /// Embedding dimension this index was opened with.
    pub fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    /// `{work_dir}/conversations` — where session JSONL + `meta/*.json`
    /// live. `/search` reads session titles from here for display.
    pub fn conversations_dir(&self) -> &Path {
        &self.conversations_dir
    }

    /// True while the indexer is still catching up with the JSONL logs.
    pub fn is_indexing(&self) -> bool {
        self.indexing.load(Ordering::Relaxed)
    }

    /// Set by the indexer after each sweep.
    pub fn set_indexing(&self, indexing: bool) {
        self.indexing.store(indexing, Ordering::Relaxed);
    }

    /// Index one message and advance this session's watermark past it.
    pub fn index_message(
        &self,
        session_id: &str,
        message_index: usize,
        role: &str,
        content: &str,
        embedding: &[f32],
    ) -> Result<()> {
        let truncated: String = content.chars().take(MAX_INDEX_CONTENT).collect();
        self.store
            .store_node(
                LABEL,
                [
                    ("session_id", Value::from(session_id)),
                    ("message_index", Value::from(message_index as i64)),
                    ("role", Value::from(role)),
                    ("content", Value::from(truncated)),
                    ("embedding", Value::Vector(std::sync::Arc::from(embedding))),
                ],
            )
            .map_err(|e| crate::error::RuntimeError::Memory(e.to_string()))?;
        self.mark_indexed(session_id, message_index + 1);
        Ok(())
    }

    /// Next JSONL line to index for `session_id` (0 = not started).
    pub fn next_line(&self, session_id: &str) -> usize {
        self.watermark
            .lock()
            .unwrap()
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    /// Record that lines `< next_line` are indexed for `session_id`.
    pub fn mark_indexed(&self, session_id: &str, next_line: usize) {
        self.watermark
            .lock()
            .unwrap()
            .insert(session_id.to_string(), next_line);
    }

    /// Remove every indexed message of a (deleted) session.
    pub fn remove_session(&self, session_id: &str) {
        let ids = self.store.db().graph_store().nodes_by_label(LABEL);
        let mut removed = 0usize;
        for id in ids {
            let is_match = self.store.get_node(id).is_some_and(|n| {
                n.get_property("session_id").and_then(|v| v.as_str()) == Some(session_id)
            });
            if is_match && self.store.delete_node(id).unwrap_or(false) {
                removed += 1;
            }
        }
        self.watermark.lock().unwrap().remove(session_id);
        tracing::info!(session_id, removed, "conversation index: purged session");
    }

    /// Reset the whole index: purge every message node and clear the
    /// per-session watermarks, then latch `indexing` so the next indexer
    /// sweep rebuilds from the JSONL history. The directory is
    /// append-only and re-derivable, so this is the self-heal path after
    /// corruption or an embedding-dimension change (ADR-081 "可删重建").
    pub fn rebuild(&self) {
        let ids = self.store.db().graph_store().nodes_by_label(LABEL);
        let mut purged = 0usize;
        for id in ids {
            if self.store.delete_node(id).unwrap_or(false) {
                purged += 1;
            }
        }
        self.watermark.lock().unwrap().clear();
        self.set_indexing(true);
        tracing::info!(purged, "conversation index: full rebuild scheduled");
    }

    /// Hybrid (or BM25-only when `embedding` is `None`) search over
    /// indexed messages, ranked by descending score.
    pub fn search(
        &self,
        query_text: &str,
        embedding: Option<&[f32]>,
        k: usize,
    ) -> Vec<ConversationHit> {
        let hits = match embedding {
            Some(emb) => self
                .store
                .hybrid_search(LABEL, "content", "embedding", query_text, emb, k),
            None => self.store.text_search(LABEL, query_text, k),
        };
        let hits = match hits {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "conversation index search failed");
                return vec![];
            }
        };
        hits.into_iter()
            .filter_map(|(id, score)| {
                let n = self.store.get_node(id)?;
                Some(ConversationHit {
                    session_id: n.get_property("session_id")?.as_str()?.to_string(),
                    message_index: n.get_property("message_index")?.as_int64()? as usize,
                    role: n.get_property("role")?.as_str()?.to_string(),
                    content: n.get_property("content")?.as_str()?.to_string(),
                    score,
                })
            })
            .collect()
    }
}

/// Whether a JSONL entry should be indexed (user/assistant dialogue only).
fn is_indexable(entry: &ConversationEntry) -> bool {
    entry.kind.as_deref() != Some(crate::conversation::ENTRY_KIND_COMPACTION)
        && (entry.role == "user" || entry.role == "assistant")
        && !entry.content.is_empty()
}

/// Background tailer: cold-start scans all `{work_dir}/conversations/*.jsonl`
/// then periodically picks up appended lines, embedding via the agent's
/// live embedding provider (read from the shared `AgentCore` slot each
/// cycle — no hard coupling to provider lifecycle).
pub struct ConversationIndexer {
    index: std::sync::Arc<ConversationIndex>,
    conversations_dir: PathBuf,
    agent_core: crate::http::SharedAgentCore,
}

impl ConversationIndexer {
    pub fn new(
        index: std::sync::Arc<ConversationIndex>,
        work_dir: &Path,
        agent_core: crate::http::SharedAgentCore,
    ) -> Self {
        Self {
            index,
            conversations_dir: work_dir.join("conversations"),
            agent_core,
        }
    }

    /// Run the tail loop forever. Re-reads the provider each cycle so an
    /// embedding sidecar that (dis)connects later is picked up; while no
    /// provider is bound, watermarks stay put and the index simply lags.
    pub async fn run(self: std::sync::Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            self.sweep().await;
        }
    }

    async fn sweep(&self) {
        let provider: Option<std::sync::Arc<dyn crate::embedding::EmbeddingProvider>> = {
            let guard = self.agent_core.read().ok();
            guard
                .as_ref()
                .and_then(|g| g.as_ref())
                .and_then(|c| c.embedding_provider.clone())
        };

        // Discover current JSONL sessions.
        let mut sessions: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.conversations_dir) {
            for e in rd.flatten() {
                if e.path().extension().is_some_and(|x| x == "jsonl")
                    && let Some(name) = e.file_name().to_str()
                    && let Some(sid) = name.strip_suffix(".jsonl")
                {
                    sessions.push(sid.to_string());
                }
            }
        }

        // Purge index entries whose JSONL file vanished (session deleted).
        let indexed: Vec<String> = self
            .index
            .watermark
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for sid in &indexed {
            if !sessions.contains(sid) {
                self.index.remove_session(sid);
            }
        }

        let mut found_pending = false;
        // Dimension guard: writing a vector whose length differs from the
        // store's HNSW dimension triggers the engine's dimension-mismatch
        // panic. Skip the whole sweep (watermarks stay put) until the
        // provider matches. The indexing flag stays latched `true` — the
        // `/search` response must keep telling the user results are
        // incomplete instead of silently going stale.
        if let Some(provider) = provider.as_deref() {
            let dim = provider.dimension();
            let index_dim = self.index.embedding_dim();
            if dim != index_dim {
                tracing::warn!(
                    provider_dim = dim,
                    index_dim,
                    "conversation index: provider dimension mismatch, deferring sweep"
                );
                self.index.set_indexing(true);
                return;
            }
        }
        for sid in &sessions {
            let path = self.conversations_dir.join(format!("{sid}.jsonl"));
            let total = crate::conversation::count_jsonl_lines(&path).unwrap_or(0);
            let next = self.index.next_line(sid);
            if next < total {
                found_pending = true;
            }
            if next >= total || provider.is_none() {
                continue;
            }
            self.index_session(sid, &path, next, total, provider.as_deref().unwrap())
                .await;
        }
        // Latch the flag: once a cycle finds nothing new, indexing is done.
        self.index.set_indexing(found_pending);
    }

    async fn index_session(
        &self,
        session_id: &str,
        path: &Path,
        mut next: usize,
        total: usize,
        provider: &dyn crate::embedding::EmbeddingProvider,
    ) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let lines: Vec<&str> = text.split('\n').collect();
        // Drop a trailing partial line (writer may be mid-append).
        let bound = if text.ends_with('\n') {
            lines.len()
        } else {
            lines.len().saturating_sub(1)
        };
        while next < bound.min(total) {
            let line = lines[next].trim();
            let entry: ConversationEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => {
                    // Non-JSON line (shouldn't happen): skip it, don't spin.
                    self.index.mark_indexed(session_id, next + 1);
                    next += 1;
                    continue;
                }
            };
            if !is_indexable(&entry) {
                self.index.mark_indexed(session_id, next + 1);
                next += 1;
                continue;
            }
            match provider.embed(&entry.content).await {
                Ok(emb) => {
                    if let Err(e) = self.index.index_message(
                        session_id,
                        next,
                        &entry.role,
                        &entry.content,
                        &emb,
                    ) {
                        tracing::warn!(session_id, line = next, error = %e, "conversation index: store write failed, skipping line");
                        self.index.mark_indexed(session_id, next + 1);
                    }
                }
                Err(e) => {
                    tracing::warn!(session_id, line = next, error = %e, "conversation index: embedding failed, skipping line");
                    self.index.mark_indexed(session_id, next + 1);
                }
            }
            next += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::ENTRY_KIND_COMPACTION;
    use acowork_memory::types::DEFAULT_EMBEDDING_DIM;

    fn open_tmp() -> (tempfile::TempDir, std::sync::Arc<ConversationIndex>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = std::sync::Arc::new(
            ConversationIndex::open(dir.path(), DEFAULT_EMBEDDING_DIM).expect("index open"),
        );
        (dir, index)
    }

    /// Deterministic unit-magnitude vector of the store's embedding dim.
    fn vec_dim(dim: usize, seed: u8) -> Vec<f32> {
        (0..dim)
            .map(|i| ((i as u8).wrapping_mul(seed) as f32) / 256.0)
            .collect()
    }

    fn entry(role: &str, content: &str, kind: Option<&str>) -> ConversationEntry {
        ConversationEntry {
            id: format!("{role}-{content}"),
            ts: "2025-01-01T00:00:00.000Z".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            metadata: None,
            kind: kind.map(String::from),
        }
    }

    #[test]
    fn is_indexable_filters_compaction_and_noise() {
        assert!(is_indexable(&entry("user", "hello", None)));
        assert!(is_indexable(&entry("assistant", "hi there", None)));
        // Dialogue noise is not indexed.
        assert!(!is_indexable(&entry("system", "nudge", None)));
        assert!(!is_indexable(&entry("thought", "internal", None)));
        assert!(!is_indexable(&entry("tool_result", "42", None)));
        // Compaction summaries are skipped even with a system role.
        assert!(!is_indexable(&entry("system", "summary", Some(ENTRY_KIND_COMPACTION))));
        // Empty content is skipped.
        assert!(!is_indexable(&entry("user", "", None)));
    }

    #[test]
    fn index_message_advances_watermark_and_is_searchable() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 3);

        index
            .index_message("s1", 0, "user", "the quick brown fox", &emb)
            .expect("index s1:0");
        index
            .index_message("s1", 1, "assistant", "jumps over the lazy dog", &emb)
            .expect("index s1:1");
        index
            .index_message("s2", 0, "user", "unrelated kitchen recipe", &emb)
            .expect("index s2:0");

        // Watermark advanced past every indexed line.
        assert_eq!(index.next_line("s1"), 2);
        assert_eq!(index.next_line("s2"), 1);

        // BM25 (no embedding) ranks the session that actually contains "fox".
        let hits = index.search("fox", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert_eq!(hits[0].message_index, 0);
        assert_eq!(hits[0].role, "user");
        assert_eq!(hits[0].content, "the quick brown fox");
        assert!(hits[0].score > 0.0);

        // "lazy dog" only matches s1:1.
        let hits = index.search("lazy dog", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_index, 1);
    }

    #[test]
    fn search_with_embedding_uses_hybrid_vector_path() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 7);
        index
            .index_message("s1", 0, "assistant", "rust borrow checker rules", &emb)
            .expect("index");

        // Same-dimension query embedding drives the hybrid path without panic.
        let hits = index.search("borrow checker", Some(&emb), 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn remove_session_purges_nodes_and_watermark() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 1);
        index.index_message("s1", 0, "user", "alpha beta", &emb).expect("s1");
        index.index_message("s2", 0, "user", "alpha gamma", &emb).expect("s2");

        index.remove_session("s1");

        assert_eq!(index.next_line("s1"), 0, "watermark reset for purged session");
        let hits = index.search("alpha", None, 10);
        assert_eq!(hits.len(), 1, "only the surviving session remains searchable");
        assert_eq!(hits[0].session_id, "s2");
    }

    #[test]
    fn content_is_truncated_to_index_cap() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 2);
        let long: String = "banana ".repeat(MAX_INDEX_CONTENT);
        index
            .index_message("s1", 0, "user", &long, &emb)
            .expect("index");
        let hits = index.search("banana", None, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content.chars().count(), MAX_INDEX_CONTENT);
    }

    #[test]
    fn rebuild_purges_nodes_resets_watermarks_and_latches_indexing() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 4);
        index.index_message("s1", 0, "user", "alpha beta", &emb).expect("s1");
        index.index_message("s2", 0, "user", "alpha gamma", &emb).expect("s2");
        index.set_indexing(false);

        index.rebuild();

        assert_eq!(index.next_line("s1"), 0, "watermark reset");
        assert_eq!(index.next_line("s2"), 0, "watermark reset");
        assert!(index.is_indexing(), "indexing latched true until caught up");
        assert!(
            index.search("alpha", None, 10).is_empty(),
            "all nodes purged — the tailer re-indexes from JSONL"
        );
    }

    #[test]
    fn open_honors_non_default_embedding_dim() {
        // Regression: the index used to open at the hardcoded default (384),
        // so a 512-dim provider (bge-small-zh-v1.5) mismatched every write
        // and the indexer deferred every sweep — search always came back
        // empty. The store must open at the dimension it is given.
        let dir = tempfile::tempdir().expect("tempdir");
        let index = ConversationIndex::open(dir.path(), 512).expect("index open");
        assert_eq!(index.embedding_dim(), 512);
        // And a 512-dim vector must be accepted (no dimension-mismatch panic).
        let emb = vec_dim(512, 9);
        index
            .index_message("s1", 0, "user", "dimension regression check", &emb)
            .expect("512-dim write accepted");
        assert_eq!(index.search("regression", None, 10).len(), 1);
    }

    #[test]
    fn reopen_recovers_watermark_without_duplicating() {
        // Regression: the watermark is in-memory but the nodes are on disk.
        // Reopening (a Runtime restart) used to reset every session to line 0,
        // so the tailer re-embedded the whole JSONL and wrote a second node
        // per message — duplicate rows, index growth on every restart.
        let dir = tempfile::tempdir().expect("tempdir");
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 5);
        {
            let idx = ConversationIndex::open(dir.path(), DEFAULT_EMBEDDING_DIM).expect("open1");
            idx.index_message("s1", 0, "user", "unique alpha marker", &emb).expect("i0");
            idx.index_message("s1", 1, "assistant", "unique beta marker", &emb).expect("i1");
            assert_eq!(idx.next_line("s1"), 2);
        }

        let idx2 = ConversationIndex::open(dir.path(), DEFAULT_EMBEDDING_DIM).expect("open2");
        // Watermark restored from the persisted nodes, not reset to 0.
        assert_eq!(idx2.next_line("s1"), 2, "watermark recovered on reopen");
        assert_eq!(
            idx2.search("alpha", None, 10).len(),
            1,
            "no duplicate rows after reopen"
        );

        // Re-indexing the same lines again must not duplicate (a restart that
        // raced a sweep): overwrite-by-key is not available, so `recover`
        // purges the strays on the NEXT open — verify the dedup path directly.
        idx2.index_message("s1", 0, "user", "unique alpha marker", &emb).expect("dup write");
        assert_eq!(idx2.search("alpha", None, 10).len(), 2, "duplicate present before recovery");
        let idx3 = ConversationIndex::open(dir.path(), DEFAULT_EMBEDDING_DIM).expect("open3");
        assert_eq!(idx3.search("alpha", None, 10).len(), 1, "recovery purges duplicates");
        assert_eq!(idx3.next_line("s1"), 2, "watermark stays correct after purge");
    }
}
