# acowork-grafeo — Memory Storage Engine

**Position**: Storage engine implementation for Agent private Memory. Grafeo implements the `MemoryStore` trait (defined in 05-memory.md §10), as the sole storage backend in Phase 1. Each Agent Runtime process embeds one GrafeoStore instance.

**v3.4 Change**: Refactored from "storage module directly called by Runtime" to "replaceable backend implementing MemoryStore trait". Runtime and MemoryManager only depend on the trait, not Grafeo's specific implementation.

**v3.5 Changes**:
- `graph_expand_hops` default value changed from 2 to 3, with early termination mechanism support
- Added `semantic/conflict.rs` (conflict candidate detection)
- Added `consolidation/conflict.rs` (conflict classification)
- Added `forgetting/purge_log.rs` (Purge recovery mechanism)
- Added `backup.rs` / `recovery.rs` (backup and recovery)
- Added `embedding/fallback.rs` (fallback chain) — deprecated, fallback logic moved up to Runtime layer `FallbackEmbeddingProvider`
- Added `vector/hnsw.rs` (HNSW parameter definitions: M=16, ef_construction=100, ef_search=64)

**v3.6 Changes (Current)**:
- Storage backend fully migrated from `rusqlite` to `grafeo-engine` (v0.5.39, crates.io)
- Removed self-developed HNSW / BM25 / RRF modules, reusing Grafeo native indexes
- Removed `embedding/` directory, Embedding generation moved up to Runtime layer
- Data model migrated from relational table structure to Grafeo LPG (Labeled Property Graph)
- Introduced PageRank, CDC/History, topology_boost, community detection etc. graph-native capabilities
- Added `conflict.rs` (three-layer signal conflict detection: semantic + temporal + context), rapid Evolution / Correction / Ambiguous determination in immediate phase

---

## Crate Structure

```
crates/acowork-grafeo/
├── Cargo.toml
└── src/
    ├── lib.rs              # Export GrafeoStore + public types
    ├── grafeo.rs           # GrafeoStore init, GrafeoDB connection management
    ├── store.rs            # MemoryStore trait implementation for GrafeoStore
    ├── graph.rs            # LPG graph operations (CRUD based on grafeo-engine API)
    ├── retrieval.rs        # Retrieval entry point (calls grafeo-engine search APIs)
    ├── decay.rs            # Decay calculation (leverages CDC history API)
    ├── conflict.rs         # Multi-signal conflict detection (semantic + temporal + context)
    ├── types.rs            # Episode / KnowledgeNode / ProceduralNode / AutobiographicalNode
    │                       # and other data types
    ├── episodic/
    │   ├── mod.rs          # Episodic layer
    │   ├── store.rs        # Write interaction records (LPG node creation)
    │   ├── search.rs       # Semantic similarity retrieval (grafeo-engine vector_search)
    │   └── consolidate.rs    # Consolidation flag and cleanup
    ├── semantic/
    │   ├── mod.rs          # Semantic layer
    │   ├── knowledge.rs    # KnowledgeNode (Fact/Preference/Relation)
    │   ├── procedural.rs   # ProceduralNode
    │   ├── autobiographical.rs  # AutobiographicalNode (forced Active)
    │   ├── conflict.rs     # Conflict candidate detection (Phase 2)
    │   ├── inference.rs    # Knowledge inference and merge
    │   └── skill.rs        # Skill experience nodes
    ├── consolidation/
    │   ├── mod.rs          # Consolidation pipeline
    │   ├── instant.rs      # Instant extraction executor (PendingKnowledgeNode)
    │   ├── offline.rs      # Offline consolidation (Phase 3)
    │   └── conflict.rs     # Conflict classification (Phase 3)
    ├── forgetting/
    │   ├── mod.rs          # Forgetting mechanism
    │   ├── scan.rs         # Background decay scan
    │   └── purge_log.rs    # Purge recovery mechanism (Phase 2)
    └── error.rs            # Error types
```

**Deleted Modules** (Grafeo natively provides, no need for self-development):
- ~~`vector/hnsw.rs`~~ — replaced with grafeo-engine native HNSW vector index
- ~~`fulltext/bm25.rs`~~ — replaced with grafeo-engine native BM25 full-text index
- ~~`retrieval/rrf.rs`~~ — replaced with grafeo-engine `hybrid_search()` built-in RRF
- ~~`retrieval/hybrid_search.rs`~~ — logic merged into `retrieval.rs`, directly calls `db.hybrid_search()`
- ~~`embedding/`~~ — Embedding generation moved to `acowork-runtime` layer
- ~~`backup.rs` / `recovery.rs`~~ — replaced with grafeo-engine WAL + `grafeo-file` native persistence
- ~~`migration.rs`~~ — Grafeo LPG has no versioned Schema migration concept, indexes created dynamically through API
- ~~`schema.rs`~~ — relational table structure definitions deprecated

---

## GrafeoStore (MemoryStore trait implementation)

```rust
use acowork_memory::MemoryStore;
use acowork_memory::{MemoryQuery, SearchResult, DecayConfig, StoreHealth, StoreStats};
use acowork_memory::{Episode, KnowledgeNode, ProceduralNode, AutobiographicalNode};
use acowork_memory::MemoryFilters;
use grafeo_engine::GrafeoDB;

/// Grafeo — MemoryStore implementation backed by grafeo-engine
/// One instance per Agent Runtime process, persisted as a single .grafeo file
pub struct GrafeoStore {
    db: GrafeoDB,
    config: GrafeoConfig,
}

pub struct GrafeoConfig {
    pub db_path: PathBuf,
    pub decay: DecayConfig,              // Forgetting params (injected from manifest)
    pub episode_retention_days: u32,     // Default episodic retention (14 days)
    pub graph_expand_hops: u8,          // Max graph expansion hops (default 3)
    pub graph_expand_per_hop: usize,    // Max edges per hop (default 5)
    pub graph_expand_max_nodes: usize,  // Max total expanded nodes (default 20)
    pub early_stop_thresholds: Vec<f32>, // Early stop thresholds (default [0.1, 0.15, 0.2])
    pub max_storage_mb: u64,            // Max storage capacity (default 5000MB)
    pub backup: BackupConfig,            // Auto backup config
}

/// Backup config (injected from manifest.toml [memory.backup])
pub struct BackupConfig {
    pub enabled: bool,                   // Backup switch (default true)
    pub schedule_hour: u8,              // Daily backup hour (default 3, i.e. 03:00)
    pub daily_retention_days: u8,        // Daily backup retention (default 7)
    pub weekly_retention_weeks: u8,      // Weekly backup retention (default 4)
    pub backup_dir: Option<PathBuf>,    // Backup dir (None = default <db_path>/../backups/)
}

impl GrafeoStore {
    /// Open a GrafeoStore instance (one independent .grafeo file per Agent)
    /// Auto-creates indexes if they do not exist
    pub fn open(config: GrafeoConfig) -> Result<Self> {
        let db = GrafeoDB::open(&config.db_path)?;

        // Initialize Grafeo native indexes on first open
        Self::init_indexes(&db)?;

        Ok(Self { db, config })
    }

    /// Create vector and text indexes for memory labels
    fn init_indexes(db: &GrafeoDB) -> Result<()> {
        // HNSW vector index for Episodic nodes
        db.create_vector_index("Episodic", "embedding", Some(384), Some("cosine"), None, None, None)?;
        // HNSW vector index for Knowledge nodes
        db.create_vector_index("Knowledge", "embedding", Some(384), Some("cosine"), None, None, None)?;
        // BM25 text index for Episodic content
        db.create_text_index("Episodic", "content")?;
        // BM25 text index for Knowledge content
        db.create_text_index("Knowledge", "content")?;
        Ok(())
    }
}

impl MemoryStore for GrafeoStore {
    // ── Episodic layer ──

    fn store_episode(&self, episode: &Episode) -> Result<()> {
        // Auto-classify content type (Informational / Artifact / Structural)
        // Artifact content is compressed to summary + artifact_refs
        // Embedding is generated by Runtime layer and passed in Episode.embedding
        episodic::store::write(&self.db, episode)
    }

    fn search_episodes(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>> {
        // Semantic search via grafeo-engine native HNSW
        let mut results = episodic::search::search(
            &self.db, query.embedding.as_slice(),
            query.filters.time_range.as_ref(), query.limit,
        )?;
        // Filter by MemoryQuery.filters
        apply_filters(&mut results, &query.filters);
        Ok(results)
    }

    fn mark_consolidated(&self, ids: &[String]) -> Result<()> {
        episodic::consolidate::mark(&self.db, ids)
    }

    fn cleanup_episodes(&self, older_than: Duration) -> Result<u64> {
        let days = older_than.as_secs() / 86400;
        episodic::consolidate::cleanup(&self.db, days as u32)
    }

    // ── Semantic layer ──

    fn store_knowledge(&self, node: &KnowledgeNode) -> Result<()> {
        // Write PendingKnowledgeNode (Phase 2) or formal KnowledgeNode (Phase 3)
        // Instant phase does not do triplet extraction or semantic dedup;
        // those are moved to offline consolidation
        semantic::knowledge::store(&self.db, node)
    }

    fn store_procedural(&self, node: &ProceduralNode) -> Result<()> {
        semantic::procedural::store(&self.db, node)
    }

    fn store_autobiographical(&self, node: &AutobiographicalNode) -> Result<()> {
        // Force status = Active (enforced by LPG property constraint)
        semantic::autobiographical::store(&self.db, node)
    }

    // ── Unified retrieval ──

    fn hybrid_search(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>> {
        // Parallel retrieval across Episodic + Knowledge labels,
        // fused via grafeo-engine native hybrid_search (RRF + optional topology boost)
        let results = retrieval::hybrid_search(
            &self.db, query,
        )?;
        apply_filters(&mut results, &query.filters);
        Ok(results)
    }

    fn graph_expand(&self, seeds: &[SearchResult], hops: u8) -> Result<Vec<SearchResult>> {
        let hops = hops.min(self.config.graph_expand_hops); // clamp
        retrieval::graph_expand(
            &self.db, seeds, hops,
            self.config.graph_expand_per_hop,
            self.config.graph_expand_max_nodes,
        )
    }

    // ── Forgetting ──

    fn run_decay_scan(&self, config: &DecayConfig) -> Result<DecayScanResult> {
        forgetting::scan::run(&self.db, config)
    }

    fn reactivate_node(&self, node_id: &str) -> Result<()> {
        forgetting::scan::reactivate(&self.db, node_id)
    }

    fn purge_expired(&self, max_dormant_age: Duration) -> Result<PurgeResult> {
        forgetting::scan::purge(&self.db, max_dormant_age)
    }

    // ── Lifecycle ──

    fn health_check(&self) -> Result<StoreHealth> {
        let start = Instant::now();
        let result = self.db.session()
            .execute("MATCH (n) RETURN count(n) AS cnt");
        let latency = start.elapsed().as_millis() as u64;
        Ok(StoreHealth {
            is_healthy: result.is_ok(),
            latency_ms: latency,
            error_count: 0,
            details: result.err().map(|e| e.to_string()),
        })
    }

    fn stats(&self) -> Result<StoreStats> {
        let session = self.db.session();
        let episode_count: u64 = session.execute(
            "MATCH (n:Episodic) RETURN count(n) AS cnt"
        )?.rows().next().map(|r| r.get::<u64>("cnt")).unwrap_or(0);
        let node_count: u64 = session.execute(
            "MATCH (n) WHERE n:Knowledge OR n:Procedural OR n:Autobiographical RETURN count(n) AS cnt"
        )?.rows().next().map(|r| r.get::<u64>("cnt")).unwrap_or(0);
        let active_count: u64 = session.execute(
            "MATCH (n) WHERE n.status = 'Active' RETURN count(n) AS cnt"
        )?.rows().next().map(|r| r.get::<u64>("cnt")).unwrap_or(0);
        let dormant_count = node_count - active_count;
        let edge_count: u64 = session.execute(
            "MATCH ()-[r]->() RETURN count(r) AS cnt"
        )?.rows().next().map(|r| r.get::<u64>("cnt")).unwrap_or(0);
        let storage_size = std::fs::metadata(&self.config.db_path)
            .map(|m| m.len()).unwrap_or(0);
        Ok(StoreStats {
            episode_count, node_count, active_node_count: active_count,
            dormant_node_count: dormant_count, edge_count,
            storage_size_bytes: storage_size,
            index_count: 4, // HNSW(Episodic) + HNSW(Knowledge) + BM25(Episodic) + BM25(Knowledge)
        })
    }

    fn close(&self) -> Result<()> {
        // GrafeoDB auto-flushes WAL on drop; explicit close is optional
        Ok(())
    }
}
```

**Note**: `MemoryStore` trait, `MemoryQuery`, `SearchResult`, `DecayConfig`, `StoreHealth`, `StoreStats`, `MemoryMiddleware` and other type definitions are in the independent `acowork-memory` crate (shared with Runtime); the Grafeo crate only implements the trait, doesn't define it. See 05-memory.md §10.

**Embedding Responsibility Moved Up**: GrafeoStore doesn't hold `EmbeddingProvider`. Embedding vectors are generated by `acowork-runtime` layer through `EmbeddingProvider` trait (Ollama `/api/embed` → Remote `/embeddings` fallback chain), passed as `Vec<f32>` into `Episode` / `MemoryQuery`. `MemoryManager.retrieve()` method head auto-generates embedding (200ms timeout), failure degrades to `text_search`. GrafeoStore only handles vector storage and HNSW indexing, dimension dynamically injected via `GrafeoConfig.embedding_dim`.

---

## Grafeo LPG Data Model

ACowork's memory types directly map to Grafeo's **Labels**, utilizing Label isolation to achieve type distinction, without additional `node_type` enum fields.

### 8.1 Node Types — Cognitive Function Layering

Node types are implemented through **LPG Labels**, distinguishing memory's **cognitive function**:

| Label              | Meaning           | Core Properties                                                                                                                      |
| ------------------ | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `Episodic`         | Experiential layer node | `content`, `embedding`, `importance`, `timestamp`, `session_id`, `role`, `content_type`, `consolidated`, `metadata`, `artifact_refs` |
| `Knowledge`        | Knowledge node    | `content`, `embedding`, `sub_type` (Fact/Preference/Relation), `confidence`, `subject`, `predicate`, `object`, `status`, `privacy`   |
| `Procedural`       | Procedural memory node | `content`, `embedding`, `procedure_id`, `success_rate`, `invocation_count`, `status`                                          |
| `Autobiographical` | Autobiographical memory node | `content`, `embedding`, `sub_type` (Identity/Capability/Limitation/Preference/History/Relationship), `status` (forced Active) |
| `SystemConfig`     | System config node | `config_key`, `config_value`, `updated_at`                                                                                        |
| `ToolInvocation`   | Tool invocation record | `tool_name`, `input_hash`, `output_summary`, `timestamp`, `latency_ms`                                                       |
| `Session`          | Session node      | `session_id`, `started_at`, `ended_at`, `agent_id`                                                                                   |

**NodeType Design Principles:**
- Implemented through **Grafeo Labels** (rather than enum fields), utilizing Label isolation to achieve type distinction
- Each Label has independent properties schema and retrieval indexes (HNSW/BM25)
- Cognitive layering and LPG Labels are one-to-one mapping (see 05-memory.md §0 Layering Principles)

### 8.2 Zone Concept — Business Scenario Partitioning (Deferred Implementation)

**⚠️ Zone functionality is deferred for implementation, this section is conceptual definition only.**

Zone is used to distinguish memory's **business scenario partitioning**, orthogonal to NodeType:
- **NodeType** answers "what type of memory is this?" (cognitive function: episodic/semantic/procedural/autobiographical)
- **Zone** answers "which business scenario does this memory belong to?" (business partition: work/personal/system)

**⚠️ Current Status (Phase 1-3):**
- `acowork-core/src/memory/traits.rs` defines `MemoryNode.zone` field, but **not yet used**
- `MemoryStore::list_by_zone()` method defined, but **GrafeoStore not implemented**
- Zone functionality deferred to Phase 4+, currently all nodes default to `default` zone

**Phase 4+ Implementation Plan (TBD):**
- Zone will be stored as **Grafeo Node Property** (rather than independent Label)
- Add `zone: String` field in each node struct
- Retrieval can filter by zone (e.g. `filters.zone = Some("work")`)

**Orthogonal relationship between Zone and NodeType:**
```
A KnowledgeNode can simultaneously belong to:
  - NodeType: Knowledge (cognitive function: semantic memory)
  - Zone: work (business scenario: work-related)
  
An Episodic can simultaneously belong to:
  - NodeType: Episodic (cognitive function: experiential layer)
  - Zone: personal (business scenario: personal life)
```

### Edge Types

| Edge Type         | Start             | End                            | Meaning                       |
| ----------------- | ----------------- | ------------------------------ | ----------------------------- |
| `HAS_MEMORY`      | `Session`         | `Episodic` / `Knowledge` / ... | Session owns memory |
| `REFERENCES`      | `Knowledge`       | `Knowledge`                    | Inter-knowledge reference relationship |
| `SELF_REFERENCES` | `Autobiographical` | `Autobiographical`             | Autobiographical self-reference (identity association) |
| `PRODUCED`        | `ToolInvocation`  | `Knowledge` / `Episodic`       | Tool invocation produces memory |
| `DERIVED_FROM`    | `Knowledge`       | `Episodic`                     | Knowledge derives from some episode |

### LPG Model Initialization Example

```rust
use grafeo_engine::GrafeoDB;

let db = GrafeoDB::open("agent_memory.grafeo")?;

// No explicit CREATE TABLE needed — LPG is schemaless
// Indexes are created via API (see GrafeoStore::init_indexes)

// Example: create an Episodic node
let mut session = db.session();
session.begin_transaction()?;
session.execute(
    "CREATE (e:Episodic { \
        episode_id: $id, \
        content: $content, \
        embedding: $emb, \
        importance: 0.5, \
        timestamp: $ts, \
        session_id: $sid, \
        role: 'user', \
        content_type: 'Informational', \
        consolidated: false \
    })",
)?;
session.commit()?;
```

---

## Retrieval Capability (Based on grafeo-engine Native API)

| Capability   | Old Description               | New Description                                                                                                                  |
| ------------ | ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| Semantic retrieval | Self-developed HNSW (M=16, ef_c=100) | `db.vector_search(label, "embedding", &vec, k, Some(ef), filters)` — Grafeo native HNSW, supports cosine/Euclidean/dot product distance, SIMD acceleration |
| Keyword retrieval | rusqlite FTS5 BM25            | `db.text_search(label, "content", query, k, filters)` — Grafeo native BM25, built-in Unicode tokenizer                            |
| Hybrid retrieval | Self-developed RRF fusion     | `db.hybrid_search(label, "content", "embedding", query, Some(&vec), k, filters)` — built-in RRF fusion, optional topology boost  |
| MMR deduplication | None                          | `db.mmr_search(label, "embedding", &vec, k, fetch_k, lambda, ef, filters)` — Maximal Marginal Relevance, ensures result diversity |
| Graph traversal | SQL simulation                | GQL: `MATCH (m)-[r*1..3]-(other) WHERE id(m) = $id RETURN other` — native LPG traversal, with query optimizer support             |
| Conflict detection | None (Phase 2 new)            | `db.vector_search()` — two-layer signal fusion (semantic similarity + temporal window), unified Ambiguous, Phase 3 LLM arbitration |

### Code Examples

```rust
use grafeo_engine::GrafeoDB;

let db = GrafeoDB::open("agent_memory.grafeo")?;

// Semantic search — Episodic layer
let results = db.vector_search(
    "Episodic",           // label
    "embedding",          // property name of the vector
    &query_embedding,     // Vec<f32> generated by Runtime LLM Provider
    10,                   // top-k
    Some(64),             // ef_search
    Some(filters),        // optional property filters
)?;

// Keyword search — Knowledge layer
let results = db.text_search(
    "Knowledge",
    "content",
    "user preference",
    10,
    Some(filters),
)?;

// Hybrid search (RRF fusion + optional topology boost)
let results = db.hybrid_search(
    "Knowledge",
    "content",            // text property
    "embedding",          // vector property
    "dark mode setting",  // text query
    Some(&query_embedding),
    10,
    Some(hybrid_filters),
)?;

// MMR search for diverse results
let results = db.mmr_search(
    "Knowledge",
    "embedding",
    &query_embedding,
    5,                    // final k
    20,                   // fetch_k (over-fetch then re-rank)
    0.5,                  // lambda (relevance vs diversity balance)
    Some(64),
    Some(filters),
)?;

// Graph expansion via GQL
let gql = format!(
    "MATCH (m)-[r*1..{}]-(other) \
     WHERE id(m) = $id \
     RETURN other LIMIT {}",
    hops, max_nodes
);
let mut session = db.session();
let expanded = session.execute_with_params(&gql, [("id", seed_id.into())])?;
```

---

## Graph Algorithm Enhancement

grafeo-engine has built-in graph algorithm procedures (`algos` feature), ACowork memory system can directly call them to improve memory quality.

### PageRank Integration

ACowork's original `importance_score` is a hand-tuned `f32`. Grafeo's PageRank algorithm can automatically evaluate memory node importance — nodes referenced by more edges have higher PageRank, as a supplement or replacement for `importance_score`.

```rust
/// Compute PageRank scores for all memory nodes
/// Used to automatically rank node importance based on graph connectivity
pub fn compute_pagerank(&self) -> Result<HashMap<String, f64>> {
    let mut session = self.db.session();
    let result = session.execute(
        "CALL grafeo.pagerank({damping: 0.85, max_iterations: 20}) \
         YIELD node_id, score \
         WHERE score > 0.001 \
         RETURN node_id, score ORDER BY score DESC"
    )?;
    // Parse result rows into HashMap<node_id, score>
    let mut scores = HashMap::new();
    for row in result.rows() {
        let id: String = row.get("node_id");
        let score: f64 = row.get("score");
        scores.insert(id, score);
    }
    Ok(scores)
}
```

**Use Cases**:
- Retrieval ranking: use PageRank score as input weight for `topology_boost`
- Forgetting protection: nodes with PageRank above threshold skip decay scan
- Importance calibration: replace hand-tuned `importance_score`, reduce manual intervention

### CDC / History

Grafeo has built-in CDC (Change Data Capture) recording the complete change history of each node. Through `db.history()` you can trace the entire creation, modification, deletion process of any memory node.

```rust
use grafeo_engine::EntityId;

/// Retrieve full change history of a memory node
/// Enables experience backtracking after decay
pub fn node_history(&self, node_id: &str) -> Result<Vec<ChangeEvent>> {
    // Grafeo CDC tracks every create / update / delete as a ChangeEvent
    let history = self.db.history(EntityId::Node(node_id))?;
    Ok(history)
}

/// Example: restore a node to a previous state after decay
pub fn restore_node_at_epoch(&self, node_id: &str, epoch: u64) -> Result<NodeSnapshot> {
    let mut session = self.db.session();
    let snapshot = session.execute_at_epoch(
        "MATCH (n) WHERE id(n) = $id RETURN n",
        epoch,
    )?;
    // Parse snapshot and optionally write back as a new node
    Ok(snapshot)
}
```

**Use Cases**:
- Experience backtracking: each time Decay modifies node properties, can view original state through `history()`
- Conflict reconciliation: compare versions of same node at different time points, assist LLM in judging merge strategy
- Audit trail: trace complete evolution chain of memory from Episodic → Knowledge

### topology_boost

Grafeo `hybrid_search()` supports `topology_boost` option — search results are re-ranked by graph connectivity. Nodes referenced by more edges get higher weight in retrieval, this is a unique advantage of graph databases.

```rust
/// Hybrid search with topology boost enabled
/// Nodes with more incoming edges are ranked higher
pub fn search_with_topology_boost(
    &self,
    query_text: &str,
    query_embedding: &[f32],
    k: usize,
) -> Result<Vec<SearchResult>> {
    let mut filters = SearchFilters::default();
    filters.topology_boost = true; // Enable graph connectivity re-ranking

    let results = self.db.hybrid_search(
        "Knowledge",
        "content",
        "embedding",
        query_text,
        Some(query_embedding),
        k,
        Some(filters),
    )?;
    Ok(results)
}
```

**Principle**:
- After vector/text retrieval returns candidate set, Grafeo executor computes graph centrality (degree, PageRank etc.) for each candidate node
- Final ranking = RRF score × topology_boost coefficient
- High connectivity nodes are usually "hub memories" (core facts, high-frequency tool invocation patterns), should be recalled first

### Community Detection

Grafeo has built-in Louvain community detection algorithm, can automatically discover implicit groups between memories.

```rust
/// Detect memory communities via Louvain algorithm
/// Identifies "capability blocks", "preference clusters", "relationship networks"
pub fn detect_memory_communities(&self) -> Result<Vec<Community>> {
    let mut session = self.db.session();
    let result = session.execute(
        "CALL grafeo.louvain() \
         YIELD community_id, node_id \
         RETURN community_id, collect(node_id) AS members"
    )?;

    let mut communities = Vec::new();
    for row in result.rows() {
        communities.push(Community {
            id: row.get("community_id"),
            members: row.get("members"),
        });
    }
    Ok(communities)
}
```

**Use Cases**:
- Capability block identification: discover procedural memory groups around specific skills, assist Skill system upgrade
- Preference clusters: discover implicit associations of user preferences (e.g. "dark mode + shortcuts + night do not disturb")
- Relationship networks: identify social/identity relationship networks between Autobiographical nodes
- graph_expand optimization: nodes within community are preferred for expansion during associative diffusion, inter-community expansion is delayed

---

## Conflict Detection (Multi-Signal Conflict Detection)

Conflict detection uses **two-layer signal fusion** design, in the immediate extraction phase (memory_store Tool Call time) quickly identifies candidate conflicts, providing input for offline consolidation's precise classification. This module corresponds to `conflict.rs`, independent of `semantic/conflict.rs` (pure semantic candidate detection) and `consolidation/conflict.rs` (offline classification), responsible for multi-signal fusion's immediate determination.

> **v3.8 Simplification**: Removed the "context negation" layer (Layer 3) based on hardcoded keywords and heuristic fast path (Evolution/Correction auto-judgment). Reason: (1) hardcoded keywords cannot cover all language and expression patterns; (2) Correction and Evolution are completely identical in code path (both mark old node Dormant), keyword matching doesn't bring additional value; (3) false positive/false negative risk is high, Phase 3 LLM unified arbitration is more reliable.

### Two Conflict Signal Layers

| Signal Layer | Data Source | Judgment Logic | Dynamic Threshold |
|--------------|-------------|----------------|-------------------|
| **Semantic Similarity** | `db.vector_search()` returns candidate node embeddings | Cosine similarity between new node and existing Active nodes | Fact 0.85 / Preference 0.80 / Relation 0.90 |
| **Temporal Conflict** | Node creation time difference | Conflicting statements for same subject within 24h | Time difference < 24h |

**Differentiated Semantic Threshold Design**:
- **Fact**: Threshold 0.85 — facts require high-precision matching, avoid misjudgment
- **Preference**: Threshold 0.80 — preferences have diverse expressions, appropriately relaxed
- **Relation**: Threshold 0.90 — relations involve multiple entities, mis-match cost is high

**Temporal Conflict Detection**:
When new node's semantic similarity with existing Active node exceeds threshold and creation time difference is within 24h window, trigger temporal conflict signal. Temporal conflict heuristic confidence is 0.7 (within 24h) or 0.5 (beyond 24h).

### Unified Ambiguous Strategy

Immediate phase **does not auto-judge any conflict**. All detected conflicts are uniformly marked as `ConflictType::Ambiguous`, both conflicting parties maintain Active state, share `conflict_group_id`, handed over to Phase 3 LLM offline arbitration for precise classification (Evolution / Correction / Ambiguous).

**Design Rationale**:
- Hardcoded keyword matching (negation words, change words) has been proven limited and unreliable
- Immediate phase's responsibility is **rapid identification** (does conflict candidate exist?), not **precise classification** (what type of conflict is it?)
- Phase 3 LLM has complete node content and context, judgment quality far exceeds hardcoded rules

### ConflictSignal Structure

```rust
/// Multi-signal conflict detection result
/// Produced by the immediate-phase conflict detector (conflict.rs)
pub struct ConflictSignal {
    /// Semantic similarity score (cosine similarity of embeddings)
    pub semantic_score: f32,

    /// Whether temporal conflict is detected (same subject, <24h, different object)
    pub temporal_conflict: bool,

    /// Suggested conflict type — always Ambiguous in Phase 1,
    /// to be reclassified by Phase 3 LLM arbitration
    pub suggested_type: ConflictType,

    /// Heuristic confidence based on temporal proximity:
    /// 0.7 if within 24h window, 0.5 otherwise
    pub heuristic_confidence: f32,
}

/// Conflict classification types
/// Phase 1 (immediate): all conflicts are Ambiguous.
/// Phase 3 (LLM): reclassifies to Evolution / Correction / Ambiguous.
pub enum ConflictType {
    /// Natural knowledge evolution over time (e.g., user moved)
    Evolution,
    /// User explicitly corrected a previous statement
    Correction,
    /// Cannot determine from signals alone — requires LLM arbitration
    Ambiguous,
}
```

### Conflict Detection API

```rust
/// Detect conflict between a new memory node and existing Active nodes.
///
/// Phase 1 (immediate): always returns Ambiguous — no auto-resolution.
/// Phase 3 (LLM arbitration): reclassifies via consolidation/conflict_llm.rs.
///
/// # Arguments
/// - `semantic_score`: Cosine similarity from vector_search (0.0-1.0)
/// - `threshold`: Dynamic threshold based on KnowledgeType
/// - `time_diff_hours`: Hours between new node and existing node creation
///
/// # Returns
/// - `Some(ConflictSignal)` if semantic_score >= threshold
/// - `None` if below threshold (no conflict)
pub fn detect_conflict(
    semantic_score: f32,
    threshold: f32,
    time_diff_hours: f64,
) -> Option<ConflictSignal> {
    if semantic_score < threshold {
        return None;
    }

    let temporal_conflict = time_diff_hours < TEMPORAL_WINDOW_HOURS as f64;
    let heuristic_confidence = if temporal_conflict { 0.7 } else { 0.5 };

    Some(ConflictSignal {
        semantic_score,
        temporal_conflict,
        suggested_type: ConflictType::Ambiguous,
        heuristic_confidence,
    })
}
```

### Connection with Offline Consolidation

Division of labor between `conflict.rs` (immediate phase) and `consolidation/conflict.rs` (offline phase):

| Phase     | Module                            | Responsibility                                       | Output                                   |
| --------- | --------------------------------- | ---------------------------------------------------- | ---------------------------------------- |
| **Immediate** | `conflict.rs`                | Two-layer signal fusion, uniformly marked Ambiguous  | `ConflictSignal` + `conflict_group_id`   |
| **Offline** | `consolidation/conflict_llm.rs` | LLM arbitration, precise classification Evolution/Correction/Ambiguous | Edge creation + old node Dormant handling |

Immediate phase only does **conflict candidate discovery** (semantic + temporal), uniformly marks Ambiguous then handed over to offline consolidation LLM for final judgment. This aligns with 05-memory.md §6.4 two-phase design.

---

## Index Description

| Index Type       | Old Implementation                | New Implementation                                                      |
| ---------------- | --------------------------------- | ----------------------------------------------------------------------- |
| Vector index     | Self-developed HNSW (`vector/hnsw.rs`) | Grafeo native HNSW vector index, created through `db.create_vector_index()` |
| Full-text index  | rusqlite FTS5 (`fulltext/bm25.rs`) | Grafeo native BM25 full-text index, created through `db.create_text_index()` |
| Hybrid retrieval | Self-developed RRF (`retrieval/rrf.rs`) | Grafeo native `hybrid_search()`, built-in RRF fusion + topology boost |
| Graph traversal index | SQL JOIN simulation           | Grafeo native adjacency index, O(degree) traversal, query optimizer supports predicate pushdown |
| Transaction isolation | rusqlite WAL                  | Grafeo MVCC snapshot isolation, native multi-version concurrency control |
| Crash recovery   | Self-developed `recovery.rs`      | Grafeo WAL replay mechanism, built-in crash recovery                    |
| Backup           | Self-developed `backup.rs`        | `grafeo-file` single-file format + file-level backup                    |

---

## Design Decisions

- **MemoryStore trait abstraction**: GrafeoStore implements the `MemoryStore` trait defined in `acowork-memory` crate; Runtime and MemoryManager only depend on the trait. Can be seamlessly replaced with other storage backends in the future (Sled / LMDB / remote service / in-memory mock)
- **Storage backend**: `grafeo-engine` (v0.5.39, crates.io), pure Rust graph database, supports LPG + GQL + HNSW + BM25 + WAL + MVCC
- **Vector index**: Grafeo native HNSW, M/ef/beam_width all configurable, distance functions support cosine/Euclidean/dot product/Manhattan, SIMD acceleration
- **Full-text index**: Grafeo native BM25, built-in Unicode tokenizer
- **Hybrid retrieval**: Grafeo native `hybrid_search()`, built-in RRF fusion, supports `topology_boost` graph connectivity reranking
- **Graph traversal**: native GQL query, with CBO/DPccp optimizer support, replaces SQL simulation
- **Embedding responsibility separation**: Embedding generated by Runtime LLM Provider, passed as `Vec<f32>` to GrafeoStore. GrafeoStore only handles storage and indexing, doesn't hold `EmbeddingProvider`
- **Database file path**: `<agent_workspace>/memory/private.grafeo` (single file `.grafeo` format)
- **Forgetting parameters** injected through `DecayConfig` (no longer hardcoded), supports per-Agent customization
- **Forgetting scan** runs in background when Agent is idle, doesn't block normal retrieval
- **Associative diffusion** parameters configurable (hops / per_hop / max_nodes), with default values
- **Consolidation pipeline** instant extraction through Tool Call mechanism (memory_store tool), offline consolidation Phase 3 uses dedicated LLM call
- **Fact node** auto semantic deduplication on write (match by subject+predicate)
- **Episode write** auto-classify content type, artifact content compressed to summary + artifact_refs
- **PageRank importance**: replace or supplement hand-tuned `importance_score`, automatic node graph importance evaluation
- **CDC history**: utilize `db.history()` to trace memory changes, support experience backtracking and audit
- **Community detection**: Louvain algorithm automatically discovers memory groups, enhance graph_expand semantic quality

---

## Dependencies

```toml
[dependencies]
acowork-memory = { workspace = true }    # MemoryStore trait + shared types
grafeo-engine = { workspace = true }      # v0.5.39, features: lpg, gql, vector-index, text-index, hybrid-search, wal, grafeo-file, algos, cdc, parallel
grafeo-common = { workspace = true }      # Shared types from Grafeo ecosystem
serde = { workspace = true }              # Serialization
serde_json = { workspace = true }         # JSON handling
thiserror = { workspace = true }          # Error definitions
tokio = { workspace = true }              # Async runtime
async-trait = { workspace = true }        # Async trait support
chrono = { workspace = true }             # DateTime handling
```

**Workspace Declaration** (`core/Cargo.toml`):

```toml
[workspace.dependencies]
grafeo-engine = { version = "0.5.39", features = [
    "lpg",
    "gql",
    "vector-index",
    "text-index",
    "hybrid-search",
    "wal",
    "grafeo-file",
    "algos",
    "cdc",
    "parallel",
] }
grafeo-common = { version = "0.5.39" }
```

---

## Feature Flags

`acowork-grafeo` itself has very minimal feature flags — complex capabilities are controlled by `grafeo-engine`'s feature flags:

```toml
[features]
default = []
```

**Note**: If you need to disable Grafeo graph algorithms to reduce compile volume, you can remove `"algos"` feature in workspace; if you need to disable CDC, remove `"cdc"` feature.

---

## Future Extension Directions

| Direction              | Description                                                                              | Phase    |
| ---------------------- | ---------------------------------------------------------------------------------------- | -------- |
| InMemoryStore          | mock implementation based on `GrafeoDB::new_in_memory()`, for unit and integration tests | Phase 3  |
| RemoteMemoryStore      | Cloud distributed storage based on Grafeo Server, supports multi-device real-time sharing | Phase 5+ |
| Incremental sync       | Cross-device incremental sync protocol based on Grafeo CDC + WAL                          | Phase 5+ |
| Temporal versioning    | Enable `grafeo-engine` `"temporal"` feature, supports memory time-versioned queries     | Phase 4+ |
| Encrypted storage      | Enable `grafeo-engine` `"encryption"` feature (AES-256-GCM), integrate with Vault key management | Phase 4+ |
| MCP exposure           | Expose memory API to external Agents through `grafeo-mcp`                                 | Phase 4+ |