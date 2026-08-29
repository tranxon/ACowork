# AGENTS.md — docs/

Public architecture & design docs for ACowork.AI (v3.x).

> **本地归档（不进 git）**：[`docs/_internal/`](./_internal/README.md) — 实施计划 / 评审报告 / 上游项目调研 / 内部诊断报告。所有 `gitignore` 拦截，仅供团队本地查阅。

## OVERVIEW

Design docs organized by language (zh/en). All original docs are in **Chinese (zh)**; English (en) translations to be added incrementally.

## STRUCTURE

```
docs/
├── AGENTS.md                # This file (index)
├── design/
│   └── zh/                  # 架构设计文档（19 篇）
│       ├── 01-overview.md                  平台总纲
│       ├── 02-agent-package.md             .agent 声明式包格式
│       ├── 03-agent-runtime.md             Agent Runtime 进程模型
│       ├── 04-gateway.md                   Gateway 生命周期 + API 边界
│       ├── 05-memory.md                    三层五类仿生记忆
│       ├── 06-communication.md             Intent / MQTT / 能力广播
│       ├── 08-security.md                  三层安全模型
│       ├── 10-debug-protocol.md            Debug Protocol（DevMode）
│       ├── 11-module-design.md             模块设计索引（→ module-design/）
│       ├── 12-tool-system.md               Tool trait + 沙箱 + WASM
│       ├── 13-skill-system.md              SKILL.md + 热加载
│       ├── 14-desktop-app.md               Tauri v2 Desktop App
│       ├── 15-conversation-persistence.md  对话持久化与 Session 恢复
│       ├── 16-ipc-grpc-migration.md        IPC 演进史（gRPC → MQTT）
│       ├── 17-web-search-provider.md       Web Search Provider 抽象
│       ├── 18-user-identity-simplified.md  用户身份简化
│       └── 19-lsp-multi-language-project-root.md  LSP relay 多语言项目根
├── module-design/
│   ├── zh/                  # Rust crate 规格文档（8 篇）
│   │   ├── 00-overview.md                  Workspace 布局 + Cargo.toml 依赖
│   │   ├── 01-core.md                      acowork-core：共享类型 / traits / proto
│   │   ├── 02-runtime.md                   acowork-runtime：主循环 / Session / Provider
│   │   ├── 03-gateway.md                   acowork-gateway：HTTP / MQTT / 生命周期
│   │   ├── 04-grafeo.md                    acowork-grafeo：图 DB + HNSW + BM25
│   │   ├── 05-vault-sign.md                acowork-vault + acowork-sign
│   │   ├── 06-architecture.md              依赖图 / 数据流 / 编译
│   │   └── 06-ask-user-question-tool.md    AskUserQuestion tool 设计
│   └── en/AGENTS.md
├── adr/
│   ├── zh/                  # 架构决策记录（49 篇）
│   └── en/ADR-009-gateway-workspace-isolation.md
├── zh/
│   ├── prd.md               # 平台需求定义
│   ├── prd-ui-ux.md         # Desktop App UI/UX 需求
│   ├── RAG-protocol-guide.md# 标准 RAG 查询协议（企业接入）
│   └── protocols/           # API 协议参考（公开文档）
│       ├── README.md        # HTTP + MQTT 双协议总览
│       ├── http.md          # Gateway HTTP REST API
│       └── mqtt.md          # 实时事件总线
├── en/
│   └── mcp-server-research.md   # MCP Server 集成参考
├── reference/
│   └── en/AGENTS.md         # 占位索引
└── _internal/               # ⚠️ gitignored — 详见 _internal/README.md
    └── archive/
        ├── plan/            # 实施计划 / 故障修复（11 篇）
        ├── review/          # 评审报告（53 篇）
        ├── reference/       # 上游项目调研（6 篇）
        ├── design/          # 评审性设计提案（1 篇）
        └── zh/              # Session 诊断报告（1 篇）
```

## WHERE TO LOOK

| Need | File |
| --- | --- |
| 平台总览 | [`design/zh/01-overview.md`](./design/zh/01-overview.md) |
| .agent 包格式 | [`design/zh/02-agent-package.md`](./design/zh/02-agent-package.md) |
| Rust crate 结构 | [`module-design/zh/00-overview.md`](./module-design/zh/00-overview.md) |
| Security / 隔离 | [`design/zh/08-security.md`](./design/zh/08-security.md) |
| Gateway 组件 | [`module-design/zh/03-gateway.md`](./module-design/zh/03-gateway.md) |
| 记忆（Grafeo） | [`module-design/zh/04-grafeo.md`](./module-design/zh/04-grafeo.md) |
| 平台 PRD | [`zh/prd.md`](./zh/prd.md) |
| Desktop UI/UX | [`zh/prd-ui-ux.md`](./zh/prd-ui-ux.md) |
| RAG 集成协议 | [`zh/RAG-protocol-guide.md`](./zh/RAG-protocol-guide.md) |
| API 协议参考（HTTP + MQTT） | [`zh/protocols/README.md`](./zh/protocols/README.md) |
| 架构决策记录 | [`adr/zh/`](./adr/zh/) |

## CONVENTIONS (THIS DIR)

- **Primary language**: 所有设计文档以 **中文** 为源语言；English 翻译随项目完成度增量补充
- **File naming**: 同名文件跨 zh/en 对应；ADR 命名 `ADR-NNN-slug.md`
- Version v3.x only — no v2.x terminology
- Rust workspace: 12 crates under `core/acowork-*`，以 [`core/Cargo.toml`](../core/Cargo.toml) `[workspace] members` 为准
- `docs/_internal/` 由 `.gitignore` 拦截，不进入开源仓库

## GITIGNORE NOTICE

```
# .gitignore
docs/_internal/
```

任何位于 `_internal/` 下的新文件不会被 git 追踪。如需公开某条归档，请单独 `git mv` 到对应公开目录。