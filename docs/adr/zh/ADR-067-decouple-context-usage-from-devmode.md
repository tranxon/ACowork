# ADR-067：解耦 Context Usage 分类用量与 DevMode

**状态**：已实施
**日期**：2026-09-04
**决策者**：大鱼
**前置 ADR**：ADR-048（DevMode 调试面板）、ADR-060（Context Block 重构）、ADR-066（Cache Tokens 累计）

**影响范围**：

- `core/acowork-core/src/protocol.rs`（`ContextUsageInfo` 新增 `sections: Option<Vec<ContextUsageSection>>`；新增 `ContextUsageSection` 结构体）
- `core/acowork-runtime/src/agent/history.rs`（新增 `messages_json_bytes` 增量计数器：`append`/`extend` O(1) 维护，结构操作重算；新增 3 个单元测试）
- `core/acowork-runtime/src/agent/context.rs`（新增 `compute_section_sizes` 自由函数；`messages` section 字节数改用 `HistoryManager::messages_json_bytes()`；**移除 `todo_context` section 与 `latest_todo_write_content` helper**——ADR-060 v2 已把 todo 移出系统提示词，todo 状态只存在于 `messages` 的工具结果里，无需单独计数；新增 2 个单元测试）
- `core/acowork-runtime/src/agent/loop_context.rs`（`process_llm_response_usage` 调用 `compute_section_sizes` 填充 `ctx_usage.sections`，并把带 sections 的 payload 缓存到 conversation；MCP tools 改用 `self.core.mcp_tools` 而非 `all_tools` 过滤前缀——后者会误计内置 `mcp_install`/`mcp_uninstall`）
- `core/acowork-runtime/src/agent/loop_session.rs`（`emit_session_state` 从缓存合并最近一次 sections，避免保留的 `session_state` 快照把 popover 分类清成 0）
- `core/acowork-runtime/src/conversation.rs`（新增 `cache_context_usage` / `last_context_usage_json`）
- `core/acowork-runtime/src/debug/observer.rs`（`ContextSnapshotRequest` 新增 `mcp_tools` 字段）
- `core/acowork-runtime/src/debug/observer_impl.rs`（`on_context_built` 改用 `compute_section_sizes` 作为唯一来源并消费 `req.mcp_tools`）
- `core/acowork-runtime/tests/context_usage_cache_e2e.rs`（所有 `ContextUsageInfo` 构造点补 `sections: None`）
- `apps/acowork-desktop/src/lib/types.ts`（`ContextUsageInfo` 新增 `sections?: ContextUsageSection[]`）
- `apps/acowork-desktop/src/components/chat/ContextUsageIcon.tsx`（移除 `useDebugStore` 依赖；改读 `chatStore.contextUsage.sections`）
- `apps/acowork-desktop/src/stores/chatStore.ts`（新增 `mergeContextUsage`：`session_state` / `fetchSessionState` 更新 `contextUsage` 时保留旧值已有而新值缺失的 `sections`，防止被无 sections 的持久化快照覆盖）
- `apps/acowork-desktop/src/components/chat/ContextUsageIcon.test.tsx`（移除 `debugState` mock 与 `useDebugStore` mock；新增 ADR-067 回归测试）
- `apps/acowork-desktop/src/stores/chatStore.test.ts`（新增 2 个 sections 保留/覆盖回归测试）

---

## 背景

输入框右下角的 `ContextUsageIcon` 在鼠标悬停时会弹出一个分类用量
popover，把当前 context 拆成 5 类（system / tools / messages / connectors /
skills），按比例展示各自的占用百分比。这 5 个百分比是按"每个 section
的字节数 / 总字节数"加权、再乘上 LLM 实报的 `usage_percent` 计算出来的，
字节数据来源于运行时在每次 LLM 调用前组装 context 时记录的 section
metadata。

在 ADR-067 之前，这些 section 字节数据只通过一个通道下发：runtime 的
`DebugObserverImpl::on_context_built` 监听器，而这个监听器只有在
`DebugObserverSlot::Dev`（即 DevMode 调试面板开启）时才会触发。
Production 模式下对应的 slot 是 `Production` 包装的 no-op。

后果很直接：**只要用户没打开 Debug 面板，输入框的 5 个分类百分比就
始终是 0**，而外层圆环的 `usage_percent` 是有的（那个走的是另一条
always-on 的 `ContextUsage` chunk 推送通道）。前端代码也是从
`useDebugStore` 的 snapshots 数组里取 sections，跟运行时是同一份
故障表现。

## 触发

用户报告输入框 popover 内的 5 个子项百分比全部显示为 0，必须打开
Debug 面板才正常。Bug 的范围判断：

- **这是 UI 状态而非调试信息**：context usage 圆环和分类 popover 是
  输入框的全局 UI，跟 Debug 面板是两套独立的展示面。Debug 面板打开
  才出数据是明显的耦合错位。
- **运行时单条数据已经在 wire 上**：`ContextUsageInfo` 每次 LLM
  调用后都会通过 `ChunkEvent::ContextUsage` 推到 chatStore，bill 的
  `usage_percent` 就是这条路来的。缺的是这同一个 payload 里的
  `sections` 字段。
- **运行时逻辑不需要 DevMode 就能产出数据**：`DebugObserverImpl` 内部
  的 `on_context_built` 实现其实是 pure function——它接收一个
  `ContextBuilder`、一个 `HistoryManager`、MCP tools 列表、model
  name，就能算出完整的 section 列表；`DebugObserverSlot::Production`
  只是因为"DevMode 才会用到这份数据"而把它禁掉了。

## 决策

把 section 字节数据的产出**从"调试快照"通道剥离**，**提升为"运行时
观测"通道**的一部分，让 `ContextUsageInfo` 这个 always-on 的 push
payload 自带 `sections` 字段。具体三步：

### 1. 提取 `compute_section_sizes` 为自由函数

把 `DebugObserverImpl::on_context_built` 内部那段按 `ContextBuilder`
字段顺序累加 section 字节数的逻辑（原 `named: Vec<NamedSection>`
构造循环）抽到 `acowork-runtime/src/agent/context.rs` 里成为顶层
`pub fn compute_section_sizes(builder, history, mcp_tools, model) -> Vec<ContextUsageSection>`。
这是**唯一的实现源**——DevMode 路径和 always-on 路径都必须走它，
保证 UI 看到的 section key 顺序与字节数完全一致。

注意：**`todo_context` 不作为独立 section 输出**。ADR-060 v2 已把
todo 快照（Block C）从系统提示词移除，todo 状态只存在于历史中
`todo_write` 工具调用的结果里，而这些结果已经计入 `messages` section
的字节数——再单独加一个 `todo_context` 就是重复计数。相应地
`latest_todo_write_content` 扫描 helper 也一并删除。

### 2. `ContextUsageInfo` 新增 `sections` 字段

`acowork-core/src/protocol.rs`：

```rust
pub struct ContextUsageSection {
    pub key: String,        // 稳定 contract: "system_prompt" / "messages" / ...
    pub size_bytes: u64,     // 精确 UTF-8 字节数
}

pub struct ContextUsageInfo {
    // ... 既有字段 ...
    pub sections: Option<Vec<ContextUsageSection>>,
}
```

字段是 `Option<Vec<…>>` 而不是裸 `Vec`，因为前向兼容：未升级的旧版
Runtime 不会填这个字段，前端 `contextUsage.sections ?? []` 的写法
让它自然降级成"无数据"，popover 退化到全 0（这是已知行为，不会崩）。

### 3. `process_llm_response_usage` 填充 `sections`

在 `loop_context.rs` 的 `process_llm_response_usage` 里，每次
构造完 `ctx_usage`、打过 `patch_session_totals` 之后，调用
`compute_section_sizes(context_builder, &history, &mcp_tools, current_model)`
并把结果挂到 `ctx_usage.sections`。MCP tools 取自 `self.core.mcp_tools`
（与 `build_chat_request` 注入 `ChatRequest.tools` 用的是同一份集合）
——**不是** `all_tools` 过滤 `mcp_` 前缀：`all_tools` 混入了内置的
`mcp_install` / `mcp_uninstall`，按前缀过滤会把这两个内置工具也算进
`tool_definitions`，导致 `tools` 分类字节偏高。DevMode 路径通过
`ContextSnapshotRequest::mcp_tools` 传递同一份集合，两条路径字节数
一致。

DevMode 路径反过来也改：`DebugObserverImpl::on_context_built` 改用
`compute_section_sizes` 拿 `base_sections`，然后只在这之上补 DevMode
专属的 metadata（完整 content、`token_estimate`、SHA256 hash），
字节大小**始终**以 `base_sections` 为准，避免两份独立算法漂移。

### 4. 阻止 `session_state` 快照清空 `sections`

`emit_session_state`（保留主题、每轮多次推送）用
`build_context_usage_from_persisted` 构建 context_usage——该路径没有
`ContextBuilder`，无法重算 sections。若不处理，它每轮到达都会把
`sections` 覆盖成空，popover 恒为 0。两层修复：

- **运行时**：`process_llm_response_usage` 算完 sections 后把完整
  payload 缓存到 `ConversationSession`（`cache_context_usage`）；
  `emit_session_state` 构建时合并缓存中的 sections
  （`last_context_usage_json`）。
- **前端防御**：`chatStore` 的 `session_state` / `fetchSessionState`
  更新 `contextUsage` 时，若新值没有 `sections` 而旧值有，保留旧值
  （`mergeContextUsage`），即使连接旧版 Runtime 也不丢分类。

### 5. `messages` 字节数 O(1) 增量计算

`compute_section_sizes` 不再每轮 `serde_json::to_string(&history)`
全量序列化。`HistoryManager` 新增 `messages_json_bytes` 计数器，
语义恒等于 `serde_json::to_string(&messages).len()`（含 `[]` 括号）：

- `append` / `extend`：O(1) 增量（单条序列化长度 + 分隔符）
- `load_restored` / `clear` / `truncate_to` / `fit_to_budget_lossless` /
  `replace_middle_with_summary` / 8 级压缩 / `abandon_tool_result` /
  `retrieve_tool_result`：重算或调整（低频操作）
- 读取：O(1)

这样非调试模式（也是默认模式）下 per-LLM-call 的额外成本只有
`tool_definitions` 一次小型序列化 + 若干 `.len()`，不碰 messages。

## 取舍

**为什么不在 `compute_section_sizes` 之外再保留 DevMode 自己的字节
累加？** 起初考虑过"DevMode 加 hash / token_estimate，always-on 只
要 size_bytes" 的分叉实现，但前端要的 section 列表只有一个
(`computeContextUsageBreakdown` 用字节数算百分比)，多一个实现就
多一份漂移风险——一旦某天加了一个新 section 字段（比如 P3-4 加
`ambiguous_confirmation_hint` 时的翻车历史），两条实现就会分别
忘改一个。统一函数从根上消除这个分叉。

**为什么不让前端去抓 `onContextBuilt` MQTT 事件？** 那是 dev-only 的
channel，production 模式下整个 topic 都没有事件。前端订阅不到，
硬要走这条路就得新增一个 always-on 的 topic，但每个 LLM 调用已经
会推 `ContextUsage` chunk——直接把这个 payload 加重一字段是
更小的改动。

**为什么 sections 字段是 `Option<Vec<…>>` 而不是 `Vec<…>` 默认空？**
前向兼容。前端按 `?? []` 兜底，后端老构造点（fallback 路径、
`build_context_usage_from_persisted` resume 路径、`tests/context_usage_cache_e2e.rs`
e2e fixture）暂时都填 `None`，等所有 push 路径统一改成计算
`compute_section_sizes` 再降级为 `Vec<…>`——这是 §"后续清理"里的
to-do，不是本次 ADR 的阻塞点。

## 实际影响

**用户可见行为**：输入框 popover 内的 5 个分类百分比现在和圆环
`usage_percent` 一起持续更新，不再依赖 Debug 面板开关。

**性能开销**：`compute_section_sizes` 在每次 LLM 调用时执行一次。
最重的部分（history 序列化）已通过 `HistoryManager::messages_json_bytes`
增量计数器消除——非调试模式下 per-LLM-call 只多一次 `tool_definitions`
的小型序列化（内置 + MCP tool schema，通常 < 50KB）和若干 `.len()`，
整体 < 0.1ms 量级，可忽略。DevMode 下 `on_context_built` 仍会为
Debug Panel 序列化一次 messages（计算 hash / token estimate / 懒加载
content），那是开发者工具成本，不在 hot path。

**wire 协议**：`ContextUsageInfo` 现在多一个 `sections` 字段。MQTT
payload 大小增加约 200-400 字节（取决于 section 数量），chunk 频率
不变（每 LLM 调用一次）。

## 后续清理（不在本次范围）

- `ContextUsageInfo.sections: Option<Vec<…>>` → `Vec<…>`（去掉 Option；
  已通过缓存 + 前端合并保证 `session_state` 也带 sections，可择机降级）
- `DebugObserverImpl::on_context_built` 的 `tool_defs_str` 本地
  序列化可以删掉，直接复用 `compute_section_sizes` 的同一份——但
  DevMode 面板要展示完整 content 还是需要独立 string 化，目前维持
  现状

## 验证

- Rust 单测：`history::tests::messages_json_bytes_*` 3 个（增量计数 vs
  全量序列化一致性、clear/truncate、abandon/retrieve）、
  `context::tests::compute_section_sizes_*` 2 个全过；`cargo test -p
  acowork-runtime --lib -- --test-threads=1` 1345 全过。
- 前端单测：`ContextUsageIcon.test.tsx` 6 个、`contextUsageBreakdown.test.ts`
  12 个、`chatStore.test.ts` 38 个（含新增的 session_state 保留 sections
  回归）全过。
- 前端相关单测：`contextUsageBreakdown.test.ts` 12 个、`cacheHitRate.test.ts` 46 个全过。
