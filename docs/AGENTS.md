# AGENTS.md — docs/

Architecture design documents for RollBall.AI platform (v3.x).

## OVERVIEW

Chinese-language design docs covering platform overview, agent package format, runtime, gateway, memory, communication, security, and implementation roadmap.

## STRUCTURE

```
docs/
├── 01-overview.md          # Platform overview, Android analogy
├── 02-agent-package.md    # .agent package format, signing
├── 03-agent-runtime.md    # Runtime main loop, tool dispatch
├── 04-gateway.md          # Gateway design overview
├── 05-memory.md           # Layered memory architecture
├── 06-communication.md     # Gateway Service API, Intent
├── 07-system-agent.md     # System Agent design
├── 08-security.md         # Security design (isolation, signing)
├── 09-roadmap-and-scenarios.md  # 6-phase implementation plan
├── 10-dev-framework.md    # DevMode, Debug Protocol
├── 11-module-design.md    # Module design index
└── module-design/         # Detailed crate specs (Rust workspace)
    ├── 00-overview.md     # Workspace structure, crate list
    ├── 01-core.md         # rollball-core (shared types, protocol)
    ├── 02-runtime.md      # rollball-runtime (Agent Runtime binary)
    ├── 03-gateway.md      # rollball-gateway (Lifecycle, IntentRouter)
    ├── 04-grafeo.md       # rollball-grafeo (graph DB engine)
    ├── 05-vault-sign.md   # rollball-vault + rollball-sign
    └── 06-architecture.md # Dependency graph, data flows
```

## WHERE TO LOOK

| Need | File |
|------|------|
| Platform overview | `01-overview.md` |
| .agent package format | `02-agent-package.md` |
| Rust crate structure | `module-design/00-overview.md` |
| Security/isolation | `08-security.md` |
| Gateway components | `module-design/03-gateway.md` |
| Memory (Grafeo) | `module-design/04-grafeo.md` |
| Implementation roadmap | `09-roadmap-and-scenarios.md` |

## CONVENTIONS (THIS DIR)

- All docs in **Chinese** (中文)
- Version v3.x only — no v2.x terminology
- Rust workspace: 6 crates under `crates/rollball-*`
- Reference: ZeroClaw is reference impl, not source of truth
