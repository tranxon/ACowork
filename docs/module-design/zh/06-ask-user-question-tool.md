# AskUserQuestion Tool 设计文档

> **ADR-016**: AskUserQuestion Tool — LLM 主动向用户提问
> **状态**: 已实现
> **日期**: 2026-05-21

---

## 1. 问题

当前 RollBall 只有**框架自动拦截**的用户交互（`ApprovalGate` 安全审批），缺少 **LLM 主动向用户提问**的机制。

Agent 在以下场景需要主动问用户：
- "你想用 A 方案还是 B 方案？"
- "请输入你的 API key"
- "这个配置项填什么？"
- "要生成什么风格的内容？"

## 2. 设计目标

1. **LLM 按需调用** — 通过普通 Tool 接口，对 LLM 透明
2. **选项 + Other** — 预定义选项 + 用户可自由输入（主流 UX 模式）
3. **复用现有 `SessionStatus` 状态机** — 与 `ApprovalGate` 共享 `WaitingApproval` 态
4. **前端原生渲染** — Desktop App 直接内联展示 question card
5. **超时 & 取消** — 用户可超时不答或显式取消

---

## 3. 全链路数据流

```
┌─────────────────────────────────────────────────────────────────┐
│ Runtime (AgentLoop)                                              │
│                                                                  │
│  LLM → ask_user_question({question, options?, timeout?})         │
│    ↓                                                             │
│  1. 生成 request_id                                              │
│  2. session.set_status(WaitingApproval { request_id })           │
│  3. try_send_chunk(ChunkEvent::AskQuestion { ... })              │
│  4. BLOCK on oneshot::Receiver  ←── 等待外部恢复信号             │
│    ↓                                                             │
│  5. 收到 approval_decision IPC 消息                              │
│  6. session.set_status(Streaming)                                │
│  7. 返回 structured result → LLM                                 │
│                                                                  │
└──────────┬──────────────────────────────────────────────────────┘
           │ ChunkEvent (AskQuestion)
           ▼
┌─────────────────────────────────────────────────────────────────┐
│ Gateway (gRPC dispatch → HTTP API / WebSocket push)              │
│                                                                  │
│  on_chunk → 识别 AskQuestion 变体                                │
│  → 通过 WS push 到前端                                           │
│  → 同时提供 HTTP POST /agents/:id/ask-answer 接口                │
│    （兼容 approval 的 IPC 恢复路径）                              │
│                                                                  │
└──────────┬──────────────────────────────────────────────────────┘
           │ WS event: { type: "ask_question", payload: {...} }
           ▼
┌─────────────────────────────────────────────────────────────────┐
│ Frontend (Desktop App)                                           │
│                                                                  │
│  收到 ask_question event                                         │
│  → 在聊天流中渲染 QuestionCard (内联)                            │
│  → 显示: title + question + options (radio / buttons)           │
│  → 最后一个选项 = "其他" + textarea                              │
│  → 用户选择/输入 → 点击提交                                     │
│  → WS 发送: { type: "ask_answer", request_id, answer }          │
│                                                                  │
└──────────┬──────────────────────────────────────────────────────┘
           │ Gateway HTTP /agents/:id/ask-answer
           ▼
┌─────────────────────────────────────────────────────────────────┐
│ Gateway → Runtime (IPC)                                          │
│                                                                  │
│  Gateway 封装为 approval_decision IntentReceived                 │
│  → push 回 Runtime                                               │
│  → 解锁阻塞的 oneshot::Receiver                                  │
│  → Tool 恢复 → LLM 继续                                          │
└─────────────────────────────────────────────────────────────────┘
```

---

## 4. 协议定义

### 4.1 Tool Schema（LLM 视角）

```json
{
  "name": "ask_user_question",
  "description": "向用户提问并等待回答。当需要用户决策、澄清或输入时调用。",
  "parameters": {
    "type": "object",
    "properties": {
      "question": {
        "type": "string",
        "description": "要问用户的问题"
      },
      "options": {
        "type": "array",
        "description": "预定义选项列表（可选），最后一个选项带 'other' tag 表示用户可以自由输入",
        "items": {
          "type": "object",
          "properties": {
            "label": {
              "type": "string",
              "description": "选项标签（显示给用户看）"
            },
            "description": {
              "type": "string",
              "description": "选项的简短描述（可选）"
            }
          }
        }
      },
      "title": {
        "type": "string",
        "description": "问题的标题/上下文（可选）"
      },
      "timeout_seconds": {
        "type": "integer",
        "description": "等待用户响应的超时秒数（默认 300）"
      }
    },
    "required": ["question"]
  }
}
```

### 4.2 ChunkEvent 变体（Runtime → Gateway）

新增 `ChunkEvent` 变体：

```rust
#[derive(Debug, Clone, Serialize)]
pub enum ChunkEvent {
    // ... existing variants ...

    /// LLM 通过 ask_user_question tool 向用户提问
    AskQuestion {
        request_id: String,
        question: String,
        options: Vec<AskQuestionOption>,
        title: Option<String>,
        timeout_seconds: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskQuestionOption {
    pub label: String,
    pub description: Option<String>,
}
```

### 4.3 WS Event（Gateway → Frontend）

```json
{
  "type": "ask_question",
  "session_id": "sess-xxx",
  "payload": {
    "request_id": "aq-171201234",
    "question": "你想用什么风格来重构这段代码？",
    "title": "重构风格选择",
    "options": [
      { "label": "函数式", "description": "使用纯函数和不可变数据" },
      { "label": "面向对象", "description": "使用类和继承" },
      { "label": "过程式", "description": "简单的步骤式实现" }
    ],
    "timeout_seconds": 120
  }
}
```

> **注意**: options 数组没有显式的 "Other"。前端约定：**用户不选任何选项直接提交 = Other**。

### 4.4 WS Event（Frontend → Gateway）

```json
{
  "type": "ask_answer",
  "session_id": "sess-xxx",
  "payload": {
    "request_id": "aq-171201234",
    "selected_label": "函数式",
    "custom_text": null
  }
}
```

Other 场景：

```json
{
  "type": "ask_answer",
  "session_id": "sess-xxx",
  "payload": {
    "request_id": "aq-171201234",
    "selected_label": null,
    "custom_text": "我用 mixin 风格"
  }
}
```

取消场景：

```json
{
  "type": "ask_answer",
  "session_id": "sess-xxx",
  "payload": {
    "request_id": "aq-171201234",
    "cancelled": true
  }
}
```

### 4.5 IPC 恢复协议（Gateway → Runtime）

**复用现有的 `approval_decision` IPC 消息机制**（`docs/06-communication.md` 定义的 IntentReceived 路径），只需要在 payload 中区分"这是 ask question 的答案，不是 approval 的批准"。

```json
{
  "action": "approval_decision",
  "payload": {
    "request_id": "aq-171201234",
    "decision": "approved",    // approved / rejected / cancelled
    "answer": {
      "selected_label": "函数式",
      "custom_text": null
    }
  }
}
```

### 4.6 Tool Result（LLM 收到的结果）

**正常响应**：
```json
{
  "responded": true,
  "selected_label": "函数式",
  "custom_text": null,
  "answer": "函数式",
  "cancelled": false
}
```

**Other**：
```json
{
  "responded": true,
  "selected_label": null,
  "custom_text": "我用 mixin 风格",
  "answer": "我用 mixin 风格",
  "cancelled": false
}
```

**取消/超时**：
```json
{
  "responded": false,
  "selected_label": null,
  "custom_text": null,
  "answer": "",
  "cancelled": true,
  "reason": "Timeout after 300 seconds"
}
```

---

## 5. Runtime 实现

### 5.1 AskUserQuestionTool

```rust
// rollball-runtime/src/tools/ask_user_question.rs

use crate::agent::loop_::ChunkEvent;
use crate::agent::session_state::SessionStatus;
use async_trait::async_trait;
use rollball_core::tool::{Tool, ToolSpec};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use uuid::Uuid;

/// LLM 调用 ask_user_question 时的输入参数
#[derive(Debug, Deserialize)]
pub struct AskUserQuestionInput {
    pub question: String,
    pub options: Option<Vec<AskQuestionOption>>,
    pub title: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
}

fn default_timeout() -> u32 { 300 }

#[derive(Debug, Deserialize)]
pub struct AskQuestionOption {
    pub label: String,
    pub description: Option<String>,
}

/// LLM 收到的 tool 执行结果
#[derive(Debug, Serialize)]
pub struct AskUserQuestionOutput {
    pub responded: bool,
    pub selected_label: Option<String>,
    pub custom_text: Option<String>,
    pub answer: String,
    pub cancelled: bool,
    pub reason: Option<String>,
}

/// Shared state for blocking/resuming
#[derive(Clone)]
pub struct AskUserQuestionState {
    pub resume_tx: mpsc::Sender<(String, serde_json::Value)>,
}

pub struct AskUserQuestionTool {
    state: AskUserQuestionState,
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str { "ask_user_question" }
    
    fn spec(&self) -> ToolSpec { /* ... schema ... */ }

    async fn call(&self, input: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let input: AskUserQuestionInput = serde_json::from_value(input)?;
        let request_id = format!("aq-{}", Uuid::new_v4());

        // 1. 获取 session_state 引用（来自 AgentCore）
        // 2. 设置 session status = WaitingApproval
        self.agent_core.session_state_mut()
            .set_status(SessionStatus::WaitingApproval { request_id: request_id.clone() });

        // 3. 发送 ChunkEvent
        self.agent_core.try_send_chunk(ChunkEvent::AskQuestion {
            request_id: request_id.clone(),
            question: input.question,
            options: input.options.unwrap_or_default(),
            title: input.title,
            timeout_seconds: input.timeout_seconds,
        });

        // 4. 阻塞等待 answer（oneshot channel）
        let (tx, rx) = oneshot::channel();
        self.state.resume_tx.send((request_id, tx)).await.map_err(|_| ToolError::Internal)?;
        
        let result = tokio::time::timeout(
            Duration::from_secs(input.timeout_seconds as u64),
            rx,
        ).await;

        // 5. 恢复 status
        self.agent_core.session_state_mut()
            .set_status(SessionStatus::Streaming { message_id: None });

        match result {
            Ok(Ok(value)) => {
                let answer: AskAnswerPayload = serde_json::from_value(value)?;
                Ok(serde_json::to_value(AskUserQuestionOutput {
                    responded: !answer.cancelled,
                    selected_label: answer.selected_label,
                    custom_text: answer.custom_text,
                    answer: answer.custom_text
                        .or(answer.selected_label)
                        .unwrap_or_default(),
                    cancelled: answer.cancelled,
                    reason: if answer.cancelled { Some("User cancelled".into()) } else { None },
                })?)
            }
            _ => {
                // 超时或通道关闭
                Ok(serde_json::to_value(AskUserQuestionOutput {
                    responded: false,
                    selected_label: None,
                    custom_text: None,
                    answer: String::new(),
                    cancelled: true,
                    reason: Some(format!("Timeout after {} seconds", input.timeout_seconds)),
                })?)
            }
        }
    }
}
```

### 5.2 与 AgentLoop 集成

在 AgentLoop 的 `run_tool_call` 中，现有的 `await_approval_decision` 函数需要扩展以支持 ask_question 场景：

```rust
// 现有逻辑：await_approval_decision 已经阻塞等待 external_decision rx
// 核心改动点：
// 1. 将（request_id, oneshot::Sender）注册到 shared map
// 2. 当 approval_decision IPC 消息到达时：查找 map，通过 Sender 发送 answer
// 3. 超时后自动清理 map 中的 entry

// AgentCore 新增字段：
struct AgentCore {
    // ... existing fields ...
    
    /// AskUserQuestion 的挂起请求表
    /// key = request_id, value = 用于唤醒阻塞 Tool 的 oneshot Sender
    pending_questions: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
}
```

---

## 6. Gateway 实现

### 6.1 ChunkEvent 路由

在 `grpc/dispatch.rs` 的 `on_chunk` handler 中：

```rust
// 对 ChunkEvent::AskQuestion 变体：
// 1. 提取 question/options/title/timeout
// 2. 通过 WS push 到前端
// 3. 不注册到 approval_request_id map（那个是给 Shell 审批用的）
//    → AskQuestion 走自己的 WS event 通道

// 对 ask_answer WS event：
// 1. 验证 request_id 存在
// 2. 通过 IntentReceived approval_decision 路径推回 Runtime
// 3. 与现有 approval_decision 共享相同的 IPC 恢复机制
```

### 6.2 WS Event 推送

```rust
// ws push 消息结构
WsMessage {
    type: "ask_question".into(),
    session_id: session_id.clone(),
    payload: serde_json::json!({
        "request_id": event.request_id,
        "question": event.question,
        "options": event.options,
        "title": event.title,
        "timeout_seconds": event.timeout_seconds,
    }),
}
```

### 6.3 ask_answer 处理端点

**WebSocket 接收**（推荐路径，延迟最低）：
```rust
// 在 ws handler 中：
match msg.type {
    "ask_answer" => {
        // 1. 验证 payload 合法性
        // 2. 提取 request_id, selected_label, custom_text, cancelled
        // 3. 通过 IntentReceived 推回 Runtime
    }
}
```

**HTTP 备选**（兼容旧前端）：
```
POST /agents/:id/ask-answer
Content-Type: application/json

{
  "request_id": "aq-xxx",
  "selected_label": null,
  "custom_text": "我用 mixin",
  "cancelled": false
}
```

---

## 7. 前端实现

### 7.1 组件设计

```
┌───────────────────────────────────────┐
│ AskQuestionCard                        │
│                                       │
│  [title]                              │  ← 可选标题，小字
│  ▸ question                           │  ← 主问题，加粗
│                                       │
│  ○ 函数式                              │  ← radio option
│    使用纯函数和不可变数据                │  ← description 小字
│  ○ 面向对象                            │
│    使用类和继承                         │
│  ○ 其他                               │  ← 最后一个选项
│     ┌───────────────────┐             │  ← 选中"其他"时出现 textarea
│     │                   │             │
│     └───────────────────┘             │
│                                       │
│  [ 提 交 ]  [ 取 消 ]                 │
│                                       │
│  120秒后自动取消                       │  ← timeout 倒计时
└───────────────────────────────────────┘
```

### 7.2 设计原则

- **拒绝 modal/dialog** — 内联在聊天流中，与关联内容行保持空间关联（与 `ApprovalGate` 的行为一致）
- **Other 默认展示** — 不选中任何选项时，提交 = Other，显示 textarea
- **全局 accent color** — 提交按钮用 accent color，取消按钮用次要色
- **单选 radio** — 使用 radio button 而非 checkbox（一次只能选一个）

### 7.3 组件伪代码

```tsx
// frontend/src/components/AskQuestionCard.tsx

interface AskQuestionCardProps {
  requestId: string;
  question: string;
  options: AskQuestionOption[];
  title?: string;
  timeoutSeconds: number;
  onAnswer: (answer: AskAnswer) => void;
  onCancel: () => void;
}

function AskQuestionCard({ requestId, question, options, title, timeoutSeconds, onAnswer, onCancel }: AskQuestionCardProps) {
  const [selectedLabel, setSelectedLabel] = useState<string | null>(null);
  const [customText, setCustomText] = useState('');
  const [showOther, setShowOther] = useState(false);
  const [timeLeft, setTimeLeft] = useState(timeoutSeconds);

  // 倒计时
  useEffect(() => {
    const timer = setInterval(() => {
      setTimeLeft(t => { if (t <= 0) { clearInterval(timer); onCancel(); return 0; } return t - 1; });
    }, 1000);
    return () => clearInterval(timer);
  }, []);

  const handleSelect = (label: string) => {
    if (label === '__other__') {
      setShowOther(true);
      setSelectedLabel(null);
    } else {
      setSelectedLabel(label);
      setShowOther(false);
    }
  };

  const handleSubmit = () => {
    if (showOther && customText.trim()) {
      onAnswer({ requestId, selectedLabel: null, customText: customText.trim() });
    } else if (selectedLabel) {
      onAnswer({ requestId, selectedLabel, customText: null });
    } else if (!selectedLabel && !showOther) {
      // 默认当作 Other + textarea
      setShowOther(true);
    }
  };

  return (
    <div className="ask-question-card">
      {title && <div className="question-title">{title}</div>}
      <div className="question-text">{question}</div>
      
      <div className="question-options">
        {options.map(opt => (
          <label key={opt.label} className="option-item">
            <input
              type="radio"
              name={`q-${requestId}`}
              checked={selectedLabel === opt.label}
              onChange={() => handleSelect(opt.label)}
            />
            <span className="option-label">{opt.label}</span>
            {opt.description && <span className="option-desc">{opt.description}</span>}
          </label>
        ))}
        
        {/* "其他"选项 */}
        <label className="option-item">
          <input
            type="radio"
            name={`q-${requestId}`}
            checked={showOther}
            onChange={() => handleSelect('__other__')}
          />
          <span className="option-label">其他</span>
        </label>
        
        {showOther && (
          <textarea
            className="other-textarea"
            placeholder="请输入..."
            value={customText}
            onChange={e => setCustomText(e.target.value)}
            rows={3}
          />
        )}
      </div>
      
      <div className="question-actions">
        <button className="btn-primary" onClick={handleSubmit}>提交</button>
        <button className="btn-secondary" onClick={onCancel}>取消</button>
        <span className="timeout-hint">{formatTime(timeLeft)}</span>
      </div>
    </div>
  );
}
```

### 7.4 WS handler 集成

```tsx
// 在 ws event handler 中：
case 'ask_question':
  // 在聊天流中插入 AskQuestionCard
  // 与 SessionStatus::WaitingApproval 联动：
  // 当 session status 变为 WaitingApproval 时，前端已经知道
  // 所以也可以通过 status 变化来触发渲染，WS event 提供详细 payload
  
  // 建议：WS event 触发渲染 + status 变化作为冗余通知
  addChatMessage({
    type: 'ask_question',
    component: <AskQuestionCard key={payload.request_id} {...payload} />,
  });
  break;

case 'ask_answer':
  // 确认 answer 已送达
  // 找到对应的 AskQuestionCard，标记为"已回复"
  break;
```

### 7.5 已有 `SessionStatus` 联动

现有的 `SessionStatus::WaitingApproval` 已经通过 `ChunkEvent::SessionStateChanged` 推送到前端。AskQuestion Tool 也会设置这个状态，所以前端的 session status 监听逻辑**无需改动**。

区别在于前端需要根据**事件类型**决定渲染什么：
- `ChunkEvent::SessionStateChanged { status: WaitingApproval }` → 前端不知道是 approval 还是 question
- 额外的 `ChunkEvent::AskQuestion { ... }` → 前端知道具体是 question 并渲染 QuestionCard

**因此**：前端仍然需要通过 WS `ask_question` event 来获取详细 payload，同时 `SessionStateChanged` 作为冗余信号。

---

## 8. 与现有系统的差异

| 维度 | 现有 ApprovalGate | 新增 AskUserQuestion Tool |
|------|-------------------|--------------------------|
| **触发源** | 框架自动（风险检测） | LLM 主动（Tool 调用） |
| **选项** | 固定 (Approve / Reject / Always) | 动态（LLM 生成的 options） |
| **"Other"** | ❌ 无 | ✅ 最后一个选项 = Other + textarea |
| **超时处理** | 默认 300s | 由 LLM 指定 |
| **SessionStatus** | WaitingApproval | 复用 WaitingApproval |
| **IPC 恢复** | approval_decision | 复用 approval_decision |
| **ChunkEvent** | 无（status 变更隐含） | 新增 AskQuestion 变体 |

---

## 9. 实现计划

### Phase 1: 核心 Tool

| 文件 | 改动 |
|------|------|
| `core/rollball-runtime/src/tools/ask_user_question.rs` | **新建** — Tool 实现 |
| `core/rollball-runtime/src/tools/mod.rs` | 注册 `ask_user_question` tool |
| `core/rollball-runtime/src/agent/loop_.rs` | 新增 `ChunkEvent::AskQuestion` 变体 + `pending_questions` map |
| `core/rollball-runtime/src/agent/session_state.rs` | 无需改动（已有 WaitingApproval） |

### Phase 2: Gateway 路由

| 文件 | 改动 |
|------|------|
| `core/rollball-gateway/src/grpc/dispatch.rs` | 新增 `AskQuestion` ChunkEvent 路由 + `ask_answer` WS 处理 |
| `core/rollball-gateway/src/http/approval.rs` | 新增 `POST /agents/:id/ask-answer` 端点 |
| `core/rollball-gateway/src/ws/types.rs` | 新增 WS event type 定义 |

### Phase 3: 前端渲染

| 文件 | 改动 |
|------|------|
| `apps/desktop/src/components/AskQuestionCard.tsx` | **新建** — 问题卡片组件 |
| `apps/desktop/src/components/ChatMessage.tsx` | 集成 AskQuestionCard 渲染 |
| `apps/desktop/src/hooks/useWebSocket.ts` | 新增 `ask_question` / `ask_answer` event handler |

---

## 10. 未解决的问题

1. **并发 question** — 如果 LLM 在一次 tool call 中调用了多个 `ask_user_question`（理论上不应该，但 LLM 不可预测），多个 question 同时阻塞怎么处理？
   - **方案**: 不支持批量。LLM 每次 loop iteration 最多一个 `ask_user_question` 调用。如果返回多个 tool call，第一个 question 阻塞后，其他被跳过。

2. **子 Agent 调用** — 如果子 Agent（通过 Task tool 调用）也想问用户？
   - **方案**: 本项目架构只有单一 Runtime，没有子 Agent 问题。后续如果需要，参考 Spring AI 的"子 Agent 不可用"限制。

3. **Always/记住我** — 类似 ApprovalGate 的 `AlwaysAllow` 模式？
   - **方案**: Phase 1 不做。AskQuestion 是 LLM 主动决策，不适合自动批准。

---

## 11. 参考

- `docs/08-security.md` §11.3 Approval Gate
- `docs/module-design/03-gateway.md` §WS 消息格式
- `docs/06-communication.md` §IPC 消息协议
- `docs/review/ask-user-question-patterns-comparison.md` 全量调研对比
- Claude Code `AskUserQuestion` tool 设计（platform.claude.com）
- Spring AI `AskUserQuestionTool` 设计（QuestionHandler 策略模式）
