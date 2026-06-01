# AGENTS.md — RollBall.AI

**Generated:** 2026-04-13
**Commit:** cec6614
**Branch:** master

## OVERVIEW

RollBall.AI is a decentralized, high-security, scalable AI Agent runtime platform modeled after Android. Each Agent is a declarative `.agent` package (config + prompts + skills, no binary), loaded by a universal Agent Runtime binary and managed by a lightweight Gateway.

## STRUCTURE

```
agent-study/
├── core/                    # Rust workspace (library crates + integration tests)
│   ├── Cargo.toml           # Workspace root
│   ├── rollball-core/       # Shared types, errors, config
│   ├── rollball-runtime/    # Agent runtime (main loop, tools, providers)
│   ├── rollball-gateway/    # Gateway (lifecycle, IPC, HTTP API)
│   ├── rollball-grafeo/     # Memory engine (graph-based, layered)
│   ├── rollball-memory/     # Memory manager (trait, middleware)
│   ├── rollball-vault/      # Encrypted key/value store
│   ├── rollball-sign/       # Package signing & verification
│   ├── tests/               # Integration tests
│   ├── rustfmt.toml
│   └── clippy.toml
├── apps/                    # Application layer (executables)
│   ├── cli/                 # Gateway CLI (planned)
│   └── desktop/             # Tauri v2 Desktop App (planned)
├── docs/                    # Architecture design docs (Chinese, v3.x)
│   ├── module-design/       # Detailed module specs (crate structure)
│   ├── plan/                # Planning docs (Chinese)
│   ├── review/              # Review reports (design + code review, numbered)
│   └── reference/           # Reference materials (ZeroClaw, Grafeo, memory research)
├── examples/                # Example .agent packages
├── ref-repo/                # Reference implementation ONLY (not source of truth)
├── dev/                     # CI/CD scripts
├── AGENTS.md                # This file
├── README.md
└── LICENSE
```

## WHERE TO LOOK

| Task | Location | Notes |
|------|----------|-------|
| Platform overview | `docs/01-overview.md` | Android analogy, core principles |
| Package format | `docs/02-agent-package.md` | `.agent` structure, signing |
| Runtime internals | `docs/03-agent-runtime.md` | Main loop, tool dispatch, Session Actor model |
| Gateway design | `docs/04-gateway.md` + `docs/module-design/03-gateway.md` | Lifecycle, IntentRouter, Vault |
| Memory architecture | `docs/05-memory.md` + `docs/module-design/04-grafeo.md` | Grafeo, layered memory, MemoryStore trait, lifecycle |
| Security design | `docs/08-security.md` + `docs/module-design/05-vault-sign.md` | Isolation, signing, permissions |
| Debug protocol | `docs/10-debug-protocol.md` | DevMode, breakpoints, recording/replay |
| Tool system | `docs/12-tool-system.md` | Built-in / WASM / Gateway tools |
| Skill system | `docs/13-skill-system.md` | SKILL.md + Grafeo experience layer |
| Desktop app | `docs/14-desktop-app.md` | Tauri layout, tray, Gateway HTTP API |
| Conversation persistence | `docs/15-conversation-persistence.md` | Session Actor model, selectedSession, JSONL, Episode, Token budget, JSONL safety |
| Communication protocol | `docs/06-communication.md` | IPC message format, gRPC session messages (v3.6), Socket API, Intent mechanism |
| Module workspace plan | `docs/module-design/00-overview.md` | 7-crate workspace structure |
| Implementation roadmap | `docs/09-roadmap-and-scenarios.md` | 7-phase plan |
| Design review reports | `docs/review/` | Design doc reviews, numbered |
| Code review reports | `docs/review/` | Source code reviews, numbered |

## ARCHITECTURE MAP

```
Desktop App (Tauri v2, 独立进程)
├── Chat / Agent List / Settings UI
├── Debug Panel (DevMode)
└── System Tray
        │
        │ Gateway HTTP API (http://127.0.0.1:19876)
        ▼
Gateway (常驻独立进程)
├── Package Manager — install/upgrade .agent packages
├── Lifecycle Manager — spawn/kill agent processes
├── Intent Router — cross-agent messaging
├── Key Vault — secure API key storage
├── Budget Tracker — usage accounting
├── Rate Limiter — request throttling
├── Socket API — Agent Runtime IPC (Unix Socket / Named Pipe)
└── HTTP API — Desktop App / CLI (Axum, localhost only)
        │
        │ Gateway Service API (Socket)
        ▼
Agent Runtime (统一二进制)
├── System Agent (com.rollball.system) — identity, preferences
├── User Agents — each has private Grafeo + LLM direct connection
└── DevMode — Debug Protocol (WebSocket, 开发者调试)
```

## CONVENTIONS (THIS PROJECT)

- Design docs in **Chinese** (中文); code comments (Rust `//`, `//!`, `///`) **MUST be in English**
- Multi-language doc translations will be produced after project completion
- `.agent` bundles are **declarative only** — no executable code
- Version v3.x terminology only — no mixing with older versions
- ref-repo is **reference only** — not source of truth for RollBall design
- Rust implementation follows workspace pattern under `core/`: `rollball-core`, `rollball-runtime`, `rollball-gateway`, `rollball-grafeo`, `rollball-vault`, `rollball-sign`

## ANTI-PATTERNS (THIS PROJECT)

- Do NOT edit `ref-repo/` — it is a separate reference project
- Do NOT mix v2.x terminology with v3.x design
- Do NOT implement executable code in `.agent` packages

## COMMANDS

Core workspace (under `core/`):
```bash
cd core && cargo build --release
cd core && cargo clippy --all-targets -- -D warnings
cd core && cargo test
./dev/ci.sh all
```

## NOTES

- Rust crate structure defined in `docs/module-design/00-overview.md`
- Code reviews follow [.opencode/style-guide.md](./.opencode/style-guide.md)
- Implementation status: **Design phase — not yet implemented**
