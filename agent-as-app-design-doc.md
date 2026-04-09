# Agent as APP — 设计文档索引

> 版本：v3.0 | 更新日期：2026-04-09

设计一个去中心化、高安全、可扩展的 AI Agent 运行时平台。核心思想是将每个 Agent 视为独立的"应用包"（类似 Android APP），由统一的 Agent Runtime 进程加载执行，运行在客户端并由轻量级 Gateway 管理生命周期。

**核心类比：APK → .agent 包，ART → Agent Runtime，AMS → Gateway**

---

## 文档目录

| 文档 | 内容 | 原章节 |
|------|------|--------|
| [01-overview.md](./docs/01-overview.md) | 总纲：背景、核心特性、架构图、职责划分、方案对比、未来扩展 | 1 + 2 + 7 + 8 |
| [02-agent-package.md](./docs/02-agent-package.md) | Agent 打包格式：包结构、签名机制、manifest.json | 3.1 |
| [03-agent-runtime.md](./docs/03-agent-runtime.md) | Agent Runtime：启动方式、内部结构、主循环 | 3.2 |
| [04-gateway.md](./docs/04-gateway.md) | Gateway 组件：PackageManager、Lifecycle、IntentRouter、沙箱、Vault、Budget、Rate | 3.3 |
| [05-memory.md](./docs/05-memory.md) | Memory 分层架构：工作记忆、私有 Grafeo、跨 Agent 知识共享 | 3.4 |
| [06-communication.md](./docs/06-communication.md) | 通信协议：Gateway Service API + Intent 机制 | 3.5 + 3.6 |
| [07-system-agent.md](./docs/07-system-agent.md) | 系统 Agent：ContentProvider、冷启动注入、身份管理、observe 机制 | 3.7 |
| [08-security.md](./docs/08-security.md) | 安全设计：隔离、签名、权限、加密等 10 条 | 4 |
| [09-roadmap-and-scenarios.md](./docs/09-roadmap-and-scenarios.md) | 实现路线图（6 Phase）+ 使用场景示例 | 5 + 6 |

---

## 快速导航

**想了解整体架构？** → 从 [01-overview.md](./docs/01-overview.md) 开始

**想开发 Agent 包？** → 看 [02-agent-package.md](./docs/02-agent-package.md) 的 manifest.json 和签名流程

**想理解运行时机制？** → [03-agent-runtime.md](./docs/03-agent-runtime.md) 的主循环图

**想了解系统 Agent 怎么工作？** → [07-system-agent.md](./docs/07-system-agent.md) 的身份管理与 ContentProvider

**想看开发计划？** → [09-roadmap-and-scenarios.md](./docs/09-roadmap-and-scenarios.md)
