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
  <a href="./apps/acowork-desktop/"><img src="https://img.shields.io/badge/status-alpha-orange" alt="Status" /></a>
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

## What is ACowork.AI?

ACowork.AI is a **decentralized, high-security, scalable AI Agent runtime platform** modeled after Android.
Instead of just building tools, ACowork lets you create **AI colleagues** — autonomous digital beings with their own
memory, workspace, and personality, each specialized in a different domain, collaborating with you and each other.

Every Agent is an independent **"digital being"**: its own runtime process, private memory, workspace, and configuration.
Think of having a team of AI specialists working alongside you — a QA analyst, a project manager, a senior engineer —
each with their own expertise and memory, communicating through the platform's Intent mechanism.

**Tune prompt, tools, and memory = build an AI colleague.** Personal and sensitive data is automatically stripped
during packaging, so you can share an agent's capabilities freely without leaking your private memories.

---

## ✨ Highlights

| | |
|---|---|
| 🧩 **Declarative agents** | `.agent` packages contain manifest + prompts + skills — **no executable code**, signed and verified at install time. |
| ⚙️ **Universal runtime** | A single Rust binary loads any `.agent` package; Agents connect directly to LLM APIs — no Gateway proxy, no extra latency. |
| 🔒 **Process-level isolation** | Every Agent runs as an independent OS process with its own filesystem, Grafeo DB, and sandboxed tool execution. |
| 🧠 **Biomimetic memory** | 3-tier / 5-class layered memory on a per-Agent Grafeo graph DB — HNSW + BM25 hybrid retrieval, associative diffusion. |
| 🛡️ **Three-layer security** | Package signing + OS process sandbox + Wasmtime tool sandbox. |
| 💬 **Intent collaboration** | Agents advertise capabilities in a registry, route requests/observations, sync or async, with the Gateway as broker. |
| 🌐 **Distributed by design** | Gateway as a single control-plane entry; Node Agents dispatch Runtimes to any host (GPU box / workstation / cloud) over MQTT + HTTP reverse proxy — single-host and multi-host share one protocol path. |
| 🛠️ **Full-stack dev loop** | Desktop App (Tauri v2) supports DevMode: conversational debug, skill hot-reload, breakpoints, recording & replay, publishing wizard. |

---

## 🏛️ Architecture

ACowork treats every Agent like **an app on your phone**. Each `.agent` package is a self-contained application
(like an APK); the universal Runtime is the OS; the Gateway is the cloud-side control plane; per-machine **Node Agents**
host Agent processes and act as the local auth / network-exposure boundary.

### Android Analogy

| Android         | ACowork                              | Role                                                                                                                |
| --------------- | ------------------------------------ | ------------------------------------------------------------------------------------------------------------------- |
| ART             | Agent Runtime                        | Universal execution engine (single binary, loopback-only)                                                           |
| APK             | `.agent` package                     | Declarative bundle (config + prompts + skills, no executable code)                                                 |
| APK Signature   | Signing Block                        | Package signing, verifies integrity and origin                                                                     |
| AMS             | Gateway                              | Single entry: MQTT broker host + HTTP unified entry + global resource authority (providers / MCP / budget / cron…)   |
| **OEM Service** | **Node Agent** (`acowork-node`)      | **Per-machine Runtime parent: process lifecycle + local package management + node reverse-proxy `:19900`**          |
| Binder IPC      | MQTT + HTTP Reverse Proxy            | IPC: real-time events + bulk query forwarding                                                                       |
| ContentProvider | System Agent                         | System-level data service (identity, preferences)                                                                   |

In single-host mode the Gateway auto-spawns a local Node (`local`); in distributed mode each target machine runs
its own Node via `acowork-node start`. **Gateway has zero local/remote code branches** — the same protocol path
is used everywhere. See [`docs/adr/zh/ADR-055-remote-runtime-node-topology.md`](./docs/adr/zh/ADR-055-remote-runtime-node-topology.md)
for the full design rationale.

### System Architecture

<p align="center">
  <img src="./assets/architecture.svg" alt="ACowork.AI System Architecture" width="100%" />
</p>

---

## 🚀 Quick Start

Cross-platform build scripts under [`dev/`](./dev/) handle ONNX Runtime discovery, profile switching, and resource
staging — prefer them over calling `cargo` directly.

### Prerequisites

| Tool         | Version     | Notes                                                                                                              |
| ------------ | ----------- | ------------------------------------------------------------------------------------------------------------------ |
| Rust         | **nightly** | `rustup default nightly`                                                                                           |
| Node.js      | >= 18       | Desktop App and Tauri CLI                                                                                          |
| PowerShell   | 7.x         | Required on Windows (`.ps1` scripts); `pwsh` recommended                                                          |
| ONNX Runtime | auto-managed | Installed by `dev/setup_ort.*` into `.ort/onnxruntime-<plat>-<arch>-<ver>/`                                       |

```bash
git clone https://github.com/tranxon/ACowork.git
cd ACowork
```

### Step 1 — Install ONNX Runtime (one-time)

```bash
# Windows PowerShell
.\dev\setup_ort.ps1

# macOS / Linux / WSL / Git Bash
./dev/setup_ort.sh
```

### Step 2 — Build & start the backend (Gateway + Runtime + Node)

```bash
# Windows — release build, then start Gateway
.\dev\build_core.ps1 -Start

# macOS / Linux — release build + start
./dev/build_core.sh

# Debug profile
.\dev\build_core.ps1 -Debug -Start      # Windows
./dev/build_core.sh --debug              # bash
```

macOS users on Apple Silicon can also use `./dev/build_macos.sh` for a one-click build with CoreML enabled.

### Step 3 — Launch the Desktop App

The Desktop App is a Tauri v2 shell — the React/TS frontend talks to the Gateway over HTTP, while the Rust side
handles the system tray and the MQTT client that subscribes to real-time events.

```bash
cd apps/acowork-desktop
npm install

# Browser-only dev server
npm run dev                # → http://localhost:5173

# Or full Tauri desktop window
npm run tauri dev
```

### ✍️ Try it: write a manifest in 30 seconds

```toml
# examples/qa-agent/manifest.toml
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

Then build & sign:

```bash
./dev/build-agent.sh examples/qa-agent   # produces com.example.qa-agent.agent
```

> **Status**: ACowork is in **alpha**. The Gateway, Runtime, Grafeo memory engine, and Desktop App are under active
> development. See [Roadmap](#-roadmap) for what is shipping today and what is next.

For more — packaging installers, signing, CI, remote-node onboarding — see [`docs/design/zh/`](./docs/design/zh/).

---

## 🧪 Agent Development Workflow

```
① Authoring       manifest.toml + prompts/ + skills/SKILL.md + optional tools/*.wasm
② Signing         acowork-keygen → acowork-sign  (developer key, APK-style signature)
③ Debugging       Desktop App DevMode → conversational debug, SKILL.md hot-reload, breakpoints, recording/replay
④ Publishing      Publishing wizard → remote registry, or share the .agent file directly
```

Developers build agents by **tuning declarative configurations** — system prompts, tool capabilities, memory behavior —
not writing imperative code. The whole pipeline from authoring to publishing is supported by the platform.

---

## 📈 Roadmap

| Phase | Scope                                                                                                          | Status         |
| ----- | -------------------------------------------------------------------------------------------------------------- | -------------- |
| 1     | Foundation + LLM interaction (MVP): package parsing, signing, Runtime main loop, Gateway basics                | ✅ Done         |
| 2     | Memory layering + System Agent: Grafeo biomimetic layers, instant extraction, associative diffusion           | 🚧 In progress |
| 3     | Permissions & sandbox: filesystem isolation, WASM sandbox (Wasmtime), Approval Gate                           | 🚧 Partial     |
| 4     | Communication & coordination: Intent, Budget Tracker, Rate Limiter, Cron                                       | 🚧 Partial     |
| 5     | Desktop App + dev framework: Debug Protocol, Skill hot-reload, recording/replay; MQTT-based IPC                | 🚧 In progress |
| 6     | Cloud & ecosystem: Memory Sync, remote `.agent` registry, Agent store                                         | 🔮 Planning    |
| 7     | Cross-platform: Windows / macOS / Android / iOS                                                                | 🔮 Planning    |

---

## 📚 Documentation

- Architecture design: [`docs/design/zh/`](./docs/design/zh/)
- Module-level design: [`docs/module-design/zh/`](./docs/module-design/zh/)
- Architecture Decision Records: [`docs/adr/zh/`](./docs/adr/zh/) (ADR-009 → ADR-058+)
- Code review & implementation notes: [`docs/review/zh/`](./docs/review/zh/)
- Developer conventions: [`AGENTS.md`](./AGENTS.md)

---

## 🧪 References & Acknowledgments

ACowork.AI's design draws inspiration from:

- [ZeroClaw 🦀](https://github.com/zeroclaw-labs/zeroclaw) — trait-driven runtime, security decorators, streaming parser
- [Grafeo](https://github.com/GrafeoDB/grafeo) — HNSW vector index, BM25 full-text search, hybrid search
- [Mem0](https://github.com/mem0ai/mem0) — multi-level memory, user/session/Agent state
- [HippoRAG](https://github.com/OSU-NLP-Group/HippoRAG) — neurobiology-inspired long-term memory, associative diffusion
- [LightMem](https://github.com/zjunlp/LightMem) — lightweight memory compression
- [OpenCode](https://github.com/anomalyco/opencode) — multi-agent collaboration, provider-agnostic design

---

## 🤝 Contributing

The project is in **active implementation (Alpha)**. Code, design feedback, and reviews are all welcome:

- Browse existing design & code-review reports in [`docs/review/zh/`](./docs/review/zh/)
- Open issues for bug reports, proposals, or design feedback
- Read [`AGENTS.md`](./AGENTS.md) for project conventions before opening a PR

---

## 📄 License

Apache-2.0 — see [`LICENSE`](./LICENSE) for details.

---

<p align="center">
  <b>ACowork.AI — Collaborate with your AI Colleagues</b><br>
  <i>Build and collaborate with AI agents like team members.</i>
</p>
