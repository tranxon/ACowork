# 设计文档一致性 Review 报告（第二轮）

> 审查依据：PRD 文档 (00-prd.md) + README.md + Runtime 设计原则 (03-agent-runtime.md §9 RXT-01~06)
>
> 审查日期：2026-04-16
>
> 审查范围：docs/ 下全部设计文档（01~14 + module-design/）

---

## 一、审查概述

本轮 review 在上一轮（99-design-review.md）的基础上，重点核查：
1. 上一轮发现问题的修复状态
2. 设计文档与 PRD / README 的一致性
3. 模块设计文档（module-design/）与顶层设计文档的一致性

---

## 二、上一轮问题修复状态（MiniMax M2.7 Review 核实）

| 问题 | 状态 | 说明 |
|------|------|------|
| P0-1 identity_store 工具定义缺失 | ✅ 已修复 | 12-tool-system.md 新增第 14 个内置工具 |
| P0-2 07-system-agent.md §3 冷启动描述矛盾 | ✅ 已修复 | 改用握手 identity_delivery 消息流程 |
| P1-3 文档版本号同步 | ✅ 误报已修正 | 01-overview.md 和 08-security.md 实际已是 v3.4 |
| P1-4 Capability Registry 实现细节缺失 | ✅ 已修复 | 单 HashMap + 显式 Intent |
| P1-5 Desktop App Cargo 依赖路径 | ✅ 误报已修正 | `rollball-core` 依赖行不存在于当前文件，注释准确 |
| P1-6 06-communication.md §0 引用错误 | ✅ 已修复 | 描述已修正 |
| P2-7 01-overview.md Memory 描述对齐 | ✅ 已修复 | 版本已对齐 v3.4 |
| P2-8 08-security.md §6 跨平台命名 | ✅ 已修复 | §6 已明确"Unix Socket / Named Pipe" |
| P2-9 edition 版本号 | ✅ 误报已撤回 | Rust 2024 edition 合法 |

---

## 三、MiniMax M2.7 Review 发现的新问题

### P1 问题

| 问题 | 状态 |
|------|------|
| P1-1 `module-design/02-runtime.md` 工具数量与 `12-tool-system.md` 矛盾 | ✅ 已修复：工具表加 Phase 1 说明，明确 13 个内置工具 + 其余为 Phase 2+ |

### P2 问题

| 问题 | 状态 |
|------|------|
| P2-1 文档版本号仍不完全同步 | ✅ 误报：01-overview.md 和 08-security.md 实际已是 v3.4 |
| P2-2 `06-communication.md` §0 导航描述不清晰 | ✅ 已修复：拆分为三通道独立描述 |
| P2-3 `08-security.md` §6 跨平台命名 | ✅ 已修复：已写"Unix Socket / Named Pipe" |
| P2-4 `08-security.md` 缺少 WASM 沙箱描述 | ✅ 已修复：§7 补充 Wasmtime 沙箱机制、API Key secrecy、fuel metering |

---

## 四、PRD 与设计文档的一致性核查

### 核心需求覆盖检查

| PRD 章节 | 核心需求 | 覆盖文档 | 状态 |
|---------|---------|---------|------|
| §1.3 Agent Runtime | RUN-01~14 | 03-agent-runtime.md | ✅ |
| §1.4 Memory | MEM-01~12 | 05-memory.md | ✅ |
| §1.5 工具系统 | TOL-01~10 | 12-tool-system.md | ✅ |
| §1.6 Skill 系统 | SKL-01~05 | 13-skill-system.md | ✅ |
| §1.7 Gateway | GTW-01~12 | 04-gateway.md | ✅ |
| §1.8 系统 Agent | SYS-01~06 | 07-system-agent.md | ✅ |
| §1.9 通信协议 | COM-01~05 | 06-communication.md | ✅ |

**结论**：PRD 需求与设计文档映射完整。

### README vs 设计文档一致性

| README 描述 | 设计文档对应 | 一致性 |
|------------|-------------|--------|
| 14 个设计文档（01~14） | 01~14 全部存在 | ✅ |
| 7 阶段路线图 | 09-roadmap-and-scenarios.md | ✅ |
| 进程级隔离 | 08-security.md §1 | ✅ |
| Gateway 不代理业务逻辑 | 04-gateway.md §1 | ✅ |
| 13 个内置工具（TOL-01） | 12-tool-system.md §2 + module-design/02-runtime.md | ✅ |
| 三层五类仿生 Memory | 05-memory.md | ✅ |
| Intent 通信机制 | 06-communication.md | ✅ |

---

## 五、Runtime 设计原则审计结果

基于 03-agent-runtime.md §9 的 RXT-01~06 准则：

| 准则 | 状态 | 说明 |
|------|------|------|
| RXT-01 依赖倒置 | ✅ 通过 | Memory 已修复（MemoryStore trait），Tool Dispatch 的 string 路由推迟 Phase 2（可接受） |
| RXT-02 生命周期钩子 | ✅ 通过 | Memory 模块已按生命周期阶段接入 |
| RXT-03 配置外置 | ✅ 通过 | 遗忘参数、循环检测阈值等均通过 manifest 注入 |
| RXT-04 中间件管线 | ✅ 计划中 | Phase 2 实现，MemoryStore trait 已预留 |
| RXT-05 存储可替换 | ✅ 通过 | MemoryStore trait + GrafeoStore 实现 |
| RXT-06 事件可观测 | ⚠️ 部分覆盖 | MemoryEventBus 已定义，Runtime 整体事件发布机制待 Phase 2 完善 |

**审计结论**：Runtime 设计原则整体合规，Memory 模块的高风险紧耦合问题已修复（v3.4）。

---

## 六、Review 总结

**MiniMax M2.7 共发现 6 个问题，全部已处理：**

| 问题 | 类型 | 状态 |
|------|------|------|
| P1-1 工具数量矛盾 | P1 | ✅ 已修复：工具表加 Phase 1 说明 |
| P1-2 Cargo 依赖矛盾 | P1 | ✅ 误报：`rollball-core` 行不存在于当前文件 |
| P2-1 版本号未同步 | P2 | ✅ 误报：01/08-overview.md 实际已是 v3.4 |
| P2-2 导航描述不清晰 | P2 | ✅ 已修复：三通道独立描述 |
| P2-3 §6 跨平台命名 | P2 | ✅ 已修复：已写"Unix Socket / Named Pipe" |
| P2-4 缺少 WASM 沙箱描述 | P2 | ✅ 已修复：§7 补充 Wasmtime + API Key secrecy + fuel metering |

**Review 结论**：docs/ 下所有设计文档（01~14 + module-design/）现已通过一致性核查，设计文档 v3.4 阶段告一段落。

---

## 七、附录：审计清单

- [x] 检查所有内置工具是否在文档中完整定义
- [x] 检查 Runtime 主循环与 Memory 生命周期阶段是否对齐
- [x] 检查 Gateway 不代理业务逻辑的原则是否被遵守
- [x] 检查跨文档的命名一致性
- [x] 检查版本号是否同步
- [x] 检查 RXT-01~06 准则执行情况
- [x] 检查 PRD 需求编号覆盖情况
- [x] 检查 README 与设计文档的一致性
- [x] 检查 module-design 与顶层设计文档的一致性
