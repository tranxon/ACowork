# 前端冒烟测试 E2E 需求

> 本文档定义从 Desktop App 前端视角出发的端到端冒烟测试需求。目标：启动真实
> Gateway（含嵌入式 rumqttd）与 Agent Runtime 进程，使用脚本客户端模拟前端
> 用户的基础操作，对涉及到的 MQTT 主题、HTTP REST 端点逐一冒烟，验证"打开
> App 后能做的所有基础操作"在协议层面可用。
>
> **架构前提**：依据 ADR-033，Gateway ↔ Runtime 与 Desktop ↔ Runtime 的实时
> 事件通道全部走 MQTT；HTTP 仅承担 CRUD、全量列表拉取、反向代理大数据查询。
> 本文档不再涉及 WebSocket 与 gRPC。

---

## 1. 概述

### 1.1 目标

- **覆盖范围**：Agent 管理、会话管理、聊天（MQTT 流式）、工具调用、Memory、
  Harness（Provider/Model/Embedding/Search）、Workspace、Skills、Settings、附件、
  交互审批等前端可见功能的最小可验证路径。
- **不覆盖范围**：UI 渲染细节（CSS / 拖拽 / 动画）、本地化文案、Tauri native
  桥接、平台窗口装饰。
- **典型被测 Agent**：`com.acowork.senior-engineer`（examples 中自带，工具最丰富，
  适合做工具调用冒烟）。

### 1.2 持久化数据保护原则（破坏性操作约束）

测试用例中任何**写或删**操作都必须遵守：

1. **仅修改/删除测试自身创建的资源**：禁止删除或修改 Agent 本地文件、数据库、
   配置参数等持久化数据中"非测试自身产生"的部分（包括默认工作区、预置
   session、Agent manifest 内置的 config 默认值、Vault 加密键值中预置条目、
   全局资源原始列表中已有 Provider/MCP 等）。
2. **创建 → 使用 → 删除成对出现**：每个破坏性用例应按"创建 → 使用 → 删除"
   顺序设计，测试结束时正好清理测试产生的临时数据。删除步骤可作为独立用例
   编号（如 `-D` 后缀）或作为上一用例的 teardown 子步骤。
3. **持久化数据范围**（不限于）：文件、数据库、JSON 配置文件、Vault 加密键
   值、MQTT Retained 状态、Workspace 目录树、Session 会话数据、Memory 节点、
   附件文档、全局资源列表、用户档案、Agent config / MCP / Search 配置。
4. **配置类副作用**：若测试用例需修改某全局配置（如 `log_level`、`default_provider`），
   应在用例开始前读取原值，结束前恢复。
5. **失败安全**：若创建步骤成功但删除步骤失败，测试框架必须把"待清理资源
   列表"写入日志/报告，由运维介入清理，**不允许**放任脏数据。

### 1.3 与现有测试体系的边界

| 测试类型 | 文件 | 范围 |
|---------|------|------|
| Cargo 单元/集成测试 | `core/**/tests/*.rs` | 模块内部逻辑 |
| MQTT 协议 E2E | `core/acowork-runtime/tests/mqtt_e2e.rs` | Broker + Retained + LWT 基础链路 |
| Stop 延迟脚本 | `dev/e2e_stop_test.ps1` | 单条用例：stop 控制流的延迟 |
| **本文档** | **新增脚本化测试套件** | **前端用户旅程冒烟（HTTP + MQTT）** |

本文档定义的新测试**不进** `cargo test`，应作为独立脚本套件（例如
`dev/e2e_frontend_smoke/`）由 `npm test` 或 `pytest` 驱动。

---

## 2. 测试架构

### 2.1 被测进程

| 进程 | 二进制 | 端口 | 启动方式 |
|------|--------|------|---------|
| Gateway | `target/release/acowork-gateway` | HTTP `:19876`、MQTT `:19875` | 测试夹具启动；嵌入式 rumqttd 自动随 Gateway 启动 |
| Agent Runtime | `target/release/acowork-runtime` | localhost HTTP `:随机` | 由 Gateway 在 `POST /api/agents/{id}/start` 时拉起，Runtime 启动后再向 Gateway HTTP 注册并直连 Broker |

> Gateway 与 Runtime 的启动顺序：先 Gateway 就绪（`/health` 200，Broker `:19875`
> 已监听），再触发 Agent start。Agent Runtime 不直接被脚本拉起。

### 2.2 客户端模拟器

- 形式：脚本客户端（推荐 Node.js / Python）。
- **必须同时具备** MQTT 客户端与 HTTP 客户端：
  - MQTT：`paho-mqtt` / `rumqttc` / `asyncio-mqtt`（任选），broker 默认 `:19875`
  - HTTP：`fetch` / `requests` / `httpx`，gateway 默认 `:19876`
- **不**启动 Tauri WebView。
- 测试客户端 MQTT 身份：`user:smoke-test:desktop:{pid}` 风格（参见 `mqtt.md §8.5`）。

### 2.3 协议参考

测试中所有请求/响应字段必须以以下文档为准：

- MQTT 主题树与 payload 格式：[`/docs/zh/protocols/mqtt.md`](../../zh/protocols/mqtt.md)
- HTTP REST：[`/docs/zh/protocols/http.md`](../../zh/protocols/http.md)
- 协议纲要（端口、错误码、鉴权、ACL）：[`/docs/zh/protocols/README.md`](../../zh/protocols/README.md)
- 架构决策依据：[`/docs/adr/zh/ADR-033-mqtt-replace-grpc-websocket.md`](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md)

---

## 3. 前置准备

### 3.1 编译与进程准备

```bash
cd core && cargo build --release
./target/release/acowork-gateway --port 19876 &
# 等待 /health 返回 200（脚本侧带重试，最长 30 s）
# 验证 Broker 已起：尝试 SUBSCRIBE acowork/global/#（无需消息，立即成功即可）
```

### 3.2 安装被测 Agent

> 安装动作属于"创建操作"，fixture 仅安装**本测试专用**的 Agent 包；不应在
> fixture 中安装多个 Agent 以免污染环境。

```bash
curl -s -F package=@examples/agent-packages/com.acowork.senior-engineer.agent \
     -H "Authorization: Bearer $TOKEN" \
     http://127.0.0.1:19876/api/agents/install
```

校验：`GET /api/agents` 返回中含 `com.acowork.senior-engineer`。

### 3.3 Provider / Model 前置

- 测试若需要可用 provider 模型，fixture 调用 `POST /api/global/providers`
  添加一条 `id="smoke-<random>"` 的 provider；后续用例按 `1.2` 约束在用例
  末尾 `DELETE` 清理。
- 若需设置 default provider/model，使用 fixture 记录原值，teardown 时恢复。

### 3.4 工具启用

senior-engineer 包通过 manifest `[[tools]]` 声明了 file_read 等（opt-in 模型）。
若 TC-CHAT-08 / TC-SETUP-03 需触发 file_read 工具调用，**仅在测试期间**通过
`PUT /api/agents/com.acowork.senior-engineer/config` 临时启用；用例结束后
恢复原值。

---

## 4. 前端页面/面板 → 协议端点映射

### 4.1 NavBar 一级导航

| 前端视图 | 组件 | 通道 |
|---------|------|------|
| **Chat**（默认） | `AppLayout` + `ChatPanel` | 4.2 全部端点 |
| **Harness** | `HarnessPage` | `/api/global/providers`、`/api/global/mcps`、`/api/global/lsps`、`/api/global/searches`、`/api/global/embedding_models` |
| **Settings** | `SettingsPage` | `/api/status`、`/api/config`、`/api/users`、`/api/user/avatar-*` |
| Projects | （TODO 占位） | 暂无功能，脚本跳过 |
| Docs | （TODO 占位） | 暂无功能，脚本跳过 |

### 4.2 Chat 视图内面板

| 面板 | 组件 | 通道 |
|------|------|------|
| Agent 列表 | `AgentList` | HTTP `GET /api/agents`、`POST /api/agents/{id}/start`、`POST /api/agents/{id}/stop` + MQTT SUB `acowork/agents/+/status` |
| 会话标签栏 | `SessionTabBar` | HTTP `GET /api/agents/{id}/sessions` + MQTT SUB `acowork/agents/+/sessions/created`/`deleted` |
| 消息列表 | `VirtualMessageList` / `MessageBubble` | HTTP `GET /api/agents/{id}/sessions/{sid}/messages`、`GET /api/agents/{id}/sessions/{sid}/state` + MQTT SUB `acowork/agents/{id}/sessions/{sid}/messages/#` |
| 输入框 | `ChatPanel` | MQTT PUB `acowork/agents/{id}/sessions/control/message` + SUB 上述 messages/# |
| Workspace | `WorkspaceExplorer` | `/api/agents/{id}/workspaces/*` |
| Memory | `MemoryPanel` | `/api/agents/{id}/memory/*` |
| Setup | `AgentSetupTab` | `/api/agents/{id}/config`、`/api/agents/{id}/mcp-servers`、`/api/agents/{id}/search-*` |
| Tools | `ToolsTab` | `/api/agents/{id}/config`（tools section） |
| Status | `StatusPanel` | `/api/agents/{id}/sessions/{sid}/state`、`/api/status` + MQTT SUB `acowork/agents/+/status` |
| Debug | `DebugPanel` | Debug 通道；`POST /api/agents/{id}/restart-debug` |

### 4.3 导航参考

- `NavBar`：`apps/acowork-desktop/src/components/layout/NavBar.tsx`
- 右侧 tabs 定义：`apps/acowork-desktop/src/stores/layoutStore.ts`（`PanelTab`）
- AppLayout 路由分发：`apps/acowork-desktop/src/components/layout/AppLayout.tsx`

---

## 5. 测试用例

每个用例命名 `TC-<模块>-<编号>`。所有响应字段断言均以 §2.3 中的协议文档为准。
**所有涉及写/删操作的用例**严格遵循 §1.2 持久化数据保护原则——前置创建、
末尾清理。

### 5.1 Chat 核心流程（必跑）

#### TC-CHAT-01 获取 Agent 列表

- **步骤**：`GET /api/agents`
- **期望**：响应 `200`，数组中含 `com.acowork.senior-engineer`，每项含
  `agent_id`、`name`、`status`、`installed`、`avatar` 字段。

#### TC-CHAT-02 启动 senior-engineer Agent 并订阅上线事件

- **前置**：TC-CHAT-01 已通过，Agent 已安装但未运行。
- **步骤**：
  1. MQTT SUB `acowork/agents/com.acowork.senior-engineer/status`（QoS 1）
  2. MQTT SUB `acowork/agents/com.acowork.senior-engineer/meta`（QoS 1）
  3. `POST /api/agents/com.acowork.senior-engineer/start`
  4. 等待 status 收到 `online`（retained），超时 30 s
- **期望**：`/start` 返回 `200 {status: "started"|"already_running"}`；`status`
  topic 收到 retained `"online"`；`meta` 收到完整 AgentMeta。

#### TC-CHAT-03 加载会话列表

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/sessions`
- **期望**：响应 `200`，数组中每项含 `session_id`、`title`、`updated_at`、
  `message_count`。

#### TC-CHAT-04 加载最新会话

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/latest-session`
- **期望**：响应 `200`，含 `session_id` 与 `session` 字段。

#### TC-CHAT-05 获取最新会话消息及元数据

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/sessions/{sid}/messages`
- **期望**：响应 `200`，含 `messages[]`（每项含 `role`、`content`、
  `message_id`、`created_at`）、`metadata`（含 `created_at`、`message_count`、
  `model`、`provider`）。

#### TC-CHAT-06 创建新会话（仅创建，由后续用例删除）

- **步骤**：
  1. MQTT PUB `acowork/agents/com.acowork.senior-engineer/sessions/control/create_session`
     - Payload：`CreateSessionCommand { agent_id }`（不含 sid/title）
  2. MQTT SUB `acowork/agents/com.acowork.senior-engineer/sessions/created`
     （QoS 1，已在 AppLayout 默认订阅之列，此处显式声明）
- **期望**：
  - 收到 `sessions/created` 事件，payload 含 `sid`（Runtime 分配）与 `title`
  - 记下 `sid` 作为后续用例输入
- **teardown**（TC-CHAT-10 同步执行）：见 §5.1.1。

#### TC-CHAT-07 发送打招呼消息并等待回复

- **前置**：TC-CHAT-02 Agent 已运行；TC-CHAT-06 已拿到新 `sid`。
- **步骤**：
  1. MQTT SUB `acowork/agents/com.acowork.senior-engineer/sessions/{sid}/messages/#`（QoS 0）
  2. MQTT SUB `acowork/agents/com.acowork.senior-engineer/sessions/{sid}/meta`（QoS 1）
  3. MQTT PUB `acowork/agents/com.acowork.senior-engineer/sessions/control/message`
     - Payload：`{ agent_id, sid, message_id: "msg-001", content: "hi" }`
  4. 收集 `messages/*` 事件直至收到 `messages/done`，超时 30 s
- **期望**：
  - 收到 `messages/chunk` 至少 1 次，delta 非空
  - 收到 `messages/done`，payload 含 `message_id == "msg-001"` 与 `usage`
  - `meta` retained 更新，含 usage 等指标
- **无副作用**：该用例仅消费消息，不创建任何持久化数据。

#### TC-CHAT-08 触发 `file_read` 工具调用

- **前置**：TC-CHAT-07 同一 `sid`；§3.4 已启用 `tools.file_read`。
- **步骤**：
  1. MQTT PUB `acowork/agents/com.acowork.senior-engineer/sessions/control/message`
     - Payload：`{ agent_id, sid, message_id: "msg-002", content: "读取 /etc/hostname 的内容" }`
  2. 收集 `messages/*` 事件直至 `messages/done`，超时 60 s
- **期望**（按到达顺序）：
  - `messages/tool_call` payload 中 `name == "file_read"`
  - `messages/tool_result` payload 含工具返回内容
  - `messages/chunk` 序列
  - `messages/done`
- **诊断**：若全程无 `tool_call`，检查 (a) `tools.file_read.enabled == true`；
  (b) Workspace 是否含 `/etc/hostname` 可访问路径；(c) Agent 工作区权限配置。
- **无副作用**：同上，不创建额外数据。

#### TC-CHAT-09 重命名会话

- **前置**：使用 TC-CHAT-06 创建的 `sid`。
- **步骤**：
  1. 记录原 title（从 `meta` retained 读）
  2. `PUT /api/agents/com.acowork.senior-engineer/sessions/{sid}/title`
     - Body：`{"title": "smoke-renamed-<random>"}`
  3. 验证 `meta` retained 中 title 已更新（MQTT 推送）
- **期望**：响应 `200`；`meta` retained 中 title 字段变更。
- **清理**：TC-CHAT-10 删除该 session，无需恢复原 title。

#### TC-CHAT-10 删除会话（清理 TC-CHAT-06 创建的 session）

- **前置**：TC-CHAT-06 创建的 `sid`。
- **步骤**：
  1. MQTT PUB `acowork/agents/com.acowork.senior-engineer/sessions/control/delete_session`
     - Payload：`{ agent_id, sid }`
  2. 等待 `acowork/agents/com.acowork.senior-engineer/sessions/deleted`
     事件含该 `sid`，超时 10 s
- **期望**：MQTT 收到 `deleted` 事件；`GET /sessions` 列表不再含该 sid。
- **职责**：本用例同时承担 TC-CHAT-06 / TC-CHAT-09 的 teardown。

#### TC-CHAT-11 停止 Agent

- **步骤**：`POST /api/agents/com.acowork.senior-engineer/stop`
- **期望**：响应 `200`；MQTT `acowork/agents/com.acowork.senior-engineer/status`
  retained 变为 `"offline"`（正常 close）或 LWT 自动变 `"offline"`（异常断开）。

### 5.2 模型切换

#### TC-MODEL-01 切换模型（仅在测试创建的 session 中）

- **前置**：Agent 已运行；测试已在 TC-CHAT-06 中创建 sid。
- **步骤**：
  1. MQTT PUB `acowork/agents/com.acowork.senior-engineer/sessions/control/model_switch`
     - Payload：`{ agent_id, sid, model_id: "<test-model>" }`
  2. 等待 `acowork/agents/com.acowork.senior-engineer/sessions/{sid}/meta`
     retained 更新中的 `model` 字段变化
- **期望**：`meta` 中 `model` 字段反映切换结果；不需要 HTTP ack（无 ack 需求）。
- **清理**：随 TC-CHAT-10 一起删除 session。

> 若改用 HTTP `POST /api/agents/{id}/control`（需 ack 场景），需在用例中显式
> 标注。

#### TC-MODEL-02 设置推理强度

- **前置**：同 TC-MODEL-01。
- **步骤**：
  1. MQTT PUB `acowork/agents/com.acowork.senior-engineer/sessions/control/reasoning_effort`
     - Payload：`{ agent_id, sid, effort: "Medium" }`
- **期望**：`meta` retained 中 `reasoning_effort` 字段变更。
- **清理**：随 TC-CHAT-10 一起删除 session。

### 5.3 流式控制（停止）

#### TC-FLOW-01 中断生成

- **前置**：使用 TC-CHAT-06 的 sid 触发长消息生成（用一条长 prompt 触发）。
- **步骤**：
  1. MQTT PUB `acowork/agents/com.acowork.senior-engineer/sessions/control/message`
     - Payload：`{ agent_id, sid, content: <长 prompt> }`
  2. 收到第一条 `messages/chunk` 后，立即 MQTT PUB
     `acowork/agents/com.acowork.senior-engineer/sessions/control/stop`
     - Payload：`{ agent_id, sid }`
  3. 等待 `messages/stopped` 事件
- **期望**：收到 `messages/stopped`；`stopped` 事件在 stop 命令后 5 s 内到达。
- **清理**：随 TC-CHAT-10 一起删除 session。

### 5.4 Memory 面板

#### TC-MEM-01 列出节点

- **前置**：Agent 已运行过若干轮对话（TC-CHAT-07/08 产生）。
- **步骤**：`GET /api/agents/com.acowork.senior-engineer/memory/nodes?page=1&size=20`
- **期望**：响应 `200 {nodes: [...], total: N}`；每节点含 `node_id`、`type`、
  `content`、`created_at`。

#### TC-MEM-02 获取统计

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/memory/stats`
- **期望**：响应 `200`，含 `total`、`by_type`、`by_status`、`bytes_used`、
  `embedding_dim`。

#### TC-MEM-03 整合 Memory（仅整合测试期产生的节点）

- **前置**：TC-CHAT-07/08 产生的会话数据。
- **步骤**：
  1. 调用 `POST /api/agents/com.acowork.senior-engineer/memory/consolidate`
     - Body：`{"force": true, "retention_days": 0}`（仅整合当前 session 范围的节点）
- **期望**：响应 `200`；MQTT 收到 `memory/nodes/{nid}/update` 或
  `agents/{id}/memory/#` 下的整合完成事件；非测试期产生的节点不应被删除
  （若不能保证范围，则跳过此用例）。
- **副作用范围**：consolidate 行为依赖实现；本用例只在能精确限制范围的
  实现下执行，否则降级为"仅触发 + 验证 HTTP 响应 200"。

#### TC-MEM-04 创建并删除 Memory 节点（create → use → delete）

> 本用例全程使用 Runtime 自身接口创建节点，再删除该节点，验证删除功能可
> 用；绝不删除既有节点。
- **前置**：记录测试前 `memory/nodes` 总数 `N0`。
- **步骤**：
  1. （Create）通过 Runtime 内置 `memory_store` 工具调用插入 1 个测试节点：
     - 给 TC-CHAT-06 sid 发消息"记住：smoke-test-key=<random>"
     - 等待 memory 节点落库（轮询 `/memory/nodes` 至总数 == N0+1）
  2. （Use）记录新增节点的 `node_id`
  3. （Delete）`DELETE /api/agents/com.acowork.senior-engineer/memory/nodes/{node_id}`
- **期望**：
  - Create：总数 +1，新增节点可见
  - Delete：响应 `204`；节点消失；总数回到 `N0`
- **失败处理**：若 Create 成功但 Delete 失败，框架必须把 `node_id` 写入
  "待清理"日志，由运维介入（**禁止**为图省事删除其他节点凑数）。

### 5.5 Setup 面板（Agent 配置）

#### TC-SETUP-01 读取 Agent 配置

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/config`
- **期望**：响应 `200`，含 `llm`、`memory`、`tools`、`permissions`、`resources` 等节。

#### TC-SETUP-02 临时启用 `file_read` 工具（create → use → restore）

- **前置**：记录原 `tools.file_read.enabled` 值。
- **步骤**：
  1. （Modify）`PUT /api/agents/com.acowork.senior-engineer/config`
     - Body：`{"tools": {"file_read": {"enabled": true}}}`
  2. （Use）供 TC-CHAT-08 使用
- **teardown**（TC-SETUP-04 恢复原值）：见下。

#### TC-SETUP-03 触发 file_read 调用

- **前置**：TC-SETUP-02 已启用 file_read；详见 TC-CHAT-08。

#### TC-SETUP-04 恢复 file_read 配置（teardown）

- **步骤**：`PUT /api/agents/com.acowork.senior-engineer/config`
  - Body：`{"tools": {"file_read": {"enabled": <原值>}}}`
- **期望**：响应 `200`；`/config` 中 `file_read.enabled` 与 TC-SETUP-02 之前一致。

#### TC-SETUP-05 列出 MCP 服务

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/mcp-servers`
- **期望**：响应 `200 {servers: [...]}`；空数组允许通过。

#### TC-SETUP-06 列出可用搜索 provider

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/search-providers`
- **期望**：响应 `200`，含 `providers[]`（每个含 `id`、`label`、`configured`）。

#### TC-SETUP-07 当前模型

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/model`
- **期望**：响应 `200`，含 `provider`、`model` 字段。

### 5.6 Workspace 面板

#### TC-WS-01 列出工作区

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/workspaces`
- **期望**：响应 `200`，数组中预置默认 workspace 至少 1 个（**只读**，禁止删除）。

#### TC-WS-02 添加测试专用工作区（create）

- **步骤**：`POST /api/agents/com.acowork.senior-engineer/workspaces`
  - Body：`{"path": "/tmp/acowork-smoke-<random>", "alias": "smoke-<random>"}`
- **期望**：响应 `200 {workspace_id: "..."}`；记下 `workspace_id` 与目录路径。
- **副作用**：本次创建的工作区目录与 workspace 注册项，需在 TC-WS-09 删除。

#### TC-WS-03 列出文件树

- **前置**：使用 TC-WS-02 创建的工作区（**勿使用默认工作区**）。
- **步骤**：`GET /api/agents/com.acowork.senior-engineer/workspaces/tree?workspace_id=<ws_id>&path=/`
- **期望**：响应 `200 {entries: [...]}`；每项含 `name`、`type`、`path`。

#### TC-WS-04 在测试工作区内创建临时文件（create）

- **前置**：TC-WS-02 创建的工作区。
- **步骤**：`POST /api/agents/com.acowork.senior-engineer/workspaces/file`
  - Query：`workspace_id=<ws_id>`
  - Body：`{"path": "/smoke-<random>.txt", "content": "smoke test content"}`
- **期望**：响应 `200`；记下文件路径。
- **清理**：TC-WS-07 删除。

#### TC-WS-05 读取临时文件（use）

- **前置**：TC-WS-04 创建的临时文件。
- **步骤**：`GET /api/agents/com.acowork.senior-engineer/workspaces/file?workspace_id=<ws_id>&path=/smoke-<random>.txt`
- **期望**：响应 `200`，含 `content == "smoke test content"`、`mime`、`size`。

#### TC-WS-06 修改临时文件（modify）

- **前置**：TC-WS-04 创建的临时文件。
- **步骤**：`PUT /api/agents/com.acowork.senior-engineer/workspaces/file?workspace_id=<ws_id>`
  - Body：`{"path": "/smoke-<random>.txt", "content": "updated content"}`
- **期望**：响应 `200`；重新读取确认内容已更新。

#### TC-WS-07 删除临时文件（delete，仅 TC-WS-04 创建的）

- **前置**：TC-WS-04 创建的临时文件。
- **步骤**：`DELETE /api/agents/com.acowork.senior-engineer/workspaces/file?workspace_id=<ws_id>&path=/smoke-<random>.txt`
- **期望**：响应 `200`；文件消失。

#### TC-WS-08 按名查找文件（仅在测试工作区内）

- **前置**：TC-WS-02 创建的工作区；临时文件 TC-WS-04 尚未删除时可作为
  命中样本，否则本用例可降级为"零结果集"。
- **步骤**：`GET /api/agents/com.acowork.senior-engineer/workspaces/find?workspace_id=<ws_id>&pattern=smoke-*.txt`
- **期望**：响应 `200 {results: [...]}`。

#### TC-WS-09 删除测试工作区（delete，仅 TC-WS-02 创建的）

- **前置**：TC-WS-02 创建的 `workspace_id`；确认该 id 对应的 alias/path 是
  测试专用（不要误删默认工作区）。
- **步骤**：
  1. `DELETE /api/agents/com.acowork.senior-engineer/workspaces/{ws_id}`
  2. `rm -rf /tmp/acowork-smoke-<random>`（清理物理目录）
- **期望**：响应 `200`；`/workspaces` 列表不再含该 workspace；物理目录已清理。

### 5.7 Harness 视图（全局资源）

#### TC-HARNESS-01 列出全局 Provider

- **步骤**：`GET /api/global/providers`
- **期望**：响应 `200 {providers: [...]}`；`api_key` 字段为掩码。

#### TC-HARNESS-02 添加测试专用 Provider（create）

- **步骤**：`POST /api/global/providers`
  - Body：`{"id": "smoke-<random>", "base_url": "https://api.example.com/v1",
    "api_key": "sk-smoke-test", "protocol": "openai"}`
- **期望**：响应 `200` 或 `201`；记下 `id`。
- **清理**：TC-HARNESS-03 删除。

#### TC-HARNESS-03 删除测试 Provider（delete，仅 TC-HARNESS-02 创建的）

- **步骤**：`DELETE /api/global/providers/<smoke-id>`
- **期望**：响应 `200` 或 `204`；`/providers` 列表不再含该 id。

#### TC-HARNESS-04 列出模型

- **步骤**：`GET /api/models`
- **期望**：响应 `200 {models: [...]}`；每项含 `provider`、`model_id`、`label`。

#### TC-HARNESS-05 嵌入模型列表

- **步骤**：`GET /api/global/embedding_models`
- **期望**：响应 `200`；每项含 `id`、`name`、`status`。

#### TC-HARNESS-06 MCP 目录

- **步骤**：`GET /api/global/mcps`
- **期望**：响应 `200 {entries: [...]}`；`env` 字段被掩码。

#### TC-HARNESS-07 搜索 Provider 密钥列表

- **步骤**：`GET /api/search/keys`
- **期望**：响应 `200`；每个 provider 的 key 字段为掩码。

### 5.8 Skills 面板

#### TC-SKILL-01 列出技能（只读）

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/skills`
- **期望**：响应 `200 {skills: [...]}`；每项含 `name`、`description`、`version`。

#### TC-SKILL-02 获取技能详情（只读）

- **前置**：TC-SKILL-01 至少 1 个 skill。
- **步骤**：`GET /api/agents/com.acowork.senior-engineer/skills/{name}`
- **期望**：响应 `200`，含 `frontmatter`、`body`、`file_path`。

#### TC-SKILL-03 技能执行历史（只读）

- **步骤**：`GET /api/agents/com.acowork.senior-engineer/skills/{name}/history`
- **期望**：响应 `200 {history: [...]}`；空数组允许通过。

> 注：技能导入（`POST /api/agents/{id}/skills/import`）属于安装操作，会
> 落地到 Agent 安装目录。**冒烟测试不应触发 import**，仅覆盖读取与既有
> skill 的展示。

### 5.9 Settings 视图

#### TC-SETTINGS-01 Gateway 状态（只读）

- **步骤**：`GET /api/status`
- **期望**：响应 `200`，含 `version`、`running_agents`、`memory_mb`、`uptime_secs`。

#### TC-SETTINGS-02 读取 Gateway 配置（只读）

- **步骤**：`GET /api/config`
- **期望**：响应 `200`，含 `log_level`、`http`、`idle_timeout`、
  `default_provider`、`default_model`。

#### TC-SETTINGS-03 临时更新日志级别（modify → restore）

- **前置**：记录原 `log_level` 值 `L0`。
- **步骤**：
  1. （Modify）`PUT /api/config` Body：`{"log_level": "debug"}`
  2. 验证 `GET /config` 中 `log_level == "debug"`
- **teardown**：见 TC-SETTINGS-06。

#### TC-SETTINGS-04 用户档案列表（只读）

- **步骤**：`GET /api/users`
- **期望**：响应 `200 {users: [...]}`。

#### TC-SETTINGS-05 创建测试用户档案（create）

- **步骤**：`POST /api/users`
  - Body：`{"display_name": "smoke-<random>"}`
- **期望**：响应 `200 {user_id: "..."}`；记下 `user_id`。
- **清理**：TC-SETTINGS-07 删除。

#### TC-SETTINGS-06 恢复日志级别（teardown）

- **前置**：记录 TC-SETTINGS-03 之前的原值 `L0`。
- **步骤**：`PUT /api/config` Body：`{"log_level": "<L0>"}`
- **期望**：`/config` 中 `log_level` 与测试前一致。

#### TC-SETTINGS-07 删除测试用户档案（delete，仅 TC-SETTINGS-05 创建的）

- **步骤**：`DELETE /api/users/{user_id}`
- **期望**：响应 `200` 或 `204`；`/users` 列表不再含该 user_id。

### 5.10 文档附件（会话级）

#### TC-DOC-01 上传测试附件到 TC-CHAT-06 创建的 session（create）

- **前置**：TC-CHAT-06 已创建 sid。
- **步骤**：`POST /api/sessions/{sid}/documents`（multipart，file 字段，
  content 为测试随机字符串）
- **期望**：响应 `200 {document_id: "..."}`；记下 `document_id`。
- **清理**：TC-DOC-04 删除。

#### TC-DOC-02 列出附件（use）

- **步骤**：`GET /api/sessions/{sid}/documents`
- **期望**：响应 `200`，含 TC-DOC-01 上传的文档。

#### TC-DOC-03 引用附件发消息

- **步骤**：MQTT PUB `acowork/agents/.../sessions/control/message`
  - Payload：`{ agent_id, sid, message_id: "msg-doc", content: "总结此文件",
    document_ids: ["<doc_id>"] }`
- **期望**：消息成功发送；agent 回复能引用附件内容。
- **清理**：附件与 session 随 TC-DOC-04 与 TC-CHAT-10 一起清理。

#### TC-DOC-04 删除附件（delete，仅 TC-DOC-01 上传的）

- **步骤**：`DELETE /api/sessions/{sid}/documents/{doc_id}`
- **期望**：响应 `200` 或 `204`；`/documents` 列表不再含该 doc_id。

### 5.11 交互（审批 / 问答）

#### TC-INTERACT-01 工具审批

- **前置**：Agent 触发 `messages/tool_approval_needed` 事件（需 manifest 或
  config 中配置需要审批的工具，例如 `shell`）。
- **步骤**：
  1. 收到 `messages/tool_approval_needed {request_id, tool, params}`
  2. `POST /api/agents/com.acowork.senior-engineer/approval`
     - Body：`{"request_id": "<id>", "decision": "allow"}`
- **期望**：响应 `200`；Agent 继续执行。
- **副作用**：审批通过可能允许工具修改磁盘文件，**因此冒烟测试不应触发此
  路径**；本用例标记为"可选/手测"，CI 默认跳过。

#### TC-INTERACT-02 回答 LLM 提问

- **前置**：Agent 触发 `messages/ask_question` 事件。
- **步骤**：
  1. 收到 `messages/ask_question {question_id, question, options}`
  2. `POST /api/agents/com.acowork.senior-engineer/question`
     - Body：`{"question_id": "<id>", "answer": "<选项 label>"}`
- **期望**：响应 `200`；Agent 继续执行。
- **副作用**：ask_question 由 LLM 自主决定是否触发，**冒烟测试不应刻意
  构造触发条件**；本用例标记为"可选/手测"，CI 默认跳过。

### 5.12 LSP 与 Debug

#### TC-LSP-01 取 LSP 端点（只读）

- **步骤**：`GET /api/lsp/endpoint`
- **期望**：响应 `200 {endpoint: "ws://..."}`；非空。
- **注**：LSP Debug 通道对外仍以 WebSocket 形式暴露（由 LSP Relay 进程提供），
  与 ADR-033 描述的 Gateway/Runtime 实时事件通道改造无关；本测试仅校验
  端点字符串，不发起连接。

#### TC-DEBUG-01 临时重启为 Debug 模式（modify → restore）

- **前置**：记录原 dev_mode 状态。
- **步骤**：
  1. `POST /api/agents/com.acowork.senior-engineer/restart-debug`
- **期望**：响应 `200`；Agent 以 debug 模式重启。
- **teardown**：手动或用例末调用 `POST /api/agents/{id}/stop` 后再 `start`
  回到正常模式；或由 fixture 兜底。

---

## 6. 验收标准

每个测试用例必须同时满足：

1. **响应码与协议文档一致**：参见 `/docs/zh/protocols/http.md §4`、
   `/docs/zh/protocols/mqtt.md §3-§9`。
2. **响应字段类型与协议一致**：JSON / Protobuf 字段类型在脚本侧做轻量校验。
3. **关键字段非空**：`message_id`、`sid`、`content`、`delta` 等不可为空。
4. **MQTT 事件按序到达**：`messages/chunk → ... → messages/done`（或中途
   `messages/error`）；`status` retained 与 LWT 行为符合 `mqtt.md §5.5`。
5. **跨进程数据一致性**：写入消息后立即通过 HTTP 拉取应可见（参见
   `http_response_immediate_state_update` 实践规范）。
6. **持久化数据保护**：每个用例结束（含失败）后，§1.2 列出的"测试自身创建
   的资源"应全部被清理或处于已知中间态；既有资源不能被改动。
7. **可重入**：同一脚本连续运行 2 次，第 2 次不应因脏数据失败（除显式有
   状态用例）。

---

## 7. 失败模式与诊断

| 失败现象 | 可能原因 | 诊断方式 |
|---------|---------|---------|
| `GET /health` 返回 5xx | Gateway 未启动 / 数据目录无权限 | 检查进程 / stderr |
| Broker `:19875` 连不上 | Gateway 内 rumqttd 未启动 | Gateway 启动日志；`lsof -i :19875` |
| `start agent` 超时 | Runtime 子进程 crash / 模型未配置 | `agent.log` + `/api/global/providers` |
| MQTT 收不到 status="online" | Runtime 未注册 / LWT 提前触发 | Gateway stderr；`/api/agents/{id}` |
| 无 `messages/tool_call` | `tools.file_read.enabled != true` | TC-SETUP-02 |
| Memory 接口超时 | 内部 pending request 卡死（历史遗留） | Runtime stderr |
| `reasoning_content` 字段缺失 | OpenAI 兼容协议未透传 reasoning | Provider 配置 |
| 流式事件 Lagged | 高频事件超过 broadcast 缓冲 | 通过 HTTP `/sessions/{sid}/messages` 拉取补齐 |
| 清理阶段失败 | Runtime 异常 / 网络断开 | 框架必须把"待清理资源"写入日志，运维介入 |

---

## 8. 实现建议

### 8.1 目录结构

```
dev/
├── e2e_frontend_smoke/
│   ├── package.json          # Node 方案：jest / vitest + mqtt + undici
│   ├── conftest.py           # Python 方案：pytest + asyncio-mqtt + httpx
│   ├── fixtures/
│   │   ├── gateway.py        # 启动 / 等待 / 关闭 Gateway；初始化 Broker 连接
│   │   ├── auth.ts            # 读取 <data_dir>/http_token
│   │   ├── mqtt_client.py    # 封装 SUB/PUBLISH，提供事件订阅原语
│   │   └── cleanup.py        # 跟踪并清理测试自身创建的临时资源
│   ├── tc_chat_06_create_session.py
│   ├── tc_chat_07_message_hi.py
│   ├── tc_chat_10_delete_session.py   # 同时承担 TC-CHAT-06/09 teardown
│   ├── ...
│   └── README.md
└── e2e_stop_test.ps1         # 既有单用例脚本
```

### 8.2 公共 Fixture

- `beforeAll`：
  1. 启动 Gateway（若未启动），轮询 `/health` 至 200。
  2. 验证 Broker `:19875` 可 SUBSCRIBE（无消息即可）。
  3. 读取 `<data_dir>/http_token`，构造 `Authorization: Bearer` 头。
  4. 若未安装 senior-engineer，安装之（仅本测试用）。
  5. 启动 agent → 等待 `status` retained == `"online"`。
  6. **不**预设 provider / 修改 agent_config；按需由各用例自建自清。
- `afterAll`：停止 agent；Gateway 可保留。

### 8.3 资源追踪清单

每个用例内维护：

- `created_resources = []` —— 记录本次用例创建的所有可清理资源
  （如 `workspace_id`、`document_id`、`node_id` 等）
- 用例 teardown 阶段：逆序清理 `created_resources`
- 失败时：写入"待清理清单"日志（不要静默）

### 8.4 用例顺序

按 §5.x 顺序执行；下游用例可复用上游创建的资源（如 TC-WS-05 复用 TC-WS-04
创建的文件）。若需独立运行，每个用例内部应自带"创建独立资源"步骤。

### 8.5 输出

- 每个用例打印：请求摘要、关键响应字段、耗时（ms）、创建的临时资源 ID
- 失败时 dump：完整响应体、当前 session `meta`、MQTT 已收到事件列表、
  "待清理资源"清单

### 8.6 运行入口

```bash
# Node 方案
cd dev/e2e_frontend_smoke && npm test

# Python 方案
cd dev/e2e_frontend_smoke && pytest -v
```

---

## 9. 与 Cargo 测试的关系

本文档定义的 E2E 测试**不进** `cargo test` 套件（依赖外部 Gateway 进程，
启动成本高，与模块内部测试隔离）。建议作为独立脚本套件运行，与
`./dev/ci.sh all` 解耦但可在 CI 中作为可选阶段触发（如 `e2e_frontend_smoke.sh`）。

---

## 10. 相关源文件索引

### 10.1 协议文档

- [`/docs/zh/protocols/README.md`](../../zh/protocols/README.md)
- [`/docs/zh/protocols/http.md`](../../zh/protocols/http.md)
- [`/docs/zh/protocols/mqtt.md`](../../zh/protocols/mqtt.md)
- [`/docs/adr/zh/ADR-033-mqtt-replace-grpc-websocket.md`](../../adr/zh/ADR-033-mqtt-replace-grpc-websocket.md)

### 10.2 Gateway 实现

| 模块 | 路径 |
|------|------|
| 路由聚合 | `core/acowork-gateway/src/http/routes.rs` |
| Agent 管理 | `core/acowork-gateway/src/http/agents.rs` |
| MQTT Broker（嵌入） | `core/acowork-gateway/src/mqtt/broker.rs` |
| Global Resources Publisher | `core/acowork-gateway/src/mqtt/global_resources_publisher.rs` |
| ACL 加载 | `core/acowork-gateway/src/mqtt/acl.rs` |
| HTTP 反向代理（Runtime 大数据查询） | `core/acowork-gateway/src/http/proxy.rs` |
| Memory | `core/acowork-gateway/src/http/memory_api.rs` |
| Workspace | `core/acowork-gateway/src/http/workspaces.rs` |
| Skills | `core/acowork-gateway/src/http/skills_api.rs` |
| 用户档案 | `core/acowork-gateway/src/http/users_api.rs` |
| 文档附件 | `core/acowork-gateway/src/http/documents.rs` |
| Cron | `core/acowork-gateway/src/http/cron_api.rs` |
| Auth | `core/acowork-gateway/src/http/auth.rs` |

### 10.3 Runtime 实现

| 模块 | 路径 |
|------|------|
| MQTT 客户端 | `core/acowork-runtime/src/mqtt/client.rs` |
| 可用资源缓存 | `core/acowork-runtime/src/mqtt/available_cache.rs` |
| Control 指令处理 | `core/acowork-runtime/src/mqtt/control_handler.rs` |
| Localhost HTTP server（反向代理目标） | `core/acowork-runtime/src/http/server.rs` |

### 10.4 Desktop 实现

| 模块 | 路径 |
|------|------|
| Tauri MQTT 客户端 | `apps/acowork-desktop/src-tauri/src/mqtt_client.rs` |

### 10.5 Agent 包

- 包文件：`examples/agent-packages/com.acowork.senior-engineer.agent`
- 源码：`examples/senior-engineer-agent/`（含 `manifest.toml`）
- 工具声明：`manifest.toml` `[[tools]]` 节