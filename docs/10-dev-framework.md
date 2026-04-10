# Agent 开发框架

> 版本：v3.1 | 更新日期：2026-04-09

---

Rollball 不只是 Agent 运行时，更是 Agent 全生命周期平台——从创建、调试、测试到发布，全链路内置支持。类比 Android 不只有 ART，还有 Android Studio。

## 1. 总体架构

```
┌────────────────────────────────────────────────────────────────┐
│                    Rollball Desktop App (Tauri)                 │
│                                                                │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────┐  │
│  │ Agent    │  │ 对话调试  │  │ Skill    │  │ 发布向导     │  │
│  │ 管理器   │  │ 面板     │  │ 编辑器   │  │              │  │
│  │          │  │          │  │          │  │ · 配置检查   │  │
│  │ · 克隆   │  │ · 消息流  │  │ · SKILL  │  │ · 签名打包   │  │
│  │ · 安装   │  │ · 断点    │  │ · Prompt │  │ · 仓库上传   │  │
│  │ · 删除   │  │ · 编辑    │  │ · 测试   │  │ · 本地安装   │  │
│  └──────────┘  └──────────┘  └──────────┘  └──────────────┘  │
│                                                                │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────────────────┐ │
│  │ Manifest │  │ 录制回放  │  │ Provider 切换器              │ │
│  │ 编辑器   │  │ 面板     │  │ · 调试 → 本地小模型           │ │
│  └──────────┘  └──────────┘  │ · 测试 → 付费大模型           │ │
│                                └──────────────────────────────┘ │
└────────────────────────────────────────────────────────────────┘
                │                           │
                │ Debug Protocol            │ Gateway Service API
                │ (WebSocket/Named Pipe)    │ (生产通道)
                ▼                           ▼
┌─────────────────────────┐    ┌─────────────────────────┐
│  Agent Runtime          │    │  Gateway                │
│  (DevMode)              │    │                         │
│                         │    │  · Key Vault            │
│  · 主循环受调试器控制    │    │  · Lifecycle Manager    │
│  · 每步可快照/回滚      │    │  · Intent Router        │
│  · 消息可编辑重执行     │    │  · ...                  │
│  · Skill 热加载         │    │                         │
│  · 录制/回放引擎        │    └─────────────────────────┘
│  · Provider 动态切换    │
└─────────────────────────┘
```

Desktop App 与 Agent Runtime 之间有两条独立通道：
- **Debug Protocol**：开发模式专用，控制执行流、编辑状态、热加载
- **Gateway Service API**：生产通道，Agent 仍通过 Gateway 获取 Key、收发 Intent 等

## 2. Agent 克隆

用户可以从任意已安装的 Agent 克隆出一个新的 Agent 进行开发。克隆粒度为用户可选项。

### 2.1 克隆模式

| 模式 | 复制内容 | 适用场景 | 类比 |
|------|---------|---------|------|
| **骨架克隆** | manifest 结构 + prompts 模板 + LLM 配置 + 权限声明 | 从模板出发构建全新 Agent | `npx create-agent` |
| **完整克隆** | 骨壳 + skills + data + 私有 Grafeo 快照 | 在已有 Agent 基础上定制/分支 | `git clone` + 修改 |

**克隆流程：**

```
用户选择源 Agent + 克隆模式
       │
       ▼
Desktop App 调用 Gateway API: AgentClone { source_id, mode, new_id }
       │
       ▼
Gateway:
  ├─ 读取源 Agent 工作区
  ├─ 按模式复制文件:
  │   ├─ 骨架: manifest.toml (清除 agent_id, 置为 new_id)
  │   │         prompts/ (完整复制)
  │   │         config/ (完整复制)
  │   │         tools/ (完整复制)
  │   │         resources/ (完整复制)
  │   │
  │   └─ 完整额外复制:
  │       skills/ (完整复制)
  │       data/ (完整复制)
  │       memory/private.grafeo (复制快照)
  │
  ├─ 写入新 Agent 工作区:
  │   ~/.local/share/agent-gateway/agents/<new_id>/
  │
  ├─ 新 Agent 标记为 dev: true (开发态)
  │
  └─ 返回克隆结果
```

**克隆限制：**
- 系统 Agent（`com.rollball.system`）不可克隆——它是平台特权 Agent，克隆出来没有 Platform 签名，无法获得系统特权。
- 克隆产生的新 Agent 是独立的，与源 Agent 没有继承关系。后续源 Agent 更新不会同步到克隆体。
- 完整克隆的 Grafeo 快照是克隆时刻的副本，之后双方各自演化。

### 2.2 从零创建

除克隆外，Desktop App 也提供从零创建 Agent 的向导：

```
Step 1: 基本信息 — agent_id, name, description, author
Step 2: LLM 配置 — 选择 Provider + 模型 + 参数（可复用 Vault 中的 Key）
Step 3: 权限声明 — 勾选所需权限模板
Step 4: 选择模板 — 可选空白模板 / 天气模板 / 日历模板 / ...
Step 5: 生成 → 创建工作区 → 进入 DevMode
```

## 3. Debug Protocol

Agent Runtime 的 DevMode 通过 Debug Protocol 与 Desktop App 通信。协议设计参考 Chrome DevTools Protocol（CDP），基于 JSON-RPC 2.0。

### 3.1 传输层

| 平台 | 传输方式 | 说明 |
|------|---------|------|
| Linux | WebSocket (`ws://127.0.0.1:19877`) | 通用性最好 |
| macOS | WebSocket | 同上 |
| Windows | WebSocket | 同上 |
| 开发中 | Named Pipe (备选) | 更安全，但 Tauri WebView 侧接入稍复杂 |

Agent Runtime 以 DevMode 启动时，额外监听一个 Debug 端口。Desktop App 连接后获得完全的调试控制权。

### 3.2 协议定义

```rust
// ── 执行控制 ──

/// 恢复自动执行
struct DebuggerResume;

/// 暂停主循环，停在下一个迭代步
struct DebuggerPause;

/// 执行一步主循环后暂停
struct DebuggerStep {
    /// 断点粒度
    granularity: StepGranularity,
}

enum StepGranularity {
    Iteration,   // 执行一整轮迭代（默认）
    Phase,       // 执行一个阶段（构建上下文 / 调LLM / 解析响应 / 工具执行）
}

// ── 状态查询 ──

/// 获取当前对话完整状态
struct DebuggerGetState;

/// 返回当前状态
struct DebuggerState {
    iteration: u32,
    phase: Phase,
    messages: Vec<Message>,
    snapshot_ids: Vec<String>,
    breakpoints: Vec<Breakpoint>,
}

enum Phase {
    BudgetCheck,
    BuildContext,
    LlmCall,
    ParseResponse,
    ToolExecution,
    AppendHistory,
    Idle,
}

// ── 断点 ──

/// 设置断点
struct DebuggerSetBreakpoint {
    condition: BreakpointCondition,
}

enum BreakpointCondition {
    OnPhase(Phase),                           // 在特定阶段暂停
    OnToolCall { tool_name_pattern: String },  // 工具名匹配时暂停
    OnIteration(u32),                          // 在第 N 轮迭代暂停
    OnToolResult { is_error: bool },           // 工具返回错误时暂停
}

/// 移除断点
struct DebuggerRemoveBreakpoint {
    breakpoint_id: String,
}

// ── 消息编辑与回滚 ──

/// 编辑某条消息
struct DebuggerEditMessage {
    index: usize,
    content: MessageContent,   // 新内容
}

/// 回滚到指定消息索引，清除后续消息
struct DebuggerRollback {
    target_index: usize,
}

/// 从当前状态重新执行
struct DebuggerReExecute;

// ── Skill 热加载 ──

/// 重新加载 skills 目录
struct DebuggerReloadSkills {
    /// 可选，只重载指定 skill
    skill_name: Option<String>,
}

// ── Provider 切换 ──

/// 动态切换 LLM Provider
struct DebuggerSwitchProvider {
    provider: String,          // "openai" / "ollama" / "anthropic" ...
    model: String,             // "gpt-4o" / "qwen3:8b" / ...
    base_url: Option<String>,  // 可选，覆盖 base_url
}

// ── 录制回放 ──

/// 开始录制当前会话
struct DebuggerStartRecording;

/// 停止录制并保存
struct DebuggerStopRecording {
    /// 录制文件保存路径（默认工作区 recordings/ 目录）
    output_path: Option<String>,
}

/// 加载录制文件并回放
struct DebuggerLoadRecording {
    path: String,
    /// 回放模式
    mode: ReplayMode,
}

enum ReplayMode {
    /// 自动回放，每步间隔 delay_ms 毫秒
    Auto { delay_ms: u64 },
    /// 手动步进，每步需 DebuggerStep 推进
    Manual,
}

// ── 事件通知（Runtime → Desktop App）──

/// 每步执行完推送
struct DebuggerOnStep {
    iteration: u32,
    phase: Phase,
    /// 本步的输入消息（如有）
    input: Option<Message>,
    /// 本步的输出（如有）
    output: Option<Message>,
    /// LLM 用量统计（如有）
    usage: Option<Usage>,
}

/// 断点命中通知
struct DebuggerOnBreakpoint {
    breakpoint_id: String,
    iteration: u32,
    phase: Phase,
}

/// 录制步骤通知
struct DebuggerOnRecordStep {
    step_index: u32,
    phase: Phase,
    /// 序列化的步骤数据（消息 + 工具调用 + 结果）
    step_data: serde_json::Value,
}
```

### 3.3 消息快照机制

Agent Runtime 在 DevMode 下的每一轮迭代结束，自动创建一个轻量快照：

```rust
struct ConversationSnapshot {
    /// 快照 ID（递增）
    id: String,
    /// 对应的迭代轮次
    iteration: u32,
    /// 快照时刻 messages 数组的长度（截断点）
    message_count: usize,
    /// 快照时刻的 LLM 用量
    cumulative_usage: Usage,
    /// 时间戳
    timestamp: SystemTime,
}
```

快照的实现极其轻量——messages 数组是 append-only 的，快照只需要记录长度。回滚时截断到目标长度即可，无需深拷贝。

```
messages: [msg0, msg1, msg2, msg3, msg4, msg5]

快照 @ iteration 2: message_count = 4  →  回滚到此处: 截断为 [msg0, msg1, msg2, msg3]
快照 @ iteration 3: message_count = 6  →  回滚到此处: 截断为 [msg0, msg1, msg2, msg3, msg4, msg5]
```

## 4. 对话调试面板

Desktop App 的核心界面，可视化 Agent 主循环的每一步。

### 4.1 消息流视图

```
┌─────────────────────────────────────────────────────────┐
│  天气 Agent (com.example.weather-dev)  [DevMode]        │
│                                                         │
│  ┌─ 控制栏 ───────────────────────────────────────────┐ │
│  │ [▶ Resume] [⏸ Pause] [⏭ Step] [⏹ Stop]           │ │
│  │ Provider: [ollama/qwen3:8b ▼]  Iteration: 3       │ │
│  │ Recording: [● Rec] [■ Stop] [▶ Replay]             │ │
│  └────────────────────────────────────────────────────┘ │
│                                                         │
│  ┌─ 消息流 ───────────────────────────────────────────┐ │
│  │                                                     │ │
│  │ [0] system: "你是天气查询助手..."                    │ │
│  │ [1] user: "北京今天天气怎么样"                        │ │
│  │ [2] assistant: tool_call(http_get, {url: "..."})    │ │
│  │     ┌── ✏️ Edit ── 🔄 Re-execute from here ──────┐ │ │
│  │ [3] tool: {"temp": 25, "condition": "晴"}          │ │
│  │     ┌── ✏️ Edit ── 🔄 Re-execute from here ──────┐ │ │
│  │ [4] assistant: "北京今天25度，晴天，适合出门"         │ │
│  │     ┌── ✏️ Edit ── 🔄 Re-execute from here ──────┐ │ │
│  │                                                     │ │
│  │  ⏸ Breakpoint hit: OnToolCall("http_get")          │ │
│  │                                                     │ │
│  └────────────────────────────────────────────────────┘ │
│                                                         │
│  ┌─ 断点面板 ────────────────────────────────────────┐  │
│  │ ● OnToolCall("http_get")                          │  │
│  │ ● OnPhase(ToolExecution)                          │  │
│  │ ○ OnToolResult(is_error=true)                     │  │
│  │ [+ Add Breakpoint]                                │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

### 4.2 消息编辑与重执行

每条消息旁提供两个操作：
- **Edit**：内联编辑消息内容，编辑后自动标记为已修改
- **Re-execute from here**：回滚到该消息之前，然后 Resume 从此处重新执行

编辑场景举例：

```
场景 1：编辑 LLM 输出的 tool_call
  [2] assistant: tool_call(http_get, {url: "https://api.weather.com/v1?city=Beijing"})
                                          ↑ 编辑为 city=Shanghai
  → Re-execute from [2] → Runtime 用修改后的 tool_call 参数执行工具

场景 2：编辑工具返回值（mock 场景）
  [3] tool: {"temp": 25, "condition": "晴"}
                ↑ 编辑为 temp=35, condition="暴雨"
  → Re-execute from [3] → Runtime 用修改后的工具结果继续生成

场景 3：编辑 assistant 回复
  [4] assistant: "北京今天25度，晴天"
                     ↑ 编辑回复内容
  → 仅修改历史，不触发重执行（除非手动 Re-execute）
```

### 4.3 执行控制

| 操作 | 说明 | 主循环行为 |
|------|------|-----------|
| Resume | 恢复自动运行 | 主循环连续执行直到遇到断点或完成 |
| Pause | 暂停 | 当前步执行完后停下，等待下一步指令 |
| Step | 单步执行 | 执行一轮迭代后自动暂停 |
| Step (Phase) | 单阶段步进 | 执行一个阶段（如"调 LLM"）后暂停 |
| Stop | 终止对话 | 结束当前会话，保留消息历史 |
| Restart | 重启对话 | 清空消息历史，从初始状态重新开始 |

## 5. Provider 动态切换

调试过程中可随时切换 LLM Provider，无需重启 Agent Runtime。

### 5.1 切换机制

```
Desktop App 发送 DebuggerSwitchProvider
       │
       ▼
Agent Runtime:
  ├─ 更新 LLM Client 的当前 provider 配置
  ├─ 如果需要新 Key:
  │   └─ 通过 Gateway KeyRelease 获取（如果 Vault 中有）
  ├─ 如果是本地 Provider (ollama):
  │   └─ 直连，无需 Key
  └─ 下一次 LLM 调用使用新 provider
```

### 5.2 典型工作流

```
1. 初始开发 → 切换到 ollama/qwen3:8b（本地免费，快速迭代 skill 和 prompt）
2. Skill 基本可用 → 切换到 openai/gpt-4o-mini（低成本测试真实 API 效果）
3. 最终验证 → 切换到 openai/gpt-4o（全功能测试，确认生产效果）
4. 发布前 → 使用目标 Provider 做完整回归测试
```

### 5.3 录制回放 + Provider 切换

回放模式下可以切换 Provider，实现"同样的对话，不同的 LLM"对比：

```
录制: 用 gpt-4o 录制了一段完整对话（含 LLM 返回值）
回放: 切换到 qwen3:8b，回放同样的用户输入和工具调用
      → 对比两个模型对同样上下文的回复差异
```

这是 A/B 测试 prompt 和 skill 在不同模型下效果的高效方式。

## 6. 录制回放

### 6.1 录制格式

录制的会话保存为 JSONL 文件，每行一个步骤：

```jsonl
{"type":"recording_header","agent_id":"com.example.weather-dev","timestamp":"2026-04-09T12:00:00Z","provider":"openai","model":"gpt-4o"}
{"type":"user_input","content":"北京今天天气怎么样","iteration":0}
{"type":"llm_request","messages_count":2,"iteration":0}
{"type":"llm_response","content":"tool_call(http_get,...)","usage":{"prompt_tokens":150,"completion_tokens":30},"iteration":0}
{"type":"tool_call","name":"http_get","params":{"url":"https://api.weather.com/v1?city=Beijing"},"iteration":0}
{"type":"tool_result","name":"http_get","result":{"temp":25,"condition":"晴"},"iteration":0}
{"type":"llm_request","messages_count":4,"iteration":1}
{"type":"llm_response","content":"北京今天25度，晴天","usage":{"prompt_tokens":200,"completion_tokens":20},"iteration":1}
```

录制文件保存在 Agent 工作区的 `recordings/` 目录下：

```
~/.local/share/agent-gateway/agents/<agent_id>/workspace/recordings/
├── 2026-04-09T120000.jsonl
├── 2026-04-09T143000.jsonl
└── ...
```

### 6.2 回放模式

| 模式 | 说明 | 适用场景 |
|------|------|---------|
| **自动回放** | 按录制顺序自动推进，每步可设延迟 | 全流程演示、回归测试 |
| **手动步进** | 每步需用户手动 Step 推进 | 逐帧检查、调试特定步骤 |
| **对比回放** | 加载多个录制文件，同屏对比 | A/B 测试不同 Provider/Prompt 的效果 |

### 6.3 回放与编辑结合

回放过程中可以随时：
- 编辑某步的消息内容，然后 Re-execute
- 切换 Provider 后从某步重新执行
- 插入新的用户消息，偏离原录制路径，进入自由调试

这让录制文件既是"回归测试用例"，也是"调试起点"。

## 7. Skill 热加载

### 7.1 加载机制

```
Desktop App 中编辑/新增 Skill
       │
       ▼
保存文件到 Agent 工作区 skills/ 目录
       │
       ▼
Desktop App 发送 DebuggerReloadSkills { skill_name: Some("weather-query") }
       │
       ▼
Agent Runtime:
  ├─ 重新扫描 skills/ 目录
  ├─ 解析更新后的 SKILL.md（YAML frontmatter + Markdown body）
  ├─ 更新 Prompt Builder 中的 skill 描述
  └─ 下一次迭代生效
```

### 7.2 Skill 编辑器

Desktop App 内置 Skill 编辑器，提供结构化编辑和预览：

```
┌─ Skill 编辑器 ──────────────────────────────────────┐
│                                                       │
│  Skill: weather-query                                 │
│                                                       │
│  ┌─ SKILL.md ───────────────────────────────────┐    │
│  │ ---                                          │    │
│  │ name: weather-query                          │    │
│  │ description: 查询城市天气并给出建议            │    │
│  │ allowed-tools: Bash(http:*) Read             │    │
│  │ ---                                          │    │
│  │                                              │    │
│  │ # Weather Query Skill                        │    │
│  │                                              │    │
│  │ 当用户询问天气时：                              │    │
│  │ 1. 使用 http_get 调用天气 API                  │    │
│  │ 2. 解析返回的 JSON 数据                        │    │
│  │ 3. 用自然语言描述天气情况                       │    │
│  └──────────────────────────────────────────────┘    │
│                                                       │
│  [🔄 Reload]  [▶ Test in Chat]                       │
└───────────────────────────────────────────────────────┘
```

"Test in Chat" 按钮执行热加载后，自动在对话面板发送一条触发消息（匹配 skill 的 trigger pattern），快速验证 skill 是否按预期工作。

## 8. 发布流程

调试完成的 Agent 从开发态转为发布态。

### 8.1 发布向导

```
Step 1: 检查
  ├─ manifest.toml 完整性校验
  ├─ 必填字段检查（agent_id, version, name, runtime_version）
  ├─ skills/ 目录下每个 SKILL.md 格式校验（YAML frontmatter 完整性）
  ├─ prompts/ 目录下文件存在性检查
  └─ 权限声明合理性检查

Step 2: 清理
  ├─ 移除 dev 标记（manifest 中 dev: false 或删除该字段）
  ├─ 清空 recordings/ 目录（不打包进发布包）
  ├─ 清空或保留 data/（用户选择）
  ├─ 清空私有 Grafeo（发布包不含个人记忆）
  └─ 重置 config/ 为默认值（可选）

Step 3: 打包
  ├─ 按 .agent 包格式打包为 ZIP
  └─ 输出到 build/<agent_id>-<version>.unsigned.agent

Step 4: 签名
  ├─ 调用 rollball-sign 签名
  ├─ 可选：选择密钥（已有 / 新生成）
  └─ 输出 build/<agent_id>-<version>.agent

Step 5: 分发
  ├─ 本地安装：Gateway Package Manager 安装到生产位置
  ├─ 仓库上传：推送到配置的仓库源
  └─ 导出文件：仅保存 .agent 文件到指定路径
```

### 8.2 从 DevMode 到生产模式

发布安装后，Agent 以生产模式运行，区别：

| 维度 | DevMode | 生产模式 |
|------|---------|---------|
| Debug Protocol | 监听 | 不监听 |
| 主循环 | 受调试器控制 | 自动连续执行 |
| 快照 | 每步自动快照 | 不快照 |
| Provider 切换 | 动态可切换 | 按 manifest 固定配置 |
| 录制 | 可录制/回放 | 不录制 |
| Skill 加载 | 热加载 | 启动时一次性加载 |

生产模式下 Agent Runtime 与当前设计完全一致，DevMode 是纯粹的超集。

## 9. 与现有架构的关系

开发框架不改变现有架构，而是在 Agent Runtime 上叠加 DevMode 层：

```
生产模式 Agent Runtime（现有设计）
    │
    └── 叠加 DevMode 扩展:
        ├─ Debug Protocol Server
        ├─ Snapshot Manager
        ├─ Recording Engine
        ├─ Provider Switcher
        └─ Skill Hot-Reloader
```

Gateway 不需要任何修改——DevMode 下的 Agent 仍然通过 Gateway Service API 获取 Key、收发 Intent、上报用量。开发框架的复杂度全部封装在 Agent Runtime 和 Desktop App 内部。

## 10. Roadmap 补充

在原有路线图基础上新增：

### Phase 2.5: 开发框架基础

- Agent Runtime DevMode：主循环步进控制 + Debug Protocol Server
- 对话快照与回滚机制
- Desktop App 骨架（Tauri）：Agent 管理器 + 对话调试面板
- Agent 克隆 API（Gateway 侧）
- 从零创建 Agent 向导

### Phase 3.5: 开发框架高级

- 消息编辑与重执行
- Skill 热加载 + Skill 编辑器
- Provider 动态切换
- 断点系统（OnPhase / OnToolCall / OnToolResult）
- Manifest 编辑器
- 发布向导（打包 + 签名 + 分发）

### Phase 4.5: 录制回放

- 录制引擎（JSONL 格式）
- 自动/手动/对比回放
- 回放 + Provider 切换（A/B 测试）
- 录制文件作为回归测试用例
