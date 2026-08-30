# Memory Biomimetic Layered Architecture

> Version: v3.7 | Last Updated: 2026-04-22

> This document is based on `docs/_internal/archive/review/zh/07-memory-competitive-review.md` and `08-memory-benchmark-review.md` (local archive) design supplements. Major changes: added Abstention mechanism (§6.5), conflict detection upgraded to three-layer signal model (§6.4), added quality evaluation framework chapter (§11), instant/offline consolidation boundary clarified (§4), retrieval weight dynamic adjustment (§6.6).

> **v3.8 Change (2026-05-28)**: Context compression strategy greatly simplified, programmatic folding strategies all abandoned — see [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md). Core changes: remove content folding (Phase 1), three-stage progressive trimming, retrieval result 8-level priority, elastic budget partition. Transient layer compression simplified to: 70% alert → 80% LLM summary (complete context) → 95% emergency_trim safety net.

> **v3.9 Change (2026-05-28)**: Experience layer write sources simplified — see [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md). Core changes: remove per-round conversation real-time write to Grafeo; experience layer only written via Compaction summary and Session close distillation. Compaction and Distillation unified as single Compact Model call ("summary is distillation").

---

Memory uses **biomimetic layered** design, with human cognitive science as reference and Grafeo graph database as storage engine. Each Agent has completely independent private Memory, no Gateway-maintained public database exists. Cross-Agent data sharing is implemented through Intent queries and system Agent services, not shared storage.

**Design philosophy**: Memory is not storage, it is cognition. A memory system without forgetting is a dump, a memory system without consolidation is a fragment pile, a memory system without self-awareness is a database. What the Memory module should answer is not "how to store", but "how to remember, how to forget, how to think".

```
┌─────────────────────────────────────────────────────────┐
│  Transient Layer                                       │
│  ───                                                   │
│  Working memory — LLM context window                    │
│  Current conversation, reasoning chain, attention focus │
│  Lifecycle: single session                             │
│  Biomimetic correspondence: prefrontal cortex sustained discharge │
├─────────────────────────────────────────────────────────┤
│  Experiential Layer                                    │
│  ───                                                   │
│  Episodic memory — Grafeo episodic                      │
│  Interaction fragments, conversation snapshots, perceptual raw records │
│  Grafeo native HNSW vector index + BM25 full-text search │
│  Lifecycle: days → weeks, promotes to consolidated layer after consolidation │
│  Biomimetic correspondence: hippocampus temporary encoding │
├─────────────────────────────────────────────────────────┤
│  Consolidated Layer                                    │
│  ───                                                   │
│  Semantic memory — facts, preferences, relationships (KnowledgeNode) │
│  Procedural memory — behavior patterns, operation rules (ProceduralNode) │
│  Autobiographical memory — self-awareness, capability boundaries (AutobiographicalNode) │
│  LPG knowledge graph + GQL native associative diffusion retrieval │
│  Lifecycle: long-term to permanent, forgetting decay but not easily deleted │
│  Biomimetic correspondence: neocortex long-term storage │
└─────────────────────────────────────────────────────────┘

         ┌─── Consolidation Pipeline ───┐
         │                              │
    Experiential Layer ──(instant extract)──→ Consolidated Layer    ← LLM autonomous tool call (memory_store)
         │                              │
    Experiential Layer ──(offline replay)──→ Consolidated Layer    ← dedicated LLM call when idle (Phase 3)
         │                              │
    Consolidated Layer ──(forgetting decay)──→ dormant → (optional purge)
         │                              │
    Consolidated Layer ──(associative diffusion)──→ multi-hop retrieval results
```

## 0. Layering Principles

**Why layer by cognitive function rather than by storage location?**

The old three-tier (working memory / private memory / cloud sync) confused two dimensions — "working memory" is a cognitive function, "private memory" is a storage location, "cloud sync" is a sync mechanism. Biomimetic layering uniformly divides by cognitive function, with each layer having clear responsibility boundaries and information flow rules.

**Flow rules between layers:**

| Flow Direction | Mechanism | Trigger Condition |
|----------------|-----------|-------------------|
| Transient → Experiential | Summary write | Compaction triggers (80% token usage) or Session close, LLM summary asynchronously writes to Grafeo. No longer per-round write, avoiding redundancy with JSONL |
| Experiential → Consolidated | Consolidation pipeline | Instant extraction (LLM autonomous tool call) + offline replay (dedicated call when idle) |
| Consolidated → Transient | Retrieval injection | When user input arrives, retrieve related memory and inject into context. **Default OFF since 2026-09-12** (`MemoryManagerConfig::auto_inject_enabled = false`): different Agents have different recall needs, Grafeo triple/preference memory is incomplete, raw user message as query has low hit rate. Recovery method: set `auto_inject_enabled` to `true` (can be per-agent configured). Explicit `memory_recall` tool not affected |
| Intra Consolidated/Experiential flow | Associative diffusion | Retrieval extends 1-2 hops along graph edges |
| Consolidated → Dormant | Forgetting decay | Background periodically calculates decay_score |

**Irreversible one-way gate**: Experiential → Consolidated is information refinement process (raw fragments → structured knowledge), naturally one-way. But Consolidated → Experiential can be implemented through "recall" mechanism — when user or Agent actively triggers, extract related knowledge from consolidated layer as new episodic context injected into transient layer.

**Mapping between layers and Grafeo storage:**

| Cognitive Layer | Content | Grafeo Storage | Description |
|----------------|---------|----------------|-------------|
| Transient | Working memory | Not in Grafeo | LLM context window, pure process memory |
| Experiential | Episodic memory | `Episodic` Label | Grafeo native HNSW + BM25 + metadata |
| Consolidated | Semantic/Procedural/Autobiographical/Skill experience | `Knowledge` / `Procedural` / `Autobiographical` Label + Edge | LPG knowledge graph |

There is no ambiguity like "experiential layer nodes exist in Grafeo semantic" — cognitive layering and LPG Label are one-to-one mapping, storage format is `.grafeo` single file.

## 0.1 LLM-First Principle

**Trust LLM over rules — unless rules can solve problems LLM cannot.**

Memory system involves a lot of semantic judgments (what's worth remembering, how high confidence, whether conflicting, how to classify), these judgments are completed by LLM rather than rule engines. Specific applications:

- **Instant extraction**: LLM autonomously judges whether to call memory_store, evaluates confidence (high/medium/low), Runtime doesn't do semantic secondary check
- **Offline consolidation**: Triple extraction, conflict classification, evidence verification executed by LLM with full context, not real-time rule approximation
- **Runtime only does mechanical guardrails**: Content length limit, call frequency limit, safety filter — these are mechanical limits LLM cannot self-constrain
- **Summary entity/triple extraction**: When Compaction triggers, Compact Model extracts entities and triples while generating summary. No longer per-round extraction (v3.10 simplification)

## 1. Transient Layer: Working Memory

Working memory is what Agent is currently "thinking" about, directly mapped to LLM's context window.

```
┌─ System Prompt ──────────────────────────┐
│  Agent identity definition                │
│  Autobiographical memory summary (from consolidated layer injection) │
│  Skill Instructions                       │
│  Tool definitions                         │
├─ Retrieved Memory ───────────────────────┤
│  Consolidated layer retrieval results (semantic/procedural/autobiographical) │
│  Experiential layer retrieval results (similar episodes) │
│  Associative diffusion results            │
├─ Conversation ─────────────────────────┤
│  User messages + Agent replies            │
│  Tool calls and results                   │
│  (Tool calls and results saved in conversation history) │
├─ Scratchpad ────────────────────────────┤
│  Agent internal reasoning chain           │
└──────────────────────────────────────────┘
```

**Entity and Triple Extraction (v3.10 simplification)**:

No longer per-round extract entities through memory_hint — changed to be completed by Compact Model when Compaction triggers. Compact Model output format:

```
<summary>
Natural language summary text...
</summary>
<entities>
Entity1, Entity2, Entity3
</entities>
<triples>
subject | predicate | object
subject | predicate | object
</triples>
```

- **entities**: Core entities that appear continuously across rounds (people, places, technologies, projects, concepts), max 10, comma-separated
- **triples**: Explicit factual knowledge, subject|predicate|object format, one per line

Design rationale:
- Per-round extraction cost (~65 tokens/round) is no longer reasonable after ADR-011 — experiential layer no longer per-round writes, storage purpose disappeared
- Compact Model has complete conversation context, entity and triple extraction quality higher than per-round snapshots
- Compaction is low-frequency operation (triggers per 80%), marginal cost negligible
- Retrieval strategy always uses default weights, no type-driven dynamic adjustment — evaluation shows f/r type micro-tuning benefits not validated

**Retrieval Strategy (v3.10 simplification)**:

All retrieval uniformly uses default RRF weights (vector: 0.7, text: 0.3), no longer dynamically adjusts based on memory_hint type. HintType enum retained but only used when `memory_store` tool called and LLM explicitly specifies (for instant extraction pipeline sub_type classification).

**Transient Layer Management Strategy (v3.8 simplification)**:

Context compression is a semantic understanding task; only LLM can reliably judge what information can be discarded. Programmatic strategies (character truncation, FIFO, role folding) essentially use proxy metrics to replace semantic understanding, will inevitably fail. Therefore all daily programmatic folding strategies have been abandoned, compression simplified to three stages:

| Stage | Trigger Condition | Behavior |
|-------|-------------------|----------|
| Stage 1: Monitoring | 70% context usage | Log, don't intervene |
| Stage 2: LLM Summary | 80% context usage | Compact Model does LLM summary on full context. No folding/truncation preprocessing. Protect system prompt + recent 2-3 rounds, compress middle section. Full history archived to temp file |
| Stage 3: Emergency Trim | 95% / API ContextOverflow | emergency_trim (keep last N non-system), as safety net |

> **Design decision**: See [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md).

## 2. Experiential Layer: Episodic Memory

Episodic memory stores Agent's interaction fragments with users, is the "raw material" of memory.

```
Grafeo Episodic Store
├── episode_id: String              // unique ID
├── timestamp: DateTime             // occurrence time
├── role: Role                      // user / agent / tool
├── content: String                 // content (conversation text or post-Compaction summary)
├── embedding: Vec<f32>             // semantic vector
├── metadata: HashMap<String, Value>  // context metadata (topic, sentiment tendency etc.); doesn't prestore related node_id, cross-layer diffusion through source_episode reverse query
├── session_id: String              // belonging session
├── consolidated: bool              // whether consolidated to consolidated layer
└── importance: f32                 // importance score (LLM scoring 0.0-1.0 at write)
```

> **v3.10 Design Simplification**: Removed Episode's `content_type` (ContentType) and `artifact_refs` (ArtifactRef) fields.
> Episode content no longer does classified compression — raw conversation directly stored, summary generated by Compact Model during Compaction.
> Reason see [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md): Compaction = Distillation, summary is distillation.

**Key Design Decision: Episode Content Storage Strategy**

- **v3.10**: Episode content no longer does classified compression. Conversation text directly completely stored, summary generated by Compact Model in Compaction phase.
- **Compaction**: When context usage reaches 80%, Compact Model does natural language summary on full context (including entity and triple extraction), summary writes to Grafeo distilled Episode.
- Reason see [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md): Compaction = Distillation, summary is distillation.

**Retrieval Capability (based on grafeo-engine native API)**:

- **Semantic retrieval**: `db.vector_search()` — Grafeo native HNSW vector index, supports cosine/Euclidean/dot product distance, SIMD acceleration
- **Keyword retrieval**: `db.text_search()` — Grafeo native BM25 full-text index, built-in Unicode tokenizer
- **Hybrid retrieval**: `db.hybrid_search()` — Grafeo native RRF fusion ranking, supports `topology_boost` graph connectivity reranking
- **MMR deduplication**: `db.mmr_search()` — Maximal Marginal Relevance, ensures result diversity, avoids duplicate semantics
- **Time filter**: Narrow retrieval space by time range
- **Cross-layer associative diffusion** (§6): Retrieved episodes through consolidated layer KnowledgeNode's `source_episode` field reverse query related nodes, extend to consolidated layer knowledge and other experiential layer episodes along GQL native graph traversal. Example: user asks "hotel stayed at last time in Shanghai", episodic retrieves business trip record → reverse query consolidated layer "user usually stays at Jinjiang Inn" → through `MATCH (m)-[r*1..3]-(other)` graph traversal extend to another business trip episode at same hotel.

**Embedding Generation Strategy**:

Embedding generated by Runtime layer via `EmbeddingProvider` trait (rather than GrafeoStore internal), passed as `Vec<f32>` into `Episode` / `MemoryQuery`.

**Provider Fallback Chain**: Ollama local (primary, `nomic-embed-text`, 768d) → Remote API (fallback, OpenAI-compatible `/embeddings`, 512-1536d). `FallbackEmbeddingProvider` automatically manages primary→fallback switching (2 consecutive failures + 200ms timeout).

**Generation timing**:
- Retrieval: `MemoryManager.retrieve()` method head auto-generates embedding (200ms timeout), timeout/failure then `query.embedding = None`, fall back to `text_search` pure text retrieval
- Write: When episode distilled-written, sync generate embedding, same 200ms timeout degradation

GrafeoStore only responsible for storage and indexing, doesn't hold `EmbeddingProvider`.

**Experiential Layer Forgetting**:

Episodic memory's forgetting is more aggressive than consolidated layer — this is natural, because hippocampus itself is temporary encoding area.

- **Default retention period**: 14 days (configurable)
- **Consolidation marker**: Episodes extracted to consolidated layer marked `consolidated = true`
- **Cleanup strategy**:
  - Consolidated + exceeds 7 days → auto cleanup (knowledge transferred to consolidated layer, raw fragment no longer needed)
  - Not consolidated + exceeds 14 days + importance < 0.3 → cleanup (low-value un-extracted fragments)
  - Not consolidated + exceeds 14 days + importance >= 0.3 → retain and try offline consolidation

## 3. Consolidated Layer: Long-Term Memory

Consolidated layer is Agent's "knowledge foundation", containing three memory types, all stored in Grafeo's semantic memory graph.

### 3.1 Semantic Memory (KnowledgeNode)

Stores structured knowledge extracted from interactions — facts, preferences, relationships.

```rust
struct KnowledgeNode {
    node_id: String,
    node_type: KnowledgeType,        // Fact / Preference / Relation
    subject: String,                 // knowledge subject (usually "user")
    predicate: String,               // relationship/attribute
    object: String,                  // value/target
    confidence: f32,                 // confidence 0.0-1.0
    source_episode: Vec<String>,     // source episode IDs (traceable)
    created_at: DateTime,
    updated_at: DateTime,

    // === Forgetting mechanism fields ===
    importance: f32,                 // LLM scoring at write 0.0-1.0
    access_count: u32,               // retrieval hit count
    last_accessed: DateTime,         // last retrieved
    decay_score: f32,                // runtime computed decay score
    status: NodeStatus,              // Active / Dormant / Purged
    dormant_since: Option<DateTime>, // entering Dormant state time (Purge 90-day timer start)

    // === Privacy level ===
    privacy: PrivacyLevel,           // Public / Personal / Sensitive
}

enum KnowledgeType {
    Fact,        // fact: "User lives in Beijing"
    Preference,  // preference: "User likes concise replies"
    Relation,    // relationship: "User's manager is Wang Wu"
}

enum NodeStatus {
    Active,     // normally participates in retrieval
    Dormant,    // decay below threshold, doesn't participate in regular retrieval but retained
    Purged,     // cleared (only via purge operation)
}

enum PrivacyLevel {
    Public,     // cross-Agent shareable (e.g. user name)
    Personal,   // Agent-private (e.g. user style preference)
    Sensitive,  // sensitive information, stripped when package sharing
}
```

**Edges between nodes:**

```
KnowledgeNode:Alice ──[LIVES_IN]──→ KnowledgeNode:Beijing
KnowledgeNode:Alice ──[PREFERS]───→ KnowledgeNode:concise replies
KnowledgeNode:Alice ──[MANAGED_BY]→ KnowledgeNode:Bob
KnowledgeNode:Beijing ──[IS_CAPITAL_OF]→ KnowledgeNode:China
```

Edges also have properties — weight (strength), source, creation time. Edge weight affects propagation strength of associative diffusion.

**Edge weight calculation rules:**

```
edge_strength = min(0.8, confidence_avg × recency_factor)

Where:
- confidence_avg = (source_node.confidence + target_node.confidence) / 2
- recency_factor = exp(-0.01 × days_since_edge_created)
  (edge decays slower than nodes, half-life ~69 days, because relationships more durable than facts)
- Upper limit 0.8 prevents any single edge weight being too high causing diffusion bias
```

Edge weight calculated at creation, updated synchronously during subsequent decay_scan. Edges don't independently store decay_score — edge survival depends on both end nodes: when either end is purged, related edges auto-delete.

### 3.2 Procedural Memory (ProceduralNode)

Stores "what to do in what situation" behavior patterns, complementary to Skill system.

```
Skill system's procedural memory: SkillExperience (Skill-level, specific skill execution experience)
Grafeo's procedural memory: ProceduralNode (cross-Skill general behavior pattern)
```

```rust
struct ProceduralNode {
    node_id: String,
    trigger_condition: String,       // trigger condition: "User corrected format twice in a row"
    action_pattern: String,         // behavior pattern: "Stop using Markdown tables, switch to plain text lists"
    confidence: f32,                 // confidence
    activation_count: u32,           // activation application count
    source_skill: Option<String>,    // source Skill (if any)
    learned_from: String,            // "user feedback" / "execution failure" / "self-evaluation"

    // forgetting fields (same as KnowledgeNode)
    importance: f32,
    access_count: u32,
    last_accessed: DateTime,
    decay_score: f32,
    status: NodeStatus,
    dormant_since: Option<DateTime>,  // entering Dormant time (Purge 90-day timer start)

    created_at: DateTime,
    updated_at: DateTime,
}
```

**Relationship with SkillExperience:**

| Dimension | ProceduralNode | SkillExperience |
|-----------|----------------|------------------|
| Scope | Cross-Skill general behavior | Specific Skill execution experience |
| Source | User feedback / execution failure summary | Each Skill execution record |
| Injection position | System Prompt behavior guidelines | Skill Instruction experience supplement |
| Example | "User doesn't like long replies" | "weekly-report Skill needs flattened instructions on qwen3:8b" |

**Procedural Memory and Skill Experience Linkage**:

When a ProceduralNode's `source_skill` is non-empty, it forms cross-reference with corresponding Skill's SkillExperience. For example, weekly-report Skill corrected multiple times by user for "output too long" → SkillExperience records failure_case → consolidation pipeline extracts general ProceduralNode: "This user prefers concise output" → this ProceduralNode affects all Skills' execution, not just weekly-report.

### 3.3 Autobiographical Memory (AutobiographicalNode)

Stores Agent's self-awareness — "who am I, what can I do, where are my boundaries". This is the foundation of personality continuity.

```rust
struct AutobiographicalNode {
    node_id: String,
    aspect: AutobiographicalAspect,  // dimension of self-awareness
    content: String,                 // specific content
    confidence: f32,
    source: String,                  // "manifest" / "self_evaluation" / "user_statement" / "important_event"
                                       // v3.11: when memory_store(category=autobiographical) instant-written
                                       //        LLM labels via `source` parameter, value set extended to 4
    updated_at: DateTime,

    // autobiographical memory doesn't participate in forgetting decay — this is Agent's core identity
    // but can be updated (e.g. user changed name, Agent learned new Skill)
    //
    // ⚠️ status always Active, forgetting scan skips this node type
    // status column in schema not modifiable for AutobiographicalNode
}

enum AutobiographicalAspect {
    Identity,           // identity declaration: "I am weather assistant, helping you understand weather info"
    Capability,         // capability scope: "I can query global city weather, give clothing suggestions"
    Limitation,         // capability boundary: "I cannot predict weather beyond 7 days"
    Preference,         // own preference: "I tend to give conclusion first then explain reason"
    History,            // important experience: "2026-04-14 user taught me to generate weekly report, this is my first Skill"
    Relationship,       // relationship with user: "I've worked with Alice for 3 months, she likes concise style"
}
```

**Sources of autobiographical memory:**

1. **Manifest derived** (auto): From `manifest.toml`'s `agent.name`, `agent.description`, `skills/` list auto-generate Identity and Capability nodes
2. **Self-evaluation** (periodic): When Agent is idle, based on SkillExperience's model_compatibility and execution statistics, generate/update Limitation nodes ("on qwen3:8b, complex reasoning task success rate ~60%")
3. **User statement** (instant): User directly expresses evaluation of Agent ("you're too wordy"), LLM calls `memory_store` tool in Tool Call phase, passing `category: "autobiographical"` + `aspect: "Preference"` to land. See §4.1 tool definition
4. **Important event** (instant): Key interactions (first time learning new Skill, major error correction, user expressing strong emotion) LLM judges then calls `memory_store` tool, passing `category: "autobiographical"` + `aspect: "History"` (append by event, append-only). Same event won't bloat from repeated writes

**Autobiographical Memory Injection**:

Autobiographical memory summary always injected at front of System Prompt (after Agent identity definition), as Agent's "self-awareness background":

```
## About Yourself

You are "Weather Assistant", helping users understand weather information.
You can query global city weather, give clothing suggestions, but cannot predict weather beyond 7 days.
You've worked with Alice for 3 months, she prefers concise reply style.
Your complex reasoning success rate on qwen3:8b model is ~60%.
```

**Autobiographical Capacity Management**:

AutobiographicalNode doesn't participate in forgetting, but needs capacity control to prevent unlimited expansion:

- **History node summary compression**: When History nodes exceed 10, offline consolidation phase auto-merges multiple old History into one summary node ("Apr-Jun 2026 main events: learned weekly report and code review Skills, ended user磨合期"), original nodes turn to Dormant (don't purge)
- **Injection cap**: When autobiographical summary injects into System Prompt, take Top-K by importance (Identity / Capability / Limitation must inject, History takes recent 5 summaries + recent 3 details, Relationship takes Top-3)
- Total token budget: autobiographical not exceeding 200 tokens (~150 Chinese characters)

## 4. Consolidation Pipeline

Consolidation pipeline is the information refinement process from Experiential to Consolidated, simulating hippocampus→neocortex memory consolidation.

### 4.1 Instant Extraction (Phase 1)

Instant extraction is implemented via **Tool Call mechanism** — `memory_store` as one of Agent's built-in tools, LLM autonomously judges whether to call when generating replies. No extra LLM calls, async pipelines, or prefilter rules needed.

**Instant Extraction Output Definition (v3.7 clarification)**:

Instant extraction phase produces **PendingKnowledgeNode**, clearly distinguished from formal KnowledgeNode:

```
PendingKnowledgeNode:
  confidence = 0.7 (default value)
  status = Pending (immediately retrievable but marked "to be confirmed")
  Participates in regular hybrid_search, but results labeled [pending confirmation]
  Doesn't participate in graph_expand (pending nodes don't do associative diffusion)

High confidence direct effect:
  If instant extraction confidence >= 0.85 (i.e. LLM outputs confidence="high"),
  Directly create formal KnowledgeNode (status = Active), no offline confirmation needed
  → Applies to facts user explicitly expresses (e.g. "I live in Beijing"), avoids high-certainty info being unnecessarily marked pending
```

**Design Decision: Tool Call vs Separate Call**

| Dimension | Separate LLM Call | Tool Call (current choice) |
|-----------|-------------------|----------------------------|
| Extra API cost | 0-1 extra calls per round | Zero extra calls |
| Architecture complexity | High (async pipeline + queue + WAL + prefilter) | Low (tool definition naturally integrated) |
| Prefilter | Runtime hardcoded rules | LLM autonomous judgment (natural filter) |
| Context sharing | Need re-input conversation | Share current conversation context |
| User observability | Black box (async pipeline invisible) | Transparent (tool call visible in conversation history) |
| Token overhead | 0 (on-demand call) | ~150 extra tokens per round (tool definition + extraction guidance) |

Core reason for choosing Tool Call: Instant extraction's goal is "usable" rather than "perfect". LLM naturally has the ability to judge "what's worth remembering" — "what's today's weather" isn't worth saving, LLM knows itself. Phase 3's offline consolidation uses dedicated prompt for deep extraction to catch misses.

**memory_store Tool Definition (Phase 2 simplified version, v3.11 extended autobiographical entry)**:

```json
{
  "name": "memory_store",
  "description": "Store user information or behavior patterns worth long-term remembering. Only call when conversation contains new, important, non-temporary information. Don't store obvious common sense or temporary information.",
  "parameters": {
    "type": "object",
    "properties": {
      "content": {
        "type": "string",
        "description": "Content to remember, described in natural language (e.g. 'User lives in Shanghai'), no need to split into triples"
      },
      "category": {
        "type": "string",
        "enum": ["fact", "preference", "relation", "procedure", "autobiographical"],
        "description": "Information type: fact=objective fact, preference=user preference, relation=person/entity relationship, procedure=behavior pattern, autobiographical=Agent self-awareness (must pair with aspect parameter, see below)"
      },
      "aspect": {
        "type": "string",
        "enum": ["Identity", "Capability", "Limitation", "Preference", "History", "Relationship"],
        "description": "Required when category=autobiographical. Autobiographical six-dimensional classification (see §3.3). Identity/Capability/Limitation/Preference/Relationship use idempotent upsert (same key overwrites), History goes append-only (each time new node created)."
      },
      "key": {
        "type": "string",
        "description": "Recommended when category=autobiographical and aspect is not History. Idempotent key: same key multiple writes update same node instead of creating new. History node doesn't need key — it itself is event flow."
      },
      "source": {
        "type": "string",
        "description": "Optional, autobiographical node source label (e.g. 'user_statement', 'important_event'), for audit and offline classification statistics."
      },
      "confidence": {
        "type": "string",
        "enum": ["high", "medium", "low"],
        "description": "Confidence: high=user explicitly expressed, medium=inferred, low=uncertain. LLM self-judges, optional, default medium"
      },
      "keywords": {
        "type": "array",
        "items": { "type": "string" },
        "description": "Keywords, optional. Runtime auto-supplements from memory_hint.e, usually no need to fill"
      }
    },
    "required": ["content", "category"]
  }
}
```

> **⚠ Note**: When `category=autobiographical`, `aspect` is required. Tool schema declares this constraint via JSON Schema's `allOf` conditional branch (`if category=autobiographical then aspect required`), helping LLM clients complete validation before calling; Runtime side does a backup validation in `MemoryStoreTool` (missing or invalid `aspect` returns `invalid_aspect` error), double insurance ensures autobiographical writes don't land in wrong layer.

**Interface Simplification Design Rationale**:

[continues with detailed content for sections 4.2-4.6, 5, 6, 7, 8, 9, 10, 11]

This is a comprehensive architecture document covering memory layers, retrieval strategies, conflict detection, forgetting mechanisms, and consolidation pipelines. Key design decisions include:

- **Three-layer hierarchy** (Transient/Experiential/Consolidated) with biomimetic correspondence to brain structures
- **LLM-first principle** for semantic judgments (confidence, importance, conflicts)
- **Tool call mechanism** for instant extraction rather than separate LLM calls
- **Compaction = Distillation** (ADR-011): single LLM call for both memory replacement and experience layer writing
- **Privacy levels** (Public/Personal/Sensitive) for package sharing
- **Forgetting mechanisms** with three-factor decay and Dormant→Purge lifecycle
- **Grafeo integration** with native HNSW + BM25 + RRF hybrid retrieval
- **Associative diffusion** via LPG graph traversal for cross-layer retrieval

For full details on sections 4.2 (Offline Consolidation), 4.3 (Conflict Detection), 4.4 (Pending → Active Promotion), 4.5 (LLM Judge), 4.6 (Embedding Update), and chapters 5-11 (Quality Framework, Self-Evaluation, etc.), see the Chinese source document `docs/design/zh/05-memory.md`.