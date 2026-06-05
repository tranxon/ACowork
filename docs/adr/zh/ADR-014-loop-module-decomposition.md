# ADR-014: AgentLoop 主循环模块拆分 — 从 God Object 到职责模块

**状态**：已实施（8/8 Phase 完成）
**日期**：2026-06-05
**决策者**：架构讨论
**影响范围**：`agent/loop_.rs`（3908 行 → 2024 行），及其所有调用方

---

## 背景

`loop_.rs` 是 RollBall Runtime 中最大的单文件，3908 行（生产代码 2635 行 + 测试 1273 行）。它包含了 AgentLoop 的全部业务逻辑，但 8 个正交关注点混合在一起，没有物理边界隔离。

### 问题 1：God Method — `execute_single_iteration`（742 行）

单个方法占文件生产代码的 **28%**，包含 25+ 个内联块，混合了 budget check、context build、LLM call、tool dispatch、loop detection、JSONL persist、debug hooks 等完全不同的职责。

**核心症状**：无法在不理解其他 24 个块的前提下修改任何一个块。每个块的变量依赖图与相邻块交织——`context_builder`、`current_model`、`response`、`self.session.history` 等变量在块之间流动，没有清晰的接口边界。

### 问题 2：5 处代码重复

| # | 重复模式 | 出现次数 | 总冗余行数 |
|---|---------|---------|-----------|
| D1 | InboundMessage → ChatMessage 注入 | 3 处（drain_inbound deferred/live + run_inner 迭代暂停） | ~60 行 |
| D2 | Think block 持久化 | 2 处（text response path + tool calls path） | ~12 行 |
| D3 | Stop handling | 2 处（pre-tool stop + post-tool stop） | ~30 行 |
| D4 | `await_approval_decision` 与 `await_question_answer` | 2 处（select! 循环结构完全同构） | ~90 行 |
| D5 | `APPROVAL_TIMEOUT_SECS` 常量 | 2 处硬编码 300 | — |

**总计约 192 行冗余代码**，每处重复都是"改一处忘另一处"的 bug 温床。

### 问题 3：职责混杂导致认知过载

`loop_.rs` 混合了 8 个正交关注点：

| 关注点 | 方法数 | 行数(估) | 代表方法 |
|--------|--------|---------|---------|
| 上下文管理 | 7 | ~310 | `compact_history_if_needed`(171)、`resolve_distill_model`(50)、`trim_history_to_budget`(20) |
| 审批子系统 | 5 | ~167 | `await_approval_decision`(103)、`handle_approval_request`(37)、`ApprovalHandle`(20) |
| 用户交互 | 3 | ~227 | `handle_ask_user_question`(59)、`handle_todo_write`(80)、`await_question_answer`(88) |
| 入站消息 | 3 | ~208 | `drain_inbound_queue`(124)、`poll_stop`(40)、`apply_user_op`(44) |
| 会话生命周期 | 7 | ~143 | `close_session_inner`(93)、构造器(52)、`transition_status`(24) |
| 记忆系统 | 3 | ~98 | `retrieve_and_inject_memories`(76)、`init_memory_store`(3)、`write_document_entries`(19) |
| Debug 钩子 | 调用点 | ~90 | 已迁移到 observer，调用点仍在此 |
| 核心编排 | 4 | ~246 | `run_inner`(194)、`run/replay`(6)、`execute_tool_by_name`(20)、`execute_single_iteration`(骨架) |

一个开发者想理解"审批超时逻辑"，需要在一个 3908 行文件中找到 103 行的方法，然后理解它与其他方法的依赖关系——**认知成本与文件总长度成正比，而非与目标方法长度成正比**。

### 问题 4：已有拆分先例证明了模式可行性

`loop_llm.rs`（405 行）和 `loop_tools.rs`（572 行）已经成功从 `loop_.rs` 中提取，采用 `impl AgentLoop` 分文件模式。两个文件独立编译、独立测试，没有引入循环依赖或性能退化。这证明**按职责拆分 `impl AgentLoop` 是安全且有效的**。

---

## 决策

将 `loop_.rs` 按 6 个正交关注点拆分为独立模块，同时将 `execute_single_iteration` 从 742 行的 God Method 重构为 ~80 行的编排骨架 + 13 个子方法。采用分阶段实施策略，每个阶段独立可编译可测试。

### 核心原则

1. **延续 `impl AgentLoop` 分文件模式** — 与 `loop_llm.rs` / `loop_tools.rs` 保持一致，不引入新的架构模式
2. **先提取方法，再移动文件** — 每次提取一个方法后立即编译测试，确保无回归后再移动到新文件
3. **重复优先于抽象** — 在拆分阶段容忍临时重复，等所有模块边界稳定后再消除
4. **分阶段交付** — 每个阶段是一个独立的 PR，可以独立 review 和回滚

### 模块划分

#### Phase 1：`loop_context.rs` — 上下文管理（最高优先级）

**理由**：行数最多（~310 行），含最大单方法 `compact_history_if_needed`（171 行），且上下文管理是主循环中耦合最深的关注点——`execute_single_iteration` 中有 6 个内联块属于此职责。

| 方法 | 来源 | 行数 |
|------|------|------|
| `compact_history_if_needed` | loop_.rs | 171 |
| `resolve_distill_model` | loop_.rs | 50 |
| `trim_history_to_budget` | loop_.rs | 20 |
| `context_trim_budget` | loop_.rs | 3 |
| `update_provider` | loop_.rs | 11 |
| `update_gateway_model_capabilities` | loop_.rs | 3 |
| `update_max_output_tokens_limit` | loop_.rs | 3 |
| `apply_runtime_config` | loop_.rs | 10 |

**从 `execute_single_iteration` 提取的子方法**：

| 新方法 | 来源块 | 行数 | 说明 |
|--------|--------|------|------|
| `check_budget_and_warn()` | B3 | 25 | Budget 前置检查 + 警告 |
| `build_chat_request()` | B5+B7 | 22 | 构建 ChatRequest + MCP tool merge |
| `check_context_overflow_and_trim()` | B6 | 38 | 上下文溢出 circuit-breaking |
| `process_llm_response_usage()` | B9 | 105 | LLM 响应后 usage 报告 + budget 更新 |
| `pre_trim_for_tool_results()` | B19 | 23 | 工具结果前预裁剪 |

#### Phase 2：`loop_approval.rs` — 审批子系统

**理由**：消除重复 #4（`await_approval_decision` 与 `await_question_answer` 同构），审批逻辑与主循环完全正交——主循环只在 `execute_tools_parallel` 中调用 `ApprovalHandle`，不需要知道审批的内部实现。

| 方法/类型 | 来源 | 行数 |
|-----------|------|------|
| `ApprovalHandle` | loop_.rs | 20 |
| `ApprovalDecision` | loop_.rs (类型) | — |
| `await_approval_decision` | loop_.rs | 103 |
| `await_question_answer` | loop_.rs | 88 |
| `handle_approval_request` | loop_.rs | 37 |
| `send_tool_approval_needed` | loop_.rs | 13 |

**去重方案**：提取通用等待器 `InboundWaiter`，封装 `tokio::select!` 循环 + deferred 缓存 + 超时 + Stop 信号处理：

```rust
/// Generic waiter for specific inbound messages.
/// Encapsulates the common select! loop shared by approval and question flows.
struct InboundWaiter<'a> {
    inbound_rx: &'a mut mpsc::Receiver<InboundMessage>,
    approval_rx: &'a mut mpsc::Receiver<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
    deferred: &'a mut Vec<InboundMessage>,
    request_id: String,
    timeout_secs: u64,
}

impl<'a> InboundWaiter<'a> {
    /// Wait for an inbound message matching the predicate.
    /// Returns the matched message, or None on timeout/stop.
    async fn wait_for<F, T>(
        &mut self,
        match_fn: F,
        on_stop: impl FnOnce() -> T,
        on_timeout: impl FnOnce() -> T,
    ) -> Option<T> { ... }
}
```

#### Phase 3：`loop_inbound.rs` — 入站消息处理

**理由**：消除重复 #1（消息注入 3 处重复），入站消息是主循环与外部世界的接口，理应有独立模块。

| 方法 | 来源 | 行数 |
|------|------|------|
| `drain_inbound_queue` | loop_.rs | 124 |
| `poll_stop` | loop_.rs | 40 |
| `apply_user_op` | loop_.rs | 44 |

**去重方案**：提取 `inject_inbound_into_history()` 辅助函数，消除 3 处消息注入重复：

```rust
/// Convert an InboundMessage into ChatMessage(s) and append to history.
/// Used by drain_inbound_queue, run_inner iteration-limit pause, and poll_stop.
fn inject_inbound_into_history(msg: InboundMessage, history: &mut HistoryManager) {
    match msg {
        InboundMessage::UserMessage { content, .. } => {
            history.push_message(ChatMessage::user(&content));
        }
        InboundMessage::SystemNotification { content, .. } => {
            history.push_message(ChatMessage {
                role: Role::User,
                name: Some("system".into()),
                content,
                ..Default::default()
            });
        }
        InboundMessage::IntentMessage { from_agent, action, params, .. } => {
            history.push_message(ChatMessage::user(&format!("[Intent from {from_agent}: {action}] {params}")));
        }
        _ => { /* 其他消息类型不注入 history */ }
    }
}
```

#### Phase 4：`loop_interaction.rs` — 用户交互

**理由**：3 个"特殊工具"（ask_user_question、todo_write、ask_question）的拦截和交互逻辑与主循环无关，是独立的用户交互子协议。

| 方法 | 来源 | 行数 |
|------|------|------|
| `handle_ask_user_question` | loop_.rs | 59 |
| `handle_todo_write` | loop_.rs | 80 |
| `await_question_answer` → Phase 2 移至 `InboundWaiter` | — | — |

#### Phase 5：`loop_session.rs` — 会话生命周期

**理由**：session 创建/关闭/蒸馏是独立于主循环编排的生命周期管理。

| 方法/类型 | 来源 | 行数 |
|-----------|------|------|
| `new` / `new_with_observer` / `from_core_and_session` | loop_.rs | 52 |
| `transition_status` | loop_.rs | 24 |
| `close_session_inner` | loop_.rs | 93 |
| `close_session_with_distillation` | loop_.rs | 3 |
| `current_session_id` | loop_.rs | 3 |
| `update_session_title` | loop_.rs | 3 |
| `update_session_workspace_id` | loop_.rs | 5 |
| `extract_think_block` / `strip_think_block` / `build_think_metadata` | loop_.rs (自由函数) | 32 |

**去重方案**：提取 `persist_think_block()` 消除重复 #2（think 持久化 2 处重复）。

#### Phase 6：`loop_memory.rs` — 记忆系统

**理由**：记忆检索/注入与 Grafeo 紧耦合，与主循环完全无关。

| 方法 | 来源 | 行数 |
|------|------|------|
| `retrieve_and_inject_memories` | loop_.rs | 76 |
| `init_memory_store` | loop_.rs | 3 |
| `write_document_entries` | loop_.rs | 19 |

### 拆分后的 `loop_.rs` 骨架

拆分后 `loop_.rs` 仅保留核心编排逻辑：

```rust
// loop_.rs — 核心编排（目标 ~800 行，含测试）

mod loop_context;   // Phase 1
mod loop_approval;  // Phase 2
mod loop_inbound;   // Phase 3
mod loop_interaction; // Phase 4
mod loop_session;   // Phase 5
mod loop_memory;    // Phase 6
mod loop_llm;       // 已存在
mod loop_tools;     // 已存在

impl AgentLoop {
    // ── 核心编排 ──
    pub async fn run(&mut self) -> Result<LoopResult> { ... }
    pub async fn replay(&mut self) -> Result<LoopResult> { ... }
    async fn run_inner(&mut self, replay: bool) -> Result<LoopResult> { ... }
    pub(crate) async fn execute_single_iteration(&mut self) -> Result<IterationResult> {
        // ~80 行编排骨架，调用各子模块方法
    }

    // ── 访问器 ──
    pub fn history(&self) -> &HistoryManager { ... }
    pub fn manifest(&self) -> &AgentManifest { ... }
    pub fn history_mut(&mut self) -> &mut HistoryManager { ... }
}
```

`execute_single_iteration` 重构后约 80 行，仅包含：

```
① debug observer hooks + resume
② check_budget_and_warn()
③ build_chat_request()
④ call_llm_streaming()         ← loop_llm.rs
⑤ process_llm_response_usage()
⑥ text response → handle_text_response() → return
⑦ deduplicate + pre_check_loop_detection()
⑧ tool dispatch → execute_tools_parallel() ← loop_tools.rs
⑨ merge results + post_check_loop_detection()
⑩ pre_trim_for_tool_results()
⑪ debug phase completion
```

---

## 文件变更清单

### Phase 1: loop_context.rs

| 文件 | 变更 | 说明 |
|------|------|------|
| `loop_context.rs` | **新增** | 8 个方法 + 5 个从 execute_single_iteration 提取的子方法 |
| `loop_.rs` | **大改** | 删除 8 个方法 + 将 execute_single_iteration 中 5 个内联块替换为方法调用 |
| `agent/mod.rs` | 小改 | 新增 `mod loop_context` |

### Phase 2: loop_approval.rs

| 文件 | 变更 | 说明 |
|------|------|------|
| `loop_approval.rs` | **新增** | ApprovalHandle + 4 个方法 + InboundWaiter 去重 |
| `loop_.rs` | **大改** | 删除 6 个审批相关方法，替换为子模块调用 |
| `loop_tools.rs` | 小改 | 导入路径调整（ApprovalHandle 来源） |
| `agent/mod.rs` | 小改 | 新增 `mod loop_approval` |

### Phase 3: loop_inbound.rs

| 文件 | 变更 | 说明 |
|------|------|------|
| `loop_inbound.rs` | **新增** | 3 个方法 + inject_inbound_into_history 辅助函数 |
| `loop_.rs` | **中改** | 删除 3 个方法，run_inner 中消息注入替换为辅助函数 |
| `agent/mod.rs` | 小改 | 新增 `mod loop_inbound` |

### Phase 4: loop_interaction.rs

| 文件 | 变更 | 说明 |
|------|------|------|
| `loop_interaction.rs` | **新增** | 2 个方法（await_question_answer 已在 Phase 2 迁入 InboundWaiter） |
| `loop_.rs` | **中改** | 删除 2 个交互方法，tool dispatch 中的拦截逻辑提取 |
| `agent/mod.rs` | 小改 | 新增 `mod loop_interaction` |

### Phase 5: loop_session.rs

| 文件 | 变更 | 说明 |
|------|------|------|
| `loop_session.rs` | **新增** | 构造器 + 生命周期方法 + 自由函数 |
| `loop_.rs` | **中改** | 删除构造器和生命周期方法 |
| `agent/mod.rs` | 小改 | 新增 `mod loop_session` |

### Phase 6: loop_memory.rs

| 文件 | 变更 | 说明 |
|------|------|------|
| `loop_memory.rs` | **新增** | 3 个记忆方法 |
| `loop_.rs` | **小改** | 删除 3 个记忆方法 |
| `agent/mod.rs` | 小改 | 新增 `mod loop_memory` |

### Phase 7: 去重 + 核心提取（execute_single_iteration 骨架化 Part 1）

| 文件 | 变更 | 说明 |
|------|------|------|
| `loop_session.rs` | **中改** | +`persist_think_to_conversation()` (D2 去重) +`handle_text_response()` |
| `loop_inbound.rs` | **中改** | +`handle_stopped()` (D3 去重) |
| `loop_.rs` | **大改** | +`await_debug_resume()`；替换 text response / stopped 内联代码 |

### Phase 8: Tool Pipeline 提取（execute_single_iteration 骨架化 Part 2）

| 文件 | 变更 | 说明 |
|------|------|------|
| `loop_tools.rs` | **大改** | +`prepare_tool_calls()` +`pre_check_loop_detection()` +`dispatch_and_merge_tools()` +`persist_and_emit_tool_results()` +`post_check_loop_detection()` |
| `loop_.rs` | **大改** | 替换 5 段内联工具 pipeline 代码为子方法调用 |

---

## 重构后预期效果

| 指标 | 重构前 | 重构后（实际） |
|------|--------|--------|
| `loop_.rs` 总行数 | 3908 | 2024 |
| `loop_.rs` 生产代码 | 2635 | ~1300（含测试） |
| `execute_single_iteration` | 742 行 | 106 行 |
| 文件数 | 3（loop_ + loop_llm + loop_tools） | 9（+6 新模块） |
| 代码重复 | 5 处 / ~192 行 | D1+D2+D3+D5 已消除（~142 行），D4 暂缓（见 Risk 4） |
| 最大单方法 | 742 行 | ~171 行（compact_history_if_needed） |

### 各模块行数预估

```
loop_.rs          ~400  (核心编排)
loop_context.rs   ~310  (上下文管理)
loop_llm.rs        405  (LLM 调用，已存在)
loop_tools.rs      572  (工具执行，已存在)
loop_approval.rs   ~170  (审批子系统)
loop_inbound.rs    ~210  (入站消息)
loop_interaction.rs ~140  (用户交互)
loop_session.rs    ~210  (会话生命周期)
loop_memory.rs      ~100  (记忆系统)
────────────────────────
总计              ~2517  (vs 当前 loop_.rs 2635 行)
```

总行数略减少（因去重消除 ~192 行 + execute_single_iteration 骨架化节省 ~662 行，但新增方法签名/参数/返回值约 ~100 行开销）。

---

## 风险和缓解

### 风险 1：子方法参数爆炸

**场景**：从 `execute_single_iteration` 提取子方法时，内联块访问了大量外层变量（`self.session.history`、`context_builder`、`current_model`、`response` 等），提取后需要通过参数传递。

**缓解**：
- 引入轻量级上下文结构体 `IterationContext`，将高频共用的变量打包：
  ```rust
  struct IterationContext<'a> {
      history: &'a mut HistoryManager,
      budget_guard: &'a mut BudgetGuard,
      current_model: &'a str,
      conversation: &'a ConversationSession,
  }
  ```
- 仅在参数 ≥ 4 个时引入上下文结构体，3 个以下直接传参
- **不在 Phase 1 引入**，等 Phase 1 完成后根据实际参数数量决定

### 风险 2：`impl AgentLoop` 分文件无编译期隔离

**场景**：每个 `loop_*.rs` 文件都可以访问 `self` 的所有字段，拆分后仍然可以互相调用对方的方法，没有强制边界。

**缓解**：
- 接受这个限制——Rust 的 `impl` 分文件模式本身就是约定而非强制
- 通过 code review 和文档约定模块边界
- 如果将来需要更强隔离，可以引入中介结构（如 `LoopContext`、`LoopSession`），但这属于后续 ADR

### 风险 3：6 个 Phase 同时进行 = 高回归风险

**场景**：每次移动方法都可能引入编译错误或行为变化。

**缓解**：
- **严格按 Phase 顺序执行**，每个 Phase 是一个独立 PR
- 每个 Phase 内部按"先提取方法 → 编译测试 → 再移动文件 → 编译测试"的节奏
- 每个 Phase 完成后运行全量测试（`cargo test`）确认无回归
- Phase 1 和 Phase 2 是最高优先级（行数最多 + 重复最多），Phase 5-6 可以后做

### 风险 4：`InboundWaiter` 泛化可能过度抽象

**场景**：`await_approval_decision` 和 `await_question_answer` 虽然结构同构，但返回类型和错误处理不同，泛化后可能反而增加理解成本。

**缓解**：
- Phase 2 先做简单的提取（移到同一文件），不急于泛化
- 如果提取到同一文件后发现两个方法的确可以共享 80% 代码，再做 `InboundWaiter`
- 如果发现差异大于共性，保持两个方法但放在同一文件中——这本身已经是改善

---

## 后果

### 变得更好的

| 维度 | 改善 |
|------|------|
| **可维护性** | 修改上下文逻辑只需理解 `loop_context.rs`（~310 行），不需要阅读 3908 行 |
| **可测试性** | 每个模块可以有独立的 `mod tests`，测试更聚焦 |
| **代码重复** | 5 处 / ~192 行重复 → 0 处 |
| **认知负载** | 新人理解 AgentLoop 的核心编排路径只需阅读 ~400 行 |
| **God Method** | `execute_single_iteration` 从 742 行 → ~80 行，每个子步骤有明确名称 |
| **Review 效率** | PR 改动集中在单个模块，reviewer 不需要理解无关关注点 |

### 变得更差的（代价）

| 维度 | 代价 |
|------|------|
| **文件数增多** | 3 → 9 个文件，需要记住方法在哪个文件中 |
| **方法签名暴露** | 部分方法需要从 `private` 提升为 `pub(crate)` 以供跨文件调用 |
| **编译隔离弱** | `impl AgentLoop` 分文件没有编译期强制边界，依赖约定 |
| **Git blame 历史** | 方法移动后 `git blame` 需要跟随文件重命名 |
| **过渡期** | 6 个 Phase 的过渡期中，部分方法可能已移动但调用方仍在旧文件 |

### 不变的

- `loop_llm.rs` 和 `loop_tools.rs` — 已存在，不受影响
- `AgentCore`、`SessionState`、`HistoryManager` 等数据结构 — 不变
- `DebugObserver` 及其调用点 — 不变（ADR-013 已完成）
- 外部 API（`AgentLoop::new`、`run`、`replay`）— 签名不变

---

## 被否决的替代方案

### A. Builder 模式替代 `execute_single_iteration`

```rust
IterationBuilder::new(&mut self)
    .check_budget()
    .build_context()
    .call_llm()
    .dispatch_tools()
    .execute()
```

**否决原因**：
- AgentLoop 的迭代不是可组合的管道——步骤之间有复杂的条件分支（text response 直接退出、loop detection 可能中断、stop 信号随时插入）
- Builder 模式暗示步骤可选和可重排，但 AgentLoop 的步骤是固定顺序的
- 增加了间接层但没有减少复杂度

### B. 状态机模式

将 `execute_single_iteration` 实现为状态机：

```rust
enum IterationState {
    BudgetCheck, BuildContext, LlmCall, ParseResponse,
    ToolDispatch, ToolResult, LoopDetection, Done
}
```

**否决原因**：
- 状态机适合有复杂状态转换和回退的场景，但 AgentLoop 的迭代是线性流程，没有回退
- 每个状态需要持久化中间变量（`response`、`tool_results` 等），状态机需要额外的状态持有结构
- 增加了大量样板代码（`match state { ... }`、`IterationState` 枚举定义）但没有解决职责混杂问题

### C. 引入 `LoopContext` 中介结构（强隔离）

```rust
struct LoopContext<'a> {
    history: &'a mut HistoryManager,
    budget: &'a mut BudgetGuard,
    model: &'a str,
}

impl LoopContext<'_> {
    fn compact_history(&mut self) { ... }
    fn check_budget(&mut self) { ... }
}
```

**否决原因**：
- 当前阶段不需要强隔离——`impl AgentLoop` 分文件模式已足够
- 引入中介结构意味着所有方法都需要从 `&mut self` 改为 `&mut LoopContext`，改动面过大
- 如果将来确实需要强隔离，可以作为后续 ADR 引入，不影响当前的文件拆分

### D. 一次性全拆（而非分 Phase）

**否决原因**：
- 一次性移动 ~2500 行代码，回归风险极高
- 如果某个模块的接口设计有问题，回滚困难
- 分 Phase 允许在每个阶段验证设计决策，及时调整

---

## 参考

- 当前代码：`core/rollball-runtime/src/agent/loop_.rs`（3908 行）
- 已有拆分先例：`loop_llm.rs`（405 行）、`loop_tools.rs`（572 行）
- 前序 ADR：ADR-013（Debug Observer Pipeline）
- 灵感来源：Martin Fowler《Refactoring》"Extract Method" + "Decompose Conditional"
