<h1 align="center">ACowork.AI — Collaborate with your AI Colleagues</h1>

<p align="center">
  <img src="assets/brand-mark.svg" alt="ACowork" width="360">
</p>

<p align="center">
  🏗️ <strong>Declarative Agent Platform · Decentralized · High-Security · Scalable</strong><br>
  ⚡️ <strong>Easy to build an agent colleague.</strong><br>
  ⚡️ <strong>Easy to share an agent colleague.</strong><br>
  ⚡️ <strong>Easy to deploy agent colleagues.</strong>
</p>

<p align="center">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/language-Rust-ff6600" alt="Language" /></a>
  <a href="./docs/design/zh/"><img src="https://img.shields.io/badge/docs-design-brightgreen" alt="Docs" /></a>
  <a href="./apps/desktop/"><img src="https://img.shields.io/badge/status-alpha-orange" alt="Status" /></a>
</p>

<p align="center">
  <a href="README.zh.md">简体中文</a>
</p>

---

<p align="center">
  <table>
    <tr>
      <td width="50%" align="center" valign="top">
        <img src="./assets/1.jpg" alt="Multi-Agent Collaboration &amp; Memory" width="100%" />
        <br />
        <em>Collaborate with multiple AI colleagues — each with private memory, real-time context awareness, and tool execution.</em>
      </td>
      <td width="50%" align="center" valign="top">
        <img src="./assets/2.jpg" alt="Debug Panel &amp; Context Snapshots" width="100%" />
        <br />
        <em>Full-stack development framework with iterative debugging, token tracking, and context snapshots for deep insight into AI reasoning.</em>
      </td>
    </tr>
  </table>
</p>

---

ACowork.AI is a **decentralized, high-security, scalable AI Agent runtime platform** modeled after Android. Instead of just building tools, ACowork lets you create **AI colleagues** — autonomous digital beings with their own memory, workspace, and personality, each specialized in different domains, collaborating with you and each other.

Every Agent is an independent **"digital being"**: its own runtime process, private memory, workspace, and configuration — fully independent personalized cognition. Like having a team of AI specialists working alongside you — a QA analyst, a project manager, a senior engineer — each with their own expertise and memory, communicating and coordinating through the platform's Intent mechanism.

ACowork **serves two types of users**: developers build agents declaratively by tuning three dimensions — **prompt, tools, and memory** — while end-users install agents from a repository. An agent's true intelligence comes from the interplay of its system prompt, available tools, and the experience accumulated in its private memory over time. The signing toolchain + DevMode + publishing wizard form a complete developer toolkit — making **"tune prompt, tools, and memory = build an AI colleague"** a reality.

Agents can be freely shared between users — Personal/Sensitive data is automatically stripped during packaging, taking only the agent's capabilities away, leaving user's private memories behind.

### 🏪 Agent as APP — Like Android for AI

ACowork treats every Agent like an **app on your phone**. Each `.agent` package is a complete, self-contained application — just like an APK. The universal Agent Runtime is the "operating system," and the Gateway manages installations, lifecycle, and permissions — just like an app store. This means anyone can build, distribute, and run AI agents with the same ease as smartphone apps.

---

## 🏛️ Core Architecture

### Android Analogy

| Android         | ACowork         | Role                                                               |
| --------------- | ------------------- | ------------------------------------------------------------------ |
| ART             | Agent Runtime       | Universal execution engine (single binary)                         |
| APK             | `.agent` package    | Declarative bundle (config + prompts + skills, no executable code) |
| APK Signature   | Signing Block       | Package signing, verifies integrity and origin                     |
| AMS             | Gateway             | Lifecycle management (install, start/stop, budget, rate)           |
| Binder IPC      | MQTT + HTTP Reverse Proxy | IPC: real-time events + bulk query forwarding (see [ADR-033](./docs/adr/zh/ADR-033-mqtt-replace-grpc-websocket.md)) |
| ContentProvider | System Agent        | System-level data service (identity, preferences)                  |
| PMS             | Package Manager     | Install/uninstall/upgrade                                          |

### System Architecture

<p align="center">
  <img src="./assets/architecture.svg" alt="ACowork.AI System Architecture" width="100%" />
</p>

---

## 🔥 Why ACowork?

| Dimension           | LangChain / CrewAI                         | OpenCode / OpenClaw                                | ACowork.AI                                                             |
| ------------------- | ------------------------------------------ | -------------------------------------------------- | -------------------------------------------------------------------------- |
| **Architecture**    | Library/Framework: your code calls its API | Coding Agent (TUI/CLI): single-agent, task-focused | **Agent Platform**: declarative `.agent` package, universal Runtime binary |
| **Agent Model**     | Code-defined agents (Python/TS)            | Built-in agents (build/plan), skill-based          | **Declarative agents**: config + prompt + SKILL.md, zero coding            |
| **Agent Isolation** | In-process (threads/coroutines)            | Process-level, single runtime                      | **Process-level**: each Agent independent process + private Grafeo         |
| **LLM Connection**  | Your code manages LLM calls                | Direct connection per agent                        | **Direct Connect**: each Agent talks directly to LLM API, not proxied      |
| **Memory System**   | Simple RAG or vector store                 | Chat-scoped / plugin-reliant                       | **Biomimetic Layered**: 3-tier, 5-class (Grafeo graph database)            |
| **Privacy Sharing** | No privacy boundary                        | Package-level sharing                              | **Zone Isolation**: Personal/Sensitive data auto-stripped on share         |
| **Distribution**    | pip package / Docker image                 | npm / brew / script install                        | **`.agent` packages**: signed, registry distribution, APK-like             |
| **Multi-Agent**     | Code-level orchestration                   | Limited (built-in agents)                          | **Intent mechanism**: Capability Registry + message routing                |
| **Security**        | Framework-level checks                     | Tool-level approval gates                          | **3-layer**: Package signing + Process sandbox + WASM sandbox              |

---

## 🛠️ Build & Run

ACowork ships with a set of cross-platform build scripts under [`dev/`](./dev/). Prefer them over calling `cargo` directly — they handle ONNX Runtime discovery, build-profile switching, target-directory creation, and the runtime resource staging (`offline_providers.json`, `embedding_models.json`) that the Gateway and Embedding Runtime binaries read from their working directory at startup.

### Prerequisites

| Tool         | Version       | Notes                                                                                                                                  |
| ------------ | ------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Rust         | **nightly**   | `rustup default nightly`                                                                                                               |
| Node.js      | >= 18         | Desktop App and Tauri CLI                                                                                                              |
| PowerShell   | 7.x           | Required on Windows (`.ps1` scripts); `pwsh` recommended                                                                               |
| ONNX Runtime | auto-managed  | Installed by `dev/setup_ort.*` into `.ort/onnxruntime-<plat>-<arch>-<ver>/`                                                           |
| Windows      | WebView2 + VS Build Tools | Pre-installed on Windows 11; older versions need [WebView2 Evergreen Runtime](https://developer.microsoft.com/microsoft-edge/webview2/) |
| macOS        | Xcode CLT + Homebrew + `pkg-config` + `cmake` | `dev/build_macos.sh` auto-installs missing tools via `brew`                                                          |
| Linux        | Tauri v2 WebKitGTK deps + `build-essential` | See [Tauri prerequisites](https://tauri.app/start/prerequisites/)                                                    |

```bash
git clone https://github.com/tranxon/ACowork.git
cd ACowork
```

### 1. Install ONNX Runtime (one-time)

```bash
# Windows (recommended)
.\dev\setup_ort.ps1
# Optional: -Version "1.21.0"  -Reinstall  -NoMirror

# macOS / Linux / WSL / Git Bash
./dev/setup_ort.sh
# Optional: --version 1.21.0  --reinstall  --no-mirror
```

After install the dynamic libraries land in `.ort/onnxruntime-<plat>-<arch>-<ver>/lib/` and the scripts in [`dev/build_core.*`](#2-build-the-backend-gateway--runtime--embed) will auto-detect them.

### 2. Build the Backend (Gateway + Runtime + Embed)

```bash
# Windows PowerShell — default: build release, do not start
.\dev\build_core.ps1
.\dev\build_core.ps1 -Debug           # debug profile (note: PowerShell switches use a single dash, --debug is a no-op)
.\dev\build_core.ps1 -Start           # release + stop old processes + start Gateway
.\dev\build_core.ps1 -Debug -Start    # debug + restart

# bash (Linux / macOS / WSL / Git Bash) — default: build release AND start Gateway
./dev/build_core.sh
./dev/build_core.sh --debug           # debug profile
./dev/build_core.sh --debug --no-start   # debug, do not start
./dev/build_core.sh --skip-embed         # skip acowork-embed
```

**Profile resolution** (highest priority first):

1. CLI flag: `-Debug` (PowerShell) or `--debug` / `--release` (bash)
2. Environment variable: `$env:ACOWORK_BUILD_PROFILE` (PowerShell) or `$ACOWORK_BUILD_PROFILE` (bash)
3. Default: `release`

> Debug profile auto-exports `ACOWORK_GATEWAY_LOG_LEVEL=debug` for any child process spawned by the script, so a follow-up `target\debug\acowork-gateway.exe` (or `target/debug/acowork-gateway`) launched from the same shell inherits verbose logging.

### 3. macOS One-Click Build (Apple Silicon first-class)

```bash
./dev/build_macos.sh               # Apple Silicon + CoreML, release (recommended)
./dev/build_macos.sh --debug       # debug profile
./dev/build_macos.sh --cpu         # CPU only (Intel Mac or compatibility)
./dev/build_macos.sh --skip-embed  # skip acowork-embed
```

Auto-detects architecture, enables CoreML on arm64, downloads/copies ONNX Runtime, configures a Cargo registry mirror on first run, and runs `brew install pkg-config cmake` when missing.

### 4. Run the Desktop App

The Desktop App is a **frontend + thin Tauri v2 backend** — the React/TS frontend (no state persistence) drives everything via the Gateway, while the Tauri Rust side handles the system tray, the MQTT client that subscribes to real-time events on `localhost:19875`, and platform integration. REST calls go over HTTP (`http://127.0.0.1:19876`). Start it after the Gateway is running.

```bash
cd apps/desktop
npm install

# Option A — Browser-only dev server
npm run dev                        # → http://localhost:5173

# Option B — Full Tauri v2 desktop window
npm run tauri dev
```

### 5. Package a Desktop Installer (optional)

```bash
# Windows — produces MSI/NSIS bundles under apps\desktop\src-tauri\target\release\bundle\
.\dev\package_desktop_windows.ps1
# Optional: -ReinstallOrt  -NoMirror

# macOS
./dev/package_desktop_macos.sh

# Linux
./dev/package_desktop_linux.sh
```

These scripts locate `.ort/`, copy `onnxruntime.{dll,dylib,so}` into `apps/desktop/src-tauri/bin/`, then run `npm run tauri build`.

### 6. Build & Sign an Agent Package

```bash
# Windows — single agent
.\dev\build-agent.ps1 examples\senior-engineer-agent

# Windows — every example
.\dev\build-agent.ps1 -All

# bash — single agent
./dev/build-agent.sh examples/senior-engineer-agent

# bash — all examples
for d in examples/*/; do [ -f "$d/manifest.toml" ] && ./dev/build-agent.sh "$d"; done
```

Each run zips the agent directory, generates developer signing keys on first invocation (`examples/.signing-keys/`), signs the package to `<agent_id>.agent`, verifies the signature, and drops the unsigned copy.

### 7. Run CI Locally

```bash
cd core
cargo check --all
cargo clippy --all-targets -- -D warnings
cargo test --all
```

Or use the bundled script:

```bash
./dev/ci.sh all         # check + clippy + test + integration
./dev/ci.sh clippy      # clippy only
./dev/ci.sh test        # unit tests only
./dev/ci.sh integration # e2e + (when MINIMAX_API_KEY is set) real-LLM integration
```

### 📝 Try It: Write a Manifest in 30 Seconds

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
<!-- prompts/system.md -->
You are a QA Agent, helping users with quality management and code review.
```

Then run `.\dev\build-agent.ps1 .\com.example.qa-agent` (or the bash equivalent) to produce `com.example.qa-agent.agent` and install it into your local Gateway.

> **Status**: ACowork is in **alpha**. The Gateway, Runtime, Grafeo memory engine, and Desktop App are under active development. Most of the command set above is the supported workflow today; agent-install UX (e.g., `acowork-gateway install …`) is still being stabilized.

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

Packages must be signed (inspired by APK Signature Scheme v2). Two signing identities are supported: Developer (self-signed) and Platform (reserved for the system Agent).

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

| Tier           | Content                              | Lifecycle      | Description                                                            |
| -------------- | ------------------------------------ | -------------- | ---------------------------------------------------------------------- |
| 🟢 Transient    | Working memory                       | Single session | Conversation history, LLM context window                               |
| 🟡 Experiential | Episodic memory                      | Persistent     | Episode nodes, associative diffusion retrieval, content classification |
| 🔴 Sediment     | Semantic/Procedural/Autobiographical | Long-term      | Knowledge graph, cross-skill common behavior, 6-dim self-cognition     |

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
  acowork-keygen        # Generate Developer Key
  acowork-sign          # Sign the .agent package

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

Developers build agents by **tuning declarative configurations** — crafting system prompts, declaring tool capabilities, and curating memory behavior — rather than writing imperative code. The entire pipeline from authoring to debugging to publishing is supported by the platform.

---

## 📈 Project Status & Roadmap

> **Current Status**: Alpha. Core Gateway, Runtime, Grafeo memory engine, Desktop UI and the MQTT-based IPC (replacing the legacy gRPC + WebSocket stack, see [ADR-033](./docs/adr/zh/ADR-033-mqtt-replace-grpc-websocket.md)) are under active development. Architecture design docs live under [docs/design/zh/](./docs/design/zh/).

| Phase   | Scope                                                                                                                                                  | Status        |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------- |
| Phase 1 | Foundation + LLM interaction (MVP): package parsing, signature verification, Runtime main loop, loop detection, Tool dedup, Rate tiers, Gateway basics | ✅ Done       |
| Phase 2 | Memory layering + System Agent: Grafeo biomimetic layers, instant extraction, associative diffusion, AutobiographicalNode                              | 🚧 ~30–40% ¹ |
| Phase 3 | Permissions & sandbox: filesystem isolation, WASM sandbox (Wasmtime), Approval Gate                                                                    | 🚧 Partial    |
| Phase 4 | Communication & coordination: Intent, Budget Tracker, Rate Limiter, Cron                                                                               | 🚧 Partial    |
| Phase 5 | Desktop App + dev framework: Debug Protocol, Skill hot-reload, recording/replay; MQTT-based IPC refactor                                               | 🚧 In progress |
| Phase 6 | Cloud & ecosystem: Memory Sync, remote registry, Agent store                                                                                           | 🔮 Planning    |
| Phase 7 | Cross-platform: Windows / macOS / Android / iOS                                                                                                        | 🔮 Planning    |

¹ Phase 2 has substantial library code (graph diffusion, forgetting, instant extraction, generalization) but most of it is **not yet wired into the runtime** — see [`docs/review/zh/22-memory-phase2-implementation-plan.md`](./docs/review/zh/22-memory-phase2-implementation-plan.md) for the gap analysis. The functional memory_store surface that the runtime actually calls today is significantly narrower than the design document implies.

### Core Crate Architecture

ACowork adopts a **12-crate Rust workspace** under [`core/`](./core/Cargo.toml). Beyond the original seven, five have been added as scope grew (MCP integration, WASM tool SDK, ONNX-based embedding runtime, LSP relay, MQTT session/event layer):

| Crate                                                | Responsibility                                                       | Status        |
| ---------------------------------------------------- | -------------------------------------------------------------------- | ------------- |
| [`acowork-core`](./core/acowork-core/)               | Shared types, errors, config, MQTT proto definitions                 | ✅            |
| [`acowork-runtime`](./core/acowork-runtime/)         | Agent Runtime: main loop, tool dispatch, Providers                   | ✅            |
| [`acowork-gateway`](./core/acowork-gateway/)         | Gateway: package management, lifecycle, Intent routing, HTTP API, embedded MQTT broker, HTTP reverse proxy | ✅ |
| [`acowork-grafeo`](./core/acowork-grafeo/)           | Graph database engine: HNSW index, BM25 search, ACID transactions    | ✅            |
| [`acowork-memory`](./core/acowork-memory/)           | Memory management: MemoryStore trait, Compaction scheduling          | 🚧 Partial    |
| [`acowork-vault`](./core/acowork-vault/)             | Encrypted key-value store                                            | ✅            |
| [`acowork-sign`](./core/acowork-sign/)               | Package signing & verification                                       | ✅            |
| [`acowork-mcp`](./core/acowork-mcp/)                 | MCP (Model Context Protocol) client/server wrapper                   | 🚧 Partial    |
| [`acowork-tool-sdk`](./core/acowork-tool-sdk/)       | SDK for building WASM custom tools (Wasmtime host side)              | ✅            |
| [`acowork-embed`](./core/acowork-embed/)             | ONNX-Runtime-based embedding model runner                            | ✅            |
| [`acowork-lsp-relay`](./core/acowork-lsp-relay/)     | LSP protocol relay (Desktop ↔ external language servers)            | ✅            |
| [`acowork-mqtt-session`](./core/acowork-mqtt-session/) | MQTT session/event multiplexing between Gateway and Runtime children | ✅            |

> Source of truth: [`core/Cargo.toml`](./core/Cargo.toml) `[workspace] members`. Status reflects what the runtime can actually call today, not what each crate's API surface declares.

---

## 📚 Design Documentation

> Full architecture design docs live in [`docs/design/zh/`](./docs/design/zh/), module-level design in [`docs/module-design/zh/`](./docs/module-design/zh/). The legacy IPC gRPC design (`16-ipc-grpc-migration.md`) is kept for historical context — the production stack now uses MQTT + HTTP reverse-proxy, see [ADR-033](./docs/adr/zh/ADR-033-mqtt-replace-grpc-websocket.md).

| Doc                                                                                | Content                                                                       |
| ---------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| [01-overview.md](./docs/design/zh/01-overview.md)                                 | Platform overview: vision, core analogy, architecture, comparison             |
| [02-agent-package.md](./docs/design/zh/02-agent-package.md)                       | `.agent` package format, signing, manifest.toml                               |
| [03-agent-runtime.md](./docs/design/zh/03-agent-runtime.md)                       | Runtime main loop, context building, loop detection, Approval Gate            |
| [04-gateway.md](./docs/design/zh/04-gateway.md)                                   | Gateway: PackageManager, Lifecycle, IntentRouter, Vault, Budget, sandbox      |
| [05-memory.md](./docs/design/zh/05-memory.md)                                     | Biomimetic memory: 3-tier 5-class, Grafeo, forgetting, associative retrieval |
| [06-communication.md](./docs/design/zh/06-communication.md)                       | MQTT pub/sub + HTTP reverse-proxy + Intent protocol + Capability Registry    |
| [07-system-agent.md](./docs/design/zh/07-system-agent.md)                         | System Agent: ContentProvider, cold-start identity injection                  |
| [08-security.md](./docs/design/zh/08-security.md)                                 | Security: process isolation, filesystem isolation, signing, WASM sandbox      |
| [10-debug-protocol.md](./docs/design/zh/10-debug-protocol.md)                     | Debug Protocol: DevMode, execution control, breakpoints, snapshots            |
| [11-module-design.md](./docs/design/zh/11-module-design.md)                       | Module-design index — maps design docs to Rust crates                         |
| [12-tool-system.md](./docs/design/zh/12-tool-system.md)                           | Tool system: Built-in, WASM sandbox, Gateway Tools                            |
| [13-skill-system.md](./docs/design/zh/13-skill-system.md)                         | Skill system: SKILL.md format, Grafeo experience layer, self-learning         |
| [14-desktop-app.md](./docs/design/zh/14-desktop-app.md)                           | Desktop App: Tauri v2, system tray, MQTT client, DevMode                      |
| [15-conversation-persistence.md](./docs/design/zh/15-conversation-persistence.md) | Conversation persistence: Session Actor, JSONL, Token budget                  |
| [16-ipc-grpc-migration.md](./docs/design/zh/16-ipc-grpc-migration.md)             | Legacy gRPC IPC design (superseded by MQTT; see ADR-033)                      |
| [17-web-search-provider.md](./docs/design/zh/17-web-search-provider.md)           | Pluggable web-search provider abstraction                                     |
| [18-user-identity-simplified.md](./docs/design/zh/18-user-identity-simplified.md) | Simplified user-identity model                                                |
| [19-lsp-multi-language-project-root.md](./docs/design/zh/19-lsp-multi-language-project-root.md) | LSP multi-language project-root discovery & relay                |

### Architecture Decision Records (ADR)

A growing set of ADRs lives under [`docs/adr/zh/`](./docs/adr/zh/) (currently 35+ records, ADR-009 → ADR-046). Below are the foundational and recently-shipped ones; for the full list browse the directory directly.

| ADR                                                                                | Decision                                                |
| ---------------------------------------------------------------------------------- | ------------------------------------------------------- |
| [ADR-009 — Gateway workspace isolation](./docs/adr/en/ADR-009-gateway-workspace-isolation.md) | Per-process namespace isolation in the Gateway          |
| [ADR-010 — Context compression simplification](./docs/adr/zh/ADR-010-context-compression-simplification.md) | Simplified context-compression strategy                  |
| [ADR-011 — Compaction as Distillation](./docs/adr/zh/ADR-011-compaction-as-distillation.md) | Compaction implemented as memory distillation            |
| [ADR-020 — Data-flow layering](./docs/adr/zh/)                                       | Three-tier data flow (Gateway / Runtime / Agent)         |
| [ADR-031 — Drop legacy IPC, consolidate on gRPC](./docs/adr/zh/)                    | Legacy IPC cleanup (later superseded by ADR-033)         |
| [**ADR-033 — MQTT replaces gRPC + WebSocket**](./docs/adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) | **MQTT replaces gRPC + WebSocket** (current IPC architecture) |
| [ADR-034 — MQTT/HTTP boundary](./docs/adr/zh/)                                      | Responsibility split between MQTT and HTTP               |
| [ADR-035 — MQTT streaming push refactor](./docs/adr/zh/)                           | Streaming push refactor on top of MQTT                   |
| [ADR-036 — MQTT status push](./docs/adr/zh/)                                        | Status broadcast via MQTT (Will + Retained)               |
| [ADR-043 — Session config/state split](./docs/adr/zh/)                              | Split Session static config from runtime state           |
| [ADR-046 — Unified attachment entries](./docs/adr/zh/)                              | Unified model for attachment entries                     |

### Module-Level Design

| Doc                                                                                | Content                                          |
| ---------------------------------------------------------------------------------- | ------------------------------------------------ |
| [00-overview.md](./docs/module-design/zh/00-overview.md)                           | Module overview: 12-crate workspace structure    |
| [01-core.md](./docs/module-design/zh/01-core.md)                                   | `acowork-core` design                            |
| [02-runtime.md](./docs/module-design/zh/02-runtime.md)                             | `acowork-runtime` design                         |
| [03-gateway.md](./docs/module-design/zh/03-gateway.md)                             | `acowork-gateway` design                         |
| [04-grafeo.md](./docs/module-design/zh/04-grafeo.md)                               | `acowork-grafeo` design                          |
| [05-vault-sign.md](./docs/module-design/zh/05-vault-sign.md)                       | `acowork-vault` / `acowork-sign` design          |
| [06-architecture.md](./docs/module-design/zh/06-architecture.md)                   | Cross-crate architecture & dependency rules      |
| [06-ask-user-question-tool.md](./docs/module-design/zh/06-ask-user-question-tool.md) | The `ask_user_question` tool specification      |

---

## 🧪 References & Acknowledgments

ACowork.AI's design is deeply inspired by the following open-source projects:

| Project                                                 | Domain           | Inspiration                                                             |
| ------------------------------------------------------- | ---------------- | ----------------------------------------------------------------------- |
| [ZeroClaw 🦀](https://github.com/zeroclaw-labs/zeroclaw) | Agent Runtime    | Trait-driven architecture, security decorator pattern, streaming parser |
| [Grafeo](https://github.com/GrafeoDB/grafeo)            | Graph Database   | HNSW vector index, BM25 full-text search, hybrid search                 |
| [Mem0](https://github.com/mem0ai/mem0)                  | Memory Layer     | Multi-level memory, user/session/Agent state management                 |
| [HippoRAG](https://github.com/OSU-NLP-Group/HippoRAG)   | Memory Framework | Neurobiology-inspired long-term memory, associative diffusion           |
| [LightMem](https://github.com/zjunlp/LightMem)          | Memory Framework | Lightweight memory compression, structured memory management            |
| [OpenCode](https://github.com/anomalyco/opencode)       | Coding Agent     | Multi-agent collaboration, provider-agnostic design                     |

> ZeroClaw is a reference implementation (`ref-repo/zeroclaw/`), not the Source of Truth for ACowork.AI design. Code reuse follows MIT / Apache-2.0 license requirements.

---

## 🤝 Contributing

The project has moved past design into **active implementation** (Alpha). Code, design feedback, and reviews are all welcome:

- Browse existing design & code-review reports in [`docs/review/zh/`](./docs/review/zh/)
- Submit issues for bug reports, proposals, or design feedback
- Read [AGENTS.md](./AGENTS.md) for project conventions before opening a PR

---

## 📄 License

Apache-2.0 — see [LICENSE](./LICENSE) for details.

---

<p align="center">
  <b>ACowork.AI — Collaborate with your AI Colleagues</b><br>
  <i>Build and collaborate with AI agents like team members.</i>
</p>
