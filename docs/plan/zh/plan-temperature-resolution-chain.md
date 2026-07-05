# 温度解析链改造实施计划

**对应 ADR**：ADR-025  
**总工作量**：约 50 行 Rust + 10 行 TypeScript  
**预计工期**：1 个开发时段

---

## 调研关键发现

**`sessionState.temperature` 不是 per-session override**：
- 前端无任何设置入口（无 `setTemperature`、无滑块、无输入框）
- 值来自 Runtime 推送的 `SessionStateChanged` chunk event
- 该值在 `session_manager.rs` 创建 session 时已走完完整解析链（always Some）
- 前端只读展示，不写

**实际解析链只有 3 层**（而非 ADR 初稿误写的 4 层）：
1. `agent_config.json.temperature`（用户 Agent 级设置）
2. `manifest.llm.temperature`（包作者默认）
3. `DEFAULT_TEMPERATURE`（硬编码 0.3）

---

## Phase 1：AgentCore 结构改造

### Step 1.1 — `agent_core.rs` 新增字段

在 `temperature_override`（Line 64）之后添加：

```rust
/// LLM temperature from manifest.toml [llm].temperature (Layer 2 in fallback chain).
/// Seeded at agent startup from manifest in cli.rs.
pub(crate) manifest_temperature: Option<f32>,
```

在 `new_with_observer` 的 `Self { ... }` 的 `temperature_override: None` 之后添加：

```rust
manifest_temperature: None,
```

在 `clone_shallow` 的 `temperature_override: self.temperature_override` 之后添加：

```rust
manifest_temperature: self.manifest_temperature,
```

### Step 1.2 — `cli.rs` seed

在 `build_agent_core()` 调用后添加：

```rust
core.manifest_temperature = manifest.llm.temperature;
tracing::info!(
    from = ?core.manifest_temperature,
    "AgentCore: seeded manifest_temperature from manifest [llm].temperature"
);
```

---

## Phase 2：注入点改造（4 处）

### Step 2.1 — `loop_context.rs` (Lines 446, 438)

```rust
// 替换 Line 446:
.or(self.core.manifest.llm.temperature)   // 旧
.or(self.core.manifest_temperature)        // 新

// 更新 Line 438 注释:
//   per-session override → agent_config.json override → manifest default → DEFAULT_TEMPERATURE.  // 旧
//   agent_config.json (Layer 1) → manifest default (Layer 2) → DEFAULT_TEMPERATURE (Layer 3).   // 新
```

### Step 2.2 — `loop_session.rs` (Lines 49, 42)

同上替换 + 注释更新。

### Step 2.3 — `session_manager.rs` (Lines 674, 667)

同上替换 + 注释更新。

### Step 2.4 — `context.rs` (Line 55)

注释更新。代码无变更。

### Step 2.5 — `session_state.rs` (Lines 180-182, 364-371)

修正注释，消除"per-session override"误导描述：

```rust
// 旧 Line 180-182:
/// Per-session temperature override (set by frontend or agent config).
/// When None, falls back to agent-level config or global default (0.7).

// 新:
/// Resolved temperature for this session (always Some after session init).
/// NOT a user-setting — this is the final value after applying the chain:
/// agent_config.json → manifest → DEFAULT_TEMPERATURE.
/// Set by SessionManager::create_or_resume_session() at session creation.
```

---

## Phase 3：温度来源追踪

### Step 3.1 — `protocol.rs`

在 `ConfigSnapshot` 结构体中新增：

```rust
/// Source of the effective temperature: "config" | "manifest" | "default".
pub temperature_source: String,
/// The manifest-level temperature for UI placeholder display.
pub manifest_temperature: Option<f32>,
```

在 `RuntimeConfigUpdate` 结构体中新增（可选，前端需要"我已明确设值"语义时用）：

```rust
/// When true, the user has explicitly set temperature in Agent Setup panel.
pub temperature_set: bool,
```

### Step 3.2 — `gateway_ipc.proto`

```protobuf
// ConfigSnapshot additions
string temperature_source = 16;
optional float manifest_temperature = 17;

// RuntimeConfigUpdate additions
optional float temperature = 19;  // 可能已存在
bool temperature_set = 20;
```

### Step 3.3 — `proto_bridge.rs`

ConfigSnapshot 转换逻辑。

### Step 3.4 — `cli.rs` ConfigSnapshot 构建

```rust
let temperature_source = if self.core.temperature_override.is_some() {
    "config"
} else if self.core.manifest_temperature.is_some() {
    "manifest"
} else {
    "default"
}.to_string();
let manifest_temperature = self.core.manifest_temperature;
```

### Step 3.5 — `session_init.rs` (first-start seed)

在 avatar seed 逻辑附近添加：

```rust
seeded.temperature = ctx.loaded.manifest.llm.temperature;
```

---

## Phase 4：Gateway + 前端

### Step 4.1 — `agent_config.rs` (Gateway)

✅ 已完成（新字段已存在）。

### Step 4.2 — `agents.rs` (Gateway)

在构建 `AgentConfigResponse` 时填充 `temperature_source` 和 `manifest_temperature`。

### Step 4.3 — `lib/types.ts` (前端)

```typescript
temperature_source?: "config" | "manifest" | "default" | null;
manifest_temperature?: number | null;
```

### Step 4.4 — `chatStore.ts` 注释修正

```typescript
/** Per-session temperature override (from Runtime, persisted in JSONL metadata) */  // 旧
/** Final resolved temperature (from Runtime, read-only display value) */             // 新
```

### Step 4.5 — `ResultsPanel.tsx` source 标记

```tsx
const tempLabel = temperature != null
  ? `${temperature.toFixed(2)} (${temperatureSource ?? 'default'})`
  : undefined;
```

### Step 4.6 — Agent Setup 温度输入框

placeholder: `留空则使用包默认值 {manifest_temperature}`
来源提示: `当前使用：来自{source}`

---

## Phase 5：构建验证

```bash
cd core && cargo build && cargo clippy --all-targets -- -D warnings && cargo test
cd apps/acowork-desktop && npx tsc --noEmit
```

---

## 文件变更清单

| # | 文件 | 操作 | 行数 |
|---|------|------|------|
| 1 | `agent_core.rs` | 新增字段 + init + clone | +3 |
| 2 | `cli.rs` | seed manifest_temperature | +3 |
| 3 | `loop_context.rs` | 替换字段 + 注释 | +2 |
| 4 | `loop_session.rs` | 替换字段 + 注释 | +2 |
| 5 | `session_manager.rs` | 替换字段 + 注释 | +2 |
| 6 | `context.rs` | 注释更新 | +1 |
| 7 | `session_state.rs` | 注释修正 | +2 |
| 8 | `protocol.rs` | 新增字段 | +5 |
| 9 | `gateway_ipc.proto` | proto 定义 | +4 |
| 10 | `proto_bridge.rs` | 转换逻辑 | +10 |
| 11 | `session_init.rs` | first-start seed | +2 |
| 12 | `agents.rs` | 填充字段 | +3 |
| 13 | `lib/types.ts` | 类型定义 | +3 |
| 14 | `chatStore.ts` | 注释修正 | +1 |
| 15 | `ResultsPanel.tsx` | source 标记 | +3 |
| | **合计** | | **~46 行** |
