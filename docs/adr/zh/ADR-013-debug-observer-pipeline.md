# ADR-013: Debug 模块边界重构 — Observer Pipeline 模式

**状态**：提议
**日期**：2026-06-05
**决策者**：架构讨论
**影响范围**：`agent_core.rs`, `loop_.rs`, `context.rs`, `session_task.rs`, `session_manager.rs`, `session_handle.rs`, `debug/` 模块

---

## 背景

RollBall Runtime 的 Debug（DevMode）功能在实现过程中逐步渗透到了非 debug 模块中，导致 debug 代码和生产代码边界模糊。具体问题如下：

### 问题 1：主循环侵入严重

`loop_.rs` 是重灾区——`execute_single_iteration` 中有 **15+ 处 debug 调用点**，5 个 debug 专用方法（`await_debug_resume`, `update_debug_phase`, `push_debug_step`, `debug_auto_pause_if_stepping`, `capture_context_snapshot`），总计约 **400+ 行** debug 代码散落在 4189 行的文件中。

侵入模式重复出现：

```rust
// 模式 A：阶段追踪 — 出现 6 次
self.update_debug_phase(DebugPhase::Xxx).await;

// 模式 B：步骤推送 + 自动暂停 — 出现 4 次
self.push_debug_step(phase, input, output);
self.debug_auto_pause_if_stepping().await;

// 模式 C：守卫条件 — 出现 10+ 次
if let Some(ctrl) = self.core.debug_ctrl() {
    // debug-only logic
}
```

这些调用使得正常执行流程的阅读和理解变得困难——你无法一眼区分"这是业务逻辑"还是"这是调试钩子"。

### 问题 2：AgentCore 成为 debug 字段堆场

`AgentCore` 持有 **6 个 `Option<T>` debug 字段** + **6 个 debug 方法**：

```rust
pub(crate) debug_ctrl: Option<Arc<Mutex<DebugController>>>,
pub(crate) pending_debug_handles: Option<Arc<Mutex<Option<DebugHandles>>>>,
pub(crate) debug_rewind_notify: Option<Arc<Notify>>,
pub(crate) debug_resume_notify: Option<Arc<Notify>>,
pub(crate) debug_event_tx: Option<DebugEventSender>,
// + set_debug_mode(), check_and_apply_pending_debug(),
//   debug_ctrl(), debug_rewind_notify(), debug_resume_notify(), debug_event_tx()
```

这些字段让 `AgentCore` 承担了 debug 状态容器的角色，违反了单一职责。`pending_debug_handles` 的 `Arc<Mutex<Option<...>>>` 嵌套尤其难读——它是绕过注入机制的产物，暴露了实现细节。

### 问题 3：ContextBuilder 的 debug 钩子与业务逻辑混杂

`ContextBuilder` 有 **8 个方法标记为 "for debug patching"**，加上 `apply_patches()` 和 7 个 section 访问器。更关键的是，`environment_override` 字段在 `build()` 方法中改变了正常逻辑分支：

```rust
// context.rs build() 中的分支
if let Some(ref env) = self.environment_override {
    // debug override 优先于自动检测
} else {
    // 正常逻辑
}
```

这意味着 debug 不仅添加了方法，还 **修改了现有行为**，使得非 debug 路径的推理需要同时理解 debug 路径。

### 问题 4：rewind 逻辑散布在三个文件

`apply_debug_rewind` 函数链横跨 `session_task.rs`（3 个函数 + 2 个调用点）和 `loop_.rs`（`await_debug_resume` 内部调用）。rewind 的状态依赖——`DebugController.rewind_target`、`conversation_snapshots`、`HistoryManager.truncate_to()`——分散在 controller、loop、session_task 三处，没有统一的边界。

### 问题 5：SessionTask 中的 debug 消息类型耦合

`SessionMessage::EnableDebugMode(DebugHandles)` 是一个 debug 专属的消息变体，使得 `SessionMessage` 枚举被 debug 领域污染。SessionTask 还持有 3 个 debug 字段和 7 处 debug 调用点。

---

## 决策

引入 **Observer Pipeline 模式**，将 debug 功能从主执行流中抽离为可插拔的观察者，通过统一的钩子接口注入，而非在业务代码中散布 `if let Some(ctrl) = self.core.debug_ctrl()` 守卫条件。

### 核心设计

#### 1. `DebugObserver` trait — debug 功能的唯一抽象边界

```rust
// debug/observer.rs

/// Pluggable observer for agent loop lifecycle events.
///
/// In production mode, a no-op implementation is used (zero-cost abstraction
/// via enum dispatch, not dynamic dispatch). In DevMode, the real
/// DebugController-backed observer is injected.
///
/// All methods have default no-op implementations so that implementing
/// only the needed hooks is ergonomic.
pub trait DebugObserver: Send + Sync {
    // ── Lifecycle ──

    /// Called at the start of each iteration, before budget check.
    fn on_iteration_start(&self, _iteration: u32, _history_len: usize) {}

    /// Called after the agent loop has been resumed from a pause.
    fn on_resume(&self) {}

    // ── Phase tracking ──

    /// Called when the agent loop enters a new phase.
    /// Returns true if a breakpoint was hit (caller should await resume).
    async fn on_phase_enter(&self, _phase: DebugPhase) -> bool { false }

    /// Called after a phase completes with its result.
    fn on_phase_step(&self, _phase: DebugPhase, _input: Option<Value>, _output: Option<Value>) {}

    /// Called after a phase completes; auto-pauses if in stepping mode.
    async fn on_phase_step_done(&self) {}

    // ── Context ──

    /// Called after ContextBuilder::build() completes.
    /// Captures a snapshot of the built context.
    async fn on_context_built(&self, _snapshot: ContextSnapshotRequest) {}

    /// Apply any pending patches to the context builder.
    /// Returns true if patches were applied.
    fn apply_pending_patches(&self, _builder: &mut ContextBuilder) -> bool { false }

    // ── Pause / Resume / Rewind ──

    /// Block until the debugger resumes execution.
    /// Returns false if the agent should stop.
    async fn await_resume(&self) -> bool { true }

    /// Check for pending rewind operations and apply them.
    async fn apply_rewind(&self, _history: &mut HistoryManager) {}

    // ── Runtime injection ──

    /// Check for bypass-injected debug handles (called each iteration start).
    fn check_pending_injection(&self) {}
}
```

#### 2. `DebugObserverSlot` — 零成本抽象的枚举分派

不使用 `Option<Box<dyn DebugObserver>>`（动态分派 + 堆分配），而使用枚举分派：

```rust
// debug/observer.rs

/// Slot that holds either a real debug observer (DevMode) or a no-op.
/// Enum dispatch ensures zero overhead in production mode — the compiler
/// sees through the variant and eliminates dead code.
pub enum DebugObserverSlot {
    Production,
    Dev(DebugObserverImpl),
}

impl DebugObserverSlot {
    /// Delegate to the inner observer (or no-op for Production variant).
    pub fn on_iteration_start(&self, iteration: u32, history_len: usize) {
        match self {
            DebugObserverSlot::Production => {}
            DebugObserverSlot::Dev(obs) => obs.on_iteration_start(iteration, history_len),
        }
    }

    // ... 同样代理所有 trait 方法
}
```

**为什么用枚举而非 `Option<Box<dyn>>`？**
- 枚举分派是静态的，编译器可以对 `Production` 变体做死代码消除
- 不需要 `alloc`，不引入虚表间接调用
- `match` 的分支预测在现代 CPU 上几乎零成本
- 方法签名可见，IDE 补全友好

#### 3. `DebugObserverImpl` — DevMode 的真实实现

```rust
// debug/observer_impl.rs

/// Real debug observer backed by DebugController, event sender, and notify handles.
pub struct DebugObserverImpl {
    ctrl: Arc<Mutex<DebugController>>,
    event_tx: DebugEventSender,
    rewind_notify: Arc<Notify>,
    resume_notify: Arc<Notify>,
    pending_injection: Option<Arc<Mutex<Option<DebugHandles>>>>,
}
```

这个结构体将当前散落在 `AgentCore` 的 5 个 `Option<T>` debug 字段收敛为一个 `DebugObserverSlot` 字段。所有 debug 逻辑（阶段追踪、快照捕获、断点检查、暂停/恢复、rewind）都封装在此处。

#### 4. AgentCore 简化

**Before**（6 个 Option 字段 + 6 个方法）：

```rust
pub struct AgentCore {
    // ... business fields ...
    pub(crate) debug_ctrl: Option<Arc<Mutex<DebugController>>>,
    pub(crate) pending_debug_handles: Option<Arc<Mutex<Option<DebugHandles>>>>>,
    pub(crate) debug_rewind_notify: Option<Arc<Notify>>,
    pub(crate) debug_resume_notify: Option<Arc<Notify>>,
    pub(crate) debug_event_tx: Option<DebugEventSender>,
}
```

**After**（1 个字段）：

```rust
pub struct AgentCore {
    // ... business fields ...
    pub(crate) debug_observer: DebugObserverSlot,
}
```

所有 debug 访问器方法删除，替换为 `self.debug_observer.on_xxx()` 调用。

#### 5. 主循环简化

**Before** — 每个注入点都有显式守卫条件：

```rust
// 迭代开头
self.core.check_and_apply_pending_debug();
let debug_iter = if let Some(ctrl) = self.core.debug_ctrl() {
    let mut ctrl = ctrl.lock().await;
    ctrl.iteration += 1;
    let msg_count = self.session.history.len();
    ctrl.create_conversation_snapshot(msg_count, usage);
    Some(ctrl.iteration)
} else {
    None
};
if !self.await_debug_resume().await {
    return Ok(IterationResult::Stopped(String::new()));
}
if let Some(ctrl) = self.core.debug_ctrl() {
    let mut ctrl_guard = ctrl.lock().await;
    if let Some(patches) = ctrl_guard.pending_patches.take() {
        context_builder.apply_patches(&patches);
    }
}

// 阶段追踪
self.update_debug_phase(DebugPhase::BudgetCheck).await;
// ... 业务逻辑 ...
self.update_debug_phase(DebugPhase::BuildContext).await;
self.capture_context_snapshot(context_builder, debug_iter, &current_model).await;
// ... 业务逻辑 ...
self.update_debug_phase(DebugPhase::LlmCall).await;
// ...
self.push_debug_step(DebugPhase::Idle, None, output);
self.debug_auto_pause_if_stepping().await;
```

**After** — 守卫条件内化到 observer，主循环只看到语义清晰的钩子调用：

```rust
// 迭代开头
self.core.debug_observer.check_pending_injection();
let debug_iter = self.core.debug_observer.on_iteration_start(
    /* iteration */, self.session.history.len()
);
if !self.core.debug_observer.await_resume(&mut self.session).await {
    return Ok(IterationResult::Stopped(String::new()));
}
self.core.debug_observer.apply_pending_patches(context_builder);

// 阶段追踪 — 单一调用，无需守卫
if self.core.debug_observer.on_phase_enter(DebugPhase::BudgetCheck).await {
    // breakpoint hit, already handled inside observer
}
// ... 业务逻辑 ...
if self.core.debug_observer.on_phase_enter(DebugPhase::BuildContext).await {
    // breakpoint hit
}
self.core.debug_observer.on_context_built(ContextSnapshotRequest::from(context_builder, debug_iter, current_model)).await;
// ... 业务逻辑 ...
self.core.debug_observer.on_phase_step(DebugPhase::Idle, None, output);
self.core.debug_observer.on_phase_step_done().await;
```

**关键变化**：
- 删除 `if let Some(ctrl)` 守卫条件——observer 内部处理 `Production` 变体的空操作
- 删除 `loop_.rs` 上的 5 个 debug 专用方法——逻辑移入 `DebugObserverImpl`
- 每个钩子调用的语义从"检查 debug 是否存在并执行"变为"通知 debug 观察者"
- `await_debug_resume` 逻辑移入 observer，主循环只看到布尔返回值

#### 6. ContextBuilder 的 patch 接口保留但标注清晰

ContextBuilder 上的 `apply_patches()` 和 section 访问器**保留不动**，但做以下调整：

- 删除 "for debug patching" 注释——这些方法属于 ContextBuilder 的公共 API，不应标记为 debug 专属
- `environment_override` 重命名为 `environment_patch`，语义从"debug 覆盖"变为"外部补丁"——这个字段未来可能有非 debug 用途（如 A/B 测试环境注入）
- section 访问器（`system_prompt()`, `tool_definitions()` 等）本质上就是 getter，保留不变

**为什么不全移入 observer？** ContextBuilder 是数据对象，`apply_patches` 是数据变换。让 observer 持有对 ContextBuilder 的可变引用来应用补丁是合理的，但把补丁逻辑本身从 ContextBuilder 拆出去没有收益——它就是一个字段级 merge 操作。

#### 7. SessionMessage 去除 debug 变体

**Before**：
```rust
pub enum SessionMessage {
    ChatMessage { ... },
    Stop { ... },
    EnableDebugMode(DebugHandles),  // debug 专属
    Close,
    // ...
}
```

**After**：删除 `EnableDebugMode` 变体，改用 `DebugObserverSlot` 的 bypass 注入通道：

```rust
// session_handle.rs
pub struct SessionHandle {
    // ...
    debug_injection: DebugInjectionChannel,  // 封装 Arc<Mutex<Option<DebugHandles>>>
}

impl DebugInjectionChannel {
    /// Inject debug observer into a running session.
    /// Called by SessionManager when Gateway pushes EnableDebugMode.
    pub fn inject(&self, handles: DebugHandles) { ... }
}
```

`SessionTask` 不再处理 `EnableDebugMode` 消息——debug 注入通过 `DebugInjectionChannel` 在 observer 层面完成。

#### 8. Rewind 逻辑收敛

`apply_debug_rewind`, `apply_debug_rewind_locked`, `apply_debug_rewind_and_patches` 三个函数全部移入 `DebugObserverImpl`：

```rust
impl DebugObserverImpl {
    /// Apply any pending rewind, patches, and re-execute flag.
    /// Single lock acquisition for all three operations.
    async fn apply_rewind_and_patches(
        &self,
        session_id: &str,
        history: &mut HistoryManager,
        context_builder: &mut ContextBuilder,
    ) { ... }
}
```

SessionTask 中的调用简化为：

```rust
self.core.debug_observer.apply_rewind_and_patches(
    &session_id, &mut agent_loop.session.history, context_builder
).await;
```

---

## 文件变更清单

| 文件 | 变更 | 说明 |
|------|------|------|
| `debug/mod.rs` | 修改 | 新增 `observer` 和 `observer_impl` 子模块导出 |
| `debug/observer.rs` | **新增** | `DebugObserver` trait + `DebugObserverSlot` 枚举 |
| `debug/observer_impl.rs` | **新增** | `DebugObserverImpl` — 当前散落各处的 debug 逻辑收敛于此 |
| `debug/controller.rs` | 不变 | 内部状态管理，不受重构影响 |
| `debug/protocol.rs` | 不变 | 协议类型定义，不受重构影响 |
| `debug/server.rs` | 小改 | RPC handler 调用路径调整（从直接操作 ctrl 改为通过 observer） |
| `agent_core.rs` | **大改** | 6 个 Option 字段 → 1 个 `DebugObserverSlot`；删除 6 个 debug 方法 |
| `loop_.rs` | **大改** | 删除 5 个 debug 方法；15+ 处调用点替换为 observer 钩子 |
| `context.rs` | 小改 | 删除 "for debug patching" 注释；`environment_override` → `environment_patch` |
| `session_task.rs` | **大改** | 删除 `EnableDebugMode` 消息处理；3 个 rewind 函数移入 observer；debug 字段移除 |
| `session_manager.rs` | 中改 | `enable_debug_mode()` 创建 `DebugObserverImpl` 而非 `DebugHandles` |
| `session_handle.rs` | 小改 | `pending_debug_handles` → `DebugInjectionChannel` |
| `session_state.rs` | 不变 | 仅注释引用，无实质代码 |

---

## 重构后的模块依赖关系

```
                        ┌─────────────────────────┐
                        │      AgentCore           │
                        │  debug_observer: Slot    │
                        └────────┬────────────────┘
                                 │
                    ┌────────────┴────────────┐
                    │                         │
            ┌───────▼───────┐        ┌───────▼───────┐
            │  Production   │        │  DevMode      │
            │  (no-op)      │        │  ObserverImpl │
            └───────────────┘        └───────┬───────┘
                                             │
                              ┌──────────────┼──────────────┐
                              │              │              │
                      ┌───────▼───┐  ┌───────▼───┐  ┌─────▼─────┐
                      │ Controller│  │ EventTx   │  │ Notify    │
                      │ (state)   │  │ (push)    │  │ (resume/  │
                      └───────────┘  └───────────┘  │  rewind)  │
                                                     └───────────┘
```

主循环只依赖 `DebugObserverSlot` 的方法签名，不依赖 `DebugController`、`DebugEventSender`、`Notify` 等具体类型。

---

## 实施策略

分 4 步推进，每步可独立编译和测试：

### Step 1：引入 DebugObserver 抽象层（非破坏性）

- 新增 `debug/observer.rs` + `debug/observer_impl.rs`
- 在 `AgentCore` 中 **新增** `debug_observer: DebugObserverSlot` 字段（与旧字段并存）
- 旧字段和方法标记 `#[deprecated]`
- **不修改** `loop_.rs`、`session_task.rs` 的调用点

### Step 2：迁移主循环钩子

- `loop_.rs` 中的 15+ 处调用点逐步替换为 observer 钩子
- 迁移顺序：`update_debug_phase` → `push_debug_step` + `debug_auto_pause_if_stepping` → `capture_context_snapshot` → `await_debug_resume` → 迭代开头的守卫块
- 每迁移一组，运行 `cargo test` 确认无回归

### Step 3：迁移 SessionTask 和 SessionManager

- 将 `apply_debug_rewind*` 系列函数移入 `DebugObserverImpl`
- 删除 `SessionMessage::EnableDebugMode`，改用 `DebugInjectionChannel`
- SessionManager 的 `enable_debug_mode()` 改为创建 `DebugObserverImpl`

### Step 4：清理和删除

- 删除 `AgentCore` 中的 6 个 deprecated debug 字段和方法
- 删除 `DebugHandles`（其职责已被 `DebugObserverImpl` + `DebugInjectionChannel` 替代）
- `context.rs` 中的注释清理和字段重命名
- 更新 `debug/mod.rs` 的公开接口

---

## 后果

### 变得更好的

| 维度 | 改善 |
|------|------|
| **主循环可读性** | 15+ 处 `if let Some(ctrl)` 守卫消失，替换为语义明确的 `observer.on_xxx()` 调用 |
| **AgentCore 职责** | 从 6+6 个 debug 关注点缩减为 1 个字段，回归"运行时核心"定位 |
| **Debug 模块内聚性** | 所有 debug 逻辑（阶段追踪、快照、暂停/恢复、rewind、补丁）收敛到 `DebugObserverImpl` |
| **可测试性** | 可以 mock `DebugObserver` 来测试主循环的各种 debug 场景，无需启动 WebSocket 服务器 |
| **零成本抽象** | Production 变体在编译时消除，运行时无额外开销 |
| **未来扩展** | 新增 debug 钩子只需在 trait 上加方法 + impl，不需要修改业务代码的守卫条件 |

### 变得更差的（代价）

| 维度 | 代价 |
|------|------|
| **间接层** | 主循环通过 observer 间接调用，无法内联到具体实现（但枚举分派的开销可忽略） |
| **Observer 方法签名** | trait 方法需要满足所有调用场景，可能比当前散落代码更"宽"；某些方法需要 `&mut self` 或 async，trait 设计需谨慎 |
| **迁移风险** | 4 步迁移中每步都需要完整的集成测试覆盖，特别是 rewind 和 pause/resume 的边界场景 |
| **DebugInjectionChannel** | 绕过注入机制仍然存在，只是换了包装——`Arc<Mutex<Option<DebugHandles>>>` 变成了 `DebugInjectionChannel`，本质未变（但至少封装了） |

### 不变的

- `debug/controller.rs` — 内部状态管理，纯数据结构，不受影响
- `debug/protocol.rs` — 协议类型，不受影响
- `debug/server.rs` — WebSocket 服务器，RPC 调用路径需微调但整体不变
- ContextBuilder 的 patch 能力 — 保留，只是调用者从 loop 直接操作改为 observer 代理

---

## 被否决的替代方案

### A. Feature Flag 编译时排除

```rust
#[cfg(feature = "debug")]
{
    self.update_debug_phase(DebugPhase::BudgetCheck).await;
}
```

**否决原因**：
- 生产构建无法包含 debug 功能，但 RollBall 的 DevMode 是运行时开关（Gateway 推送 `EnableDebugMode`），不是编译时选择
- Feature flag 不支持"运行时注入"场景
- `cfg` 条件编译会让两种模式的代码永远无法同时测试

### B. 宏消除样板代码

```rust
debug_hook!(self, on_phase_enter, DebugPhase::BudgetCheck);
```

**否决原因**：
- 宏只是隐藏了守卫条件，没有改变 debug 逻辑散落各处的本质
- 宏展开后的代码仍然耦合在业务文件中
- 降低了可读性——开发者需要理解宏展开才能理解行为

### C. 完全事件驱动（Event Bus）

将 debug 功能完全基于事件总线：主循环发出事件，debug 模块订阅事件。

**否决原因**：
- 主循环的暂停/恢复是 **同步阻塞语义**——发出事件后必须等待 debug 恢复才能继续执行，事件总线的异步特性与这种同步需求冲突
- 事件总线引入了时序不确定性（事件处理顺序、背压），而 debug 需要确定性
- 增加了全局基础设施的复杂度

### D. 动态分派 (`Option<Box<dyn DebugObserver>>`)

**否决原因**：
- 虚表间接调用在热路径上有可测量的开销（尽管微小）
- `Box<dyn>` 意味着堆分配，对于每次迭代都会访问的对象不理想
- 枚举分派在所有维度上都不劣于动态分派，且提供了更好的编译器优化机会

---

## 参考

- 当前代码：`core/rollball-runtime/src/agent/loop_.rs` (4189 行), `agent_core.rs`, `context.rs`, `session_task.rs`
- 设计文档：`docs/design/zh/10-debug-protocol.md`
- 灵感来源：Chrome DevTools Protocol 的 CDP Session 模型、LLDB 的 Observer 模式
