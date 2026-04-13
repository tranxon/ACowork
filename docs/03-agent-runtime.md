# Agent Runtime（统一执行引擎）

> 版本：v3.2 | 更新日期：2026-04-13

---

Agent Runtime 是平台提供的唯一二进制可执行文件，类似 Android 的 ART 虚拟机。Gateway 为每个 Agent 启动一个 Agent Runtime 进程，将 .agent 包路径和 Gateway endpoint 作为启动参数传入。

## 1. 启动方式

```bash
agent-runtime \
    /path/to/agent-package \
    --endpoint unix:///tmp/agent-gateway.sock \
    --agent-id com.example.weather \
    --workspace /home/user/.local/share/agent-gateway/agents/com.example.weather/workspace \
    --config-dir /home/user/.local/share/agent-gateway/agents/com.example.weather/config
```

**启动参数说明：**

| 参数 | 必需 | 说明 |
|------|------|------|
| `<agent-package>` | 是 | .agent 包路径（解压后的目录或 ZIP 文件） |
| `--endpoint` | 是 | Gateway Service API 端点，格式按平台不同 |
| `--agent-id` | 是 | Agent 标识符，与 manifest 中一致 |
| `--workspace` | 是 | Agent 工作区目录（含 data/、memory/、runtime/） |
| `--config-dir` | 否 | 用户配置目录（默认取 workspace/config/） |

**endpoint 格式由 Gateway 按平台决定：**

| 平台 | 格式 | 示例 |
|------|------|------|
| Linux | `unix://<path>` | `unix:///tmp/agent-gateway.sock` |
| macOS | `unix://<path>` | `unix:///tmp/agent-gateway.sock` |
| Windows | `pipe://<name>` | `pipe://agent-gateway` |

Runtime 内部按 scheme 选择传输实现，与 06-communication.md 的合同层/实现层分离一致。

**身份信息获取：**

用户身份信息（name、city、language 等）**不通过命令行参数传入**（避免 `/proc/<pid>/cmdline` 泄露）。Runtime 启动后通过 Gateway Service API 握手，由 Gateway 将 identity 注入。流程：

```
1. Runtime 连接 Gateway Socket
2. 握手（handshake / handshake_ack）
3. Gateway 推送 IdentityDelivery 消息：
   { "type": "identity_delivery", "fields": {"name":"张三","city":"Shanghai",...} }
4. Runtime 存入内存，供 Prompt Builder 使用
```

这和 KeyRelease 同一通道，所有敏感数据都走 Socket，不暴露在进程命令行中。

## 2. 内部结构

```
Agent Runtime 二进制
├── Package Loader      # 解析 .agent ZIP，加载 manifest + prompts + config
├── Prompt Builder      # 组装 system prompt（identity + tools + skills + memory context）
├── History Manager     # 对话历史管理（token 预算、trim、压缩）
├── LLM Client          # 直连 LLM Provider API（OpenAI/Claude/Ollama 等）
├── Tool Dispatcher     # 解析 LLM 输出的 tool_calls，路由到工具实现
│   ├── Built-in Tools  # 内置工具（见第 5 节完整清单）
│   ├── WASM Tools      # .agent 包中声明的 WASM 工具（沙箱内执行）
│   └── Gateway Tools   # 需要 Gateway 协调的操作（见第 6 节）
├── Permission Checker  # 根据 manifest 权限表校验工具调用权限
├── Memory Client       # 读写私有 Grafeo
├── Grafeo (嵌入式)     # 私有 Memory（情景记忆 + 语义记忆）
├── Skill Loader        # 加载 .agent 包中的 Skills
├── Budget Manager      # 本地预算预检 + 用量上报
└── Loop Controller     # 主循环控制（迭代次数、超时、循环检测）
```

## 3. 主循环

Agent Runtime 的核心是 LLM 交互循环（参考 ZeroClaw 的 `run_tool_call_loop`）：

```
用户消息 / Intent / 定时触发
       │
       ▼
┌──────────────────────────────────────────────┐
│  Agent Runtime 主循环 [iteration: 0..N]       │
│                                               │
│  ① 预算预检                                   │
│     ├─ 本地预算缓存不足 → 按 action_on_exhaust │
│     │   处理（stop / fallback / warn）         │
│     └─ 预算耗尽且无 fallback → 终止循环  ──► END│
│                                               │
│  ② 构建上下文（按优先级拼接，见 3.1）          │
│     ├─ System Prompt (from prompts/)          │
│     ├─ Identity Context (from Gateway 注入)   │
│     ├─ Tool Definitions (from manifest.tools) │
│     ├─ Capability Overview (from Gateway 推送) │
│     ├─ Skill Instructions (from skills/)      │
│     ├─ Memory RAG (from 私有 Grafeo)          │
│     └─ 对话历史 (from History Manager)        │
│                                               │
│  ③ 调用 LLM (直连 API)                        │
│     ├─ RateAcquire 速率协调                    │
│     ├─ streaming 或 blocking                   │
│     └─ 失败 → 重试或 fallback（见第 7 节）     │
│                                               │
│  ④ 解析响应                                    │
│     ├─ text → 返回结果/回复用户  ──────────► END│
│     └─ tool_calls → ⑤                         │
│                                               │
│  ⑤ 工具调度与执行                              │
│     ├─ Permission Check (manifest)             │
│     ├─ Built-in Tool → 直接执行                │
│     ├─ WASM Tool → Wasmtime 沙箱执行           │
│     └─ Gateway Tool → Socket 调用              │
│     └─ 执行失败 → 错误信息作为 tool result      │
│                    返回给 LLM，由 LLM 决策      │
│                                               │
│  ⑥ 结果追加到历史                              │
│                                               │
│  ⑦ 用量上报（异步，不阻塞）                    │
│                                               │
│  ⑧ 循环检测（见 3.2）                          │
│     └─ 检测到循环 → 终止迭代  ─────────────► END│
│                                               │
│  ⑨ 迭代计数检查                                │
│     └─ 达到 max_iterations → 强制终止 ──────► END│
│                                               │
│  └─→ 回到 ①（下一轮迭代）                     │
└──────────────────────────────────────────────┘
```

### 3.1 上下文构建规则

Prompt Builder 按以下顺序拼接上下文，越靠前优先级越高（LLM 对靠前内容的注意力权重更高）：

| 顺序 | 部分 | 来源 | 说明 |
|------|------|------|------|
| 1 | System Prompt | `prompts/system.md` + `prompts/constraints.md` | Agent 身份定义和行为约束，不可被后续覆盖 |
| 2 | Identity Context | Gateway 注入 | 用户身份信息（name、city 等），Agent "认识"用户 |
| 3 | Tool Definitions | `manifest.toml [tools]` | 转换为 JSON Schema 格式的工具描述，供 LLM 调用 |
| 4 | Capability Overview | Gateway 推送 | 已安装 Agent 及其能力摘要，供 LLM 知道可以向谁协作 |
| 5 | Skill Instructions | `skills/*/SKILL.md` | 可选技能指令，扩展 Agent 的行为模式 |
| 6 | Memory RAG | 私有 Grafeo 检索结果 | 相关历史记忆，按语义相关性排序 |
| 7 | Conversation History | History Manager | 当前对话的完整消息序列 |

**Token 预算分配：** 当上下文总长度超过模型限制时，按 **7 → 6 → 5** 的顺序裁剪（优先保留 System Prompt、Identity 和 Tool Definitions），History Manager 负责 trim 和压缩。

### 3.2 循环检测策略

防止 LLM 陷入重复调用同一工具的死循环：

**检测规则：** 连续 N 次（默认 3）出现相同的 `(tool_name, params)` 组合，判定为循环。

**处理方式：** 终止当前迭代，将循环检测信息作为最后的 assistant 消息写入历史，并向用户返回提示。

**配置：**

```toml
# manifest.toml 中可覆盖默认值
[loop_detection]
threshold = 3          # 连续相同调用的检测阈值
action = "terminate"   # terminate（终止迭代）| warn（追加警告后继续）
```

### 3.3 循环退出条件

| 条件 | 触发时机 | 行为 |
|------|---------|------|
| LLM 返回纯 text | 步骤 ④ | 正常结束，返回结果给用户 |
| 预算耗尽 | 步骤 ① | 按 `action_on_exhaust` 处理；stop 则终止 |
| 达到 max_iterations | 步骤 ⑨ | 强制终止，返回已执行结果 |
| 循环检测触发 | 步骤 ⑧ | 按 `loop_detection.action` 处理 |
| 单轮迭代超时 | 步骤 ③/⑤ | 超时后终止当前迭代 |
| Gateway 停止信号 | 任意步骤 | 优雅退出，保存当前状态 |
| LLM 调用重试耗尽 | 步骤 ③ | 无 fallback provider 时终止 |

## 4. Runtime 默认配置

当 manifest.toml 中未显式声明时，Runtime 使用以下默认值：

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `max_iterations` | 20 | 单次对话最大迭代次数 |
| `iteration_timeout_ms` | 30000 | 单轮迭代超时（含 LLM 调用 + 工具执行） |
| `history_max_tokens` | 128000 | 对话历史上限（超过后触发 trim/compress） |
| `loop_detection.threshold` | 3 | 重复调用检测阈值 |
| `loop_detection.action` | "terminate" | 循环检测后的行为 |
| `llm.routing.retry.max_attempts` | 3 | LLM 调用重试次数 |
| `llm.routing.retry.backoff` | "exponential" | 重试退避策略 |

## 5. Built-in Tools 清单

以下工具由 Agent Runtime 内置实现，Agent 可在 manifest 中声明使用，无需提供实现代码。

| 工具名 | 功能 | 所需权限 | 说明 |
|--------|------|---------|------|
| `memory_recall` | 语义检索私有 Grafeo | `memory:read` | 混合搜索（向量 + BM25），返回相关记忆片段 |
| `memory_store` | 写入私有 Grafeo | `memory:write` | 存储情景记忆和/或语义记忆节点 |
| `http_get` | HTTP GET 请求 | `network:<url_pattern>` | 支持 JSON 响应自动解析 |
| `http_post` | HTTP POST 请求 | `network:<url_pattern>` | 支持 JSON body 和表单 |
| `shell` | 执行 shell 命令 | `filesystem:exec` | 受沙箱限制，超时可中断 |
| `file_read` | 读取文件 | `filesystem:read:<path>` | 限制在工作区和授权目录内 |
| `file_write` | 写入文件 | `filesystem:write:<path>` | 限制在工作区和授权目录内 |
| `intent_send` | 发送 Intent 到其他 Agent | `intent:send:<target>` | 通过 Gateway 路由 |

**工具声明示例（manifest.toml）：**

```toml
[[tools]]
name = "http_get"
type = "builtin"
permissions = ["network:https://api.weather.com"]

[[tools]]
name = "memory_recall"
type = "builtin"
permissions = ["memory:read"]
```

## 6. Gateway Tools（需 Gateway 协调的操作）

以下操作不属于"工具"（不由 LLM tool_call 触发），而是 Runtime 在特定流程中主动向 Gateway 发起的请求，通过 Gateway Service API 通信：

| 操作 | 触发时机 | 说明 |
|------|---------|------|
| `KeyRelease` | 启动握手后 | 获取 LLM API Key（一次性） |
| `IdentityDelivery` | 启动握手后 | 获取用户身份信息（由 Gateway 主动推送） |
| `IntentSend` / `IntentReceived` | LLM 调用 `intent_send` 工具时 | 跨 Agent 消息路由 |
| `BudgetQuery` | 预算预检时 | 查询剩余预算 |
| `UsageReport` | 每轮迭代后（异步） | 上报 LLM 用量 |
| `RateAcquire` | 调用 LLM 前 | 申请速率令牌 |
| `PermissionRequest` | 运行时请求额外权限 | 弹窗让用户确认 |

**关键原则：** LLM 调用和工具执行不走 Gateway——Agent 直连 LLM API、本地执行工具。Gateway 只管必须集中化的协调。

## 7. 错误处理策略

### 7.1 LLM 调用失败

```
LLM 调用失败（网络超时 / API 错误 / token 超限）
       │
       ▼
按 manifest.llm.routing.retry 配置重试
  ├─ max_attempts (默认 3)
  ├─ backoff: exponential
       │
       ├─ 重试成功 → 继续
       │
       └─ 重试耗尽 → 检查 fallback:
            ├─ manifest 中配置了 fallback provider → 切换到 fallback
            │   └─ fallback 也失败 → 终止循环，返回错误信息
            └─ 无 fallback → 终止循环，返回错误信息
```

### 7.2 工具执行失败

工具执行失败**不终止循环**。错误信息作为 tool result 返回给 LLM，由 LLM 决定下一步（换参数重试、换工具、或放弃）：

```
工具执行失败（WASM 崩溃 / 权限不足 / 超时）
       │
       ▼
构造 error tool result:
  { "error": true, "message": "工具执行超时", "tool_name": "http_get" }
       │
       ▼
追加到对话历史 → LLM 在下一轮迭代看到错误信息并自主决策
```

### 7.3 Gateway 断连

```
Gateway Socket 断连
       │
       ▼
进入优雅降级模式:
  ├─ 本地 LLM 推理继续（已有 API Key）
  ├─ 缓存待上报的用量数据
  ├─ Intent 收发暂停（无法路由）
  ├─ 定期尝试重连 Gateway
  │
  └─ 超过 reconnection_timeout (默认 60s) → 保存状态后退出
```

## 8. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| 身份信息传输 | 通过 Gateway Socket 推送 | 避免命令行参数泄露（/proc 可读） |
| 工具执行失败处理 | 返回错误给 LLM 决策 | LLM 有能力自主调整策略，比直接终止更灵活 |
| Gateway 断连 | 优雅降级而非立即退出 | 保留本地推理能力，短暂断连不影响体验 |
| 循环检测 | 相同 (tool, params) 连续 N 次 | 简单有效，覆盖最常见的死循环模式 |
| 上下文裁剪顺序 | History → Memory → Skills | 系统指令和身份信息不可裁剪，历史最可丢弃 |
