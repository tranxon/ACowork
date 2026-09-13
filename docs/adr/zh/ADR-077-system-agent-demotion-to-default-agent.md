---
# ADR-077: System Agent 降级为预装 default 普通 Agent — 移除 Gateway 特权内置路径

**状态**：草案
**日期**：2026-09-13
**决策者**：大鱼

**前置**：
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（Node 拓扑 — System Agent 路径修订其 §6.2）
- [ADR-059](./ADR-059-parallel-onboarding-handshake.md)（Bootstrap 握手 — 本文档修订其 §4.3 / §6.1 / §7.5 / §12.2 / §15.1 中 System Agent 的 Required 子系统定位）
- [ADR-075](./ADR-075-node-identity-uuid-and-node-name.md)（Node 身份 UUID 化 — 本文档修订其 D6 `"local"` 占位边界描述）
- [ADR-073](./ADR-073-agent-instance-identity-decomposition.md)（Agent 实例身份分解 — 本 ADR 与之一致：System Agent 的 instance 身份与普通 agent 无差别）

---

## 1. 决策摘要

### 1.1 一句话

**`com.acowork.system` 从"Gateway 直管的特权内置 Runtime + BootstrapState required capability"降级为"随 Gateway 二进制分发的预装 default 普通 Agent"：Gateway 不再 auto-install / auto-start 它、不再持有它的运行时状态、不再阻止它被卸载、不再把它当 BootstrapState 的 required capability。它仍然 bundled（随 Gateway 分发），仍然通过标准 Intent 协议（`identity:query` / `identity:observe`）对外提供能力，其余安装 / 启动 / 分发 / 崩溃恢复行为跟 Calendar / Search / senior-engineer-agent 完全平权。**

### 1.2 背景原则

**Gateway 的职责边界 = 通信 + 资源管理 + 反代**（见 [AGENTS.md](../../../AGENTS.md) 与 [ADR-009 §5](./ADR-009-gateway-workspace-isolation.md)）。System Agent 承担的是"用户身份 / 偏好的语义存储与校验"这一**业务逻辑**，把它做成 Gateway 进程内特权内置组件，等于让 Gateway 反向依赖一条业务链路（identity 语义校验通过 LLM round-trip 完成、auto-start 依赖 Node retained inventory 聚合、BootstrapState ready 依赖它 ready）。这与职责边界原则直接冲突。

System Agent 的功能（identity / preference）**保留**；被移除的是它的**承载形式与特权**：

- 不再是 Gateway 进程启动链路的一部分（不阻塞 BootstrapState）；
- 不再由 Gateway 用特判代码 auto-install / auto-start；
- 不再有"不能卸载"约束；
- 它跟普通 agent 一样，装在哪个 Node，其 `installed_agents.node_id` 就是那个 Node 的 UUID。

### 1.3 语义决策

| # | 决策 | 结论 |
|---|---|---|
| D1 | System Agent 角色 | **bundled default 普通 Agent**。与 senior-engineer-agent / document-manager-agent 同类，唯一区别是随 Gateway 二进制分发（bundled），不是用户从远端 store 下载 |
| D2 | Gateway auto-install / auto-start | **整体删除**。[gateway/mod.rs](../../../core/acowork-gateway/src/gateway/mod.rs) 中等待 retained inventory → 查 install 表 → fallback bundled install → 再等 30s → 取 instance_id / node_id → 发 start → 解析 NodeEvent ack 的整段 auto-start task 删除。System Agent 的首次安装由 **onboarding** 完成（Desktop onboarding wizard 安装，或用户手动 install） |
| D3 | BootstrapState required capability | **删除**。`system_agent` 不再注册为 Required 子系统。`BootstrapState` 转 READY 不再等待 System Agent。同步修订 [ADR-059 §4.3 / §6.1 / §7.5 / §12.2 / §15.1](./ADR-059-parallel-onboarding-handshake.md) |
| D4 | "不能卸载" 约束 | **删除**。Desktop `agentStore` 中"System Agent cannot be uninstalled"守卫删除；UI 上 System Agent 与普通 agent 一致，可装可卸 |
| D5 | `installed_agents.node_id` | **不再写 `"local"` 字面量**。System Agent 与普通 agent 一样，记录其宿主 Node 的 UUID |
| D6 | `SYSTEM_AGENT_ID` 常量 | **保留**。它仍是该 agent 包的 `agent_id` 字符串（`com.acowork.system`），Intent 路由 / 包标识仍需要。删除的是"Gateway 对 `SYSTEM_AGENT_ID` 的特权处理"，不是常量本身 |
| D7 | Identity / preference 数据存储 | **不在本次范围**。身份 / 偏好数据由 Gateway 在 open / create session 时通过 HTTP / MQTT 提供给 agent，该设计与 **ADR-076（multi-user）** 合并进行。本 ADR 仅完成"降级"；System Agent 仍保留 `memory_recall` / `memory_store` tool 与 `identity:query` / `identity:observe` Intent 协议，数据源切换在 ADR-076 落地 |
| D8 | `bundled` 分发 | **保留**。System Agent 仍随 Gateway 二进制分发（`examples/system-agent`）；仅"首次是否自动安装"从"Gateway 强制"改为"onboarding / 用户决定" |
| D9 | Gateway 直管 Runtime 语义 | **收敛**。[ADR-075 D6](./ADR-075-node-identity-uuid-and-node-name.md) 描述的"`"local"` = Gateway 直管 agent 占位"收窄为"`"local"` = Gateway 进程内服务占位（doc_proxy / embedding / pm_proxy / reverse proxy 等非 .agent Runtime 的进程内调用链）"。不再存在"Gateway 直管的 .agent Runtime"这一类别 |
| D10 | 兼容性 | **不保留**。项目未上线。[gateway_data_dir] 中 `installed_agents` / `running_agents` 里 `node_id == "local"` 且 `agent_id == com.acowork.system` 的旧记录，启动时检测到即删除；由 onboarding 重新安装到本机 Node UUID |

---

## 2. 问题背景

### 2.1 现状（ADR-059 之后）

System Agent 是 Gateway 进程启动链路的一环：

```text
Gateway 启动
  → 起本地 Node
  → 等本地 Node retained installed_agents（最多 10s）
  → 查 install 表是否有 com.acowork.system
      → 没有：dispatch bundled install → 再等 30s retained
  → 取 instance_id + node_id
  → 发 Node control start 命令
  → 等 NodeEvent ack
  → 聚合 BootstrapState：system_agent = Required，ready 后 phase 转 READY
```

它同时被写进 BootstrapState 的 Required 子系统集合（[dispatch.rs](../../../core/acowork-gateway/src/mqtt/dispatch.rs) 中 `${SYSTEM_AGENT_ID} 聚合成功 → registry.register("system_agent", Required).mark_ready()`）。

### 2.2 三个具体的错位

**(a) 职责越界**：Gateway 的职责是通信 / 资源管理 / 反代。System Agent 是 identity 业务逻辑的载体。为了启动一个 identity 业务 agent，Gateway 需要：轮询 Node retained inventory、维护一个 bundled fallback install、等 30s 超时、解析 NodeEvent ack —— 这些都是"业务 agent 生命周期管理"，不是 Gateway 的职责。

**(b) 启动强耦合**：BootstrapState 的 READY 依赖 System Agent ready。任何一个 identity 业务 agent 的启动延迟（Node 慢、install 慢、LLM provider 慢）都会阻塞整个平台进入 READY，Desktop 主聊天区不可用。业务 agent 不应成为平台就绪的前置条件。

**(c) 特权不可卸载**：`agentStore` 阻止卸载 System Agent，`dispatch.rs` 对 `SYSTEM_AGENT_ID` 特判，ADR-059 把它列为 Required —— 一个"业务 agent"拥有超出所有其他 agent 的特权。这与"所有 agent 平权、实例身份一致"（ADR-073）的方向相反。

### 2.3 目标

System Agent 与普通 agent 的**唯一**区别应只剩两点：

1. **bundled**：随 Gateway 二进制分发（因为它是平台建议的 default agent）；
2. **默认建议安装**：onboarding 提示安装（因为 identity / preference 是多数用户需要的），但不强制、不阻塞、可卸载。

除此之外，安装 / 启动 / 分发 / 崩溃恢复 / BootstrapState 参与度，全部与普通 agent 一致。

---

## 3. 详细设计

### 3.1 移除 Gateway 特权路径

| 位置 | 现状 | 改后 |
|---|---|---|
| [gateway/mod.rs](../../../core/acowork-gateway/src/gateway/mod.rs) auto-start task | ~200 行：等 inventory → fallback bundled install → 等 30s → 发 start → 等 ack | **删除**。System Agent 不再由 Gateway 拉起 |
| [gateway/mod.rs](../../../core/acowork-gateway/src/gateway/mod.rs) `dispatch_bundled_agent_install` | `node_id = LOCAL_NODE_ID` 供 System Agent 调用 | 保留函数（其他 bundled 调用点仍用），**System Agent 调用点删除**；函数内部 node_id 语义按剩余调用者确定 |
| [mqtt/dispatch.rs](../../../core/acowork-gateway/src/mqtt/dispatch.rs) `SYSTEM_AGENT_ID` 特判 | 聚合 installed inventory 时 `registry.register("system_agent", Required).mark_ready()` | **删除**该特判分支，installed inventory 聚合对 System Agent 与其他 agent 一视同仁 |
| [gateway/mod.rs](../../../core/acowork-gateway/src/gateway/mod.rs) capability 注释 | 列 `system_agent` 为必需子系统 | 删除该行注释，必需子系统集合不再含 `system_agent` |

### 3.2 BootstrapState 参与度

System Agent 从 **Required 子系统**中移除。它可作为 **Optional 子系统**（若仍需在 `version` 递增时驱动 Desktop 刷新其 status），或完全不注册（Desktop 通过 `/api/agents` 常规列表获取 System Agent 状态）。

**选择完全不注册**：System Agent 的 readiness 由它自己的 Runtime 发布 `acowork/agents/{agent_id}/ready` retained 表达，Desktop 从常规 agent status 路径消费，不进 BootstrapState。理由：BootstrapState 是"平台能否安全引导"的聚合，业务 agent 的 ready 不属于此语义（§5.4 OCP：不暴露业务子系统）。

同步修订 ADR-059：§4.3 场景矩阵、§6.1 依赖 DAG（删除 `SYS_PREPARE` / `SYS_INSTALL` 节点及 `system_agent` 边）、§7.5、§12.2.3、§14、§15.1。

### 3.3 前端

| 位置 | 改动 |
|---|---|
| `apps/acowork-desktop/src/stores/agentStore.ts` | 删除 "System Agent cannot be uninstalled" 守卫 |
| `apps/acowork-desktop/src/stores/agentStore.ts` | onboarding 完成后"默认选中 System Agent"改为不强制（无显式 default 时选列表首项） |
| `apps/acowork-desktop/src/components/onboarding/OnboardingFlow.tsx` | System Agent 仍出现在 onboarding 建议列表（可勾选），但不再"必须装 / 必须启动" |
| `apps/acowork-desktop/src/components/layout/SplashScreen.tsx` | 不再 poll System Agent ready |
| `apps/acowork-desktop/src-tauri/src/commands/gateway.rs` | 移除 `dependency: SYSTEM_AGENT_ID` 的 BootstrapState 依赖声明 |

### 3.4 `"local"` 占位边界收紧（修订 ADR-075 D6）

ADR-075 D6 原文把 `"local"` 定义为"Gateway 直管 agent 占位"。System Agent 迁出后不再有"Gateway 直管的 .agent Runtime"。剩余 `node_id: "local"`（生产代码）语义收窄为：**Gateway 进程内服务占位**（doc_proxy / embedding / pm_proxy / reverse proxy / intent router 等非 .agent Runtime 的进程内调用链的占位键），不代表任何 Runtime 的宿主。

ADR-075 D6 与 [gateway/state.rs](../../../core/acowork-gateway/src/gateway/state.rs) `AgentInfo.node_id` 注释同步更新。

### 3.5 数据兼容

**不保留**历史路径。Gateway 启动时扫描 `installed_agents` / `running_agents`：

- 若存在 `node_id == "local"` 且 `agent_id == "com.acowork.system"` 的记录 → 删除（陈旧特权路径残留）；
- System Agent 由 onboarding 重新安装到本机 Node UUID。

理由与 ADR-075 D10 一致：项目未上线，无需迁移代码。

边界：本 ADR 不改变 System Agent 的 `agent_id`、`manifest.toml` 内容、prompt、skills；`manifest.toml` 中的 `system = true` 标记保留（描述性元数据，标识"随 Gateway bundled"，不再是 Gateway 特权开关）或删除（若无任何消费方），实施时确认。

---

## 4. 影响面清单

### 4.1 Rust（core/acowork-gateway）

| 文件 | 改动 |
|---|---|
| `gateway/mod.rs` | 删除 System Agent auto-start task（~200 行）；删除 `use SYSTEM_AGENT_ID`（若不再使用）；capability 注释更新 |
| `mqtt/dispatch.rs` | 删除 `SYSTEM_AGENT_ID` 的 `registry.register("system_agent", Required)` 特判分支；测试段更新 |
| `http/agents.rs` | list 排序 `sort_pins_system_agent_first` 取舍：保留为 UX 偏好（注释说明）或删除（不特判）；`resolve_agent_node_id` 中 `LOCAL_NODE_ID` 兜底语义按 D9 收紧注释 |
| `gateway/state.rs` | `AgentInfo.node_id` 注释按 D9 更新；`pub const SYSTEM_AGENT_ID` 保留 |
| `http/bootstrap_api.rs` | 测试 fixture 中 `system_agent` 从 Required 集合移除 |

### 4.2 前端（apps/acowork-desktop/src）

| 文件 | 改动 |
|---|---|
| `stores/agentStore.ts` | 删除不可卸载守卫；onboarding 默认选中逻辑调整 |
| `components/onboarding/OnboardingFlow.tsx` | System Agent 从"必须"改"建议" |
| `components/layout/SplashScreen.tsx` | 移除 System Agent ready 轮询 |
| `src-tauri/src/commands/gateway.rs` | 移除 `SYSTEM_AGENT_ID` BootstrapState 依赖 |

### 4.3 文档 / 测试

| 文件 | 改动 |
|---|---|
| `docs/adr/zh/ADR-059-parallel-onboarding-handshake.md` | §4.3 / §6.1 / §7.5 / §12.2.3 / §14 / §15.1 同步 |
| `docs/adr/zh/ADR-055-remote-runtime-node-topology.md` | §6.2 System Agent 路径描述更新 |
| `docs/adr/zh/ADR-075-node-identity-uuid-and-node-name.md` | D6 `"local"` 占位边界描述更新 |
| `core/acowork-gateway/tests/bootstrap_integration.rs` | System Agent Required 相关测试删除 / 改"非必需" |
| `core/acowork-gateway/tests/node_ready_e2e.rs` | 审计 System Agent ready 相关断言 |
| `dev/e2e_frontend_smoke/onboarding_installs_all_agents.py` | `if SYSTEM_AGENT_ID not in installed: fail` 改为"若在 installed 则校验 instance_id 为 UUID"，不再是 fail 条件 |
| `dev/e2e_frontend_smoke/smoke_test.py` | System Agent ready 不再是 fail 条件 |
| `e2e-frontend-smoke-test.md` | 同步 |
| `docs/design/{zh,en}/02-agent-package.md`、`18-user-identity-simplified.md`、`06-communication.md` | bundled agent / identity 路径描述更新；identity 存储指向 ADR-076 |
| `examples/system-agent/prompts/system.md` | 头部注明 ADR-077 后为普通 agent，身份数据源将由 Gateway 提供（ADR-076） |

### 4.4 不变

| 项目 | 说明 |
|---|---|
| `examples/system-agent/manifest.toml` | 除 `system = true` 标记取舍外不动 |
| `prompts/system.md` / `skills/` / `tools` | 不动 |
| `memory_recall` / `memory_store` tool | 不动 |
| `core/acowork-core/src/permission.rs` `IdentityRead` / `IdentityWrite` | 不动（权限定义与 agent 实现解耦） |
| `intent/privacy.rs` | 不动 |

---

## 5. 收益

1. **职责归位**：Gateway 回归通信 / 资源管理 / 反代边界，不再内嵌 identity 业务链路
2. **启动解耦**：BootstrapState READY 不再依赖任何业务 agent，冷启动关键路径缩短（删掉 10s + 30s 两段等待）
3. **代码删减**：Gateway ~200 行 auto-start 特判 + BootstrapState 一条 Required 边 + Desktop 不可卸载守卫 + 4 处测试 fixture 全部删除
4. **agent 平权**：System Agent 与普通 agent 实例身份、生命周期、可卸载性一致（对齐 ADR-073）
5. **`"local"` 边界干净**：不再有"Gateway 直管 .agent Runtime"这一混合类别

---

## 6. 待确认（实施前定稿）

| # | 问题 | 倾向 |
|---|---|---|
| Q1 | `manifest.toml` 的 `system = true` 标记保留还是删除 | 保留为描述性元数据（标识 bundled），实施时确认无消费方 |
| Q2 | `sort_pins_system_agent_first`（list 排序）保留还是删除 | 保留为 UX 偏好并加注释说明"这是展示排序，不是特权" |
| Q3 | System Agent 是否仍注册为 Optional 子系统 | 不注册：其 readiness 由自身 Runtime `ready` retained 表达，Desktop 走常规 agent status 路径 |
| Q4 | onboarding 是否默认勾选安装 System Agent | 默认勾选（多数用户需要 identity 记忆），但可取消；需持久化"用户上次卸了它则不自动勾选" |

---

## 7. 实施顺序（ADR 定稿后）

1. 删除 Gateway auto-start task（`gateway/mod.rs`）
2. 删除 `dispatch.rs` 的 `system_agent` Required 注册分支
3. 前端删除不可卸载守卫 + onboarding 调整 + SplashScreen 轮询移除
4. `"local"` 占位注释按 D9 收紧（state.rs / agents.rs / dispatch.rs）
5. 启动清理陈旧 `node_id == "local"` 的 System Agent 记录
6. 同步 ADR-059 / ADR-055 / ADR-075 引用点
7. 更新 e2e / bootstrap 测试
8. `cargo test` + `cargo clippy -- -D warnings` + e2e smoke
