# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

RollBall.AI is a decentralized AI Agent runtime platform modeled after Android ("Agent as APP"). This repository is **currently in the design/research phase** — there is no production source code yet. The `zeroclaw/` subdirectory is a **separate reference implementation only**, not the RollBall source of truth.

The platform serves two user roles:
- **End users** — install Agents from repositories, configure API Keys, use directly
- **Agent developers** — write manifest.toml + prompts + SKILL.md, sign and publish (zero executable code)

- Design docs are written in **Chinese (中文)**
- Code and code comments use **English**
- Version: **v3.x** terminology only — never mix with v2.x

## Repository Structure

- `docs/` — Architecture design documents (17 files, Chinese, v3.x)
- `docs/module-design/` — Detailed module/crate specifications (7 files)
- `docs/plan/` — planning documents (1 file)
- `ref-doc/` — Reference materials (ZeroClaw analysis, memory research)
- `zeroclaw/` — Reference implementation (independent project, do NOT edit from this root)
- `.opencode/style-guide.md` — Code review style guide (security, memory safety, error handling)
- `AGENTS.md` — Cross-tool AI assistant instructions

## Architecture (Design Phase)

Core analogy: **Android model**

```
Desktop App (Tauri v2)
    │ HTTP API (localhost:19876)
    ▼
Gateway (standalone daemon)
├── Package Manager, Lifecycle Manager, Intent Router
├── Key Vault, Budget Tracker, Rate Limiter
└── Socket API → Agent Runtime IPC
    │
    ▼
Agent Runtime (universal binary, like Android ART)
├── System Agent (com.rollball.system)
└── User Agents (each with private Grafeo memory + direct LLM connection)
```

Key design decisions:
- `.agent` packages are **declarative only** (manifest.toml + prompts + skills, no executable code)
- **Process-level isolation**: each Agent runs as a separate process
- **Grafeo**: biomorphic 3-layer, 5-class graph memory database
- **7-crate workspace**: `rollball-core`, `rollball-memory`, `rollball-runtime`, `rollball-gateway`, `rollball-grafeo`, `rollball-vault`, `rollball-sign`

## Key Design Documents

| Topic | File |
|-------|------|
| **PRD (authoritative requirements)** | `docs/00-prd.md` |
| Platform overview & Android analogy | `docs/01-overview.md` |
| .agent package format & signing | `docs/02-agent-package.md` |
| Runtime main loop & tool dispatch | `docs/03-agent-runtime.md` |
| Gateway components | `docs/04-gateway.md` |
| Grafeo memory architecture | `docs/05-memory.md` |
| Security & isolation | `docs/08-security.md` |
| Tool system (builtin/WASM/Gateway) | `docs/12-tool-system.md` |
| Skill system | `docs/13-skill-system.md` |
| Workspace & crate structure | `docs/module-design/00-overview.md` |
| Implementation roadmap (7 phases) | `docs/09-roadmap-and-scenarios.md` |

## Requirements & Priorities

The PRD (`docs/00-prd.md`) is the authoritative source for "what to build and why". Design docs (01–14) describe "how".

Requirements use coded prefixes: PKG (packaging), FMT (format), RUN (runtime), MEM (memory), TOL (tools), SKL (skills), GTW (gateway), SYS (system agent), COM (communication), SEC (security), DSK (desktop), PLT (cross-platform), RAG (enterprise RAG).

| Priority | Meaning | Phase |
|----------|---------|-------|
| P0 | Platform core — must exist for Rollball to function | Phase 1 |
| P1 | Platform essential — significant usability impact if missing | Phase 1–2 |
| P2 | Platform enhancement — improves experience and security | Phase 3–4 |
| P3 | Ecosystem expansion — future capabilities | Phase 5–7 |

**ADRs** are recorded in PRD section 7:
- ADR-001: Enterprise RAG as independent retrieval channel, not integrated into memory abstraction
- ADR-002: PrivacyLevel controls packaging boundary only (share/export filtering), decoupled from cloud sync
- ADR-003: Memory lifecycle architecture (MemoryStore trait, MemoryManager, middleware pipeline) + Runtime extensibility principles (RXT-01~06)
- ADR-004: Hardware sensor access — 3-tier model by frequency (Intent / IPC / Direct Channel), Phase 5+
- ADR-005: Shell security — file provenance tracking + command risk grading + approval gate in Phase 1

## Commands

No build commands yet (design phase). Planned implementation commands:
```bash
cargo build --release
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
./dev/ci.sh all
```

## Conventions

- Rust Edition 2024, MSRV 1.87+
- Security-first: flag `unsafe` blocks, avoid `unwrap()`/`expect()` in production, validate all external input
- Use `Result<T>` for fallible operations, propagate with `?`
- Minimize unnecessary `.clone()` — prefer borrowing
- Review priority: CRITICAL (security/memory safety) > HIGH (logic/error handling) > MEDIUM (quality/performance) > LOW (style/docs)
- Prefer well-maintained crates with security audit history; avoid unnecessary dependencies

## ZeroClaw 代码复用准则

ZeroClaw 是完整的 Agent 软件实现，Rollball 在开发中应优先复用其代码，避免重复造轮子。

**可复用条件**：
- 代码边界清晰（完整的模块、完整的函数、独立的 trait）
- 不需要对原代码做大幅修改即可适配 Rollball 的架构
- Rollball 设计文档中明确标注"借鉴 ZeroClaw"的部分

**优先复用领域**：
- Tool trait 和工具池注册机制（`tools/`）
- Provider trait 和工厂模式
- Schema 清洗逻辑
- History Manager 的 token 计算和裁剪策略
- Streaming 响应解析
- JSON-RPC 协议处理
- Security Policy 和装饰器模式（`PathGuardedTool`、`RateLimitedTool`）

**不可直接复用（需要重新设计）**：
- 与 Rollball 架构不兼容的部分（如 ZeroClaw 单进程模式 vs Rollball 多进程 IPC 模式）
- 与 Rollball 特有的 .agent 包、签名机制、Intent 路由深度耦合的逻辑
- ZeroClaw 特定的配置格式和内部状态管理

**复用时的要求**：
- 在代码注释中标明来源：`// Adapted from ZeroClaw: <file_path>`
- 显著修改时说明改动原因：`// Rollball deviation: <reason>`
- 直接复制到 Rollball crate 内（作为 fork 或 adaptation），而非通过 workspace 依赖引用 zeroclaw crate

## Anti-Patterns

- Do NOT edit `zeroclaw/` from this project root — it is a separate reference project
- Do NOT mix v2.x terminology with v3.x design
- Do NOT put executable code in `.agent` packages
- Do NOT create a monolithic config schema — split config per crate

## 6. 建议命令（PowerShell）

> 当前电脑存在加密系统，如果需要读取文件，如果使用 Get-Content 可能会失败，改用 python -c 的方式读取：
> ```powershell
> python -c "with open('.env.development.example', 'r', encoding='utf-8') as f: print(f.read())"
> ```