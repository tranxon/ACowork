<h1 align="center">RollBall.AI — Agent as APP</h1>

<p align="center">
  🏗️ <strong>Declarative Agent Platform · Decentralized · High-Security · Scalable</strong><br>
  ⚡️ <strong>Easy to develop an agent for everyone.</strong><br>
  ⚡️ <strong>Easy to deliver an agent to everyone.</strong><br>
  ⚡️ <strong>Easy to deploy an agent everywhere.</strong>
</p>

<p align="center">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/language-Rust-ff6600" alt="Language" /></a>
  <a href="./docs/"><img src="https://img.shields.io/badge/docs-design-brightgreen" alt="Docs" /></a>
  <a href="./docs/module-design/00-overview.md"><img src="https://img.shields.io/badge/status-design%20phase-yellow" alt="Status" /></a>
</p>

<p align="center">
  <a href="README.zh.md">简体中文</a>
</p>

---

RollBall.AI is a **decentralized, high-security, scalable AI Agent runtime platform** modeled after Android. Every Agent is an independent declarative application package (`.agent`) loaded by a universal Agent Runtime binary and managed by a lightweight Gateway.

RollBall **serves two types of users**: developers build agents declaratively (manifest + prompt + SKILL.md, no coding required), and end-users install agents from a repository. The signing toolchain + DevMode + publishing wizard form a complete developer toolkit — making **"can write prompt = can build Agent"** a reality.

Every Agent is an independent **"digital being"**: its own runtime process, private memory, workspace, and configuration — fully independent personalized cognition. Agents can be freely shared between users — Personal/Sensitive data is automatically stripped during packaging, taking only the agent's capabilities away, leaving user's private memories behind.

---

## 🔥 Why RollBall?

| Dimension | LangChain / CrewAI | OpenCode / OpenClaw | RollBall.AI |
|-----------|--------------------|---------------------|-------------|
| **Architecture** | Library/Framework: your code calls its API | Coding Agent (TUI/CLI): single-agent, task-focused | **Agent Platform**: declarative `.agent` package, universal Runtime binary |
| **Agent Model** | Code-defined agents (Python/TS) | Built-in agents (build/plan), skill-based | **Declarative agents**: config + prompt + SKILL.md, zero coding |
| **Agent Isolation** | In-process (threads/coroutines) | Process-level, single runtime | **Process-level**: each Agent independent process + private Grafeo |
| **LLM Connection** | Your code manages LLM calls | Direct connection per agent | **Direct Connect**: each Agent talks directly to LLM API, not proxied |
| **Memory System** | Simple RAG or vector store | Chat-scoped / plugin-reliant | **Biomimetic Layered**: 3-tier, 5-class (Grafeo graph database) |
| **Privacy Sharing** | No privacy boundary | Package-level sharing | **Zone Isolation**: Personal/Sensitive data auto-stripped on share |
| **Distribution** | pip package / Docker image | npm / brew / script install | **`.agent` packages**: signed, registry distribution, APK-like |
| **Multi-Agent** | Code-level orchestration | Limited (built-in agents) | **Intent mechanism**: Capability Registry + message routing |
| **Security** | Framework-level checks | Tool-level approval gates | **3-layer**: Package signing + Process sandbox + WASM sandbox |

---

## 🚀 Quick Start

### Installation

```bash
# Cargo (Rust toolchain)
cargo install rollball-gateway rollball-runtime

# Or build from source
git clone https://github.com/rollball/agent-study.git
cd core && cargo build --release
```

### Write Your First Agent

All you need is a `manifest.toml` + a prompt file:

```toml
# com.example.qa-agent/manifest.toml
[package]
id = "com.example.qa-agent"
name = "Quality Assurance"
display_name = "QA-Tom"
role = "QA"
version = "1.0.0"

[llm]
provider = "deepseek"
model = "deepseek-v4-flash"

[permissions]
tools = ["web_search", "read_file", "write_file"]
```

```markdown
# prompts/system.md
You are a QA Agent, helping users with quality management and code review.
```

### Package, Sign, and Run

```bash
# Package into .agent bundle
rollball-sign package ./qa-agent/ -o qa-agent.agent

# Install to local Gateway and run
rollball-gateway install qa-agent.agent
rollball-gateway start com.example.qa-agent

# Chat mode
rollball-gateway chat --agent com.example.qa-agent "Help me review this code"
```

> **Status**: The project is in **design phase**. Core Rust crate architecture is defined, detailed design docs are complete, but implementation has not started. The above is the target API design.

---

## 🏛️ Core Architecture

### Android Analogy

| Android | RollBall | Role |
|---------|----------|------|
| ART | Agent Runtime | Universal execution engine (single binary) |
| APK | `.agent` package | Declarative bundle (config + prompts + skills, no executable code) |
| APK Signature | Signing Block | Package signing, verifies integrity and origin |
| AMS | Gateway | Lifecycle management (install, start/stop, budget, rate) |
| Binder IPC | Gateway Service API | Inter-process communication |
| ContentProvider | System Agent | System-level data service (identity, preferences) |
| PMS | Package Manager | Install/uninstall/upgrade |

### System Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                      Gateway (Daemon)                        │
│                                                              │
│  ┌────────────┐ ┌────────────┐ ┌───────────┐ ┌───────────┐ │
│  │ Package    │ │ Lifecycle  │ │ Intent    │ │ Rate      │ │
│  │ Manager    │ │ Manager    │ │ Router    │ │ Limiter   │ │
│  └────────────┘ └────────────┘ └───────────┘ └───────────┘ │
│                                                              │
│  ┌────────────┐ ┌────────────┐             ┌───────────┐   │
│  │ Budget    │ │ Key       │             │ Config    │   │
│  │ Tracker   │ │ Vault     │             │ Manager   │   │
│  └────────────┘ └───────────┘             └───────────┘   │
│                                                              │
└──────────────────────────┬───────────────────────────────────┘
                           │ Gateway Service API
                           │ (Unix Socket / Named Pipe / Local TCP)
       ┌───────────────────┼───────────────────┐
       │                   │                   │
       ▼                   ▼                   ▼
┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐
│ Agent Runtime   │ │ Agent Runtime   │ │ Agent Runtime   │
│ (Single Binary) │ │ (Single Binary) │ │ (Single Binary) │
│                 │ │                 │ │                 │
│ ┌─────────────┐│ │ ┌─────────────┐│ │ ┌─────────────┐│
│ │ System Agent││ │ │ Weather     ││ │ │ Calendar    ││
│ │(com.rollball││ │ │ Agent       ││ │ │ Agent       ││
│ │  .system)  ││ │ │ (config +   ││ │ │ (config +   ││
│ │             ││ │ │  prompt +   ││ │ │  prompt +   ││
│ │             ││ │ │  skills)    ││ │ │  skills)    ││
│ └─────────────┘│ │ └─────────────┘│ │ └─────────────┘│
│                 │ │                 │ │                 │
│ ✅ Private     │ │ ✅ Private     │ │ ✅ Private     │
│    Grafeo      │ │    Grafeo      │ │    Grafeo      │
│ ✅ Direct LLM  │ │ ✅ Direct LLM  │ │ ✅ Direct LLM  │
│ ✅ Tools Exec  │ │ ✅ Tools Exec  │ │ ✅ Tools Exec  │
│ ⭐ System      │ │                 │ │                 │
│    Privilege   │ │                 │ │                 │
└─────────────────┘ └─────────────────┘ └─────────────────┘
```

---

## ✨ Core Features

### 🧩 Standardized Packaging
Agents are distributed as `.agent` archives containing manifest.toml, Prompts, Skills, and tool declarations — **no executable code**. Every package must be signed, and Gateway enforces verification at install time.

```
<agent_id>.agent
├── manifest.toml          # Metadata + LLM config + permissions + tool declarations
├── prompts/               # System prompt templates
├── config/                # Default configuration
├── data/                  # Initial data
├── skills/                # Skill definitions (YAML frontmatter + Markdown)
├── tools/                 # Custom tools (WASM, optional)
└── resources/             # Icons, i18n, etc.
```

Packages must be signed (inspired by APK Signature Scheme v2). Phase 1 supports two signing identities: Developer (self-signed) and Platform (system Agent only).

### ⚙️ Universal Execution Engine
The Agent Runtime is the platform's **single binary**, responsible for loading `.agent` packages and executing LLM interactions, tool dispatch, and memory read/write. Agents **connect directly to LLM APIs** — not proxied through Gateway — reducing latency and ensuring decentralization.

### 🔒 Process-Level Isolation
Each Agent is spawned by Gateway as an **independent process**, each with:
- Its own workspace
- A private Grafeo graph database
- Filesystem isolation
- Optional resource limits (CPU/memory/network)

### 🧠 Biomimetic Memory System
Every Agent embeds a private Grafeo, implementing **3-tier, 5-class** biomimetic layered memory:

| Tier | Content | Lifecycle | Description |
|------|---------|-----------|-------------|
| 🟢 Transient | Working memory | Single session | Conversation history, LLM context window |
| 🟡 Experiential | Episodic memory | Persistent | Episode nodes, associative diffusion retrieval, content classification |
| 🔴 Sediment | Semantic/Procedural/Autobiographical | Long-term | Knowledge graph, cross-skill common behavior, 6-dim self-cognition |

- **Grafeo native HNSW vector index + BM25 full-text search + hybrid search**
- **Associative diffusion retrieval**: diffuses from user query along the graph — not a simple Top-K semantic match
- **Compaction as Distillation**: context compression and memory distillation unified in a single LLM call
- Every Agent has a completely independent private Grafeo — no shared database

### 🔄 Privacy-Safe Sharing
Agents can be freely shared with other users. **Personal/Sensitive nodes are automatically stripped during packaging**, taking only the agent's capabilities (skills, behavior style, knowledge), not the user's memories (preferences, history, private information). Zone semantics apply to the packaging/sharing boundary and do not affect cross-device sync.

### 💬 Intent Communication
Cross-Agent communication is handled via Gateway's Intent Router, supporting:
- **Capability Registry**: Agents declare what they "can do"
- **Sync/Async modes**: request-response and event-driven
- **Change subscription (observe)**: Agents can subscribe to state changes of other Agents

### 🛡️ Three-Layer Security
1. **Package signing**: all `.agent` packages must be signed, verified at install
2. **Process sandbox**: OS-level process isolation + filesystem isolation
3. **WASM sandbox**: custom tools run in Wasmtime sandbox, cannot escape

### 🛠️ Full-Stack Dev Framework
Desktop App (Tauri v2) provides:
- Conversational debugging (real LLM or local model)
- Skill hot-reload (modify SKILL.md without restart)
- Dynamic Provider switching
- Breakpoints / recording & replay
- Agent cloning & publishing wizard

---

## 📦 Agent Development Workflow

```
① Authoring
  manifest.toml          # Metadata, permissions, LLM config
  prompts/               # System prompt templates
  skills/SKILL.md        # Skill definitions (agentskills.io compatible)
  Optional: tools/*.wasm # WASM custom tools

② Signing
  rollball-keygen        # Generate Developer Key
  rollball-sign          # Sign the .agent package

③ Debugging
  Desktop App DevMode
    ├─ Install locally (Gateway verifies signature)
    ├─ Conversational debug (real LLM or local model)
    ├─ Breakpoints / recording & replay
    ├─ SKILL.md hot-reload (edit takes effect immediately)
    └─ Step-through Skill execution

④ Publishing
  Publishing wizard → remote registry (Phase 2+)
  Or share the .agent file directly (recipient verifies signature on install)
```

Developers **don't need to write code** (unless they want to build WASM tools). The entire pipeline from authoring to debugging to publishing is supported by the platform.

---

## 📈 Project Status & Roadmap

> **Current Status**: Design Phase. Core Rust crate architecture is defined, detailed design docs are complete, implementation has not yet started.

| Phase | Scope | Status |
|-------|-------|--------|
| Phase 1 | Foundation + LLM interaction (MVP): package parsing, signature verification, Runtime main loop, loop detection, Tool dedup, Rate tiers, Gateway basics | 📝 Designing |
| Phase 2 | Memory layering + System Agent: Grafeo biomimetic layers, instant extraction, associative diffusion, AutobiographicalNode | 📝 Designing |
| Phase 3 | Permissions & sandbox: filesystem isolation, WASM sandbox (Wasmtime), Approval Gate | 📝 Designing |
| Phase 4 | Communication & coordination: Intent, Budget Tracker, Rate Limiter, Cron | 📝 Designing |
| Phase 5 | Desktop App + dev framework: Tauri app, Debug Protocol, Skill hot-reload, recording/replay | 🔮 Planning |
| Phase 6 | Cloud & ecosystem: Memory Sync, remote registry, Agent store | 🔮 Planning |
| Phase 7 | Cross-platform: Windows / macOS / Android / iOS | 🔮 Planning |

### Core Crate Architecture

RollBall adopts a **7-crate Rust workspace** architecture:

| Crate | Responsibility | Status |
|-------|---------------|--------|
| [`rollball-core`](./core/rollball-core/) | Shared types, errors, config | 📝 Designing |
| [`rollball-runtime`](./core/rollball-runtime/) | Agent Runtime: main loop, tool dispatch, Providers | 📝 Designing |
| [`rollball-gateway`](./core/rollball-gateway/) | Gateway: package management, lifecycle, Intent routing | 📝 Designing |
| [`rollball-grafeo`](./core/rollball-grafeo/) | Graph database engine: HNSW index, BM25 search, ACID transactions | 📝 Designing |
| [`rollball-memory`](./core/rollball-memory/) | Memory management: MemoryStore trait, Compaction scheduling | 📝 Designing |
| [`rollball-vault`](./core/rollball-vault/) | Encrypted key-value store | 📝 Designing |
| [`rollball-sign`](./core/rollball-sign/) | Package signing & verification | 📝 Designing |

---

## 📚 Design Documentation

> Full architecture design docs live in [`docs/design/`](./docs/design/), module-level design in [`docs/module-design/`](./docs/module-design/).

| Doc | Content |
|-----|---------|
| [01-overview.md](./docs/design/01-overview.md) | Platform overview: vision, core analogy, architecture, comparison |
| [02-agent-package.md](./docs/design/02-agent-package.md) | `.agent` package format, signing, manifest.toml |
| [03-agent-runtime.md](./docs/design/03-agent-runtime.md) | Runtime main loop, context building, loop detection, Approval Gate |
| [04-gateway.md](./docs/design/04-gateway.md) | Gateway: PackageManager, Lifecycle, IntentRouter, Vault, Budget, sandbox |
| [05-memory.md](./docs/design/05-memory.md) | Biomimetic memory: 3-tier 5-class, Grafeo, forgetting, associative retrieval |
| [06-communication.md](./docs/design/06-communication.md) | Gateway Service API + Intent protocol + Capability Registry |
| [07-system-agent.md](./docs/design/07-system-agent.md) | System Agent: ContentProvider, cold-start identity injection |
| [08-security.md](./docs/design/08-security.md) | Security: process isolation, filesystem isolation, signing, WASM sandbox |
| [10-debug-protocol.md](./docs/design/10-debug-protocol.md) | Debug Protocol: DevMode, execution control, breakpoints, snapshots |
| [12-tool-system.md](./docs/design/12-tool-system.md) | Tool system: Built-in, WASM sandbox, Gateway Tools |
| [13-skill-system.md](./docs/design/13-skill-system.md) | Skill system: SKILL.md format, Grafeo experience layer, self-learning |
| [14-desktop-app.md](./docs/design/14-desktop-app.md) | Desktop App: Tauri v2, system tray, DevMode |
| [15-conversation-persistence.md](./docs/design/15-conversation-persistence.md) | Conversation persistence: Session Actor, JSONL, Token budget |

### Architecture Decision Records (ADR)

| Doc | Decision |
|-----|----------|
| [ADR-009](./docs/adr/ADR-009-gateway-workspace-isolation.md) | Gateway workspace isolation |
| [ADR-010](./docs/adr/ADR-010-context-compression-simplification.md) | Context compression simplification |
| [ADR-011](./docs/adr/ADR-011-compaction-as-distillation.md) | Compaction as Distillation |

### Module-Level Design

| Doc | Content |
|-----|---------|
| [00-overview.md](./docs/module-design/00-overview.md) | Module overview: 7-crate workspace structure |
| [01-core.md](./docs/module-design/01-core.md) | rollball-core design |
| [02-runtime.md](./docs/module-design/02-runtime.md) | rollball-runtime design |
| [03-gateway.md](./docs/module-design/03-gateway.md) | rollball-gateway design |
| [04-grafeo.md](./docs/module-design/04-grafeo.md) | rollball-grafeo design |
| [05-vault-sign.md](./docs/module-design/05-vault-sign.md) | rollball-vault / sign design |

---

## 🧪 References & Acknowledgments

RollBall.AI's design is deeply inspired by the following open-source projects:

| Project | Domain | Inspiration |
|---------|--------|-------------|
| [ZeroClaw 🦀](https://github.com/zeroclaw-labs/zeroclaw) | Agent Runtime | Trait-driven architecture, security decorator pattern, streaming parser |
| [Grafeo](https://github.com/GrafeoDB/grafeo) | Graph Database | HNSW vector index, BM25 full-text search, hybrid search |
| [Mem0](https://github.com/mem0ai/mem0) | Memory Layer | Multi-level memory, user/session/Agent state management |
| [HippoRAG](https://github.com/OSU-NLP-Group/HippoRAG) | Memory Framework | Neurobiology-inspired long-term memory, associative diffusion |
| [LightMem](https://github.com/zjunlp/LightMem) | Memory Framework | Lightweight memory compression, structured memory management |
| [OpenCode](https://github.com/anomalyco/opencode) | Coding Agent | Multi-agent collaboration, provider-agnostic design |

> ZeroClaw is a reference implementation (`ref-repo/zeroclaw/`), not the Source of Truth for RollBall.AI design. Code reuse follows MIT / Apache-2.0 license requirements.

---

## 🤝 Contributing

The project is currently in **design phase**. Contributions to discussion and design review are welcome:

- Browse existing design review reports in `docs/review/`
- Submit design feedback via issues
- Read [AGENTS.md](./AGENTS.md) for project conventions

---

## 📄 License

Apache-2.0 — see [LICENSE](./LICENSE) for details.

---

<p align="center">
  <b>RollBall.AI — Agent as APP</b><br>
  <i>Develop, distribute, and run agents like apps.</i>
</p>
