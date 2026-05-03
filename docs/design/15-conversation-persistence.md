# 对话持久化与 Session 机制设计

> 版本：v3.9 | 更新日期：2026-05-03

> 本文档定义 RollBall Agent 的对话持久化架构，采用"原始文件 + 提炼记忆"双层设计。原始对话以 JSONL 格式按 session 存储，用于界面渲染和历史回放；Grafeo Episode 存储从对话提炼的情景记忆摘要，服务于检索和关联扩散。主要变更（v3.9）：Session ID 去掉 agent_id 前缀（§1.2）、去掉 sessions.json 改用 JSONL 第一行元数据做索引（§1.3）、新增 Channel 单写入者并发写入设计（§2.5）、Episode 提炼改为事件触发而非每轮（§3.3）、AutobiographicalNode 改为打包（§5）、Gateway 通过 IPC 访问对话而非直接读文件（§6）、新增分页加载 API（§6.2）。

---

## 0. 概述

### 0.1 问题背景

当前 RollBall Agent 的对话存在以下问题：

1. **切换 Agent 后聊天记录丢失**：对话历史仅存在于 Runtime 进程内存（`Vec<Message>`），进程退出即消失
2. **记忆链条断裂**：MemoryManager.record() 将 user_message 和 assistant_response 合并为一个 Episode，content 存储原始对话全文，导致 Grafeo 体积膨胀、检索噪声大
3. **对话恢复不一致**：Gateway conversations/latest API 从 Grafeo Episode 读取，但 Episode 经过压缩/提炼，用户看到的"历史消息"与实际对话内容不一致
4. **缺乏 Session 管理**：Runtime 启动时无 session_id 生成逻辑，无法区分不同时间段的对话

### 0.2 双层架构总览

```
┌─────────────────────────────────────────────────────────────────┐
│  原始对话层（JSONL 文件）                                        │
│  ─────────────────────────                                       │
│  职责：完整记录用户看到的一切（含 tool_calls、think 块、markdown）│
│  格式：JSONL（每行一个 JSON 对象，追加写入）                      │
│  存储：<agent_workspace>/conversations/<session_id>.jsonl         │
│  消费者：Desktop App 加载历史、用户导出、审计回放                 │
│  生命周期：与 session 同生命周期，App 卸载后可选保留              │
├─────────────────────────────────────────────────────────────────┤
│  提炼记忆层（Grafeo Episode）                                    │
│  ─────────────────────────                                       │
│  职责：存储从对话提炼的情景记忆摘要，服务于检索和关联扩散         │
│  格式：Grafeo Episodic 节点（LPG Label）                         │
│  存储：<agent_workspace>/memory/<agent_id>.grafeo                │
│  消费者：Runtime 记忆检索、巩固管道（Episode → KnowledgeNode）    │
│  生命周期：天→周，巩固后晋升至沉淀层                              │
└─────────────────────────────────────────────────────────────────┘

两层之间的关联：
  Episode.metadata["source_session_id"] → 指向原始 JSONL 文件
  两层各自独立写入，互不依赖

记忆分层流向：
  Conversation（实时写入）→ Episode（事件触发提炼）→ KnowledgeNode（长周期离线巩固）
```

### 0.3 设计目标

| 目标 | 衡量标准 |
|------|---------|
| 对话不丢失 | 切换 Agent 后回来，聊天记录完整恢复 |
| 所见即所存 | 历史消息与当时对话内容完全一致（含 tool_call、think） |
| 记忆可检索 | Grafeo 检索返回语义相关的情景摘要，而非原始对话噪声 |
| 数据隔离安全 | Agent 打包分享时不包含用户私有对话和记忆数据 |
| 实现低成本 | Phase 1 使用 LLM 提取 Episode 摘要，自动选择 cost 最低的可用模型控制成本 |

---

## 1. Session 机制设计

### 1.1 Session 生命周期

```
                          App 启动 / Agent 首次对话
                                    │
                                    ▼
                           ┌─────────────────┐
                           │   Created（创建） │
                           │  生成 session_id  │
                           │  创建 JSONL 文件  │
                           └────────┬────────┘
                                    │
                                    ▼
                           ┌─────────────────┐
                    ┌─────→│  Active（活跃）   │←─────┐
                    │      │  消息正常写入      │      │
                    │      └────────┬────────┘      │
                    │               │               │
                    │   用户长时间无交互            │
                    │   或切换到其他 Agent          │
                    │               │               │
                    │               ▼               │
                    │      ┌─────────────────┐      │
                    │      │  Idle（空闲）     │──────┘
                    │      │  等待恢复或结束    │  用户发新消息
                    │      └────────┬────────┘
                    │               │
                    │   用户主动结束 / App 退出
                    │               │
                    │               ▼
                    │      ┌─────────────────┐
                    │      │  Ended（结束）    │
                    │      │  写入 ended_at   │
                    │      │  JSONL 文件关闭   │
                    │      └─────────────────┘
                    │
                    │   App 重启后选择恢复历史 session
                    │               │
                    └───────────────┘
                        Resumed（恢复）
```

**状态转换规则：**

| 从 | 到 | 触发条件 | 行为 |
|----|-----|---------|------|
| — | Created | Runtime 启动后首次收到用户消息 | 生成 session_id，创建 JSONL 文件（首行写入 session 元数据） |
| Created | Active | JSONL 文件创建成功 | 正常写入消息 |
| Active | Idle | 用户超过 `session_idle_timeout`（默认 30 分钟）无交互 | 更新 JSONL 首行元数据的 last_active_at |
| Idle | Active | 用户发送新消息 | 更新 JSONL 首行元数据，继续写入当前 JSONL |
| Idle | Ended | 用户主动结束对话 / App 关闭 | 写入 ended_at，关闭 JSONL 文件句柄 |
| Active | Ended | 用户主动结束对话 / App 关闭 | 同上 |
| Ended | Active | 用户选择恢复历史 session | 创建新 JSONL 文件，session_id 复用原 ID + `_r1` 后缀 |

恢复后的 session 创建新的 JSONL 文件，首行元数据中通过 `resumed_from` 字段指向原始 session_id。

### 1.2 Session ID 生成策略

格式：`{timestamp}_{short_uuid}`

```
示例：20260502_a1b2c3d4

组成：
  timestamp  = 20260502               （创建日期，YYYYMMDD 格式）
  short_uuid = a1b2c3d4               （8 位十六进制，取自 UUID v4 前 32 bit）
```

**设计理由：**

| 特性 | 说明 |
|------|------|
| 可排序 | timestamp 前缀保证按时间自然排序，文件名即按时间排序 |
| 无冲突 | short_uuid 提供 42 亿种组合，同一天内创建多 session 不冲突 |
| 简洁 | 无需 agent_id 前缀，因为每个 Agent 有物理隔离的独立工作区 |
| 跨平台 | 仅含字母、数字、下划线，兼容所有文件系统 |

**为什么不需要 agent_id 前缀：** 每个 Agent 拥有独立的 `conversations/` 目录（物理隔离），session_id 仅在 Agent 工作区内需要唯一，无需跨 Agent 标识。去掉前缀后文件名更短、更易读。

**恢复 session 的 ID 规则：**

```
原始 session_id：20260502_a1b2c3d4
第一次恢复：     20260502_a1b2c3d4_r1
第二次恢复：     20260502_a1b2c3d4_r2
```

### 1.3 Session 索引机制（文件系统即索引）

**不使用 sessions.json 索引文件**，而是利用 JSONL 文件本身作为索引源。

**核心设计：JSONL 文件第一行为 session 元数据**

每个 `.jsonl` 文件的第一行固定写入 session 元数据（JSON 对象），后续行为消息行：

```rust
/// Session metadata line — always the first line of a .jsonl file.
#[derive(Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Fixed marker to distinguish from ConversationLine
    pub _type: String,                 // always "session_meta"
    pub session_id: String,            // e.g. "20260502_a1b2c3d4"
    pub agent_id: String,              // e.g. "com.rollball.senior-engineer"
    pub created_at: String,            // ISO 8601
    pub last_active_at: String,        // ISO 8601
    pub ended_at: Option<String>,      // ISO 8601, None if still active
    pub message_count: u32,
    pub title: String,                 // 第一条用户消息的前 100 字符或 LLM 生成摘要
    pub status: String,                // "active" | "idle" | "ended"
    pub resumed_from: Option<String>,  // 如果是恢复的 session，指向原始 session_id
}
```

**元数据行示例：**

```json
{"_type":"session_meta","session_id":"20260502_a1b2c3d4","agent_id":"com.rollball.senior-engineer","created_at":"2026-05-02T14:30:52Z","last_active_at":"2026-05-02T15:12:33Z","ended_at":null,"message_count":47,"title":"帮我分析这个 Rust 项目的模块依赖关系","status":"active","resumed_from":null}
```

**获取 session 列表的方式：**

```
1. 扫描 <agent_workspace>/conversations/ 目录下的 *.jsonl 文件
2. 读取每个文件的第一行
3. 解析为 SessionMetadata
4. 按文件名排序（timestamp 前缀保证时序）
```

```rust
/// List all sessions by scanning conversation files.
pub fn list_sessions(workspace: &Path) -> Result<Vec<SessionMetadata>> {
    let conv_dir = workspace.join("conversations");
    let mut sessions = Vec::new();

    for entry in std::fs::read_dir(&conv_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "jsonl") {
            if let Ok(meta) = read_session_metadata(&path) {
                sessions.push(meta);
            }
        }
    }

    // Sort by filename (timestamp prefix ensures chronological order)
    sessions.sort_by(|a, b| b.session_id.cmp(&a.session_id)); // newest first
    Ok(sessions)
}

/// Read session metadata from the first line of a JSONL file.
fn read_session_metadata(path: &Path) -> Result<SessionMetadata> {
    let file = std::fs::File::open(path)?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line)?;
    let meta: SessionMetadata = serde_json::from_str(line.trim())?;
    Ok(meta)
}
```

**为什么不用 sessions.json：**

| 考量 | sessions.json 方案 | 文件系统即索引方案 |
|------|-------------------|------------------|
| 状态一致性 | 索引文件可能和实际数据不同步（崩溃、并发写入） | 元数据在数据文件内部，天然一致 |
| 并发冲突 | 多个写入者更新索引文件需要加锁 | 无索引文件，无冲突点 |
| 崩溃恢复 | 索引文件损坏需重建 | 单个 JSONL 文件损坏仅影响自身 |
| 实现复杂度 | 需原子写入、rename、锁机制 | 目录扫描 + 读首行，简单可靠 |
| 性能 | 100 个 session 的索引 < 1ms 读取 | 100 个文件首行读取 < 10ms，可接受 |

### 1.4 Session 在 Runtime 中的实现位置

**当前 Runtime 启动流程（03-agent-runtime.md §1）：**

```
agent-runtime <package_path> --endpoint <socket> --agent-id <id> --workspace <dir>
       │
       ▼
Package Loader → Prompt Builder → History Manager → AgentLoop
```

**新增 Session 初始化：**

```
AgentLoop::new()
       │
       ├─ 扫描 <workspace>/conversations/ 目录
       │   ├─ 找到 status=active 的 JSONL → 恢复该 session
       │   └─ 无 active session → 创建新 session
       │
       ├─ 创建/恢复 ConversationSession
       │   ├─ session_id 生成（新 session）
       │   ├─ 打开 JSONL 文件（append 模式）
       │   └─ 初始化 message_count（从首行元数据读取）
       │
       └─ ConversationSession 注入 AgentLoop
```

**ConversationSession 结构体：**

```rust
/// Manages a single conversation session's JSONL file and metadata.
pub struct ConversationSession {
    session_id: String,
    agent_id: String,
    workspace: PathBuf,
    jsonl_file: std::fs::File,
    metadata: SessionMetadata,     // 首行元数据（内存缓存）
    message_count: u32,
    status: String,               // "active" | "idle" | "ended"
}

impl ConversationSession {
    /// Create a new session (generates a new session_id).
    /// Writes SessionMetadata as the first line of the JSONL file.
    pub fn create(agent_id: &str, workspace: &Path) -> Result<Self>;

    /// Resume an existing session from JSONL file.
    pub fn resume(session_id: &str, workspace: &Path) -> Result<Self>;

    /// End the current session. Updates first-line metadata with ended_at.
    pub fn end(&mut self) -> Result<()>;

    /// Append a message line to the JSONL file.
    pub fn append_message(&mut self, role: &str, content: &str, metadata: serde_json::Value) -> Result<()>;

    /// Update first-line metadata (message_count, last_active_at, etc.).
    pub fn update_metadata(&mut self) -> Result<()>;
}
```

**在 AgentLoop 中的持有方式：**

```rust
pub struct AgentLoop {
    // ... existing fields ...
    conversation: ConversationSession,   // 新增
}
```

### 1.5 多 Session 管理

同一 Agent 可以有多个历史 session，但**同一时刻只有一个是 Active**。

```
conversations/ 目录下的文件：
  20260502_a1b2c3d4.jsonl  → 首行 status=active（当前对话）
  20260501_b2e1f5g6.jsonl  → 首行 status=ended（历史对话）
  20260430_c5d9h7i8.jsonl  → 首行 status=ended（历史对话）

规则：
  1. Runtime 启动时，扫描 conversations/ 目录，最多恢复一个 active session
  2. 用户主动“新建对话” → 当前 session 首行元数据更新为 ended，创建新 session
  3. 用户“恢复历史对话” → 当前 session 结束，恢复目标 session
  4. Ended 的 session 仍可通过 API 读取（只读），不可追加写入
```

**Session 容量约束：**

| 约束 | 默认值 | 说明 |
|------|--------|------|
| 最大历史 session 数 | 100 | 超过后最旧的 session 自动归档 |
| 单 session 最大消息数 | 100,000 | 触发自动轮转（见 §2.6） |

### 1.6 Session 恢复策略

**应用重启时的恢复决策：**

```
App 重启 → Desktop App 连接 Gateway → Gateway 启动 Agent Runtime
       │
       ▼
Runtime 初始化 ConversationSession:
  ├─ conversations/ 目录存在且有 active session
  │   └→ 恢复该 session，加载 JSONL 历史到 History Manager
  │
  ├─ conversations/ 目录存在但无 active session（上次正常退出）
  │   └→ 创建新 session（旧 session 已 ended，不自动恢复）
  │
  └─ conversations/ 目录不存在（首次运行）
      └→ 创建目录，创建新 session
```

**设计选择：不自动恢复 Ended session。** 理由：

- Ended 表示用户主动结束对话，新对话从新 session 开始符合用户预期
- 自动恢复旧 session 可能导致上下文混乱（用户可能已经忘了之前的对话内容）
- 用户可通过 Desktop App 的"历史会话"列表手动恢复

---

## 2. JSONL Conversation 文件规范

### 2.1 文件存储路径

```
<agent_workspace>/conversations/<session_id>.jsonl

示例：
~/.local/share/agent-gateway/agents/com.rollball.senior-engineer/workspace/
  conversations/
    20260502_a1b2c3d4.jsonl
    20260501_b2e1f5g6.jsonl
    20260430_c5d9h7i8.jsonl
```

### 2.2 文件结构总览

每个 JSONL 文件由**首行元数据** + **消息行**组成：

```
┌─────────────────────────────────────────────────────────────────┐
│  第 1 行：Session 元数据（SessionMetadata，见 §1.3）            │
│  _type = "session_meta”，与消息行区分                            │
├─────────────────────────────────────────────────────────────────┤
│  第 2 行起：消息行（ConversationLine）                          │
│  每行一条消息，追加写入                                          │
└─────────────────────────────────────────────────────────────────┘
```

**首行元数据示例：**

```json
{"_type":"session_meta","session_id":"20260502_a1b2c3d4","agent_id":"com.rollball.senior-engineer","created_at":"2026-05-02T14:30:52Z","last_active_at":"2026-05-02T15:12:33Z","ended_at":null,"message_count":47,"title":"帮我分析这个 Rust 项目的模块依赖关系","status":"active","resumed_from":null}
```

**读取时区分首行元数据和消息行：** 通过 `_type` 字段区分——首行固定包含 `"_type": "session_meta"`，消息行无此字段。

### 2.3 消息行格式定义

每行一个 JSON 对象，UTF-8 编码，以 `\n` 结尾。

```rust
/// A single line in the conversation JSONL file.
#[derive(Serialize, Deserialize, Clone)]
pub struct ConversationLine {
    /// Unique message ID (UUID v4)
    pub id: String,
    /// ISO 8601 timestamp with millisecond precision
    pub ts: String,
    /// Message role: "user" | "assistant" | "tool_call" | "tool_result" | "system" | "think"
    pub role: String,
    /// Full message content (markdown, code blocks, etc.)
    pub content: String,
    /// Optional metadata
    #[serde(default)]
    pub metadata: serde_json::Value,
}
```

**各 role 的完整定义：**

| role | 含义 | content 内容 | metadata 必需字段 | metadata 可选字段 |
|------|------|-------------|------------------|------------------|
| `user` | 用户消息 | 用户输入的原始文本 | — | — |
| `assistant` | Agent 回复 | LLM 输出的完整文本（含 markdown） | — | `model`, `provider`, `token_count`, `duration_ms` |
| `think` | 推理过程 | LLM 返回的 ` inmedia` 标签内容 | — | `model` |
| `tool_call` | 工具调用请求 | 工具参数 JSON 字符串 | `tool_name`, `tool_call_id` | `risk_level` |
| `tool_result` | 工具执行结果 | 工具返回的完整内容 | `tool_name`, `tool_call_id` | `success`, `duration_ms` |
| `system` | 系统消息 | 循环检测警告、迭代超时等 | — | `reason` |

**metadata 字段说明：**

```json
{
  "model": "qwen-plus",                      // LLM 模型名
  "provider": "dashscope",                    // LLM Provider
  "token_count": {                            // Token 用量
    "prompt_tokens": 1234,
    "completion_tokens": 567,
    "total_tokens": 1801
  },
  "tool_name": "file_read",                   // 工具名称
  "tool_call_id": "call_abc123",              // 工具调用 ID（关联 tool_call 和 tool_result）
  "duration_ms": 3200,                        // 执行耗时
  "success": true,                            // 工具执行是否成功
  "risk_level": "high",                       // 风险等级（Approval Gate 相关）
  "reason": "loop_detection_warning"          // 系统消息原因
}
```

### 2.3 消息行示例

**一次完整对话的 JSONL 输出：**

```jsonl
{"_type":"session_meta","session_id":"20260502_a1b2c3d4","agent_id":"com.rollball.senior-engineer","created_at":"2026-05-02T14:30:52Z","last_active_at":"2026-05-02T14:30:52Z","ended_at":null,"message_count":0,"title":"","status":"active","resumed_from":null}
{"id":"550e8400-e29b-41d4-a716-446655440000","ts":"2026-05-02T14:30:52.123Z","role":"user","content":"帮我分析这个 Rust 项目的模块依赖关系","metadata":{}}
{"id":"550e8400-e29b-41d4-a716-446655440001","ts":"2026-05-02T14:30:55.456Z","role":"think","content":"我需要先查看 Cargo.toml 来了解 workspace 结构，然后分析各 crate 之间的依赖。","metadata":{"model":"qwen-plus"}}
{"id":"550e8400-e29b-41d4-a716-446655440002","ts":"2026-05-02T14:30:56.789Z","role":"tool_call","content":"{\"path\":\"Cargo.toml\"}","metadata":{"tool_name":"file_read","tool_call_id":"call_abc123"}}
{"id":"550e8400-e29b-41d4-a716-446655440003","ts":"2026-05-02T14:30:57.012Z","role":"tool_result","content":"[workspace]\nmembers = [\"rollball-core\",\"rollball-runtime\",\"rollball-gateway\",\"rollball-grafeo\",\"rollball-memory\",\"rollball-vault\",\"rollball-sign\"]","metadata":{"tool_name":"file_read","tool_call_id":"call_abc123","success":true,"duration_ms":45}}
{"id":"550e8400-e29b-41d4-a716-446655440004","ts":"2026-05-02T14:31:02.345Z","role":"assistant","content":"这个项目采用 Cargo workspace 结构，包含 7 个 crate：\n\n| crate | 职责 |\n|-------|------|\n| rollball-core | 共享类型、错误、配置 |\n| rollball-runtime | Agent 运行时 |\n| rollball-gateway | Gateway 服务 |\n| rollball-grafeo | 记忆引擎 |\n| rollball-memory | 记忆管理器 |\n| rollball-vault | 加密存储 |\n| rollball-sign | 包签名 |\n\n依赖关系：runtime → core, memory, grafeo; gateway → core, grafeo; ...","metadata":{"model":"qwen-plus","provider":"dashscope","token_count":{"prompt_tokens":1234,"completion_tokens":567,"total_tokens":1801},"duration_ms":4200}}
```

### 2.4 特殊内容处理

**think 内容处理：**

LLM 返回的 ` inmedia` 标签内容，在写入 JSONL 前提取为独立的 `role="think"` 行：

```
LLM 返回：
<think>
我需要先查看 Cargo.toml 来了解项目结构。
</think>
这个项目采用 Cargo workspace 结构...

→ 写入两行:
{ "role": "think", "content": "我需要先查看 Cargo.toml 来了解项目结构。", ... }
{ "role": "assistant", "content": "这个项目采用 Cargo workspace 结构...", ... }
```

**tool_call 处理：**

LLM 返回的 tool_calls 数组，每个调用拆分为独立的 `role="tool_call"` 行：

```
LLM 返回 tool_calls:
[{"function": {"name": "file_read", "arguments": "{\"path\": \"main.rs\"}"}, "id": "call_1"},
 {"function": {"name": "shell_exec", "arguments": "{\"cmd\": \"cargo test\"}"}, "id": "call_2"}]

→ 写入两行:
{ "role": "tool_call", "content": "{\"path\": \"main.rs\"}", "metadata": {"tool_name": "file_read", "tool_call_id": "call_1"} }
{ "role": "tool_call", "content": "{\"cmd\": \"cargo test\"}", "metadata": {"tool_name": "shell_exec", "tool_call_id": "call_2"} }
```

**tool_result 处理：**

每个工具执行结果作为独立的 `role="tool_result"` 行，通过 `tool_call_id` 关联到对应的 `tool_call`：

```json
{ "role": "tool_result", "content": "fn main() { println!(...); }", "metadata": {"tool_name": "file_read", "tool_call_id": "call_1", "success": true} }
{ "role": "tool_result", "content": "running 42 tests ... all passed", "metadata": {"tool_name": "shell_exec", "tool_call_id": "call_2", "duration_ms": 3200, "success": true} }
```

### 2.5 写入时机与并发控制

在 Runtime 主循环中，每产生一条消息**立即**追加写入 JSONL 文件：

```
主循环步骤                          JSONL 写入
──────────                          ──────────
⓪ 消息合并（UserMessage）          → 写入 role="user" 行
③ 调用 LLM（streaming）             → 不写入（streaming 中）
③ LLM 返回 think 内容               → 写入 role="think" 行
④ LLM 返回 tool_calls               → 写入 N 行 role="tool_call"
④ LLM 返回纯 text                   → 写入 role="assistant" 行
⑤ 工具执行完成                      → 写入 N 行 role="tool_result"
⑧ 循环检测 Warning                  → 写入 role="system" 行
```

#### Channel 单写入者架构

多线程场景下（工具执行线程、主循环线程并发产生消息），采用 `mpsc::channel` 保证写入原子性：

```
工具线程 A ──┐
工具线程 B ──┼──→ mpsc::Sender<ConversationEntry> ──→ Writer 线程（独占文件，顺序 append）
主循环线程 ──┘
```

**设计要点：**

- 所有线程（主循环、工具执行线程）通过 `mpsc::Sender` 发送消息条目
- 独立的 Writer 线程持有文件句柄，顺序写入
- 每次写入后 `flush()`，确保崩溃恢复
- 无需文件锁，无竞争

```rust
/// Conversation entry sent through the channel.
pub enum ConversationEntry {
    /// A message line to append.
    Message(ConversationLine),
    /// Update session metadata (e.g., message_count, last_active_at).
    UpdateMetadata { message_count: u32, last_active_at: String },
    /// End the session (write ended_at, close file).
    EndSession,
}

/// Background writer that exclusively owns the JSONL file handle.
pub struct ConversationWriter {
    jsonl_file: std::fs::File,
    receiver: mpsc::Receiver<ConversationEntry>,
    metadata: SessionMetadata,
}

impl ConversationWriter {
    /// Run the writer loop. Call from a dedicated thread.
    pub fn run(&mut self) {
        while let Ok(entry) = self.receiver.recv() {
            match entry {
                ConversationEntry::Message(line) => {
                    if let Err(e) = self.write_line(&line) {
                        tracing::error!("Failed to write JSONL: {}", e);
                    }
                }
                ConversationEntry::UpdateMetadata { message_count, last_active_at } => {
                    self.metadata.message_count = message_count;
                    self.metadata.last_active_at = last_active_at;
                    // Rewrite first line (seek + overwrite)
                    if let Err(e) = self.rewrite_metadata() {
                        tracing::error!("Failed to update metadata: {}", e);
                    }
                }
                ConversationEntry::EndSession => {
                    self.metadata.ended_at = Some(chrono::Utc::now().to_rfc3339());
                    self.metadata.status = "ended".to_string();
                    let _ = self.rewrite_metadata();
                    break;
                }
            }
        }
    }

    fn write_line(&mut self, line: &ConversationLine) -> Result<()> {
        // Seek to end before writing
        self.jsonl_file.seek(std::io::SeekFrom::End(0))?;
        serde_json::to_writer(&self.jsonl_file, line)?;
        writeln!(&mut self.jsonl_file)?;
        self.jsonl_file.flush()?;  // Ensure crash recovery
        Ok(())
    }
}
```

**ConversationSession 改为通过 Channel 写入：**

```rust
impl ConversationSession {
    /// Send a message to the writer thread via channel.
    pub fn append_message(&self, role: &str, content: &str, metadata: serde_json::Value) {
        let line = ConversationLine {
            id: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            role: role.to_string(),
            content: content.to_string(),
            metadata,
        };
        if let Err(e) = self.sender.send(ConversationEntry::Message(line)) {
            tracing::error!("Failed to send message to writer: {}", e);
        }
    }
}
```

### 2.6 文件轮转策略

**触发条件（二选一先到）：**

| 条件 | 阈值 | 说明 |
|------|------|------|
| 消息行数 | 100,000 行 | 避免单文件过大影响解析 |
| 文件大小 | 50 MB | 避免磁盘占用过大 |

**轮转流程：**

```
当前 JSONL 达到阈值
       │
       ▼
1. 结束当前 session → 写入 ended_at
2. 创建新 session → 新的 session_id 和 JSONL 文件
3. 新消息写入新文件
4. 历史 session 仍可通过 conversations/ 目录查找
```

**设计选择：轮转而非分片。** 不采用"同一个 session 拆成 part1.jsonl / part2.jsonl"的分片方案，因为：

- 分片增加了文件管理复杂度（需要追踪 part 编号）
- Session 边界是天然的切分点
- 用户对"新对话"的感知是合理的

### 2.7 错误恢复

JSONL 格式的天然容错特性：

| 错误场景 | 影响 | 恢复方式 |
|---------|------|---------|
| 单行 JSON 解析失败 | 仅该行丢失，其余行正常 | 读取时 `skip_invalid_lines = true`，记录警告日志 |
| 文件末尾截断（写入中途崩溃） | 最后一行可能不完整 | 读取时检测最后一行是否以 `}\n` 结尾，不完整则丢弃 |
| sessions.json 损坏 | 不适用（不使用 sessions.json） | 无索引文件，天然免疫 |
| 重复写入（异常恢复后重放） | 出现重复消息行 | 每行有唯一 `id`，前端去重渲染 |

```rust
/// Read all messages from a JSONL file, skipping invalid lines.
/// First line (session metadata) is automatically skipped.
pub fn read_jsonl(path: &Path) -> Vec<ConversationLine> {
    let mut messages = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    let mut is_first_line = true;

    if let Ok(file) = std::fs::File::open(path) {
        for line in std::io::BufReader::new(file).lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let line = line.trim();
            if line.is_empty() { continue; }

            // Skip first line (session metadata)
            if is_first_line {
                is_first_line = false;
                if line.contains("\"session_meta\"") { continue; }
            }

            match serde_json::from_str::<ConversationLine>(line) {
                Ok(msg) => {
                    // Deduplicate by message ID
                    if seen_ids.insert(msg.id.clone()) {
                        messages.push(msg);
                    }
                }
                Err(e) => {
                    tracing::warn!("Skipping invalid JSONL line: {}", e);
                }
            }
        }
    }

    messages
}
```

---

## 3. Episode 提炼职责设计

### 3.1 Episode 不再做什么（与旧设计的区别）

| 旧行为 | 问题 | 新行为 |
|--------|------|--------|
| 存储原始对话全文 | Grafeo 体积膨胀，检索噪声大 | 仅存提炼摘要（见 §3.2） |
| 作为"对话历史恢复"的数据源 | 压缩/提炼后的内容与用户看到的实际对话不一致 | 对话恢复由 JSONL 文件负责 |
| role 设为 "conversation"（混合 user+assistant） | 语义模糊，检索时无法区分用户意图和 Agent 响应 | role 保持 "user"/"assistant"，但 content 为摘要 |
| 包含完整 tool_call 参数和输出 | 工具输出可能包含敏感信息或极长内容 | 仅存工具使用摘要 |

### 3.2 Episode 要做什么（新职责定义）

Episode 是对一轮或多轮对话的**语义压缩摘要**，服务于记忆检索和关联扩散。

提炼内容应包括：

| # | 提炼项 | 说明 | 示例 |
|---|--------|------|------|
| 1 | **对话摘要** | 这轮对话的核心主题和结论（1-3 句话） | "讨论了 Grafeo Episode 存储策略，决定采用双层架构" |
| 2 | **用户意图标注** | 用户在这轮对话中的意图分类 | 意图 = 指令（让 Agent 做某事） |
| 3 | **关键决策记录** | 对话中产生的技术选型或架构决策 | 决策 = "JSONL 追加写入，而非 SQLite" |
| 4 | **情感/态度信号** | 用户满意度（Phase 3 可选） | （Phase 3 实现） |
| 5 | **工具使用摘要** | 调用了哪些工具、成功/失败、核心结果 | "file_read(成功), shell_exec(失败:超时)" |
| 6 | **关联线索** | 与其他记忆节点的潜在关联关键词 | keywords = ["Grafeo", "Episode", "JSONL"] |

### 3.3 提炼时机（事件触发，非每轮）

**不在每轮对话结束时提炼 Episode**，而是采用事件触发机制。理由：每轮提炼产生大量低价值 Episode（如简单问答），造成 Grafeo 膨胀和噪声。

#### 触发时机一：上下文压缩时（Phase 1 实现）

当 History Manager 的 FIFO 裁剪机制将历史消息丢弃前，对即将被裁剪的部分做 Episode 提炼。

```
主循环检测到 history token 数接近上限
       │
       ▼
FIFO 裁剪：移除最旧的消息
       │
       ▼
在移除前，对被裁剪的消息段执行 Episode 提炼
       │
       ▼
提炼结果写入 Grafeo（Episode 节点）
```

**设计理由：** 被裁剪的消息即将从上下文窗口消失，提炼为 Episode 确保其语义信息不丢失；仍在上下文窗口内的消息不需要提炼，因为 LLM 可以直接看到。

**提炼方式：使用 LLM 进行语义压缩提取**

为被裁剪的消息段构造提炼 prompt，调用 LLM 生成结构化摘要。LLM 提取比规则模板更精确，能理解上下文语义，产出更高质量的摘要。

**模型选择策略：**

Runtime 的 LLM 配置列表中每个模型带有 `cost` 参数（单位：$/1K tokens）。提炼时自动选择 `cost` 最低的可用模型作为提炼模型，最大限度控制成本。

| 步骤 | 行为 |
|------|------|
| 1. 获取可用模型 | 从 Runtime LLM 配置列表筛选状态为可用的模型 |
| 2. 按 cost 排序 | 按 `cost` 升序排列，取最便宜的一个 |
| 3. 调用提炼 | 使用选定模型发送提炼 prompt |
| 4. 降级处理 | 若所有模型均不可用（如网络中断、配额耗尽），本次不执行提炼，等下次触发时机再尝试 |

**LLM 提炼 Prompt 设计：**

提炼指令应要求 LLM 输出以下结构化字段：

| 字段 | 说明 |
|------|------|
| `summary` | 对话核心主题和结论（1-3 句话） |
| `intent_type` | 用户意图分类（提问/指令/反馈/讨论） |
| `decision` | 对话中产生的关键决策或技术选型（如有） |
| `tool_summary` | 工具调用列表及结果状态（如 `file_read(成功), shell_exec(失败:超时)`） |
| `keywords` | 与其他记忆节点潜在关联的关键词列表 |

Prompt 应包含被裁剪消息的完整内容（JSONL 片段或结构化文本），并明确要求以 JSON 格式返回上述字段。

**示例输出（LLM 返回的 JSON）：**

```json
{
  "summary": "讨论了对话持久化方案，决定采用 JSONL + Episode 双层架构",
  "intent_type": "指令",
  "decision": "采用 JSONL 追加写入格式存储原始对话",
  "tool_summary": "file_read(成功), shell_exec(成功)",
  "keywords": ["JSONL", "Episode", "对话持久化"]
}
```

提炼完成后，Runtime 将 LLM 返回的 JSON 映射为 `DistilledEpisode` 结构体，写入 Grafeo。

#### 触发时机二：Session 结束时（Phase 1 实现）

用户主动结束或超时关闭 session 时，对整个 session 做摘要提炼。

```
Session 转为 Ended 状态
       │
       ▼
读取完整 JSONL 文件
       │
       ▼
对整个 session 的对话生成摘要 Episode
  - 核心主题（1-3 句话）
  - 关键决策列表
  - 工具使用统计
       │
       ▼
写入 Grafeo（Episode 节点）
```

**Session 级 Episode vs 裁剪级 Episode：**

| 维度 | 裁剪级 Episode | Session 级 Episode |
|------|--------------|------------------|
| 触发时机 | FIFO 裁剪时 | Session 结束时 |
| 粒度 | 消息片段（被裁剪的部分） | 整个 session |
| 内容 | 局部摘要 | 全局摘要 |
| metadata.session_scope | "trimmed" | "full_session" |

#### 触发时机三：离线巩固——做梦机制（Phase 3 实现）

Agent 空闲时，长周期批量回顾多个 Episode，提炼为 KnowledgeNode。

```
Agent 空闲（无活跃 session）
       │
       ▼
巩固管道启动（定时或触发式）
       │
       ▼
批量读取多个 Episode
       │
       ▼
LLM 辅助提炼：
  - 发现跨 session 的隐式关联
  - 生成更高质量的摘要
  - 提取事实 → KnowledgeNode
  - 提取偏好 → KnowledgeNode（偏好）
  - 提取关系 → KnowledgeNode（关系）
  - 升级/降级 Pending 节点
  - 提取情感/态度信号
```

**深度提炼由巩固管道触发（见 05-memory.md §4），不在本设计文档范围内。**

#### 记忆分层总览

```
Conversation（实时写入）
  ↓ 写入时机：每条消息产生时
  ↓ 存储格式：JSONL 文件
  ↓
Episode（事件触发提炼）
  ↓ 触发时机：上下文压缩时 / Session 结束时
  ↓ 存储格式：Grafeo Episodic 节点
  ↓
KnowledgeNode（长周期离线巩固）
  ↓ 触发时机：Agent 空闲时（做梦机制）
  ↓ 存储格式：Grafeo Semantic 节点

Phase 1 实现范围：前两个触发时机
Phase 3 实现范围：做梦机制（离线巩固）
```

### 3.4 Episode 字段映射（新旧对比）

| 字段 | 旧含义 | 新含义 | 变更说明 |
|------|--------|--------|---------|
| `content` | 原始对话全文 | 提炼摘要文本 | 长度从数百~数千字符降至 100~300 字符 |
| `content_type` | 内容分类 | 保持不变 | Informational 现在表示"摘要文本"而非"原始对话" |
| `role` | "user"/"assistant"/"tool" | 保持不变 | 不再使用 "conversation" 混合角色 |
| `session_id` | 关联到 Grafeo Session 节点 | 保持不变 | 同时指向原始 JSONL 文件（同名） |
| `metadata` | 通用元数据 | **新增 `source_session_id`** | 指向原始 JSONL 文件的 session_id |
| `metadata` | — | **新增 `intent_type`** | 用户意图分类 |
| `metadata` | — | **新增 `tool_summary`** | 工具使用摘要 |
| `metadata` | — | **新增 `decision`** | 关键决策记录 |
| `metadata` | — | **新增 `keywords`** | 关联线索关键词 |
| `artifact_refs` | 代码/文件引用 | 保持不变 | 引用文件系统中的工件 |
| `importance` | 重要性评分 | 保持不变 | 评分逻辑不变 |
| `embedding` | 向量嵌入 | 保持不变 | 但对摘要文本生成，质量更高 |

**新增 metadata 字段示例：**

```rust
Episode {
    // ...
    metadata: HashMap::from([
        ("source_session_id", json!("20260502_a1b2c3d4")),
        ("intent_type", json!("指令")),
        ("tool_summary", json!("file_read(成功), shell_exec(失败:超时)")),
        ("decision", json!("采用 JSONL 追加写入格式")),
        ("keywords", json!(["Grafeo", "Episode", "JSONL"])),
    ]),
}
```

### 3.5 Episode 与 KnowledgeNode 的边界

```
┌──────────────────────────────────────────────────────────┐
│  Episode（经历层）                                        │
│  ──────────────                                          │
│  级别：会话级记忆                                         │
│  核心问题："发生了什么"                                    │
│  特征：带时间上下文（什么时候、什么场景）                    │
│  生命周期：天→周，巩固后晋升                              │
│  示例："5月2日讨论了对话持久化方案，决定用 JSONL"          │
├──────────────────────────────────────────────────────────┤
│  KnowledgeNode（沉淀层）                                  │
│  ──────────────                                          │
│  级别：事实级记忆                                         │
│  核心问题："知道了什么"                                    │
│  特征：去时间化，相对持久                                  │
│  生命周期：长期至永久                                      │
│  示例："RollBall 对话持久化采用 JSONL + Episode 双层架构" │
└──────────────────────────────────────────────────────────┘

提炼方向：Episode → KnowledgeNode（通过巩固管道）

关键区别：
- Episode: "昨天和用户讨论后决定用 JSONL"（有时间、有场景）
- KnowledgeNode: "对话持久化用 JSONL 格式"（纯事实、无时间）
```

**巩固管道中的转换：**

```
Episode（会话级记忆）
  │ 巩固管道（即时提取 / 离线回放）
  │
  ├─ 语义提取 → KnowledgeNode（事实）
  │   "RollBall 对话持久化采用 JSONL 格式"
  │
  ├─ 偏好提取 → KnowledgeNode（偏好）
  │   "用户偏好简洁的回复风格"
  │
  └─ 关系提取 → KnowledgeNode（关系）
      "JSONL 与 Episode 是配套的双层设计"
```

---

## 4. 数据流全景图

```
┌──────────┐     消息       ┌──────────┐
│  Desktop  │ ────────────→ │  Gateway  │ ── IPC ──→ ┌──────────┐
│   App     │ ←──────────── │          │ ←──────── │  Runtime  │
└──────────┘  WS streaming  └──────────┘           └─────┬──────┘
     │                           │                       │
     │                           │                       │
     │  GET /conversations       │                       │  主循环每步
     │  (Gateway → IPC → Runtime │                       │  产生消息
     │   → 读取 JSONL → 返回)    │                       │
     │                           │                       ▼
     │                           │              ┌────────────────────┐
     │                           │              │ JSONL 文件（原始）  │
     │                           │              │ role=user/assistant │
     │                           │              │ role=think/tool_*   │
     │                           │              └────────────────────┘
     │                           │                       │
     │                           │                       │ 事件触发
     │                           │                       │ (FIFO裁剪 / Session结束)
     │                           │                       ▼
     │                           │              ┌────────────────────┐
     │                           │              │ Grafeo Episode     │
     │  GET /memory/search       │              │ (提炼摘要)          │
     │  (从 Grafeo 检索)         │              │ 检索 + 关联扩散     │
     │                           │              └────────┬───────────┘
     │                           │                       │
     │                           │                       │ 离线巩固
     │                           │                       │ (做梦机制，Phase 3)
     │                           │                       ▼
     │                           │              ┌────────────────────┐
     │                           │              │ KnowledgeNode      │
     │                           │              │ (沉淀层事实)        │
     │                           │              └────────────────────┘
     │                           │
     │ ←─────────────────────────┘
     │   JSONL 完整历史（通过 IPC） → 渲染对话界面
     │   Grafeo 检索结果 → 记忆面板
```

**数据流详细说明：**

| 数据流 | 方向 | 数据格式 | 触发时机 |
|--------|------|---------|---------|
| 用户消息 → Runtime | Desktop App → Gateway → Runtime IPC | GatewayRequest | 用户发送消息 |
| Runtime → JSONL 写入 | Runtime → 本地文件系统 | JSONL 行 | 每条消息产生时 |
| Runtime → Episode 写入 | Runtime → Grafeo | Episode 节点 | 上下文压缩时 / Session 结束时 |
| Runtime → WS 推送 | Runtime → Gateway → Desktop App | WS chunk/tool_call/done | LLM streaming |
| Desktop App → 加载历史 | Desktop App → Gateway IPC → Runtime → 读取 JSONL | JSONL 分页数据 | 切换 Agent / 重启 |
| Desktop App → 记忆检索 | Desktop App → Gateway → Grafeo | Episode/KnowledgeNode | 记忆面板搜索 |
| Episode → KnowledgeNode | 巩固管道（离线） | 节点属性转换 | Agent 空闲时（做梦机制） |

---

## 5. Agent 打包与对话数据隔离

### 5.1 设计约束

RollBall 的核心设计理念是 Agent 可以打包为 `.agent` 文件进行分享。这带来两个关键问题：

1. **隐私问题**：JSONL 对话文件包含用户的完整聊天记录，属于用户私有数据，绝对不能被打包进 `.agent` 分享出去
2. **体积问题**：长期使用的 Agent 对话文件和记忆数据库可能非常大，打包进去不合理

### 5.2 数据分类：可分享 vs 私有

| 分类 | 目录/文件 | 说明 | 打包策略 |
|------|----------|------|----------|
| **可分享（Agent 定义）** | `manifest.toml` | Agent 元信息和配置 | ✅ 必须包含 |
| | `prompts/` | System prompt、constraints | ✅ 必须包含 |
| | `skills/` | 技能定义（SKILL.md） | ✅ 必须包含 |
| | 工具声明 | manifest.toml 中的 [tools] | ✅ 必须包含 |
| | 签名文件 | 包签名元数据 | ✅ 必须包含 |
| **可分享（Agent 能力记忆）** | KnowledgeNode（Public） | Agent 通用知识 | ✅ 包含 |
| | ProceduralNode | Agent 技能经验 | ✅ 包含 |
| | AutobiographicalNode | Agent 自传体记忆（自身认知和成长） | ✅ 包含 |
| **私有（运行时数据）** | `conversations/` | JSONL 对话文件 | ❌ 必须排除 |
| | Episode | 对话情景记忆（含用户信息） | ❌ 必须排除 |
| | KnowledgeNode（Private） | 用户相关知识 | ❌ 必须排除 |
| | `memory/` | Grafeo 数据库原始文件 | ❌ 必须排除 |
| | `workspace/` 配置 | 用户工作区状态 | ❌ 必须排除 |
| | `config/` 用户修改 | 用户自定义配置 | ⚠️ 可选排除 |

**排除清单（类似 .gitignore 机制）：**

PackageManager 在构建 `.agent` 包时，必须自动跳过以下路径：

```
# Agent 运行时数据排除清单（内置，不可覆盖）
conversations/          # JSONL 对话文件（含首行 session 元数据）
memory/                # Grafeo 数据库
workspace/             # 工作区状态
config/                # 用户自定义配置
data/                  # 用户数据目录
runtime/               # 运行时临时文件
*.log                  # 日志文件
```

### 5.3 Grafeo 记忆数据的打包策略

Grafeo 中的记忆节点按类型和隐私属性决定打包策略：

**打包规则：**

| 记忆类型 | 打包策略 | 理由 |
|---------|---------|------|
| **KnowledgeNode（Public）** | ✅ 打包 | Agent 通用知识，属于 Agent 能力本体，应随 Agent 分享 |
| **KnowledgeNode（Private）** | ❌ 排除 | 与特定用户/项目相关，属私有数据 |
| **ProceduralNode** | ✅ 打包 | Agent 技能经验（如何做某事），属于 Agent 能力 |
| **AutobiographicalNode** | ✅ 打包 | Agent 自传体记忆，记录自身认知和成长（如“我擅长 Rust”、“我学会了更严格地检查边界条件”），属于 Agent 能力本体 |
| **Episode** | ❌ 排除 | 情景记忆含用户对话摘要，涉及隐私 |

**为什么 AutobiographicalNode 需要打包：**

AutobiographicalNode 是 Agent 对自身的认知记录，例如：
- “我擅长 Rust 系统编程”
- “我在代码审查中学会了关注边界条件”
- “用户通常偏好简洁的回复风格”

这些是 Agent 的能力本体，不是用户私有数据。分享 Agent 时，这些认知应随 Agent 一起迁移，让新用户获得一个已有“经验”的 Agent。

**隐私类型（Public/Private）主要用于 KnowledgeNode：**

- Public：Agent 通用知识（如“RollBall 使用 JSONL 格式存储对话”）— 与特定用户无关
- Private：与特定用户/项目相关的知识（如“用户的项目使用 React 框架”）— 不可分享

**PackageManager 打包 Grafeo 数据时的处理：**

```
打包时遍历 Grafeo 图：
  1. KnowledgeNode: 检查 privacy = "public" → 打包；"private" → 排除
  2. ProceduralNode: 全部打包
  3. AutobiographicalNode: 全部打包
  4. Episode: 全部排除
  5. 边（Edge）: 仅打包两端节点均被打包的边
```

### 5.4 PackageManager 实现要求

```rust
/// Directories that must be excluded when building an .agent package.
const PACKAGE_EXCLUDE_DIRS: &[&str] = &[
    "conversations",
    "memory",
    "workspace",
    "config",
    "data",
    "runtime",
];

/// File patterns to exclude when building an .agent package.
const PACKAGE_EXCLUDE_PATTERNS: &[&str] = &[
    "*.log",
    "*.tmp",
];

/// Build an .agent package from an installed agent directory.
pub fn build_agent_package(agent_dir: &Path, output: &Path) -> Result<()> {
    let mut archive = zip::ZipWriter::new(std::fs::File::create(output)?);

    for entry in walkdir::WalkDir::new(agent_dir)
        .into_iter()
        .filter_entry(|e| !should_exclude(e.path(), agent_dir))
    {
        // ... add to archive ...
    }

    archive.finish()?;
    Ok(())
}

fn should_exclude(path: &Path, base: &Path) -> bool {
    let relative = path.strip_prefix(base).unwrap_or(path);
    let name = relative.to_string_lossy();

    PACKAGE_EXCLUDE_DIRS.iter().any(|dir| name.starts_with(dir))
        || PACKAGE_EXCLUDE_PATTERNS.iter().any(|pat| glob_match(pat, &name))
}
```

### 5.5 Agent 卸载与对话数据处理

当 Agent 被卸载重装时，对话记录的处理策略：

| 场景 | 策略 | 理由 |
|------|------|------|
| 卸载（默认） | 保留 conversations/ 和 memory/ | 用户数据不应因卸载而丢失 |
| 卸载（用户选择清除数据） | 删除 conversations/ 和 memory/ | 用户显式选择，需二次确认 |
| 升级 | 保留所有运行时数据 | 升级不涉及数据清除 |
| 克隆（skeleton 模式） | 不复制运行时数据 | 克隆的是 Agent 定义，不是用户数据 |
| 克隆（full 模式） | 复制运行时数据 | 用户显式选择完整克隆 |

**卸载时的交互设计：**

```
用户点击"卸载 Agent"
       │
       ▼
弹出确认对话框：
  "确定要卸载 [Agent 名称] 吗？"
  ☐ 同时删除对话记录和记忆数据
  [取消] [卸载]
       │
       ├─ 未勾选 → 卸载，保留 conversations/ 和 memory/
       └─ 已勾选 → 二次确认 → 删除全部
```

### 5.6 安装时的目录初始化

安装 Agent 时，PackageManager 需确保 `conversations/` 目录存在：

```rust
/// Initialize runtime directories for a newly installed agent.
pub fn init_agent_directories(workspace: &Path) -> Result<()> {
    std::fs::create_dir_all(workspace.join("conversations"))?;
    std::fs::create_dir_all(workspace.join("memory"))?;
    std::fs::create_dir_all(workspace.join("workspace"))?;
    std::fs::create_dir_all(workspace.join("config"))?;
    std::fs::create_dir_all(workspace.join("data"))?;
    std::fs::create_dir_all(workspace.join("runtime"))?;

    // Create conversations directory (no sessions.json needed — index is first-line metadata)
    // Nothing else to initialize for conversations/

    Ok(())
}
```

---

## 6. Gateway API 变更

### 6.1 现有 API 变更

**`GET /api/agents/{id}/conversations/latest` — 数据源与访问路径变更**

| 项目 | 旧行为 | 新行为 |
|------|--------|--------|
| 数据源 | 从 Grafeo Episode 读取 | 从 JSONL 文件读取 |
| 访问路径 | Gateway 直接读文件 | Gateway → IPC → Runtime → 读取 JSONL → 返回 |
| 返回内容 | 压缩/提炼后的内容 | 完整原始对话（含 tool_call、think） |
| 消息角色 | role 仅 user/assistant | role 含 user/assistant/think/tool_call/tool_result/system |
| 排序 | 按 turn_index + timestamp | 按 JSONL 行序（即时间序） |

**为什么 Gateway 不能直接读 JSONL 文件：**

1. **数据隔离**：JSONL 文件属于 Agent 私有数据，Runtime 是唯一访问者
2. **并发安全**：Runtime 的 Writer 线程独占文件句柄，Gateway 直接读取可能导致读写冲突
3. **一致性**：Runtime 持有内存中的 message_count 和元数据状态，通过 IPC 返回的数据更准确
4. **架构清晰**：遵循“Gateway 不触碰 Agent 私有数据”的原则

**新的返回格式：**

```json
{
  "session_id": "20260502_a1b2c3d4",
  "messages": [
    {
      "id": "550e8400-e29b-41d4-a716-446655440000",
      "role": "user",
      "content": "帮我分析这个项目的模块依赖关系",
      "timestamp": 1746195052,
      "metadata": {}
    },
    {
      "id": "...",
      "role": "think",
      "content": "我需要先查看 Cargo.toml...",
      "timestamp": 1746195055,
      "metadata": { "model": "qwen-plus" }
    },
    {
      "id": "...",
      "role": "tool_call",
      "content": "{\"path\": \"Cargo.toml\"}",
      "timestamp": 1746195056,
      "metadata": { "tool_name": "file_read", "tool_call_id": "call_1" }
    },
    {
      "id": "...",
      "role": "tool_result",
      "content": "[workspace]\nmembers = [...]",
      "timestamp": 1746195057,
      "metadata": { "tool_name": "file_read", "tool_call_id": "call_1", "success": true }
    },
    {
      "id": "...",
      "role": "assistant",
      "content": "这个项目的模块依赖关系如下...",
      "timestamp": 1746195060,
      "metadata": { "model": "qwen-plus", "token_count": { "total": 4359 } }
    }
  ]
}
```

### 6.2 新增 API

**`GET /api/agents/{id}/conversations` — 会话列表**

Gateway 转发 IPC 请求到 Runtime，Runtime 扫描 conversations/ 目录读取首行元数据返回。

```json
// Response
{
  "sessions": [
    {
      "session_id": "20260502_a1b2c3d4",
      "created_at": "2026-05-02T14:30:52Z",
      "last_active_at": "2026-05-02T15:12:33Z",
      "message_count": 47,
      "title": "帮我分析这个 Rust 项目的模块依赖关系",
      "status": "active"
    }
  ]
}
```

**`GET /api/agents/{id}/conversations/{session_id}/messages` — 分页加载历史消息**

Gateway 转发 IPC 请求到 Runtime，Runtime 读取 JSONL 文件返回分页数据。

```
请求参数：
  cursor    : 上一页最后一条消息的 ID（首次加载不传）
  limit     : 每页条数（默认 50，最大 200）
  direction : backward（向上加载更早的）/ forward（向下加载更新的）
```

```json
// Response
{
  "session_id": "20260501_b2e1f5g6",
  "messages": [
    { "id": "msg_001", "role": "user", "content": "...", "ts": "...", "metadata": {} },
    { "id": "msg_002", "role": "assistant", "content": "...", "ts": "...", "metadata": {} }
  ],
  "has_more": true,
  "cursor": "msg_001"
}
```

**前端加载行为说明：**

```
┌─────────────────────────────────────────────────┐
│  对话界面                                        │
│  ┌─────────────────────────────────────────────┐ │
│  │ ↑ 加载更多（向上滚动触发）                    │ │
│  │   loadMore(cursor=msg_001, direction=backward)│ │
│  ├─────────────────────────────────────────────┤ │
│  │ msg_001  用户：帮我分析...                    │ │
│  │ msg_002  Agent：这个项目...                   │ │
│  │ ...                                         │ │
│  │ msg_047  用户：谢谢                           │ │
│  ├─────────────────────────────────────────────┤ │
│  │ ↓ 新消息实时到达                              │ │
│  │   WebSocket 推送，追加到底部                   │ │
│  └─────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────┘

初始加载：请求最新 50 条消息（不传 cursor）
向上滚动到顶部：触发 loadMore(cursor=first_msg_id, direction=backward)
新消息到达：WebSocket 实时追加到底部
类似移动端 RecyclerView 的加载方式
```

**`GET /api/agents/{id}/conversations/{session_id}` — 指定会话的元数据**

```json
// Response
{
  "session_id": "20260501_b2e1f5g6",
  "agent_id": "com.rollball.senior-engineer",
  "created_at": "2026-05-01T09:15:22Z",
  "last_active_at": "2026-05-01T10:45:11Z",
  "ended_at": "2026-05-01T10:45:11Z",
  "message_count": 23,
  "title": "今天的天气怎么样？",
  "status": "ended",
  "resumed_from": null
}
```

**`POST /api/agents/{id}/conversations/new` — 新建对话**

```json
// Request (empty body)
{}

// Response
{
  "session_id": "20260502_d4c2e6f8",
  "status": "created"
}
```

**`POST /api/agents/{id}/conversations/{session_id}/resume` — 恢复历史对话**

```json
// Request (empty body)
{}

// Response
{
  "session_id": "20260501_b2e1f5g6",
  "status": "resumed",
  "message_count": 23
}
```

**`DELETE /api/agents/{id}/conversations/{session_id}` — 删除指定会话**

```json
// Response
{
  "deleted": true,
  "session_id": "20260501_b2e1f5g6"
}
```

### 6.3 API 路由汇总

```rust
// 新增/变更的路由（所有 conversation API 通过 IPC 转发给 Runtime）
Router::new()
    // --- 对话（变更：全部通过 IPC 转发）---
    .route("/api/agents/:id/conversations", get(list_conversations))           // IPC → Runtime list_sessions
    .route("/api/agents/:id/conversations/latest", get(get_latest_conversation)) // IPC → Runtime read_jsonl
    .route("/api/agents/:id/conversations/:session_id", get(get_session_meta))  // IPC → Runtime read_metadata
    .route("/api/agents/:id/conversations/:session_id/messages", get(get_messages)) // IPC → Runtime read_jsonl (分页)
    .route("/api/agents/:id/conversations/new", post(new_conversation))        // IPC → Runtime create_session
    .route("/api/agents/:id/conversations/:session_id/resume", post(resume_conversation)) // IPC → Runtime resume_session
    .route("/api/agents/:id/conversations/:session_id", delete(delete_conversation)) // IPC → Runtime delete_session
```

### 6.4 Gateway 通过 IPC 访问对话数据

**Gateway 不直接读取 Agent 的 JSONL 文件**，所有 conversation 相关 API 都通过 IPC 转发给 Runtime 处理。

**正确访问路径：**

```
Desktop App → Gateway HTTP API → IPC → Runtime → 读取 JSONL → 返回分页数据
```

**Runtime 是 Agent 私有数据的唯一访问者。** 理由：

1. Runtime 的 Writer 线程独占文件句柄，外部直接读取可能导致读写冲突
2. Runtime 持有内存中的元数据状态（message_count 等），通过 IPC 返回的数据更准确
3. 遵循“Gateway 不触碰 Agent 私有数据”的架构原则

**Gateway API handler 实现（IPC 转发模式）：**

```rust
/// Get conversation messages via IPC to Runtime.
async fn get_messages(
    agent_id: Path<String>,
    session_id: Path<String>,
    cursor: Query<Option<String>>,
    limit: Query<Option<u32>>,
    direction: Query<Option<String>>,
    gateway_state: &GatewayState,
) -> Result<Json<MessagesResponse>, ApiError> {
    // Forward to Runtime via IPC
    let request = IpcRequest::ConversationMessages {
        session_id: session_id.into_inner(),
        cursor: cursor.into_inner(),
        limit: limit.unwrap_or(50),
        direction: direction.unwrap_or_else(|| "backward".to_string()),
    };

    let response = gateway_state.send_ipc(&agent_id, request).await?;
    // ... parse response ...
}
```

**Runtime 侧 IPC 处理：**

```rust
/// Handle IPC request for conversation messages.
fn handle_conversation_messages(&self, req: &ConversationMessagesRequest) -> IpcResponse {
    let jsonl_path = self.workspace.join("conversations")
        .join(format!("{}.jsonl", req.session_id));

    if !jsonl_path.exists() {
        return IpcResponse::Error("Session not found");
    }

    let messages = read_jsonl_paginated(
        &jsonl_path,
        req.cursor.as_deref(),
        req.limit,
        &req.direction,
    );

    IpcResponse::ConversationMessages(MessagesResponse {
        session_id: req.session_id.clone(),
        messages,
        has_more: /* check if more messages exist */,
        cursor: /* last message id */,
    })
}
```

---

## 7. 对现有代码的影响分析

### 7.1 Runtime 需要改什么

| 文件 | 变更 | 优先级 | 说明 |
|------|------|--------|------|
| `loop_.rs` | 主循环增加 JSONL 写入调用 | P0 | 每个步骤产出消息时通过 channel 发送 ConversationEntry |
| `loop_.rs` | FIFO 裁剪时触发 Episode 提炼 | P0 | 替代当前每轮结束时的 MemoryManager.record() |
| `cli.rs` | 初始化 ConversationSession | P0 | 替代 `// TODO(conversation-persist)` 注释 |
| `cli.rs` | 启动时 session 恢复逻辑 | P1 | 扫描 conversations/ 目录，决定创建新 session 还是恢复 |
| `history.rs` | 支持从 JSONL 文件加载历史 | P1 | `load_from_jsonl()` 方法 |
| `manager.rs` | Session 切换支持 | P2 | 结束当前 session → 创建/恢复新 session |
| 新增 `conversation.rs` | ConversationSession + ConversationWriter 结构体 | P0 | Channel 单写入者架构、JSONL 写入、首行元数据维护 |
| 新增 `episode_distill.rs` | Episode 事件触发提炼逻辑 | P0 | FIFO 裁剪时提炼 + Session 结束时提炼 |

**loop_.rs 变更示意：**

```rust
// 主循环步骤⓪ 消息合并后
self.conversation.append_message("user", &user_content, json!({}));

// 主循环步骤③ LLM 返回 think 内容后
self.conversation.append_message("think", &think_content, json!({"model": model_name}));

// 主循环步骤④ LLM 返回 tool_calls 后
for call in &tool_calls {
    self.conversation.append_message("tool_call", &call.arguments, json!({
        "tool_name": call.name,
        "tool_call_id": call.id,
    }));
}

// 主循环步骤⑤ 工具执行完成后
for result in &tool_results {
    self.conversation.append_message("tool_result", &result.content, json!({
        "tool_name": result.tool_name,
        "tool_call_id": result.call_id,
        "duration_ms": result.duration_ms,
        "success": result.success,
    }));
}

// 主循环步骤④ LLM 返回纯 text 后
self.conversation.append_message("assistant", &text_content, json!({
    "model": model_name,
    "provider": provider_name,
    "token_count": usage,
    "duration_ms": elapsed,
}));

// History FIFO 裁剪时 — 触发 Episode 提炼（替代原每轮 MemoryManager.record()）
// 在 history.trim_oldest() 内部触发
let trimmed_messages = self.history_manager.trim_oldest(tokens_to_free);
if !trimmed_messages.is_empty() {
    let distilled = self.distill_episode(&trimmed_messages);
    self.memory_manager.record_distilled(distilled).await?;
}

// Session 结束时 — 触发 Session 级 Episode 提炼
// 在 ConversationSession::end() 中触发
```
```

### 7.2 Gateway 需要改什么

| 文件 | 变更 | 优先级 | 说明 |
|------|------|--------|------|
| `chat.rs` | conversation API 改为 IPC 转发 | P0 | 不再直接读 JSONL，改为通过 IPC 请求 Runtime 处理 |
| `chat.rs` | 新增 session 管理 API handlers | P1 | list/new/resume/delete，均通过 IPC |
| `chat.rs` | 新增分页加载 API handler | P1 | cursor + limit + direction，通过 IPC 转发 |
| `routes.rs` | 新增路由注册 | P1 | 见 §6.3 |
| `gateway.rs` | IPC 消息类型扩展 | P0 | 新增 ConversationMessages / SessionList 等 IPC 请求/响应类型 |
| 删除 `gateway.rs` | 删除 `get_agent_workspace()` 直接读取方法 | P0 | Gateway 不再直接访问 Agent workspace 文件 |

**chat.rs 变更示意（IPC 转发模式）：**

```rust
// 旧实现：从 Grafeo Episode 读取
pub async fn get_latest_conversation(...) {
    let memory_store = gw.memory_store.clone();
    let episodes = store.get_episodes(None, 10000)?;
    // ... filter, sort, map to ConversationMessage ...
}

// 新实现：通过 IPC 转发给 Runtime
pub async fn get_latest_conversation(...) {
    let response = gw.send_ipc(&agent_id, IpcRequest::LatestConversation).await?;
    // ... parse IPC response to API response ...
}

// 分页加载：通过 IPC 转发
pub async fn get_messages(...) {
    let response = gw.send_ipc(&agent_id, IpcRequest::ConversationMessages {
        session_id,
        cursor,
        limit,
        direction,
    }).await?;
    // ... parse IPC response to API response ...
}
```

### 7.3 Grafeo 需要改什么

| 文件 | 变更 | 优先级 | 说明 |
|------|------|--------|------|
| `types.rs` | Episode.metadata 新增 `source_session_id` 等字段文档 | P1 | 字段语义变更，代码结构不变 |
| `store.rs` | store_episode 的 content 含义从"原始对话"变为"提炼摘要" | P1 | 逻辑不变，但注释和文档需更新 |
| `memory.rs` (rollball-memory) | MemoryManager.record() 改为 record_distilled() | P0 | 接受提炼后的 Episode 而非原始对话 |
| `memory.rs` (rollball-memory) | 新增 Episode 提炼辅助方法 | P1 | LLM 提炼结果解析、metadata 字段组装等 |

**MemoryManager 接口变更：**

```rust
// 旧接口
fn record(&self, user_message: &str, assistant_response: &str) -> Result<()>;

// 新接口
fn record_distilled(&self, episode: &DistilledEpisode) -> Result<()>;

/// Distilled episode produced by LLM-based semantic extraction.
pub struct DistilledEpisode {
    pub session_id: String,
    pub turn_index: u32,
    pub role: String,            // "user" | "assistant"
    pub content: String,         // 提炼摘要（非原始对话）
    pub content_type: ContentType,
    pub metadata: EpisodeMetadata,
    pub importance: f32,
}

pub struct EpisodeMetadata {
    pub source_session_id: String,   // 指向原始 JSONL 文件
    pub intent_type: String,         // 提问/指令/反馈/讨论
    pub tool_summary: String,        // "file_read(成功), shell_exec(失败)"
    pub decision: Option<String>,    // 关键决策
    pub keywords: Vec<String>,       // 关联线索
}
```

### 7.4 Desktop App 需要改什么

| 文件 | 变更 | 优先级 | 说明 |
|------|------|--------|------|
| `chatStore.ts` | `loadConversationHistory` 解析新的消息格式 | P0 | 支持 think/tool_call/tool_result/system 角色 |
| `chatStore.ts` | 新增 session 切换方法 | P1 | loadConversationList / newConversation / resumeConversation |
| `types.ts` | ChatMessage 类型扩展 | P0 | 新增 think/tool_call/tool_result 类型支持 |
| Chat UI 组件 | 支持 think 块折叠/展开 | P2 | think 内容默认折叠，点击展开 |
| Chat UI 组件 | 支持 tool_call/tool_result 渲染 | P2 | 已有基础，需适配新数据格式 |
| 新增 SessionList 组件 | 历史会话列表 UI | P2 | 展示 conversations/ 目录下的会话列表（通过 API 获取） |

**chatStore.ts 变更示意：**

```typescript
// 旧实现：从 Episode 读取，role 仅有 user/assistant
const historyMessages: ChatMessage[] = data.messages.map((msg) => ({
  id: `history-${msg.turn_index}-${msg.role}-${msg.timestamp}`,
  type: msg.role === "user" ? "user" : msg.role === "assistant" ? "assistant" : "system",
  content: msg.content,
  timestamp: msg.timestamp * 1000,
}));

// 新实现：从 JSONL 读取，role 含 user/assistant/think/tool_call/tool_result/system
const historyMessages: ChatMessage[] = data.messages.map((msg) => ({
  id: msg.id,
  type: msg.role as ChatMessageType,  // 直接映射
  content: msg.content,
  timestamp: new Date(msg.ts).getTime(),
  toolName: msg.metadata?.tool_name,
  toolData: msg.metadata?.tool_call_id ? parseToolData(msg) : undefined,
  toolStatus: msg.metadata?.success === false ? "error" : "success",
}));

// 分页加载：向上滚动时加载更早的消息
async function loadMore(cursor: string) {
  const response = await fetch(
    `/api/agents/${agentId}/conversations/${sessionId}/messages?cursor=${cursor}&limit=50&direction=backward`
  );
  const data = await response.json();
  if (data.has_more) {
    nextCursor = data.cursor;  // 保存用于下次加载
  }
  // 将 data.messages 插入到列表顶部
}
```

---

## 8. 实现路线

### Phase 2（当前阶段）— 基础持久化

| 步骤 | 内容 | 验收标准 | 涉及 crate |
|------|------|---------|-----------|
| S1 | 新增 `ConversationSession` + `ConversationWriter` 结构体 | 单元测试：通过 channel 发送消息，Writer 线程正确写入 JSONL 行 | rollball-runtime |
| S2 | JSONL 首行元数据机制 | 单元测试：创建 session 写入首行元数据，list_sessions 扫描目录返回正确列表 | rollball-runtime |
| S3 | 主循环集成 JSONL 写入（Channel 架构） | 集成测试：一次完整对话后 JSONL 文件包含首行元数据和所有角色行 | rollball-runtime |
| S4 | FIFO 裁剪时 Episode 提炼（LLM 语义压缩） | 单元测试：裁剪消息后触发提炼，输出格式正确，metadata 字段完整 | rollball-runtime, rollball-memory |
| S5 | Session 结束时 Episode 提炼 | 单元测试：结束 session 后生成全局摘要 Episode | rollball-runtime, rollball-memory |
| S6 | MemoryManager.record → record_distilled | 单元测试：Episode 不再包含原始对话全文 | rollball-memory, rollball-grafeo |
| S7 | Gateway conversation API 改为 IPC 转发 | 集成测试：API 通过 IPC 返回完整原始对话，含分页 | rollball-gateway, rollball-runtime |
| S8 | Desktop App 适配新消息格式和分页加载 | 手动测试：切换 Agent 后历史完整显示，向上滚动加载更多 | rollball-desktop |
| S9 | PackageManager 打包排除清单（含 Grafeo 节点类型过滤） | 集成测试：构建 .agent 包不含 conversations/ 和 Episode/Private KnowledgeNode | rollball-gateway |

### Phase 3 — Session 管理 & 离线巩固

| 步骤 | 内容 | 验收标准 |
|------|------|---------|
| S10 | Session 列表 API + Desktop App 会话列表 UI | 用户可查看和切换历史会话 |
| S11 | 新建/恢复/删除会话 API | 用户可在 App 中管理多个会话 |
| S12 | Session 恢复（重启 App 后自动恢复） | 重启 App 后自动加载最近活跃 session |
| S13 | 离线巩固（做梦机制） | Agent 空闲时批量提炼 Episode → KnowledgeNode |
| S14 | JSONL 文件轮转 | 单 session 超过 100K 行或 50MB 后自动创建新 session |
| S15 | Agent 卸载时对话数据保留/清除选项 | 用户可选择是否保留对话记录 |

### Phase 5 — Desktop App 完善

| 步骤 | 内容 | 验收标准 |
|------|------|---------|
| S16 | think 块折叠/展开 UI | think 内容默认折叠，点击可展开 |
| S17 | 会话搜索（按内容搜索历史会话） | 搜索结果高亮匹配内容 |
| S18 | 对话导出（Markdown / JSON） | 用户可导出指定会话为本地文件 |

---

## 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| 对话文件格式 | JSONL（非 SQLite / JSON 数组） | 追加写入零开销；单行损坏不影响整体；无需额外依赖；与 LLM 行业实践一致 |
| Session ID 格式 | `{timestamp}_{short_uuid}`（无 agent_id 前缀） | 每个 Agent 有物理隔离的独立工作区，无需跨 Agent 标识；文件名更短、更易读 |
| Session 索引机制 | JSONL 首行元数据（非 sessions.json） | 无状态、无冲突；不存在索引文件和实际数据不一致的问题 |
| Episode 提炼时机 | 事件触发（FIFO 裁剪 / Session 结束），非每轮 | 避免低价值 Episode 膨胀；仅在语义信息可能丢失时提炼 |
| Episode 提炼方式（Phase 1） | LLM 语义压缩提取 | 理解上下文语义，产出高质量摘要；自动选择 cost 最低模型控制成本；无可用模型时降级为不提炼 |
| 对话文件写入机制 | Channel 单写入者（mpsc::channel + Writer 线程） | 无锁无竞争；工具线程和主循环通过 channel 发送，Writer 线程独占文件 |
| 对话文件写入时机 | 每条消息产生时立即写入 + flush | 单条消息丢失不可接受；写入耗时 < 1ms |
| 对话恢复策略 | App 重启时恢复最近的活跃 session | 用户期望对话连续性；异常退出不应丢失上下文 |
| 数据隔离策略 | 打包时内置排除清单 + Grafeo 节点类型过滤 | 隐私安全优先；AutobiographicalNode 和 Public KnowledgeNode 随 Agent 打包分享 |
| Gateway 访问对话方式 | 通过 IPC 转发给 Runtime（非直接读文件） | Runtime 是 Agent 私有数据的唯一访问者；避免读写冲突；架构更清晰 |
| 分页加载 | cursor + limit + direction（非 offset） | 避免长对话 offset 性能问题；cursor 基于消息 ID，插入/删除不影响分页 |
| 文件轮转策略 | Session 边界轮转（非文件内分片） | 用户对“新对话”的感知是合理的；避免分片管理复杂度 |
| 记忆打包策略 | AutobiographicalNode 和 Public KnowledgeNode 打包，Episode 和 Private KnowledgeNode 排除 | Agent 自身认知属于能力本体应分享；用户相关记忆属私有数据不分享 |
