# ADR-025：温度解析链分层与可观测性

**状态**：提案  
**日期**：2026-07-05  
**决策者**：大鱼  
**影响范围**：

- `core/acowork-runtime/src/agent/agent_core.rs`（新增 `manifest_temperature` 字段）
- `core/acowork-runtime/src/cli.rs`（manifest → AgentCore 的 seeding 逻辑，ConfigSnapshot 构建）
- `core/acowork-runtime/src/agent/loop_context.rs`（温度解析点 #1）
- `core/acowork-runtime/src/agent/loop_session.rs`（温度解析点 #2）
- `core/acowork-runtime/src/agent/session/session_manager.rs`（温度解析点 #3）
- `core/acowork-runtime/src/agent/context.rs`（温度解析点 #4，注释更新）
- `core/acowork-runtime/src/agent/session/session_state.rs`（注释修正）
- `core/acowork-runtime/src/agent_config.rs`（first-start seed 逻辑）
- `core/acowork-gateway/src/http/agent_config.rs`（`AgentConfigResponse` 新增 `temperature_source` / `manifest_temperature`）
- `apps/acowork-desktop/src/lib/types.ts`（前端类型适配）
- `apps/acowork-desktop/src/stores/chatStore.ts`（注释修正）
- `apps/acowork-desktop/src/components/results/ResultsPanel.tsx`（温度来源显示）

---

## 背景

### 当前状态

ACowork 的温度解析链在代码层面已有 3 层 fallback，但存在以下问题：

1. **manifest 层直接引用 `self.core.manifest.llm.temperature`** —— 这要求 `manifest` 字段在每个引用点都可见，破坏了 `AgentCore` 作为"温度配置 holder"的封装性。

2. **无温度来源溯源**：前端 ResultsPanel 只显示最终温度数值（如 `0.7`），用户无法判断该值来自自己在 Agent Setup 面板的设置、包作者预设、还是系统默认。

3. **`sessionState.temperature` 命名误导**：前端注释称其为 "Per-session temperature override (from Runtime, persisted in JSONL metadata)"，但实际上它**不是用户设置的 override**——前端没有任何设置入口。它是 Runtime 在 session 创建时经过完整解析链后推送给前端**只读展示**的最终值。

4. **first-start 未从 manifest seed temperature**：类比 avatar 的 seed 模式，`agent_config.json` 首次启动时未从 manifest 获取默认值。

5. **前端无 `manifest_temperature` 提示**：温度输入框缺少"留空则使用包默认值 X.X"的 placeholder。

### 关键发现：`sessionState.temperature` 的真实语义

调研确认：

- `sessionState.temperature` 的赋值来源仅有两处（`chatStore.ts:1588, 2111`），都来自 Runtime 推送给前端的 `SessionStateChanged` chunk event 的 `temperature` 字段
- 前端**没有任何代码**设置或修改此值（无 `setTemperature`、无温度滑块、无输入框）
- `SessionState.temperature` 在 Runtime 侧的类型注释为 "Per-session temperature override (set by frontend or agent config)"，但实际只有 `session_manager.rs` 设置了它——且设置的是**经过完整解析链后的最终值**（always Some）
- 在 `emit_session_state()` 和 `build_chat_request()` 中再次走 fallback 链，是因为 `session.temperature()` 可能为 None（极端情况如 session 刚创建还未 init），但在稳态下 session.temperature() 始终是 Some

**结论**：`sessionState.temperature` 是**显示字段**，不是用户设置入口。所谓"4 层解析链"中的 Layer 1（per-session override）在当前架构中不存在。

---

## 设计

### 实际温度解析链（3 层）

```text
Layer 1 (最高优先级)  agent_config.json.temperature   用户 Agent 级设置
    ↓ 如果 None
Layer 2               manifest.llm.temperature         包作者默认
    ↓ 如果 None
Layer 3 (最终 fallback)  DEFAULT_TEMPERATURE = 0.3     系统硬编码
```

注意：`runtime_overrides.temperature` 是一个**瞬态层**——它在 Gateway 推送 `RuntimeConfigUpdate` 时短暂存在，用于在写入 `agent_config.json` 前传递新值。一旦 `apply_runtime_config()` 将其持久化到 `core.temperature_override`，`runtime_overrides` 就完成使命。

### 数据流全景

```mermaid
flowchart TD
    subgraph "启动"
        A["加载 manifest.toml"] --> B["读取 [llm].temperature"]
        B --> C["seed AgentCore.manifest_temperature"]
        C --> D["首次启动？"]
        D -->|是| E["seed agent_config.json.temperature = manifest.llm.temperature"]
        D -->|否| F["加载已有 agent_config.json.temperature"]
    end

    subgraph "Session 创建（唯一解析点）"
        G["session_manager::create_or_resume()"]
        G --> H{"runtime_overrides\n(瞬态) ?"}
        H -->|Some| I_HIGH
        H -->|None| J{"core.temperature_override\n(agent_config.json)"}
        J -->|Some| I_HIGH["Layer 1: config 值"]
        J -->|None| K{"core.manifest_temperature\n(manifest [llm])"}
        K -->|Some| I_MID["Layer 2: manifest 值"]
        K -->|None| I_LOW["Layer 3: DEFAULT_TEMPERATURE"]
        I_HIGH & I_MID & I_LOW --> L["session.set_temperature(resolved)\nalways Some"]
        L --> M["前端: sessionState.temperature\n= 展示用最终值 ⭐"]
    end

    subgraph "每次 LLM 调用（安全网，很少命中）"
        N["loop_context / loop_session"]
        N --> O{"session.temperature()"}
        O -->|Some| P["直接使用\n（稳态路径）"]
        O -->|None Q["重新走 fallback 链\n（异常恢复）"]
    end

    subgraph "前端展示"
        R["Agent Setup 面板"] --> S["输入框 placeholder:\n'留空则使用包默认值 {manifest_temperature}'"]
        R --> T["来源提示:\n'当前使用：来自{source}'"]
        U["ResultsPanel"] --> V["温度值 + source 标记\n如 0.50 (manifest)"]
    end
```

### `AgentCore` 新字段

```rust
/// LLM temperature override (from agent_config.json, set via Agent Setup panel).
/// Layer 1 in the resolution chain.
pub(crate) temperature_override: Option<f32>,

/// LLM temperature from manifest.toml [llm].temperature.
/// Layer 2 in the resolution chain — seeded at agent startup.
/// Separated from direct `manifest.llm.temperature` access so that
/// the resolution chain is self-contained in AgentCore.
pub(crate) manifest_temperature: Option<f32>,
```

**设计理由**（与 `temperature_override` 对齐）：
- **封装**：温度解析逻辑完全由 `AgentCore` 持有，调用方不需要知道 manifest 的结构
- **可测试**：可以在不加载 manifest 的情况下构造 `AgentCore` 并测试解析链
- **一致性**：与 `temperature_override` 字段的使用模式完全一致

### `SessionStateSnapshot`（前端 DTO）

```rust
/// Final resolved temperature value (always Some after session init).
/// NOT a user-set override — this is the display value resulting from
/// the full resolution chain: agent_config.json → manifest → DEFAULT_TEMPERATURE.
pub temperature: Option<f32>,
```

不需要新增 `temperature_source` 到 `SessionStateSnapshot`——因为 `sessionState.temperature` 已经在前端独立呈现，它不是 Agent 配置面板的一部分。温度来源信息只需要在 **Agent 配置维度**传递（`AgentConfigResponse`）。

### 温度来源追踪：`AgentConfigResponse`

在 Gateway `AgentConfigResponse` 中（已添加）：

```rust
/// Source of the effective temperature value:
/// - "config"   — from agent_config.json (user's Agent Setup panel setting)
/// - "manifest" — from manifest.toml [llm].temperature (package author default)
/// - "default"  — from DEFAULT_TEMPERATURE (hardcoded 0.3)
pub temperature_source: Option<String>,

/// The manifest-level temperature — for frontend placeholder display
/// e.g. "留空则使用包默认值 0.5"
pub manifest_temperature: Option<f32>,
```

判定逻辑：
```python
if config.temperature_set or config.temperature is not None:
    source = "config"
elif manifest_temperature is not None:
    source = "manifest"
else:
    source = "default"
```

注意 `config.temperature_set`：当用户主动在 Agent Setup 面板中操作了温度输入框（哪怕是清空），前端应标记 `temperature_set = true`，这样 source 就是 "config"（用户的选择），而不是回退到 "manifest"。

### 4 个注入点的统一模式

所有 4 个点使用同一模式（`loop_context.rs` 示例）：

```rust
// Resolve temperature via the per-agent chain:
//   Layer 1: agent_config.json (user's UI setting)
//   Layer 2: manifest.toml [llm].temperature (package author default)
//   Layer 3: DEFAULT_TEMPERATURE (hardcoded final fallback)
// Note: session.temperature() is always Some in steady state —
// the fallback below is a safety net for edge cases.
let temperature = self
    .session
    .temperature()
    .or(self.core.temperature_override)
    .or(self.core.manifest_temperature)
    .unwrap_or(crate::config::DEFAULT_TEMPERATURE);
```

唯一例外：`session_manager.rs:670-676` 多一个 `runtime_overrides.temperature` 作为瞬态最高优先级层。

---

## 实施计划

### Phase 1：AgentCore 结构改造

| # | 文件 | 变更 | 风险 |
|---|------|------|------|
| 1.1 | `agent_core.rs` | 新增 `manifest_temperature: Option<f32>` 字段 + `new_with_observer` 初始化 + `clone_shallow` | 低 — 纯新增字段 |
| 1.2 | `cli.rs` | seed `core.manifest_temperature = manifest.llm.temperature` 在 `build_agent_core()` 之后 | 低 — 确保 `manifest` 已加载 |

### Phase 2：注入点改造 + 注释统一

| # | 文件 | 行 | 变更 |
|---|------|----|------|
| 2.1 | `loop_context.rs` | 446, 438 | `manifest.llm.temperature` → `manifest_temperature` + 注释更新 |
| 2.2 | `loop_session.rs` | 49, 42 | 同上 |
| 2.3 | `session_manager.rs` | 674, 667 | 同上 |
| 2.4 | `context.rs` | 55 | 注释更新（代码无变更） |
| 2.5 | `session_state.rs` | 180-181, 364-371 | 修正注释：消除"per-session override"误导描述 |

### Phase 3：温度来源追踪（Agent 配置维度）

| # | 文件 | 变更 |
|---|------|------|
| 3.1 | `acowork-core/src/protocol.rs` | `ConfigSnapshot` 新增 `temperature_source: String` + `manifest_temperature: Option<f32>` |
| 3.2 | `acowork-core/src/gateway_ipc.proto` | proto 定义 `string temperature_source = 16; optional float manifest_temperature = 17` |
| 3.3 | `acowork-core/src/proto_bridge.rs` | 转换逻辑 + `RuntimeConfigUpdate` 新增 `temperature_set: bool` |
| 3.4 | `acowork-runtime/src/cli.rs` | ConfigSnapshot 构建时计算 `temperature_source` |
| 3.5 | `acowork-runtime/src/agent_config.rs` | first-start seed `seeded.temperature = manifest.llm.temperature`（在 session_init.rs 中） |

### Phase 4：Gateway + 前端适配

| # | 文件 | 变更 |
|---|------|------|
| 4.1 | `acowork-gateway/src/http/agent_config.rs` | ✅ 已完成（`temperature_source` + `manifest_temperature`） |
| 4.2 | `acowork-gateway/src/http/agents.rs` | 填充 `temperature_source` 和 `manifest_temperature` |
| 4.3 | `apps/acowork-desktop/src/lib/types.ts` | `AgentConfigResponse` 新增字段 |
| 4.4 | `apps/acowork-desktop/src/stores/chatStore.ts` | 修正 `SessionChatState.temperature` 注释 |
| 4.5 | `apps/acowork-desktop/src/components/results/ResultsPanel.tsx` | 温度行添加 source 标记 |
| 4.6 | Agent Setup 面板 | 温度输入框 placeholder + 来源显示 |

### Phase 5：构建验证

```bash
cd core && cargo build && cargo clippy --all-targets -- -D warnings && cargo test
cd apps/acowork-desktop && npx tsc --noEmit
```

---

## 替代方案

### 方案 A：保持现状，只改注释
直接使用 `self.core.manifest.llm.temperature`，不新增字段。

**优点**：改动最小  
**缺点**：调用方需了解 manifest 内部结构，不利于单元测试，未来 manifest 结构变更时所有引用点都需改。**否决**。

### 方案 B：使用辅助方法
在 `AgentCore` 上新增 `fn effective_manifest_temperature(&self) -> Option<f32>`。

**缺点**：与方案 A 相比没有实质性优势，仍依赖 manifest.lm.temperature 存在。

### 方案 C（选定）：字段存储 + seeding
在 `AgentCore` 中存储 `manifest_temperature: Option<f32>` 字段，在 cli.rs 中从 manifest 一次性 seed。

**优点**：封装性好、可测试、与 `temperature_override` 模式一致。

---

## 取消的功能：per-session temperature override

调研确认当前架构中**没有** per-session 温度 override 的概念。`sessionState.temperature` 在 ADR-024 新架构下是**经过完整解析的最终展示值**，不是用户设置入口。

如需在未来增加此功能（用户在会话中拖拽滑块临时调整温度），建议作为独立功能提案，包含：
1. Runtime `SessionState.temperature` 改为真正的 override（set by user）
2. 前端会话工具栏新增温度滑块
3. 解析链扩展为 4 层：per-session → agent_config.json → manifest → default
4. `emit_session_state()` 中的 fallback 链变为真正激活（session.temperature() 可能为 None）

当前提案不包含此功能。

---

## 风险与缓解

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|----------|
| 漏改 `manifest.llm.temperature` 引用 | 低 | 中 | Phase 2 逐个点验证 + `content_search` 全局扫描 |
| Proto 版本号变更致不兼容 | 低 | 高 | 确保双向 grpc 编译通过 |
| manifest.temperature 为 None | 中 | 低 | fallback 到 Layer 3 正常兜底 |
