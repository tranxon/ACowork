# AGENTS.md — RollBall.AI

**Generated:** 2026-04-13
**Commit:** cec6614
**Branch:** master

## OVERVIEW

RollBall.AI is a decentralized, high-security, scalable AI Agent runtime platform modeled after Android. Each Agent is a declarative `.agent` package (config + prompts + skills, no binary), loaded by a universal Agent Runtime binary and managed by a lightweight Gateway.

## STRUCTURE

```
agent-study/
├── docs/                    # Architecture design docs (Chinese, v3.x)
├── docs/module-design/      # Detailed module specs (crate structure)
├── ref-doc/                 # Reference materials (ZeroClaw, memory research)
├── zeroclaw/                # Reference implementation ONLY (not source of truth)
├── .opencode/               # OpenCode config (style-guide.md)
└── AGENTS.md               # This file
```

## WHERE TO LOOK

| Task | Location | Notes |
|------|----------|-------|
| Platform overview | `docs/01-overview.md` | Android analogy, core principles |
| Package format | `docs/02-agent-package.md` | `.agent` structure, signing |
| Runtime internals | `docs/03-agent-runtime.md` | Main loop, tool dispatch |
| Gateway design | `docs/04-gateway.md` + `docs/module-design/03-gateway.md` | Lifecycle, IntentRouter, Vault |
| Memory architecture | `docs/05-memory.md` + `docs/module-design/04-grafeo.md` | Grafeo, layered memory |
| Security design | `docs/08-security.md` + `docs/module-design/05-vault-sign.md` | Isolation, signing, permissions |
| Module workspace plan | `docs/module-design/00-overview.md` | 6-crate workspace structure |
| Implementation roadmap | `docs/09-roadmap-and-scenarios.md` | 6-phase plan |

## ARCHITECTURE MAP

```
Gateway (常驻)
├── Package Manager — install/upgrade .agent packages
├── Lifecycle Manager — spawn/kill agent processes
├── Intent Router — cross-agent messaging
├── Key Vault — secure API key storage
├── Budget Tracker — usage accounting
└── Rate Limiter — request throttling
        │
        │ Gateway Service API (Unix Socket / Named Pipe)
        ▼
Agent Runtime (统一二进制)
├── System Agent (com.rollball.system) — identity, preferences
└── User Agents — each has private Grafeo + LLM direct connection
```

## CONVENTIONS (THIS PROJECT)

- Design docs in **Chinese** (中文)
- `.agent` bundles are **declarative only** — no executable code
- Version v3.x terminology only — no mixing with older versions
- ZeroClaw is **reference only** — not source of truth for RollBall design
- Rust implementation follows workspace pattern: `rollball-core`, `rollball-runtime`, `rollball-gateway`, `rollball-grafeo`, `rollball-vault`, `rollball-sign`

## ANTI-PATTERNS (THIS PROJECT)

- Do NOT edit `zeroclaw/` — it is a separate reference project
- Do NOT mix v2.x terminology with v3.x design
- Do NOT implement executable code in `.agent` packages

## COMMANDS

This is a design/research repository — no build commands.
Implementation will use:
```bash
cargo build --release
cargo clippy --all-targets -- -D warnings
./dev/ci.sh all
```

## NOTES

- Rust crate structure defined in `docs/module-design/00-overview.md`
- Code reviews follow [.opencode/style-guide.md](./.opencode/style-guide.md)
- Implementation status: **Design phase — not yet implemented**
