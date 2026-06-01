# OpenClaw vs ZeroClaw Agent 框架架构对比分析报告

> 源码版本：2026-04-05
> 拆解范围：主循环、工作流、上下文注入、Skills 机制

---

## 一、总体架构对比

| 维度 | OpenClaw (TypeScript) | ZeroClaw (Rust) |
|------|----------------------|-----------------|
| **主循环入口** | `pi-embedded-runner/attempt.ts` | `loop_.rs::run()` → `process_message()` → `run_tool_call_loop()` |
| **核心文件行数** | ~800 行（attempt.ts） | 9549 行（loop_.rs） |
| **Agent 抽象** | `EmbeddedAgent` + 上游 Pi 框架 | `Agent` struct（agent.rs, 1892 行） |
| **Context 管理** | `ContextEngine` 接口（可插拔） | `build_context()` 函数（内置） |
| **Tool Dispatcher** | 委托上游 Pi 框架 | `XmlToolDispatcher` / `NativeToolDispatcher` |
| **Skills 加载** | `SKILL.toml` + `SKILL.md`（上游规范） | `SKILL.toml` + 原生 Rust 解析 |
| **Tool 类型** | 上游定义 | Shell / HTTP / Script 三类 |
| **Streaming** | `EmbeddedPiSession`（订阅模型） | `TurnEvent` channel（流式） |
| **历史管理** | `limitHistoryTurns()` | `trim_history()` / `fast_trim_tool_results()` |
| **Thinking** | 上游实现 | `/think:<level>` 指令（thinking.rs, 427 行） |

---

## 二、主循环 Architecture

### 2.1 ZeroClaw 主循环（loop_.rs）

ZeroClaw 的主循环分三层，职责清晰：

```
run()                       ← CLI/Daemon 入口，3464 行
  └─> process_message()     ← 频道入口，4454 行
        └─> run_tool_call_loop()  ← 核心迭代，2275+ 行
```

**`run_tool_call_loop()` 内部结构（逐轮迭代）：**

```rust
for iteration in 0..max_iterations {
    // 1. 先发制人的 token 预算管理（预估 + trim）
    // 2. 检查模型切换请求
    // 3. 每轮重新构建 tool_specs（支持延迟加载的 MCP tools）
    // 4. Vision provider 路由
    // 5. 发送 LLM 请求（streaming 或 blocking）
    // 6. 解析响应（text + tool_calls）
    // 7. 执行工具（并行或串行）
    // 8. 循环检测（检测重复工具调用）
    // 9. 将结果追加到 history
}
```

**关键设计点：**
- **每轮重建 tool_specs**：MCP tools 支持延迟加载，每轮检查是否有新增工具
- **流式和非流式两套路径**：通过 `TurnEvent` channel 支持实时事件订阅
- **内置循环检测**：通过 `loop_detector` 检测重复工具调用模式

### 2.2 OpenClaw 主循环（attempt.ts）

OpenClaw 将核心循环委托给上游 `@mariozechner/pi-coding-agent` 框架，本地只做封装：

```
agent-runner-utils.ts::buildEmbeddedRunBaseParams()
  └─> EmbeddedAgent.run()   ← 上游核心循环
        └─> attempt.ts       ← 每个 attempt 的执行逻辑
```

**本地封装的职责：**
- `buildEmbeddedContextFromTemplate()` — 组装每轮的上下文模板
- `resolveSkillsPromptForRun()` — 解析 Skills prompt
- `applySkillEnvOverridesFromSnapshot()` — 应用技能环境覆盖

---

## 三、工作流分解

### 3.1 ZeroClaw 完整工作流

```
用户消息
    │
    ▼
process_message()
    │ 验证会话 & 加载会话状态
    ▼
run_tool_call_loop()
    │
    ├── 1. Token 预算预检（emergency_trim if 超过 80%）
    ├── 2. 检查模型切换（/model:xxx）
    ├── 3. 重建 tool_specs（Deferred MCP tools）
    ├── 4. Vision routing
    │
    ▼
    Agent::build_system_prompt()
    │ 组装顺序:
    │   DateTime → Identity → ToolHonesty → Tools
    │   → Safety → Skills → Workspace → Runtime → ChannelMedia
    ▼
    发送 LLM 请求
    │
    ├── [流式] TurnEvent channel ← 实时事件推送
    └── [阻塞] 直接等待 Response
    │
    ▼
    解析 Response
    │  ┌─ Text → 直接回复用户
    │  └─ tool_calls → 进入工具执行阶段
    │
    ▼
    工具执行（并行/串行）
    │
    ▼
    结果追加到 history
    │
    ▼
    [loop detection] ──→ 若检测到循环则终止
    │
    └──→ 回到 step 1（下一轮迭代）
```

### 3.2 OpenClaw 完整工作流

```
用户消息
    │
    ▼
ContextEngine::assemble()   ← 组装上下文
    │   - 检索 Memory（RAG）
    │   - 加载 Bootstrap Files
    │   - Token 预算检查
    ▼
EmbeddedAgent::run()
    │
    ├── 内嵌上游 tool_call_loop
    │   ├── 发送请求（含 skills snapshot）
    │   ├── 解析响应
    │   ├── 执行工具
    │   └── 循环直到完成
    │
    ▼
ContextEngine::afterTurn()  ← 轮次后处理
    │   - 追加消息到 session file
    │   - 触发自动压缩（若配置）
    ▼
返回结果给用户
```

---

## 四、上下文注入（Context Injection）

### 4.1 ZeroClaw 上下文组装

**入口：`build_context()`（loop_.rs, line 307）**

```rust
// 1. Memory RAG 检索
let mem_context = build_context(
    mem.as_ref(),
    &effective_msg,
    config.memory.min_relevance_score,
    session_id.as_deref()
).await;

// 2. Hardware RAG（可选）
let rag_limit = if config.agent.compact_context { 2 } else { 5 };
let hw_context = hardware_rag.as_ref()
    .map(|r| build_hardware_context(r, &effective_msg, &board_names, rag_limit))
    .unwrap_or_default();

// 3. 组装 Enriched Message
let context = format!("{mem_context}{hw_context}");
let enriched = if context.is_empty() {
    format!("[{now}] {effective_msg}")
} else {
    format!("{context}[{now}] {effective_msg}")
};
```

**上下文注入点：**

| 上下文类型 | 加载时机 | 位置 |
|-----------|---------|------|
| System Prompt | 每轮重建 | `build_system_prompt()` → `PromptContext` |
| Memory RAG | 每轮 | `build_context()` |
| Hardware RAG | 每轮 | `build_hardware_context()` |
| Bootstrap Files | 会话启动 | `bootstrap.ts::resolveBootstrapContextForRun()` |
| Skills | 会话启动（snapshot） | `skills_to_prompt_with_mode()` |

### 4.2 OpenClaw 上下文组装

**入口：`ContextEngine::assemble()`**

```typescript
assemble(params: {
  sessionId: string;
  messages: AgentMessage[];
  tokenBudget?: number;
  model?: string;
  prompt?: string;
}): Promise<AssembleResult>
```

**上下文类型：**

| 上下文类型 | 加载时机 | 位置 |
|-----------|---------|------|
| Bootstrap Files | 会话启动 | `bootstrap-files.ts::resolveBootstrapContextForRun()` |
| Memory | 每轮 assemble | ContextEngine 实现 |
| Skills Snapshot | 会话进入时 | `SessionSkillSnapshot`（sessions/types.ts） |
| Session History | 每轮 | `assemble()` 返回的 `AssembleResult` |

**Bootstrap Files 优先级（openclaw）：**
- `AGENTS.md` / `SOUL.md` / `USER.md` / `TOOLS.md`
- `bootstrap.md` — 首次会话引导
- `memory.md` — 跨会话持久记忆
- `HEARTBEAT.md` — 心跳保活（`contextMode: "lightweight"` 时只保留此文件）

**ContextEngine 生命周期接口：**

```typescript
interface ContextEngine {
  // 启动时：加载 bootstrap files 和初始上下文
  bootstrap(): Promise<void>;

  // 每轮前：组装完整上下文
  assemble(params: AssembleParams): Promise<AssembleResult>;

  // 每轮后：处理消息摄入、后处理
  afterTurn(params: AfterTurnParams): Promise<void>;

  // 压缩：Token 预算超限时触发
  compact(params: CompactParams): Promise<CompactResult>;

  // 摄入单条消息（用于实时注入）
  ingest(params: IngestParams): Promise<IngestResult>;

  // 准备子 agent 诞生时的上下文
  prepareSubagentSpawn(params: SubagentParams): Promise<SubagentResult>;
}
```

---

## 五、Skills 机制深度拆解

### 5.1 Skill 定义对比

**OpenClaw Skill（canonical 上游类型）：**

```typescript
// openclaw/src/agents/skills/skill-contract.ts
interface Skill {
  name: string;
  description: string;
  filePath: string;        // SKILL.md 的文件系统路径
  instructions?: string;   // 可选，内联指令
  version?: string;
  author?: string;
}
```

**ZeroClaw Skill（原生 Rust）：**

```rust
// zeroclaw/src/skills/mod.rs
struct Skill {
    name: String,
    description: String,
    version: String,
    author: String,
    tags: Vec<String>,
    tools: Vec<SkillTool>,        // 技能自带工具
    prompts: Vec<String>,         // 附加 prompt 片段
}

struct SkillTool {
    name: String,
    description: String,
    kind: String,                 // "shell" | "http" | "script"
    command: String,
}
```

### 5.2 Skill 加载流程

**OpenClaw 加载路径：**

```
工作空间 .workbuddy/skills/<name>/SKILL.md
项目级 .workbuddy/skills/<name>/SKILL.md
用户级 ~/.workbuddy/skills/<name>/SKILL.md
```

**ZeroClaw 加载路径：**

```
load_workspace_skills()
  └─> load_skills_from_directory("~/.zeroclaw/workspace/skills/<name>/")

load_open_skills()
  └─> git clone https://github.com/besoeasy/open-skills
  └─> 加载其中的 SKILL.toml + SKILL.md
```

### 5.3 Skill 文件结构

**ZeroClaw 典型 SKILL.toml：**

```toml
[skill]
name = "example-skill"
description = "Does something useful"
version = "1.0.0"
author = "someone"

[tool.shell.my_command]
description = "Run a shell command"
command = "echo hello"
```

**OpenClaw 典型 SKILL.md：**

````markdown
# Skill: example-skill

## Description
Does something useful

## Instructions
Follow these steps:
1. Step one
2. Step two
````

### 5.4 Skill 渲染与 System Prompt 注入

#### ZeroClaw — 双模式渲染

**Full 模式（全部内联）：**

```rust
"Skill instructions and tool metadata are preloaded below.
 Follow these instructions directly; do not read skill files at runtime..."

<available_skills>
  <skill>
    <name>example-skill</name>
    <description>Does something useful</description>
    <location>~/.zeroclaw/workspace/skills/example-skill/</location>
    <instructions>
      <instruction>Follow these steps:
1. Step one
2. Step two</instruction>
    </instructions>
  </skill>
</available_skills>
```

**Compact 模式（按需加载）：**

```rust
"Skill summaries are preloaded below to keep context compact.
 Skill instructions are loaded on demand: call `read_skill(name)`..."

<available_skills>
  <skill>
    <name>example-skill</name>
    <description>Does something useful</description>
    <location>~/.zeroclaw/workspace/skills/example-skill/</location>
  </skill>
</available_skills>
```

#### OpenClaw — 统一 XML 格式

**所有模式统一渲染为（system-prompt.ts）：**

```xml
## Skills (mandatory)
Before replying: scan <available_skills> <description> entries.
- If exactly one skill clearly applies: read its SKILL.md at <location> with `read_skill`, then follow it.
- If multiple could apply: choose the most specific one, then read/follow it.
- If none clearly apply: do not read any SKILL.md.
Constraints: never read more than one skill up front; only read after selecting.

<available_skills>
  <skill>
    <name>example-skill</name>
    <description>Does something useful</description>
    <location>/path/to/SKILL.md</location>
  </skill>
</available_skills>
```

**关键差异：** OpenClaw 的 Skill 内不内联 `<instructions>`，由 LLM 通过 `read_skill` 工具按需读取；ZeroClaw 的 Full 模式则直接内联全部指令。

### 5.5 Skill 快照与会话持久化

**OpenClaw SessionSkillSnapshot：**

```typescript
// openclaw/src/config/sessions/types.ts
export type SessionSkillSnapshot = {
  prompt: string;                          // skills 渲染后的 prompt
  skills: Array<{
    name: string;
    primaryEnv?: string;
    requiredEnv?: string[];
  }>;
  skillFilter?: string[];                   // 启用的 skill 名称列表
  resolvedSkills?: Skill[];                // 解析后的完整 Skill 对象
  version?: number;
};
```

**会话进入时：** 快照被创建并存储；后续每轮复用同一快照，避免重复解析。

**ZeroClaw：** 无显式快照机制，每轮通过 `skills_prompt_mode` 配置决定渲染模式。

---

## 六、Tool Dispatcher 机制

### 6.1 ZeroClaw — 两种分发模式

```rust
// zeroclaw/src/agent/dispatcher.rs
pub enum ToolDispatcher {
    Xml(XmlToolDispatcher),      // 解析 <tool_call>{"name":"...","arguments":{...}}</tool_call>
    Native(NativeToolDispatcher), // 原生函数调用格式
}
```

**XmlToolDispatcher 解析流程：**

```
LLM 文本输出
  └─> 正则匹配 <tool_call>...</tool_call>
        └─> JSON 解析 name + arguments
              └─> 路由到对应 Tool 实现

Tool 结果
  └─> 包装为 ToolResult 消息
        └─> 追加到 conversation history
```

### 6.2 OpenClaw — 委托上游

OpenClaw 不实现自己的 dispatcher，委托给上游 `@mariozechner/pi-coding-agent` 的 `ToolDispatcher`。上游支持流式工具调用解析。

---

## 七、Thinking 机制

### ZeroClaw（本地实现）

```rust
// zeroclaw/src/agent/thinking.rs
enum ThinkingLevel {
    Off,      // 无思考
    Minimal,
    Low,
    Medium,   // 默认
    High,
    Max,
}
```

**指令格式：** `/think:<level>`（如 `/think:high`）

**效果：**
- 调整 temperature 参数
- 在 system prompt 前添加 thinking 指令前缀

### OpenClaw

thinking 机制由上游 Pi 框架实现。

---

## 八、Context 压缩与历史管理

### ZeroClaw 压缩策略

```rust
// 1. emergency_history_trim() — 紧急裁剪
//    删除最老的非系统消息，直到预算足够

// 2. fast_trim_tool_results() — 激进裁剪
//    将长 tool result 截断到 500 chars

// 3. trim_history() — 常规裁剪
//    保留系统 prompt + 最近 N 条消息
//    token 估算：~4 chars/token
```

### OpenClaw 压缩策略

通过 `ContextEngine::compact()` 接口实现：

```typescript
compact(params: {
  sessionId: string;
  tokenBudget?: number;
  force?: boolean;
  compactionTarget: "budget" | "threshold";
  customInstructions?: string;
}): Promise<CompactResult>
```

---

## 九、关键架构差异总结

| 差异点 | OpenClaw | ZeroClaw |
|--------|----------|----------|
| **语言范式** | TypeScript/Node.js，动态类型 | Rust，静态类型 + 所有权 |
| **复杂度分布** | 分散在上游框架，本地薄封装 | 单文件 9549 行，全部自包含 |
| **Context 接口** | 可插拔 `ContextEngine` 接口，支持多种实现 | 内置 `build_context()` 函数 |
| **Skill 工具类型** | 复用上游工具类型 | 自定义 `ShellTool`/`HttpTool`/`ScriptTool` |
| **多会话管理** | Session 文件 + SessionSkillSnapshot | 通过 `Session` struct 管理 |
| **Tool 结果处理** | 上游处理 | `fast_trim_tool_results()` 主动压缩 |
| **心跳机制** | `contextMode: "lightweight"` 保留 HEARTBEAT.md | 消息通道保活 |
| **并行工具执行** | 上游决定 | `execute_tools_parallel()` 显式并行 |

---

## 十、核心流程图（Text-based）

### ZeroClaw 完整消息处理流程

```
HTTP/WebSocket
    │
    ▼
process_message()
    │
    ▼
┌─────────────────────────────────────────┐
│  run_tool_call_loop()  [iteration: 0..N] │
│                                         │
│  ① preemptive_token_check()             │
│     └─ emergency_trim if > 80% budget   │
│                                         │
│  ② check_model_switch()                  │
│     └─ /model:xxx directive             │
│                                         │
│  ③ rebuild_tool_specs()                 │
│     └─ 重新加载 deferred MCP tools       │
│                                         │
│  ④ build_system_prompt()                │
│     ├─ DateTime                         │
│     ├─ Identity                         │
│     ├─ ToolHonesty                      │
│     ├─ Tools (tool_specs)               │
│     ├─ Safety                           │
│     ├─ Skills [skills_to_prompt()]      │
│     ├─ Workspace                        │
│     ├─ Runtime                          │
│     └─ ChannelMedia                     │
│                                         │
│  ⑤ send_llm_request()                   │
│     └─ streaming or blocking            │
│                                         │
│  ⑥ parse_response()                     │
│     ├─ text → 回复用户                  │
│     └─ tool_calls → ⑦                  │
│                                         │
│  ⑦ execute_tools()                      │
│     ├─ parallel: execute_tools_parallel│
│     └─ sequential: for each            │
│                                         │
│  ⑧ loop_detector.check()               │
│     └─ 检测重复调用模式 → 终止          │
│                                         │
│  ⑨ append_to_history()                  │
│     └─ result messages                  │
│                                         │
│  ⑩ after_iteration_cleanup()            │
└─────────────────────────────────────────┘
    │
    ▼
返回 TurnResult / 流式事件
```

---

## 十一、文件索引

### ZeroClaw 关键文件

| 文件 | 行数 | 职责 |
|------|------|------|
| `zeroclaw/src/agent/loop_.rs` | 9549 | 主循环入口、tool call loop |
| `zeroclaw/src/agent/agent.rs` | 1892 | Agent struct、turn()、build_system_prompt() |
| `zeroclaw/src/skills/mod.rs` | 2192 | Skill 加载、渲染、双模式 |
| `zeroclaw/src/agent/prompt.rs` | 686 | System prompt 构建器 |
| `zeroclaw/src/agent/dispatcher.rs` | 444 | XML/Native tool dispatcher |
| `zeroclaw/src/agent/history.rs` | 173 | 历史裁剪、token 估算 |
| `zeroclaw/src/agent/thinking.rs` | 427 | Thinking level 指令解析 |

### OpenClaw 关键文件

| 文件 | 职责 |
|------|------|
| `openclaw/src/agents/pi-embedded-runner/run/attempt.ts` | 核心 attempt 执行 |
| `openclaw/src/agents/system-prompt.ts` | Skills XML 渲染 |
| `openclaw/src/agents/skills/skill-contract.ts` | Skill 类型定义 |
| `openclaw/src/context-engine/types.ts` | ContextEngine 接口 |
| `openclaw/src/config/sessions/types.ts` | SessionSkillSnapshot |
| `openclaw/src/auto-reply/reply/agent-runner-utils.ts` | 上下文组装工具 |
| `openclaw/src/agents/bootstrap-files.ts` | Bootstrap 文件加载 |
