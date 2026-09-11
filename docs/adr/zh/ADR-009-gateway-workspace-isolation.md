# ADR-009：Gateway Workspace 隔离

**状态**：已接受（2026-09 按 ADR-055 复审并修订，见 §5）
**决策者**：大鱼
**英文原文**：[ADR-009](../en/ADR-009-gateway-workspace-isolation.md)（本文为中文对应版，含 §5 复审增补）
**前置**：
- [ADR-033](./ADR-033-mqtt-replace-grpc-websocket.md)（MQTT 替换 gRPC + WebSocket — IPC 主通道）
- [ADR-034](./ADR-034-mqtt-http-boundary.md)（MQTT / HTTP 职责边界 — Gateway 仅作 reverse proxy）
- [ADR-040](./ADR-040-runtime-adapter-use-case-layer.md)（Runtime 侧 use-case 分层）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（远程 Node 拓扑 — Gateway 与 Runtime **可跨机器**）
- [ADR-058](./ADR-058-workspace-fs-watcher-mqtt-event.md)（workspace 文件系统事件 → MQTT）
- 复审依据：[docs/review/zh/gateway-runtime-isolation-review.md](../../review/zh/gateway-runtime-isolation-review.md)

---

## 1. 背景

Gateway 历史上直接访问 agent workspace 目录 —— 读写 `{install_path}/workspace/` 下的文件以及 `{install_path}/manifest.toml`。这违反了一条基本原则：**Gateway 只管理自己的 `{data_dir}`，workspace 归 Runtime 所有。**

当时定位出 5 处违规：

| ID | 文件 | 操作 | 路径 |
|----|------|------|------|
| V1 | `workspaces.rs` | 读 + 写 | `{install_path}/workspace/config/agent_workspaces.json` |
| V2 | `agents.rs` | 读 + 写 | `{install_path}/manifest.toml` |
| V3 | `agents.rs` | 读 | `{install_path}/prompts/*.md` |
| V4 | `lifecycle/manager.rs` | 写 | `{install_path}/workspace/.identity_delivery.json` |
| V5 | `config_api.rs` | 删 | `{install_path}/workspace/logs/*.log` |

### 1.1 关键观察：停止状态的 agent 没有 UI

Desktop 对**未运行**的 agent 只显示 Status 页；Setup / Memory / Chat / Workspace 四类 UI 全部隐藏。由此推出：

- Gateway 从不需要为停止的 agent 读 workspace / config / manifest —— UI 不消费这些数据；
- Gateway 从不需要为停止的 agent 写 workspace —— 没有任何用户操作能触发；
- 停止的 agent 没有 Runtime 进程，也就没有 IPC 目标。

### 1.2 既有的 IPC 基础设施

协议里已经有几条消息类型可以承载相应数据：

- `WorkspaceContextUpdate` —— 5 个 workspace handler 中已有 3 个在用；
- `RuntimeConfigUpdate.active_tools` —— `update_agent_config` 已在推送；
- `IdentityDelivery` —— 协议已定义但从未使用；
- `LogRotate` —— 已用于运行中 agent 的日志清理。

## 2. 决策

**Gateway 永不读写 agent workspace 文件。** 规则是绝对的：

- **运行中的 agent**：所有读写走 IPC。Gateway 把数据发给 Runtime，由 Runtime 落盘到自己的 workspace。
- **停止的 agent**：API 返回"不可操作"或空数据。**没有文件兜底**。

这样双路径代码（IPC + 文件兜底）可以整个删掉。

### 2.1 迁移计划

| 违规 | 策略 | 细节 |
|------|------|------|
| V1 | IPC 推送 + Runtime 落盘 | Gateway 发送携带完整配置的 `WorkspaceContextUpdate`；Runtime 自己写 `agent_workspaces.json`。停止 → 返回空列表。同时修 bug：`update_workspace` 当时漏了 IPC 推送。 |
| V2 | 删除 `write_manifest_tools` | `active_tools` 的持久化已在 per-agent config（`{data_dir}/agent_configs/{id}.json`）。删掉 `write_manifest_tools()`；`read_manifest_tools()` 仅保留为安装期 metadata 发现用（不回写）。停止 → 返回空 tools。 |
| V3 | 删除 `read_system_prompt` | 系统提示词是 Runtime 内部事物，Gateway 无权读。停止 → 返回 null；运行中 → 系统提示词来自 per-agent config 的 `system_prompt_override`。 |
| V4 | `AgentHelloResult` 投递 | 删掉 `start_agent()` 里的 `std::fs::write(.identity_delivery.json)`；给 `AgentHelloResult` 加 `identity_entries: Vec<IdentityEntry>`。Runtime 在 IPC 握手后拿到身份并注入系统提示词。 |
| V5 | Runtime 自清理 | 删除 Phase 3（为停止的 agent 删日志）。Runtime 启动时清理自己的旧日志。或者：接受它作为"类包管理器"的例外（启动前清理）。 |

### 2.2 例外

以下对 `install_path` 的 Gateway 操作**显式允许**，因为它们管理的是"agent 的安装物"而不是"运行时 workspace"：

- **包管理器**：install、uninstall、upgrade、clone、publish；
- **Agent 列表**：`list_agents` 期间读 `agent.yaml` / `manifest.toml` 取 metadata（name、version、description）。

这些是安装期操作，不是运行期数据访问。

## 3. 后果

### 3.1 变简单的地方

- Gateway 代码更简单 —— 不再构造 workspace 路径，不再有双路径逻辑；
- 不存在 Gateway / Runtime 争抢同一文件的 race；
- 归属模型干净：Gateway 拥有 `{data_dir}`，Runtime 拥有 `{workspace}`；
- 停止态 agent 的 handler 变成"返回空 / 不可操作"，实现极简。

### 3.2 变困难的地方

- V4 需要调整 Runtime 的初始化顺序（系统提示词必须在 IPC 握手**之后**构建，而不是之前）；
- Workspace API 行为变化：停止的 agent 返回空 workspace 列表（前端本来就已适配 —— 停止态不展示 workspace UI）；
- 将来若要展示停止态 agent 的配置，必须改存到 `{data_dir}`。

### 3.3 兼容性

- 运行中的 agent：无破坏性变更（大部分操作本来就走 IPC）；
- 停止的 agent：API 由"读 workspace"变为"返回空 / null"；
- 前端已兼容 —— 停止态 agent 不展示 config / workspace / setup UI。

## 4. 当时的落地情况

| 违规 | 状态 |
|------|------|
| V1 | 已清理（workspace 全部走反向代理） |
| V2 | `write_manifest_tools` 已删；`read_manifest_tools` 改为 `#[allow(dead_code)]` 残留 |
| V3 | `read_system_prompt` 改为 `#[allow(dead_code)]` 残留 |
| V4 | 已清理 |
| V5 | 已清理 |

**注意**：V2 / V3 的"改法"（留 `#[allow(dead_code)]` 死代码）是错误的 —— 死代码是下一次违规的温床。§5 已按"直接删除"重做。

---

## 5. 复审与修订（ADR-055 之后，2026-09）

### 5.1 为什么必须复审

```
ADR-009（已接受）    时代背景：Gateway + Runtime 同进程、gRPC
   |                 Gateway 直接读 runtime 文件"能跑就算了"
ADR-040（拆进程）     Gateway → Runtime 改为 MQTT + HTTP
   |                 Gateway 内的 file-access 代码没清理
ADR-055（跨机器）     Gateway 与 Runtime 可以在不同机器
   |                 同地址访问变成跨网络 fs 访问
现状                 部分违规已从"代码气味"升级为"硬正确性故障"
```

在单机部署下，`install_path` 恰好是本机路径，违规"看起来能跑"。ADR-055 之后 `install_path` 是 **Node 本地路径**（由 Node 上报），Gateway 机器上根本不存在 —— 这类违规**直接 5xx**。

### 5.2 复审新增的违规

| ID | 位置 | 问题 | 严重度 |
|----|------|------|--------|
| V-A | `acowork-gateway/src/http/skills_api.rs` | Gateway 自己实现了一份 "Minimal SKILL.md parser"，读 `{install_path}/skills/` | 跨机器必坏 + **两套 parser 语义漂移**（已导致用户可见 bug） |
| V-B | `acowork-gateway/src/http/agents.rs`（avatar 读取端点） | `std::fs::read` 读 `{install_path}/assets/avatar*` 与 `manifest.avatar` | 跨机器必坏 |
| V-C | `acowork-gateway/src/http/agents.rs`（workspace / avatar 文件浏览） | 对照 ADR-034 §11.2.A 22a–22h 复核：**已合规** —— 全部经 `http/proxy.rs` 与 `http/workspaces.rs` 反代 | 合规 |
| V-D | `acowork-gateway/src/http/agents.rs`：`read_system_prompt` / `read_manifest_tools` / `write_manifest_tools` | `#[allow(dead_code)]` 残留死代码 | 隐患 |

### 5.3 本次修订的落地

1. **Skill 全部反代**：Runtime 新增 `GET /agents/{id}/skills`、`/skills/{name}`、`/skills/{name}/history`（`core/acowork-runtime/src/http/skills.rs`，复用唯一 parser `crate::skills::parser`）；Gateway 删除本地 parser 与三个读端点，改由 `http/proxy.rs` 反代。`POST /skills/import` 保留在 Gateway —— 它不读 Runtime 私有文件，而是委托本机 Node 控制面解包（ADR-055 §6.2），跨机器本来就正确。
2. **Avatar 读取反代**：Runtime 新增 `GET /agents/{id}/avatar`、`/avatar-file`、`/manifest/avatar-assets`（`core/acowork-runtime/src/http/avatar.rs`）；Gateway 三个读端点改反代。**写**端点（`DELETE /avatar-file`、`POST /manifest/avatar`、`avatar-config`）保留在 Gateway —— 它们还牵涉 Gateway 自有的 avatar 缓存与 publish 流程。
3. **删除 V2 / V3 / V-D 死代码**（不留 `#[allow(dead_code)]` 纪念品）。
4. **规则本身不可绕过**：`dev/ci.sh` 增加 lint（见下）；本 ADR 补中文版；`AGENTS.md` 增加一条边界规则。

### 5.4 修订后的边界规则

> **Gateway = 通信 + 资源管理 + 反代。**
> Agent Runtime 私有数据（skills、prompts、conversations、memory、agent 自己的 embedding、`{install_path}/assets`）的任何读写，**只能通过 Runtime HTTP 反代**，不得在 Gateway 进程内直接访问文件系统。

唯一例外见 §2.2（安装期包管理 + agent 列表 metadata 读取）。

### 5.5 尚未处理（另行开单）

- **（§5.4 红线残留，最高优先级）** `install_path` 是 **Node 上报的 node-local 路径**，但以下三处仍把它当文件系统根用，跨机器必坏：
  - `http/agents.rs` `update_agent_manifest_avatar`（Publish 向导写 `manifest.toml`）
  - `http/agents.rs` `validate_path_within_install` + `delete_avatar_file`（删 `{install_path}/assets/*`）

  注意 `manifest.toml` 的**内容**本来就已经在 Gateway 内存里了 —— Node 的 retained `InstalledAgentInfo` 报文直接携带 `manifest_toml` 全文（`state.rs::upsert_installed_from_node`）。所以正确修法不是"读"，而是把**写**也改成同一条路：MQTT push 给 Node → Node 自己落盘，Gateway 的 in-memory `AgentInfo.manifest` 与 avatar 缓存另行同步。
- `{install_path}/workspace` 在 4 处被拼成字符串塞进 `RunningAgentInfo.workspace`（`mqtt/dispatch.rs` ×3、`http/agents.rs` ×1）：**纯展示/日志字段，不触碰文件系统**，拓扑上无害。但这 4 处会被 §5.4 的 ceiling lint 计入，属于误报，留着作为"路径不得当真用"的哨兵。
- Embedding sidecar API（Gateway 提供）与 per-agent embedding（Runtime 内部）的边界缺少文字契约，值得补一份短 ADR。
- `agents/{id}/prompts/reload`（ADR-063 §3.7.6）：确认是 MQTT push → Runtime reload，而不是直接写文件。

## 6. 参考

- 英文原文：[ADR-009](../en/ADR-009-gateway-workspace-isolation.md)
- 复审报告：[gateway-runtime-isolation-review.md](../../review/zh/gateway-runtime-isolation-review.md)
- [ADR-055 远程 Runtime Node 拓扑](./ADR-055-remote-runtime-node-topology.md)
- [ADR-058 workspace 文件系统事件](./ADR-058-workspace-fs-watcher-mqtt-event.md)
