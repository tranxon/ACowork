# 24 — Compaction Display & Identity-Aware Summary — Code Review

**Date**: 2026-06-22
**Reviewer**: Senior Engineer
**Status**: ✅ Approved

## Scope

| File | Summary |
|------|---------|
| `core/acowork-core/proto/gateway_ipc.proto` | `ConversationEntryDto` adds `string kind = 6` |
| `core/acowork-core/src/protocol.rs` | `ConversationEntryDto` adds `kind: Option<String>` |
| `core/acowork-core/src/proto_bridge.rs` | `kind` bidirectional mapping + roundtrip test |
| `core/acowork-runtime/src/conversation.rs` | **重点改动** — `SessionMetadata.last_compaction_offset` 相对偏移持久化、`ConversationWriter` 注入偏移到 metadata、`read_active_lower` 优先读 metadata O(1) + fallback scan；display-group pagination |
| `core/acowork-runtime/src/agent/history.rs` | `compact_via_llm` accepts `identity_context: Option<&str>`; +3 tests with `CaptureProvider` |
| `core/acowork-runtime/src/agent/loop_context.rs` | Threads `self.session.identity_context()` into `compact_via_llm` |
| `core/acowork-runtime/src/agent/loop_session.rs` | Snapshots identity before `tokio::spawn` for tail distillation |
| `core/acowork-runtime/src/agent/session_state.rs` | New field `identity_context: Option<String>` + accessors |
| `core/acowork-runtime/src/agent/session/session_task.rs` | Dual-write identity to `ContextBuilder` and `SessionState` |
| `core/acowork-runtime/src/episode_distill.rs` | All compact/distill fns accept `identity_context`; `compact_with_llm` now sends `system + user` (was `user` only) |
| `core/acowork-runtime/src/prompt.rs` | `build_compaction_system_prompt()` appends identity block + language directive; +5 tests |
| `core/acowork-runtime/src/cli.rs` | `handle_get_session_messages` propagates `kind` to DTO |
| `core/acowork-gateway/src/http/chat.rs` | `MessageEntryResponse` adds `kind` field |
| `apps/acowork-desktop/src/lib/types.ts` | `MessageType` adds `"compaction"`, `CompactionEventMeta`, `ConversationEntry.kind` |
| `apps/acowork-desktop/src/stores/chatStore.ts` | `convertConversationEntry` detects `kind="compaction"` → `type:"compaction"`; `stripSummaryTags` |
| `apps/acowork-desktop/src/components/chat/ChatPanel.tsx` | Compaction flushes explore, renders as standalone item |
| `apps/acowork-desktop/src/components/chat/CompactionCard.tsx` | **New.** Folded summary card, dark/light theme, Markdown body |
| `apps/acowork-desktop/src/components/common/UserAvatar.tsx` | Extracts icon catalogue to leaf module; re-exports for back-compat |
| `apps/acowork-desktop/src/lib/builtinIcons.ts` | **New.** Leaf module to break TDZ circular dependency |
| `apps/acowork-desktop/src/lib/avatar.ts` | Imports from `./builtinIcons` instead of `UserAvatar` |

## Verdict

**Approved** — 所有 Important Issues 已在实现中解决。

---

## Resolved: `locate_last_compaction_offset` 性能问题

**问题**: `locate_last_compaction_offset` 在无 compaction 的大文件上累积内存 + O(n²) 反序列化，且在每次分页请求时都被调用。

**解决方案**: 相对偏移持久化。

### 设计

```
写入时（Writer 线程，自动）：
  AppendEntry(entry) where entry.kind == "compaction"
    → seek(End) 捕获 abs_offset
    → write_entry(entry, already_positioned=true)
    → self.last_compaction_offset = abs_offset - self.meta_end

  UpdateMetadata(meta) 到达时
    → meta.last_compaction_offset = self.last_compaction_offset
    → rewrite_metadata(&meta)  // 持久化

rewrite_metadata 执行后
    → self.meta_end = new_meta.len() + 1  // metadata 行可能变长
    → 无需更新 last_compaction_offset（相对 offset 不变）

读取时（read_messages_paginated）：
  read_active_lower(file, meta_end)
    ├── metadata 有 last_compaction_offset
    │     → abs = meta_end + relative       (O(1))
    └── 没有（legacy 文件）
          → locate_last_compaction_offset   (O(N), 仅首次)
```

### 为什么相对偏移不受 metadata 重写影响

`rewrite_metadata` 换第一行并用 read+join+rename。若第一行增长 Δ 字节，所有 data 行平移 Δ：

```
meta_end_new = meta_end_old + Δ
abs_new = abs_old + Δ
relative = abs_old - meta_end_old  (不变)
→ meta_end_new + relative = (meta_end_old + Δ) + (abs_old - meta_end_old) = abs_old + Δ = abs_new  ✓
```

### 实现要点

- `SessionMetadata.last_compaction_offset: Option<u64>` — 相对于 data-start 的偏移
- `ConversationWriter` 新增 `meta_end: u64` 和 `last_compaction_offset: Option<u64>`
- Writer 循环在 `AppendEntry` 时检测 `kind="compaction"`，写入前捕获 abs_offset
- Writer 循环在 `UpdateMetadata` 时自动注入偏移
- `ConversationSession::new` / `resume` 计算 `meta_end` 并传给 writer
- `read_messages_paginated` 新增 `read_active_lower()` 函数

### 效果

```
                    正常 close      崩溃/强杀
元数据已写入 offset    O(1)          O(1)      ← 首次 compaction 后面的 UpdateMetadata 写入
元数据未写入           O(1)          O(N) 一次  ← legacy 文件首次打开 scan

之前：每次分页都是 O(N)
之后：常态 O(1)
```

---

## Remaining Minor Issues

### 2. `history.rs` — 按长度切片断言可能在语义变化时 panic

**文件**: `core/acowork-runtime/src/agent/history.rs`
**涉及行**: ~1530

```rust
assert_eq!(system[0..crate::prompt::COMPACTION_SYSTEM_PROMPT.len()].to_string(), ...);
```

如果 prompt 结构变更为 identity 在前，`system` 可能短于 `COMPACTION_SYSTEM_PROMPT`，`[0..N]` panic。

**建议**: 改用 `assert!(system.starts_with(...))`。`prompt.rs` 同名测试已用此模式。

### 3. `session_task.rs` — identity 双写隐式约定

**文件**: `core/acowork-runtime/src/agent/session/session_task.rs`
**涉及行**: ~452, ~1196-1201

`ContextBuilder::set_identity_context(String)` 与 `SessionState::set_identity_context(Option<String>)` 签名不同，但调用方需"知道两者都把空字符串当清空"。两个 call site 重复相同双写模式。

**建议**: 统一签名或抽工具函数。

---

## Minor Suggestions

- **`conversation.rs` display group 判定逻辑三处重复**: 提取 `fn classify(e: &ConversationEntry) -> GroupKind`
- **`episode_distill.rs` `compact_with_llm` 行为变化**: 之前只发 user message，现在发 system + user。在函数 doc 上加说明
- **`chatStore.ts` `as number ?? 0`**: 改为 `typeof x === "number" ? x : 0` 避免 NaN 语义
- **`CompactionCard.tsx` token stats fallback**: 加注释说明旧 JSONL 可能缺 `before_tokens`/`after_tokens`
- **`distill_on_session_end`**: 当前无调用方（dead public API），不属本次引入，但建议在 PR 描述中记录预期调用链

---

## What Went Well

- **测试覆盖**: 新增 18 个测试（proto_bridge 1, prompt 5, history 3, conversation 9），全部 23/23 通过
- **一致性**: `ConversationEntry.kind` → `ConversationEntryDto.kind` → `MessageEntryResponse.kind` 链路完整
- **前后端对齐**: `ChatPanel.displayMessages` 的 group 判定与 `count_display_groups`/`trim_*` 规则一致
- **循环依赖修复** (`builtinIcons.ts`): 提取 leaf module 消除 TDZ
- **CompactionCard**: 遵循 `ExploreBlock` 设计语言，dark/light 全覆盖
- **语言注入**: 用相对偏移替代全文件扫描，零 schema、零 fragile parsing，O(1) 热路径
