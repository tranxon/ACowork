# Agent "Ask User Question" 实现模式对比

> **调研范围**: Claude Code, LangGraph, Spring AI, CrewAI, OpenAI Agents SDK, OpenCode, ZeroClaw, RollBall
> **日期**: 2026-05-21

## 1. 核心问题

在 Agent 交互中，LLM 需要向用户提问（选项选择/自由文本确认/决策询问），然后**暂停执行**等待用户回复，再**恢复执行**。这本质上是一个"中断-等待-恢复"的三步循环。

所有实现方式的本质共性：

```
LLM 决定要问用户 → 暂停执行 → 等待用户输入 → 恢复 LLM → LLM 继续
```

区别在于"如何暂停"和"如何恢复"。

---

## 2. 六大实现模式

### 2.1 工具即机制（Tool-as-Mechanism）

**代表**: Claude Code (AskUserQuestion), Spring AI (AskUserQuestionTool), 本项目 (AskUserQuestion tool)

**原理**: 将"问用户"定义为一个普通 Tool，LLM 调用时 framework 阻塞等待用户响应，响应回来后作为 tool result 返回给 LLM。

**Claude Code 实现**:
```
LLM 调用 ask_user(question, options)
  → SDK 拦截工具调用，渲染 UI options
  → BLOCKING 等待用户选择
  → 用户选择后 → 返回 tool result {answer: "xxx"}
  → LLM 拿到结果继续执行
```

**Spring AI 实现**:
```
AskUserQuestionTool.call()
  → QuestionHandler.handleQuestions(questions)
  → BLOCKING: CompletableFuture.get(timeout)
  → 用户通过 REST endpoint 提交答案 → future.complete()
  → 返回 Map<Question, Answer> → LLM 继续
```

**关键特征**:
- 对 LLM 完全透明——它就是另一个 Tool
- 阻塞发生在 Tool 执行层，不需要特殊 runtime 支持
- Tool input = 问题结构，Tool output = 用户回答
- 所有 LLM provider 都支持（只要是 function calling）
- 缺点: 子 Agent/子任务中无法使用（Spring AI 明确文档说明）

**适用场景**: LLM 主动发起的自由提问（"你选哪个方案？"、"请澄清需求"）

---

### 2.2 图中断（Graph Interrupt）

**代表**: LangGraph (`interrupt()`)

**原理**: 在 Graph 节点中调用 `interrupt()` 暂停整个图执行，保存状态到 Checkpointer，外部恢复时传入 `Command(resume=...)`。

```
Node 中: decision = interrupt({"question": "Approve?"})
  → 图立即暂停, 状态持久化到 Checkpointer
  → 外部读取 result.interrupts 获取问题
  → 用户选择后: graph.invoke(Command(resume="yes"), config)
  → 节点从头重新执行, interrupt() 返回 "yes"
  → decision = "yes", 继续后续逻辑
```

**关键特征**:
- 不是 Tool，而是**图执行引擎层**的原生机制
- 中断可放在节点内任意位置（甚至工具内部）
- 恢复时节点**从头执行**（需要幂等设计）
- 需要 Checkpointer 持久化（SQLite/PostgreSQL）
- 支持静态断点 (`interrupt_before`/`interrupt_after`) 和动态中断 (`interrupt()`)

**优点**:
- 细粒度控制——可以在任意逻辑点暂停
- 状态持久化——进程重启也能恢复（优秀的生产级设计）
- 支持 `Command(goto=...)` 改变恢复后流向

**缺点**:
- 依赖 LangGraph 图执行模型
- 恢复时重跑节点内代码（非幂等操作有风险）
- `interrupt()` 不能在 try/catch 中

---

### 2.3 执行前钩子（Pre-Execution Hook）

**代表**: ZeroClaw (`ApprovalManager`), RollBall (`ApprovalGate`)

**原理**: 在 Tool 执行前插入审批检查，符合条件则暂停等待用户确认。本质上是一种**安全守卫（Guard）**模式。

**ZeroClaw 实现**:
```
run_tool_call_loop():
  for each tool_call:
    if approval_mgr.needs_approval(tool_name):
      if channel == "cli":
        response = prompt_cli_interactive()    // 阻塞 stdin
      else:
        request_id = create_pending_request()  // 异步挂起
        response = poll_resolution(request_id) // 轮询等待
    if response == Yes:
      execute_tool()
```

**RollBall 实现**:
```
shell_tool.execute():
  let risk = assess_risk(command)
  if risk >= Medium:
    let response = approval_gate.request_approval(request)
    match response:
      Approved → execute
      Rejected → return error
```

**关键特征**:
- 不是 LLM 主动调用，而是**框架自动拦截**
- 主要用于**安全审批**场景，而非 LLM 主动提问
- ZeroClaw 支持 CLI 阻塞 + 非 CLI 异步两种模式
- ZeroClaw 支持 `Always` 会话级自动批准 + 审计日志
- RollBall 的 `ApprovalGate` 是 trait，可插拔实现（CLI / GUI / auto）

**优点**:
- 安全场景的完美方案——强制性，LLM 无法绕过
- 可以作为 Tool 系统的一个透明层
- 支持细粒度策略（按 tool / 按参数 / 按风险级别）

**缺点**:
- 只适用于"是否批准"二元决策
- 自由度不够——不能问"你选A还是B"这样的多选项问题
- 与 Tool 实现耦合

---

### 2.4 任务级 Flag

**代表**: CrewAI (`human_input=True`)

**原理**: 在 Task 定义中设置布尔标志，Agent 执行该任务前/后暂停等待人类输入。

```python
task = Task(
    description="分析市场趋势...",
    human_input=True,  # 就是这一行
)
```

**关键特征**:
- 最简单粗暴——一个布尔值搞定
- 位置固定：任务**结果输出前**暂停
- 流程：Agent 完成任务分析 → 暂停 → 打印提示 → 等待 stdin → 合并用户输入 → 输出最终结果
- 没有"Always"、"Session allowlist"等高级特性

**适用场景**: 简单的内容审核/确认节点

---

### 2.5 文件 IPC

**代表**: OpenCode (`ask_user` 自定义 Tool)

**原理**: 通过文件系统实现两个进程间通信，LLM 写问题文件，独立 CLI 助手读问题、展示给用户、写回答文件，LLM 侧轮询读回答。

```
OpenCode 进程                  CLI 助手进程
  │                               │
  ├─ 写入 question.json ─────────►│
  │  (轮询 response.json)         ├─ 展示问题给用户
  │      │                        ├─ 用户输入回答
  │      │◄───────────────────────┼─ 写入 response.json
  │ 读到 response.json            │
  │ 返回给 LLM                    │
```

**关键特征**:
- 纯文件系统，零 runtime 侵入
- 完全异步解耦——两个进程各自独立
- 支持自由文本输入（多行，空行提交）
- 支持 timeout 和 cancel
- 实现成本极低

**优点**: 
- 最轻量级的实现，不依赖任何框架
- 可以在任何编程语言中复现
- 天然支持跨进程

**缺点**:
- 文件系统有竞态风险
- 没有持久化和恢复能力（进程重启即丢失）
- 需要额外进程运行 CLI 助手

---

### 2.6 状态机（State Machine）

**代表**: RollBall (`SessionStatus::WaitingApproval`), OpenAI Agents SDK (`stop_on_first_tool`)

**原理**: Runtime 层面维护 session 状态机。当需要用户输入时，session 状态切换到等待态，外部 UI 检测到状态变化后展示交互界面，用户确认后发送恢复信号触发状态回切。

**RollBall 实现**:
```
AgentLoop 执行中:
  requires_approval() → true
    → session_state.set_status(WaitingApproval { request_id })
    → emit ChunkEvent::SessionStateChanged
    → LOOP PAUSE (不再继续 LLM 调用)
    → 外部 Gateway 收到状态变更 → 通知前端显示 approval dialog
    → 用户点击 Approve → Gateway 发回 approval_decision IPC
    → AgentLoop 恢复 → session_state.set_status(Streaming)
    → 继续执行 tool
```

**OpenAI Agents SDK 实现**:
```python
agent = Agent(
    tools=[ask_user_tool],
    tool_use_behavior="stop_on_first_tool"  
    # 工具调用后暂停，将控制权返回给外部
)
# 外部 Orchestrator 检测到 tool call → 展示给用户 → 收集回答 → 恢复
```

**关键特征**:
- 状态切换由 Runtime 驱动，前端只读
- 天然支持多 session 隔离（每个 session 独立状态）
- 适合 IPC 架构（Runtime ↔ Gateway ↔ UI）
- 需要配套的"恢复"协议

**优点**:
- 架构最清晰——状态可见、可观察、可审计
- 适合分布式/进程分离架构
- 与 ADR-014 状态机设计完全对齐

**缺点**:
- 实现复杂度最高（需要完整的 IPC + 状态同步链路）
- 延迟较高（状态变更 → IPC 传输 → UI 渲染 → 用户操作 → IPC 回传）

---

## 3. 全景对比

| 维度 | Tool-as-Mechanism | Graph Interrupt | Pre-Exec Hook | Task Flag | File IPC | State Machine |
|------|:-:|:-:|:-:|:-:|:-:|:-:|
| **代表实现** | Claude Code, Spring AI | LangGraph | ZeroClaw | CrewAI | OpenCode | RollBall |
| **LLM 感知** | 透明（就是 Tool） | 不感知 | 不感知（自动触发） | 不感知 | 感知（Tool） | 不感知 |
| **暂停粒度** | Tool 调用层 | 图节点内任意点 | Tool 执行前 | Task 输出前 | Tool 调用层 | Session 级 |
| **恢复方式** | Tool result 返回 | Command(resume=) | 继续/拒绝 | 控制台输入 | 文件响应 | IPC 恢复信号 |
| **持久化** | ❌ | ✅ (Checkpointer) | ❌ | ❌ | ❌ | ❌ (内存 Session) |
| **问答灵活性** | 高（任意选项） | 高（任意 JSON） | 低（仅批准/拒绝） | 低 | 高（自由文本） | 中（仅批准/拒绝） |
| **安全审批** | ❌ 不适用 | ✅ 可做 | ✅ 原生支持 | ❌ | ❌ | ✅ 可做 |
| **实现复杂度** | 低 | 高 | 中 | 极低 | 极低 | 高 |
| **进程分离** | ❌ 同进程 | ❌ 同进程 | ❌ 同进程 | ❌ 同进程 | ✅ 天然跨进程 | ✅ 原生跨进程 |
| **state-of-the-art** | 通用标准模式 | LangGraph 独家 | 安全专用模式 | 简单场景 | 轻量方案 | 复杂 IPC 架构 |

---

## 4. 按交互场景分类

### 场景 A: LLM 主动提问（"问你个事"）
LLM 在推理过程中主动需要用户决策或澄清。

| 模式 | 适用性 | 理由 |
|------|:------:|------|
| **Tool-as-Mechanism** | ⭐⭐⭐⭐⭐ | 最自然——LLM 按需调用，框架无需预判 |
| **Task Flag** | ⭐⭐⭐ | 简单但缺乏灵活性 |
| **File IPC** | ⭐⭐⭐ | 轻量方案，适合非标准架构 |
| **Graph Interrupt** | ⭐⭐⭐⭐ | 灵活但依赖 LangGraph |

### 场景 B: 安全审批（"这个操作安全吗"）
高风险操作需要用户显式批准。

| 模式 | 适用性 | 理由 |
|------|:------:|------|
| **Pre-Exec Hook** | ⭐⭐⭐⭐⭐ | 强制拦截，LLM 无法绕过，安全性最高 |
| **State Machine** | ⭐⭐⭐⭐ | 适合 IPC 架构的安全层 |
| **Tool-as-Mechanism** | ⭐⭐ | LLM 可以决定不调用，不安全 |
| **Task Flag** | ⭐ | 无法精细控制 |

### 场景 C: 前端交互（Desktop App + Gateway 架构）

| 模式 | 适用性 | 理由 |
|------|:------:|------|
| **State Machine** | ⭐⭐⭐⭐⭐ | 原生支持分布式架构 |
| **Pre-Exec Hook + IPC** | ⭐⭐⭐⭐ | 需要额外 IPC 穿透 |
| **Tool-as-Mechanism** | ⭐⭐⭐ | 需要 Tool 执行时能跨进程阻塞 |

---

## 5. 对 RollBall 的建议

RollBall 当前已经具备 **两种模式** 的雏形：

### 已有: `ApprovalGate` (Pre-Execution Hook)
```
rollball-runtime/src/security/approval_gate.rs
→ 针对 Shell 安全审批，trait 化，可插拔
→ 当前只有 CLI 实现，Desktop 待做
```

### 已有: `SessionStatus::WaitingApproval` (State Machine)
```
rollball-runtime/src/agent/session_state.rs
→ session 级别的状态机
→ Gateway → Frontend 链路已通（ChunkEvent）
→ 有 IPC 协议（approval_decision 消息）
```

### 缺失: `AskUserQuestion` Tool (Tool-as-Mechanism)
```
→ LLM 主动提问的场景没有覆盖
→ 当前只有安全审批（框架自动触发）
→ 缺少 LLM 按需调用 "问用户" 的能力
```

### 推荐方案

| 需求 | 使用模式 | 优先级 |
|------|----------|:------:|
| Shell 高风险命令审批 | `ApprovalGate` (已有) | P0 ✓ |
| Tool 安全拦截（任意 tool） | `PermissionChecked` wrapper (Phase 3) | P1 |
| **LLM 主动向用户提问** | **新增 `AskUserQuestion` Tool** | **P1** |
| Desktop UI 交互 | `ApprovalGate` 的 GUI 实现 + `SessionStatus` 联动 | P2 |

**新增 `AskUserQuestion` Tool 的架构建议**:

```
LLM 调用 ask_user_question(question, options?)
  → Tool 执行：
    1. session_state.set_status(WaitingApproval { request_id })
    2. emit ChunkEvent(question 内容)
    3. BLOCKING 等待 (通过 mpsc channel)
  → 用户在前端选择 → IPC approval_decision 到达
  → Tool 收到决策 → 返回结构化 result
  → LLM 继续
```

与现有 `ApprovalGate` 共享 `WaitingApproval` 状态和 IPC 链路，只是触发源不同（LLM 主动 vs 框架拦截）。

---

## 6. 结论

1. **Tool-as-Mechanism 是行业共识**：所有主流框架（Claude Code、Spring AI、OpenAI SDK）都将其实现为一个普通 Tool，这是最自然、最通用的模式
2. **Graph Interrupt 是 LangGraph 的差异化优势**：提供更精细的控制和持久化，但绑定框架
3. **Pre-Execution Hook 是安全场景的标配**：零信任安全模型下必不可少
4. **State Machine 是 IPC 架构的底层基础设施**：RollBall 的分布式架构决定了状态驱动是最合理的
5. **File IPC 是最轻量的方案**：适合快速原型和跨语言场景

最终推荐 RollBall: **Tool-as-Mechanism + State Machine 双模式**——用 Tool 触发 LLM 主动提问，用 ApprovalGate 处理框架强制安全审批，两者共享同一套 `SessionStatus::WaitingApproval` 状态机和 IPC 链路。
