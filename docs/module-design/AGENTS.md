# AGENTS.md — docs/module-design/

Detailed Rust crate specifications for RollBall.AI implementation.

## OVERVIEW

7 markdown files defining the 6-crate Rust workspace structure, module responsibilities, and data flows.

## STRUCTURE

```
module-design/
├── 00-overview.md     # Workspace layout, Cargo.toml deps, crate list
├── 01-core.md         # rollball-core: shared types, protocol, traits
├── 02-runtime.md      # rollball-runtime: Agent Runtime binary
├── 03-gateway.md      # rollball-gateway: IPC gateway, lifecycle mgmt
├── 04-grafeo.md       # rollball-grafeo: graph DB + HNSW + BM25
├── 05-vault-sign.md   # rollball-vault + rollball-sign: secrets, signing
└── 06-architecture.md # Dependency graph, data flows, compilation
```

## WHERE TO LOOK

| Crate | Spec File |
|-------|-----------|
| rollball-core | `01-core.md` |
| rollball-runtime | `02-runtime.md` |
| rollball-gateway | `03-gateway.md` |
| rollball-grafeo | `04-grafeo.md` |
| rollball-vault + sign | `05-vault-sign.md` |
| Architecture overview | `06-architecture.md` |

## KEY CONVENTIONS

- **Trait-driven**: Tool, Provider, Memory, Channel all use Rust traits
- **Multi-crate**: Gateway and Runtime are separate binaries (process boundary = crate boundary)
- **Feature flags**:控制在 10 个以内 (vs ZeroClaw's 30+)
- **Config per crate**: Each crate has own config struct, not a monolithic schema
- **IPC**: Gateway↔Runtime via Unix Socket / Named Pipe (not HTTP)
