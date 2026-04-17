# 设计文档 Review 报告

> 审查依据：PRD 文档 (00-prd.md) + Runtime 设计原则 (03-agent-runtime.md §9 RXT-01~06)
>
> 审查日期：2026-04-16
>
> 审查范围：docs/ 下全部设计文档（01~14）

---

## 一、审查概述

本次 review 基于三个维度进行：

1. **PRD 一致性审计**：设计是否与 PRD 需求编号对应，有无遗漏或矛盾
2. **Runtime 设计原则审计**：是否有违反 RXT-01~06 准则的设计
3. **文档间一致性审计**：各文档描述是否一致，接口定义是否对齐

---

## 二、发现的问题汇总

### 🔴 P0 - 严重问题（必须修复）

#### 问题 1：~~`identity_store` 工具定义缺失~~ ✅ 已修复

**解决方案（方案 B）**：在 12-tool-system.md 新增第 14 个内置工具 `identity_store`（系统 Agent 专用），理由：身份数据有专属语义（字段级 confidence、变更通知、结构化查询），memory_store 通用 Fact 节点无法自然承载。

**文档改动**：12-tool-system.md §2 新增 `identity_store`；07-system-agent.md §4 三条写入路径统一经 `identity_store` 出口。

---

#### 问题 2：~~07-system-agent.md §3 冷启动描述与 03-agent-runtime.md 矛盾~~ ✅ 已修复

**解决方案**：以 03-agent-runtime.md 为准（安全优先），删除命令行参数示例，改为握手 `identity_delivery` 消息流程。详见 07-system-agent.md §3。

**文档改动**：07-system-agent.md §3 重写为 Gateway → 系统 Agent 查询 → 握手 `identity_delivery` 注入流程；§4 新增身份信息三条渠道完整描述。

---

### 🟡 P1 - 重要问题（应尽快修复）

#### 问题 3：文档版本号不同步

**位置**：多个文档

| 文档 | 当前版本 | 最后更新 |
|------|---------|---------|
| 00-prd.md | v1.2 | 2026-04-16 |
| 03-agent-runtime.md | v3.4 | 2026-04-16 |
| 05-memory.md | v3.4 | 2026-04-16 |
| 01-overview.md | v3.0 | 2026-04-09 |
| 07-system-agent.md | v3.0 | 2026-04-09 |
| 08-security.md | v3.0 | 2026-04-09 |

**问题描述**：01-overview.md、07-system-agent.md、08-security.md 版本停留在 v3.0，与核心文档（v3.4）存在差距。01-overview.md 的"三层五类仿生 Memory"描述与 05-memory.md 的最新设计可能有出入。

**建议**：将 01-overview.md、07-system-agent.md、08-security.md 统一更新到 v3.4，确保描述一致。

---

#### 问题 4：~~06-communication.md 的 Capability Registry 实现细节缺失~~ ✅ 已修复

**解决方案**：Rollball 不支持隐式 Intent，所有 Intent 调用必须显式指定 `target`（Agent ID）。Capability Registry 简化为单 HashMap，`"{agent_id}:{action}"` 作为 Key，无需 priority 机制。详见 06-communication.md §2.2。

**设计要点**：
- 单 HashMap：`capabilities: HashMap<String, CapabilityDef>`
- Key 格式：`"com.weather.app:weather:query"`
- 安装：解析 manifest，依赖检查（O(1)），增量写入
- 卸载：`retain` 过滤同 agent_id 前缀条目
- 重启：扫描 manifest 重建

---

#### 问题 5：14-desktop-app.md Cargo.toml 依赖路径可能错误

**位置**：14-desktop-app.md §7.4（第 497 行）

**问题描述**：
```toml
rollball-core = { path = "../../crates/rollball-core" }
```

根据 docs/module-design/00-overview.md 的 workspace 结构，主要 crate 包括 rollball-core、rollball-memory、rollball-runtime、rollball-gateway、rollball-grafeo、rollball-vault、rollball-sign。Desktop App 如果需要复用 Memory 相关代码，应该依赖 `rollball-memory` 或 `rollball-grafeo`，而非 `rollball-core`。

**建议**：确认 Desktop App 实际需要的 crate 依赖，按需改为 `rollball-memory` 或其他。

---

#### 问题 6：06-communication.md §9 引用了不存在的章节

**位置**：06-communication.md §0 第 43 行

**问题描述**：
> Socket API 和 HTTP API 的详细定义分别在 1-2 节和 9 节（见 [04-gateway.md](./04-gateway.md) 第 9 节）。

06-communication.md 本身没有"§9"节，HTTP API 详细定义实际上在 04-gateway.md 的 §9。这句话引用的是自身文档而非外部文档，应该是笔误或结构错误。

**影响**：文档导航错误，读者可能困惑。

**建议**：修正为"见 [04-gateway.md](./04-gateway.md) §9"。

---

### 🟢 P2 - 次要问题（建议改进）

#### 问题 7：01-overview.md 与 05-memory.md 的 Memory 描述可能不一致

**位置**：01-overview.md §2 vs 05-memory.md 全文

**问题描述**：01-overview.md §2 提到"三层五类仿生 Memory"，但该文档最后更新于 2026-04-09，而 05-memory.md 在 2026-04-16 有重大更新（v3.4，记忆生命周期架构）。两者对 Memory 分层的描述可能存在不一致。

**建议**：01-overview.md §2 的 Memory 描述应与 05-memory.md 对齐，或明确标注为概要描述。

---

#### 问题 8：08-security.md 与其他文档的命名不一致

**位置**：08-security.md §6

**问题描述**：08-security.md §6 提到"API Key 不通过环境变量分发，通过 **Unix Socket** 一次性传输"。这里的"Unix Socket"在跨平台语境下不够准确——Windows 使用 Named Pipe，而非 Unix Socket。

**影响**：Windows 开发者可能误解。

**建议**：改为"通过进程间通信（Unix Socket / Named Pipe）"，与其他文档保持一致。

---

#### 问题 9：~~P2-9 已撤回~~ — review 报告误报

**原描述**：13-skill-system.md 的示例中 `edition = "2024"` 不存在。

**实际情况**：

1. `edition = "2024"` 出现在 `14-desktop-app.md` 和 `docs/module-design/00-overview.md`，而非 13-skill-system.md。
2. Rust 2024 edition 已在 Rust 1.85（2025-02-20 发布）中稳定化，`edition = "2024"` 是合法的。

**结论**：此问题为误报，无需修复。

---

## 三、Runtime 设计原则审计结果

基于 03-agent-runtime.md §9 的 RXT-01~06 准则：

| 准则 | 状态 | 说明 |
|------|------|------|
| RXT-01 依赖倒置 | ✅ 通过 | Memory 已修复（MemoryStore trait），Tool Dispatch 的 string 路由推迟 Phase 2（可接受） |
| RXT-02 生命周期钩子 | ✅ 通过 | Memory 模块已按生命周期阶段接入 |
| RXT-03 配置外置 | ✅ 通过 | 遗忘参数、循环检测阈值等均通过 manifest 注入 |
| RXT-04 中间件管线 | ✅ 计划中 | Phase 2 实现，MemoryStore trait 已预留 |
| RXT-05 存储可替换 | ✅ 通过 | MemoryStore trait + GrafeoStore 实现 |
| RXT-06 事件可观测 | ⚠️ 部分覆盖 | MemoryEventBus 已定义，但 Runtime 整体的事件发布机制未完整设计 |

**审计结论**：Memory 模块的高风险紧耦合问题已修复（v3.4），Runtime 设计原则整体合规。

---

## 四、PRD 需求对应检查

| PRD 章节 | 核心需求 | 文档覆盖 | 状态 |
|---------|---------|---------|------|
| §1.3 Agent Runtime | RUN-01~14 | 03-agent-runtime.md | ✅ 完整覆盖 |
| §1.4 Memory | MEM-01~12 | 05-memory.md | ✅ 完整覆盖 |
| §1.5 工具系统 | TOL-01~10 | 12-tool-system.md | ✅ 完整覆盖（v3.4 新增 identity_store） |
| §1.6 Skill 系统 | SKL-01~05 | 13-skill-system.md | ✅ 完整覆盖 |
| §1.7 Gateway | GTW-01~12 | 04-gateway.md | ✅ 完整覆盖 |
| §1.8 系统 Agent | SYS-01~06 | 07-system-agent.md | ✅ 完整覆盖（v3.4 身份三渠道 + Onboarding） |
| §1.9 通信协议 | COM-01~05 | 06-communication.md | ✅ 完整覆盖 |

---

## 五、修复优先级建议

| 优先级 | 问题 | 预计工作量 | 状态 |
|--------|------|-----------|------|
| P0-1 | identity_store 工具定义缺失 | 低 | ✅ 已修复（方案 B：新增专用工具） |
| P0-2 | 07-system-agent.md §3 冷启动描述矛盾 | 低 | ✅ 已修复（改用握手 identity_delivery） |
| P1-3 | 文档版本号同步 | 中（需全面 review 01/07/08） | ✅ 已修复 |
| P1-4 | Capability Registry 实现细节 | 中 | ✅ 已修复（v3.4：单 HashMap + 显式 Intent） |
| P1-5 | Desktop App Cargo 依赖路径 | 低（确认修正） | ✅ 已修复 |
| P1-6 | 06-communication.md §0 引用错误 | 低（文字修正） | ✅ 已修复 |
| P2-7 | 01-overview.md Memory 描述对齐 | 低 | ✅ 已修复 |
| P2-8 | 08-security.md §6 跨平台命名 | 低 | ✅ 已修复 |
| P2-9 | edition 版本号 | — | ❌ 误报，已撤回 |

---

## 六、附录：审计清单

- [x] 检查所有内置工具是否在文档中完整定义
- [x] 检查 Runtime 主循环与 Memory 生命周期阶段是否对齐
- [x] 检查 Gateway 不代理业务逻辑的原则是否被遵守
- [x] 检查跨文档的命名一致性
- [x] 检查版本号是否同步
- [x] 检查 RXT-01~06 准则执行情况
- [x] 检查 PRD 需求编号覆盖情况
