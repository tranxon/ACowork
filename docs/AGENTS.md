# AGENTS.md — docs/

Architecture design documents for RollBall.AI platform (v3.x).

## OVERVIEW

Design documents organized by language (zh/en). All original docs are in **Chinese (zh)**; English (en) translations to be added incrementally.

## STRUCTURE

```
docs/
├── AGENTS.md                # This file
├── zh/                      # 中文设计文档（源语言）
│   ├── prd.md               # 平台需求定义
│   ├── prd-ui-ux.md         # Desktop App UI/UX 需求
│   ├── RAG-protocol-guide.md# 标准查询协议指南
│   └── session-diagnostic.md# Session 管理诊断报告
├── en/                      # English docs
│   └── mcp-server-research.md
├── design/
│   ├── zh/                  # 架构设计文档（16篇）
│   │   ├── 01-overview.md
│   │   ├── 02-agent-package.md
│   │   ├── 03-agent-runtime.md
│   │   ├── 04-gateway.md
│   │   ├── 05-memory.md
│   │   ├── 06-communication.md
│   │   ├── 08-security.md
│   │   ├── 10-debug-protocol.md
│   │   ├── 11-module-design.md   # 模块设计索引
│   │   ├── 12-tool-system.md
│   │   ├── 13-skill-system.md
│   │   ├── 14-desktop-app.md
│   │   ├── 15-conversation-persistence.md
│   │   ├── 16-ipc-grpc-migration.md
│   │   ├── 17-web-search-provider.md
│   │   └── 18-user-identity-simplified.md
│   └── en/                  # (待翻译)
├── module-design/
│   ├── zh/                  # Rust crate 规格文档（8篇）
│   │   ├── 00-overview.md
│   │   ├── 01-core.md
│   │   ├── 02-runtime.md
│   │   ├── 03-gateway.md
│   │   ├── 04-grafeo.md
│   │   ├── 05-vault-sign.md
│   │   ├── 06-architecture.md
│   │   └── 06-ask-user-question-tool.md
│   └── en/
│       └── AGENTS.md
├── plan/
│   ├── zh/                  # 实施计划（7篇）
│   └── en/                  # (待翻译)
├── adr/
│   ├── zh/                  # 架构决策记录（3篇中文）
│   └── en/                  # 架构决策记录（1篇英文）
├── review/
│   ├── zh/                  # 设计/代码评审报告（35篇）
│   └── en/                  # (待翻译)
└── reference/
    ├── zh/                  # 参考调研文档（6篇）
    └── en/
        └── AGENTS.md
```

## WHERE TO LOOK

| Need | File |
|------|------|
| Platform overview | `design/zh/01-overview.md` |
| .agent package format | `design/zh/02-agent-package.md` |
| Rust crate structure | `module-design/zh/00-overview.md` |
| Security/isolation | `design/zh/08-security.md` |
| Gateway components | `module-design/zh/03-gateway.md` |
| Memory (Grafeo) | `module-design/zh/04-grafeo.md` |
| Implementation roadmap | `plan/zh/plan-overview.md` |

## CONVENTIONS (THIS DIR)

- **Primary language**: All design docs written in **Chinese (中文)**
- **English docs**: Created for reference materials and ADRs originally in English; new English translations follow at project completion
- **File naming**: Same filename across zh/en for correspondence
- Version v3.x only — no v2.x terminology
- Rust workspace: 7 crates under `core/rollball-*`
