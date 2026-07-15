# 前端最后一轮 Assistant 回复未渲染 — 分析报告

> 范围：仅分析 `apps/acowork-desktop`（Tauri v2 桌面前端）代码，**未做任何修改**。
> 现象：前端与 agent 对话，最后一轮 agent 的回复没有渲染出来；conversation JSONL 文件的最后一条 `assistant` 记录是存在的（说明数据已落盘，问题在前端渲染/投递）。

---

## 一、结论速览（按可能性排序）

1. **最终投递存在竞态，且没有重试**：最后一条消息能否出现，完全依赖 `session_state_changed → idle` 触发的一次"确定性最终增量拉取"，拉完立即 `stopPolling()`。一旦这次拉取在竞态下被丢弃 / 未触发，消息就**永久丢失**，因为轮询马上停了，没有后续补偿。
2. **流式契约前后端不匹配（最值得先排查的硬伤）**：后端把"正在流式输出中的那一行"放在**独立的 `streaming` 字段**返回，而前端 `PaginatedMessages` 类型里**根本没有 `streaming` 字段**，前端 `loadSessionMessages` 也**只消费 `data.messages`、从不读 `data.streaming`**。这意味着前端**从未从轮询里创建过流式占位消息**，活体流式内容完全没接上（这本身就是一个独立的 bug，且会让"最终投递"成为最后一条消息唯一的出现机会）。
3. **增量 merge 的清理逻辑会删除占位**：增量合并时，凡是 `isStreaming && 不在本轮响应里` 的消息会被从 `messages[]` 和可变内容 Map 中删除（chatStore.ts:1228-1235）。该逻辑依赖"占位 id 与落盘后真实 id 衔接正确"，一旦衔接在竞态下错位，最后一条消息会先被删、又没被补回。

---

## 二、数据流梳理（正常设计意图）

后端（runtime `cli.rs`）的增量接口 `/sessions/{sid}/messages?incremental=true`：
- 返回 `messages` = **已落盘的完整 JSONL 行**（按后端 `DeliveryCursor` 行号投递，每次最多 `limit=50` 行）。
- 返回 `streaming` = **内存里正在输出的那一行**的 delta（`StreamingLineDelta`，带 `line` 行号、角色、已累积内容），它**还不是 JSONL 里的完整行**。
- 网关 `proxy.rs` 对该路由是**纯透传**，不做任何合并。

前端期望（`lib/types.ts`）：
- `PaginatedMessages` 只有 `messages: ConversationEntry[]`，**没有 `streaming` 字段**（types.ts:712-719）。
- `ConversationEntry.is_streaming` 的注释明确写道："ADR-027: true when this entry is an in-progress streaming line **projected into messages[] by the Gateway**"（types.ts:695-698）。
  → 也就是说，前端代码是**按"网关把流式行投影进 `messages[]`（带 `is_streaming=true`、id=`streaming:{line}`）"这个契约写的**。

**矛盾点**：后端实际实现（`core/acowork-runtime/src/cli.rs:2982-3018`）把流式放在独立 `streaming` 字段，`messages[]` 里的 DTO 只含 `id/ts/role/content/metadata/kind`，**不含 `is_streaming`**。于是：
- 前端只读 `data.messages`（chatStore.ts:1138 `converted = mergeDocumentUploads(data.messages ?? [], ...)`），**`data.streaming` 全程从未被读取**（全局搜索 `data.streaming` 仅在 debug 日志里出现）。
- `convertConversationEntry` 虽然读 `entry.is_streaming`（chatStore.ts:1527），但后端 DTO 根本不带这个字段 → 永远是 `undefined`。
- 结论：**前端从头到尾没有为"正在输出"的那一行建过流式占位消息**，活体流式渲染链路是断的。

---

## 三、关键代码位置

| 关注点 | 位置 |
|---|---|
| 增量合并 + 占位清理（删 `isStreaming` 且不在响应内） | `stores/chatStore.ts:1187-1260`（清理在 1228-1235） |
| `loadSessionMessages` 只吃 `data.messages`，忽略 `data.streaming` | `stores/chatStore.ts:1138` |
| `done` 事件：明确注释"旧 bug 是 done 里开轮询导致竞态丢段"，改为不在 done 里拉取 | `stores/chatStore.ts:1759-1789` |
| `session_state_changed → idle`：做"最后一次确定性增量拉取"，随后 `stopPolling()` | `stores/chatStore.ts:2039-2054` |
| `loadSequence` 竞态保护（丢弃过期响应） | `stores/chatStore.ts:1082-1121`、`1118`、`1127` |
| 流式内容存在 React 状态之外的可变 Map | `stores/chatStore.ts:13-86`、渲染侧 `components/chat/MessageBubble.tsx:228`（读 `useStreamingContent`） |
| 显示分组（thought/工具 进 explore_group，assistant 单独渲染） | `components/chat/ChatPanel.tsx:405-450` |
| 轮询器：状态非 active 时直接 `stop()` | `lib/polling.ts:181-194` |
| 后端增量响应：streaming 独立字段 + DTO 无 `is_streaming` | `core/acowork-runtime/src/cli.rs:2982-3018` |
| 前端响应类型无 `streaming` 字段 | `lib/types.ts:712-719` |

---

## 四、为什么会"只丢最后一条"

要点：**只有最后一轮**的 assistant 回复是"在 `done` 之前一直在流式、落盘发生在 `done` 时刻"的那一行。更早的轮次早已落盘并被之前的轮询投递过，所以不受影响；而最后这一行，它的"出现"完全取决于 **`idle` 那一次最终增量拉取**能否把落盘后的真实行拿回来。

### 4.1 最终拉取的触发条件有"漏触发"风险
idle 分支（chatStore.ts:2039）：
```js
} else if (prevActive && !nextActive) {
  get().loadSessionMessages(agentId, sid, undefined, 50, "backward", true).finally(() => {
    stopPolling(agentId, sid);
  });
}
```
只有当本地记录的 `prev` 状态是 `streaming/waiting_approval/paused` 时才会触发最终拉取。如果**首包 `session_state_changed → streaming`（或 `new_data_available`）在前端侧被漏收**（MQTT 订阅尚未建立、或事件在 UI 初始化前到达），本地 `prev` 就不是 active 态，于是 `idle` 到来时**两个分支都不进**，最终拉取根本不会被调用；而此时轮询也早已因状态非 active 被 `PollingManager.doPoll` 停掉（polling.ts:181-194）。→ 最后一条消息永远不再被拉取。

### 4.2 即便触发了，也存在"被丢弃/删了没补回"的竞态
- 最终拉取 `loadSessionMessages` 会 `abort` 掉在途的旧 doPoll 并把 `loadSequence+1`（chatStore.ts:1086-1095）。旧轮询若已在 `json()` 之后、尚未过 `loadSequence` 校验前，会以 `loadSequence !== seq` 被丢弃（1118/1127）——这本是保护，但**它同样会把"刚好包含最后一条落盘消息"的那次响应丢弃**。正常情况下由本次最终拉取（seq 更大）补回；可一旦最终拉取自身因为 `idle` 事件的时序、`stopPolling` 的 `.finally` 在 `loadSessionMessages` resolve 之前就停了轮询而**没有任何重试兜底**，最后一条就可能彻底丢失。
- 更隐蔽的一条：增量合并的清理（chatStore.ts:1228-1235）会删除"流式占位 `streaming:{line}` 且不在本轮响应里"。设计意图是"它已落盘成真实 id 的行，会被 upsert 补回"。但**前端实际根本没有创建过 `streaming:{line}` 占位**（见第二节契约不匹配），所以这条清理在当前代码下是空转——这反过来说明：一旦后端某天改成"把流式行投影进 `messages[]`"，占位与落盘后真实 id 的衔接就必须在合并逻辑里严丝合缝，否则就是"先删后补"的竞态温床（也就是 `done` 注释里描述的"missing-segments"那类 bug 的同源问题）。

### 4.3 与日志的对照
最新日志 `20260715_173607.log` 显示：`Session status changed old=Streaming new=Idle`（17:36:25.137），且 `flush_streaming_line ... role=assistant ... wrote to JSONL`（17:36:25.136）先于 idle 发布。即后端确实在 idle **之前**已落盘。这说明：只要前端在 idle 时那次最终拉取被正确触发并成功返回，最后一条 assistant 应当能拿到。因此**问题大概率出在"最终拉取没被触发 / 被竞态丢弃 / 拉到但被合并逻辑漏掉"这三者的组合**，而不是后端没写盘。

---

## 五、如何快速坐实根因（代码里已埋好 DEBUG 日志）

不需要改代码，打开 DevTools 控制台复现一次，重点看这几行日志：

1. `[ChatStore:DEBUG] new_data_available for ...` —— 流式期间 `status` 是不是 `streaming`？轮询 `messageCount` 是否随拉取增长？
2. `[ChatStore:DEBUG] Last incremental message: id=... role=...` —— 出问题那次拉取里，**最后一条 `assistant` 的 `id` 和 `role` 是什么**？有没有出现过 `role=assistant`？
3. `[ChatStore:DEBUG] session_state_changed ... prev=... → next=idle` —— `prev` 是不是 `streaming`？**如果不是**，则 4.1 漏触发坐实。
4. `[ChatStore] Discarding stale response (seq ...)` / `Discarding stale response after json parse` —— 有没有出现"包含最后一条的响应被丢弃"？
5. `[PollingManager:DEBUG] doPoll SKIPPED ... status=idle` —— idle 后轮询是否被立即停掉，而最终拉取却没补到消息。

把以上日志贴出来，就能 100% 定位是 4.1（漏触发）还是 4.2（竞态丢弃）。

---

## 六、修复方向（仅建议，未实施）

1. **先对齐流式契约**（最高优先级，且是独立硬伤）：要么让网关/运行时把流式行投影进 `messages[]`（带 `is_streaming=true`、id=`streaming:{line}`）以匹配前端 `PaginatedMessages`/`convertConversationEntry`；要么改前端 `loadSessionMessages` 消费 `data.streaming` 字段并自行生成 `streaming:{line}` 占位。否则活体流式渲染一直是断的。
2. **最终投递必须"确认收到才停"**：idle 那次最终拉取应等它真正把 `has_more=false` 且包含了末尾 assistant 行之后，再 `stopPolling`；或拉取结果缺失末尾消息时**自动再拉一次**做补偿，而不是 `.finally` 里无脑停。
3. **去掉"漏触发"死角**：即使本地 `prev` 不是 active，只要收到 `idle` 且当前 `messages` 末尾不是对应 assistant 行，也应触发一次最终增量拉取兜底。
4. **占位衔接要稳健**：若保留"占位 id ≠ 落盘真实 id"的模型，合并逻辑要保证"删除占位"与"upsert 落盘行"在同一原子更新里完成，且删除前先确认落盘行已存在，避免"先删后补"窗口。

---

## 七、一句话总结

最后一条 assistant 回复没渲染，根因不在后端（日志证明已落盘），而在前端**"最终投递"链路脆弱**：流式契约前后端脱节导致活体流式没接上、最后一条消息只能靠 `idle` 那一次增量拉取出现，而该拉取既可能因本地状态没处于 active 而**根本不触发**，也可能在 `loadSequence` 竞态下**被丢弃**，且拉完立即停轮询、**没有任何重试兜底**——于是最后一行永久消失。建议先用第五节里的既有 DEBUG 日志复现一次坐实具体是哪一种。
