# AGENTS.md — ACowork.AI

## Overview

ACowork.AI is a decentralized, high-security, scalable AI Agent runtime platform. Each Agent is a declarative `.agent` package (config + prompts + skills, no binary), loaded by a universal Agent Runtime binary and managed by a lightweight Gateway.

## Build & Test

```bash
cd core && cargo build --release
cd core && cargo clippy --all-targets -- -D warnings
cd core && cargo test
./dev/ci.sh all
```

## Debug & Log 

When debugging, consult the latest log file to rapidly identify the root cause
{HOME_DIR} is the current user home folder.

* gateway Log: {HOME_DIR}\.acowork\acowork-gateway\data\logs
* desktop Log: {HOME_DIR}\.acowork\desktop-app\logs
* runtime Log: {HOME_DIR}\.acowork\acowork-gateway\config\packages\com.acowork.senior-engineer\workspace\logs

## Project Structure

```
├── core/                    # Rust workspace (12 crates + integration tests)
│   ├── acowork-core/                # Shared types, errors, config, MQTT proto definitions
│   ├── acowork-embed/               # ONNX-Runtime-based embedding model runner
│   ├── acowork-gateway/             # Gateway: HTTP API, embedded MQTT broker, HTTP reverse proxy, lifecycle, package mgr
│   ├── acowork-grafeo/              # Memory engine (graph-based, layered)
│   ├── acowork-lsp-relay/           # LSP protocol relay (Desktop ↔ external language servers)
│   ├── acowork-mcp/                 # MCP (Model Context Protocol) wrapper
│   ├── acowork-memory/              # Memory manager (trait, middleware)
│   ├── acowork-mqtt-session/        # MQTT session / event multiplexing between Gateway and Runtime children
│   ├── acowork-runtime/             # Agent runtime (main loop, tools, providers, sessions, MQTT client, localhost HTTP)
│   ├── acowork-sign/                # Package signing & verification
│   ├── acowork-tool-sdk/            # SDK for building WASM custom tools (Wasmtime host side)
│   ├── acowork-vault/               # Encrypted key/value store
│   └── tests/                       # Integration tests
├── apps/                    # Application layer (executables)
│   ├── cli/                 # Gateway CLI (planned)
│   └── acowork-desktop/     # Tauri v2 Desktop App (frontend + thin Rust backend with system tray / MQTT client)
├── docs/                    # Architecture design docs (Chinese, v3.x)
│   ├── design/              # architecture design docs (Chinese under design/zh/, English under design/en/ — pending)
│   ├── module-design/       # Detailed module specs (crate structure)
│   ├── plan/                # Planning docs
│   ├── review/              # Design & code review reports (numbered, under review/zh/)
│   ├── adr/                 # Architecture decision records (35+, under adr/zh/)
│   ├── zh/                  # PRD, RAG protocol guide, session diagnostic, protocols/
│   │   └── protocols/       # API protocol reference (HTTP + MQTT, see §Architecture below)
│   └── reference/           # Reference materials (ZeroClaw, Grafeo, memory research)
├── examples/                # Example .agent packages
├── ref-repo/                # Reference implementation ONLY (not source of truth)
└── dev/                     # Build/Package/CI/CD scripts
```

> Workspace members source of truth: [`core/Cargo.toml`](./core/Cargo.toml) `[workspace] members`.

## Architecture

```
Desktop App (apps/acowork-desktop, Tauri v2 — 独立进程)
├── React/TS UI (no state persistence)
│   ├── Chat / Agent List / Settings UI
│   ├── Debug Panel (DevMode)
│   └── System Tray (Tauri Rust backend)
├── HTTP Client      ─→ :19876 REST
└── MQTT Subscriber  ─→ :19875 pub/sub events
        │
        ▼
Gateway (keep alive process — Rust)
├── HTTP API (Axum, :19876, localhost only)         # Desktop / CLI REST, Gateway → Runtime reverse proxy
├── MQTT Broker (rumqttd, :19875, embedded)         # Real-time events, status, Will + Retained
├── HTTP Reverse Proxy → Agent Runtime localhost HTTP  # Bulk queries: session history, config writes
├── Package Manager — install/upgrade .agent packages
├── Lifecycle Manager — spawn/kill agent processes
├── Intent Router — cross-agent messaging (Intent 主题走 MQTT)
├── Global Resources Publisher — secure API key storage, provider / mcp / embedding model list
├── Budget Tracker — usage accounting
├── Rate Limiter — request throttling
└── Global Resources — secure API key storage, provider list, mcp list, embedding model list
        │
        │ MQTT pub/sub (:19875)         ← events, status, Will+Retained
        │ HTTP Reverse Proxy →          ← bulk queries / session history
        ▼
Agent Runtime (universal binary — Rust)
├── MQTT client (rumqttc) + localhost HTTP server (random port)
├── System Agent (com.acowork.system) — identity, preferences
├── User Agents — each has private Grafeo + LLM direct connection
└── DevMode — Debug Protocol (HTTP RPC + MQTT events, ADR-048; mirrors production IPC stack)
```

**协议分工**（自 [ADR-033](./docs/adr/zh/ADR-033-mqtt-replace-grpc-websocket.md) 起统一为 HTTP + MQTT）：

- **HTTP REST**（`http://127.0.0.1:19876`）— Desktop / CLI 触发 + 配置写回 + 大数据查询；Gateway 内部转为对 Runtime localhost HTTP 的反向代理
- **MQTT**（`localhost:19875`）— 实时事件（chat chunk / tool_call / done）、状态同步（Will + Retained）、设备生命周期
- **Debug Protocol**（DevMode 专用，复用生产 IPC 通道，ADR-048）— HTTP RPC `/api/agents/{id}/debug/{*rest}`（Gateway 反代 → Runtime）+ MQTT 调试事件 `acowork/agents/{id}/debug/events/{type}`：步进调试、Skill 热加载、录制回放
- 历史 gRPC 双向流 + WebSocket 流式推送均已下线，参见 `docs/design/zh/16-ipc-grpc-migration.md` 与 `docs/zh/protocols/README.md`

## Conventions

- Design docs in both Chinese and English; Rust code comments (`//`, `//!`, `///`) **MUST be in English**
- Rust implementation follows workspace pattern under `core/` (12 crates, structure defined in `docs/module-design/zh/00-overview.md`; source of truth is `core/Cargo.toml` `[workspace] members`)
- Code reviews follow `.opencode/style-guide.md`
- The Desktop App serves solely as a state presentation and interaction interface for the gateway/runtime backend. It neither hosts business logic nor persists any state.

## Rules (Do NOT)

- Do NOT edit `ref-repo/` — it is a separate reference project, not source of truth
- Do NOT commit in Chinese
- Do NOT act before the user confirms your plan

## Key Documentation

```
├── docs/
│   ├── AGENTS.md            # guide of design docs
```
