# AGENTS.md — ACowork.AI

## Overview

ACowork.AI is a decentralized, high-security, scalable AI Agent runtime platform. Each Agent is a declarative `.agent` package (config + prompts + skills, no binary), loaded by a universal Runtime binary and managed by a Gateway.

## Build & Test

### Core (Rust workspace, 13 crates)

```bash
cd core
cargo build --release          # all 13 crates
cargo clippy --all-targets -- -D warnings
cargo test                     # unit + integration tests
# ./dev/ci.sh all              # check + clippy + test + integration
```

### Desktop App (Tauri v2)

Required core binaries (built by `core:build:debug|release`):
`acowork-gateway`, `acowork-runtime`, `acowork-embed`, `acowork-lsp-relay`, `acowork-node`.

```bash
# Release — beforeBuildCommand auto-runs core:build:release + npm build + tauri bundle
cd apps/acowork-desktop && npm run tauri build

# Dev — build core debug once, then HMR + Tauri window
cd apps/acowork-desktop
npm run core:build:debug
npm run tauri dev
```

## Logs

`{HOME_DIR}` = current user home folder.

- Gateway: `{HOME_DIR}\.acowork\acowork-gateway\data\logs`
- Desktop: `{HOME_DIR}\.acowork\desktop-app\logs`
- Runtime: `{HOME_DIR}\.acowork\acowork-gateway\config\packages\com.acowork.senior-engineer\workspace\logs`

## Project Structure

```
core/                  # Rust workspace (13 crates; source of truth: core/Cargo.toml [workspace] members)
  acowork-core/        # Shared types, errors, config, MQTT proto
  acowork-embed/       # ONNX-Runtime embedding model runner
  acowork-gateway/     # HTTP API, embedded MQTT broker, reverse proxy, lifecycle, package mgr
  acowork-grafeo/      # Graph-based layered memory engine
  acowork-lsp-relay/   # LSP protocol relay (Desktop <-> external language servers)
  acowork-mcp/         # MCP (Model Context Protocol) wrapper
  acowork-memory/      # Memory manager (trait, middleware)
  acowork-mqtt-session/# MQTT session / event multiplexing (Gateway <-> Runtime)
  acowork-node/        # Node Agent (ADR-055) — per-machine daemon hosting Runtime processes
  acowork-runtime/     # Agent runtime (main loop, tools, providers, sessions)
  acowork-sign/        # Package signing & verification
  acowork-tool-sdk/    # WASM custom tool SDK (Wasmtime host)
  acowork-vault/       # Encrypted key/value store

apps/
  acowork-desktop/     # Tauri v2 desktop app (frontend + thin Rust backend, system tray)
  cli/                 # Gateway CLI (planned)

docs/                  # Public architecture docs
  design/{zh,en}/      # 17 design docs (zh only; en TBD)
  module-design/{zh,en}/ # Rust crate specs (8 zh + en placeholder)
  adr/{zh,en}/         # 51 ADRs (en: 1, zh: 50)
  prd/{zh,en}/         # Platform + Desktop UI/UX PRD
  protocols/{zh,en}/   # HTTP + MQTT + RAG protocol reference
  mcp-server-research/{zh,en}/

examples/              # Example .agent packages
dev/                   # Build / package / CI scripts
```

## Architecture

```
Desktop App (Tauri v2) — separate process
  React/TS UI (no state persistence) · system tray · HTTP :19876 · MQTT :19875
        |
        v
Gateway (keep-alive process, Rust)
  HTTP API :19876 (Axum, localhost) · embedded MQTT broker :19875 (rumqttd)
  Reverse proxy -> Runtime localhost HTTP · package mgr · lifecycle mgr
  Intent router · global resources · budget tracker · rate limiter
        |
        | MQTT pub/sub :19875
        v
Node Agent (acowork-node, ADR-055) — per-machine daemon
  MQTT control plane (acowork/nodes/#) · process table (spawn/kill/reap Runtime)
  Local package mgr · identity/enrollment · reverse proxy :19900
  LSP sidecar supervisor · node-local fs browse
        |
        | MQTT pub/sub :19875
        v
Agent Runtime (universal binary, Rust)
  MQTT client + localhost HTTP · system agent · user agents (private Grafeo)
  DevMode debug protocol (ADR-048)
```

## Conventions

- Design docs zh + en; Rust code comments (`//`, `//!`, `///`) MUST be English
- Workspace members source of truth: `core/Cargo.toml [workspace] members`
- Desktop App = presentation/interaction only — no business logic, no state persistence

## Rules (Do NOT)

- Do NOT commit in Chinese
- Do NOT act before the user confirms your plan
- Do NOT kill gateway or runtime process when testing — you are running inside it