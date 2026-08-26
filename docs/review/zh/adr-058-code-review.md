# ADR-058 代码评审报告（本地未提交改动）

**日期**：2026-08-25
**评审对象**：`git status` 全部未提交改动（13 个修改文件 + workspace/ 新模块 + 前端 lib + 2 份 review 文档）
**对照标准**：`docs/adr/zh/ADR-058-workspace-fs-watcher-mqtt-event.md`（修订版）
**评审方法**：逐 diff 人工审读 + 源码实证（rumqttc 0.25.1 / rumqttd 0.20.0 源码）+ 本机实跑验证

---

## 一、总体结论

> **📌 状态更新（2026-08-25 评审后）**：原报告发现 1 个 High 缺陷（H-1 idle sleep 唤醒兜底失效）阻塞提交。**评审建议项已全部处置完毕**（H-1/M-1/M-3/L-1/L-3 已修复，M-2 记入待办，L-2 保持观察），全量回归验证绿（前端 185 tests、Rust e2e+单测、双侧 clippy 零告警）。用户已实测确认「LLM 修改文件 → 前端实时同步」正常。**当前可按 ADR 6-commit 计划提交**，提交后手工验收重点补「Desktop 保持连接 + Runtime 休眠唤醒」路径。以下为原始评审结论（保留存档）。

**实现质量高，架构纪律优秀，但存在 1 个 High 级功能缺陷（idle sleep 唤醒兜底失效），修复前不建议按 6-commit 提交。**

| 维度 | 评级 | 一句话结论 |
|------|------|-----------|
| 功能完整性 | ⚠️ 良好 | W0–W5 主链路全部落地且可用；但 W5 验收项"idle sleep 唤醒后 FileTree 与磁盘一致"在「Desktop 全程连接」场景下**不成立**（见 §三 H-1，**已修复**） |
| 测试覆盖 | ⚠️ 良好偏科 | 聚合器/watcher_set/前端 store 逻辑覆盖扎实；MQTT 发布链路、Tauri 解码分支、唤醒转换检测**零测试**——恰好是 H-1 藏身之处（**已补齐：全链路 e2e + 纯函数单测，前端 185 tests**） |
| 架构合理性 | ✅ 优秀 | 高内聚低耦合开闭三原则全部达标，对现有模块零污染；唯一耦合瑕疵为沿袭既有模式（见 §五 C-1） |

---

## 二、验证结果（本机实跑，非转抄实施报告）

| 检查 | 结果 |
|------|------|
| `cargo test -p acowork-runtime --lib workspace::` | ✅ 12 passed（fs_watcher 9 + watcher_set 3） |
| `cargo clippy -p acowork-runtime --all-targets` | ✅ 零告警 |
| `npx vitest run`（desktop 全量） | ✅ 12 文件 177 tests passed（含新增 10） |
| `npx tsc --noEmit` | ✅ 零错误 |

---

## 三、发现的问题（按严重度）

### H-1【High】idle sleep 唤醒兜底在「Desktop 全程连接」场景失效 —— W5 验收项不成立

ADR §3.4 明确：唤醒兜底是数据一致性的**主路径**（idle sleep 是常规路径），验收项为「Runtime idle sleep 唤醒后，FileTree 与磁盘状态一致」。但当前实现在 Desktop 未断线的情况下**检测不到唤醒转换**。

**完整证据链（全部源码实证）**：

1. `idle_watcher.rs`（模块文档 + L446-470）：到期 → publish `"sleeping"`（retained）→ `RuntimeMqttClient::disconnect()` → `process::exit(0)`
2. `mqtt/client.rs:1022-1024` 注释声称 *"invokes `disconnect()` so the broker kicks the Last-Will ('offline')"***——该断言错误**：
   - MQTT 规范：客户端发送干净 DISCONNECT 包时，broker **不得**发布 Will；
   - rumqttd 0.20.0 源码实证（`router/routing.rs:870-876`）：
     ```rust
     Packet::Disconnect(_, _) => {
         disconnect = true;
         // delete the last will message
         self.last_wills.remove(&client_id);
         break;
     }
     ```
     干净 DISCONNECT 直接**删除** Will，`offline` 永远不会发布；
   - `process::exit(0)` 跳过所有 Drop，`RuntimeMqttClient::Drop` 里 best-effort 的 `offline` publish 同样不执行。
3. 因此休眠后 retained status 停留在 **`"sleeping"`**；Desktop 全程连接时收到的事件序列是 `sleeping → online`，**中间没有 `offline`**。
4. `chat_mqtt.rs:1215`：`"sleeping" => ParsedAgentStatus { online: true, sleeping: true }`（sleeping 被映射为 online=true）。
5. `workspaceFsEvents.ts:120`：`if (prev && !prev.online && online)` —— `sleeping` 状态下 `prev.online === true`，唤醒后 `online === true`，**转换条件永远为 false**。
6. 与 ADR §3.4 的字面要求「**offline/sleeping → online** 转换」不符：实现只检测了 offline→online，漏了 sleeping→online。

**后果**：Desktop 保持连接 + Runtime 休眠再唤醒（ADR 定义的常规路径）→ 断眠期间磁盘变更全部丢失且**无兜底**，FileTree 与磁盘长期不一致——这正是 ADR 风险表标为「中」的第一风险的缓解措施失效场景。

**修复（一行）**：

```typescript
// workspaceFsEvents.ts — agent-event listener
if (prev && (!prev.online || prev.sleeping) && online) {
    scheduleFullTreeSync(`agent-wake:${agentId}`);
}
```

同时建议把该转换判断提取为纯函数（如 `isWakeTransition(prev, next)`）并补单测——它当前内联在 listener 闭包里不可测，这也是 H-1 未被 22 个新测试捕获的原因。附带建议修正 `mqtt/client.rs:1022` 的错误注释（属既有代码，可另行小 commit）。

> **✅ 2026-08-25 已修复**：`workspaceFsEvents.ts` 已提取纯函数 `isWakeTransition(prev, next)`（`wasDown = !prev.online || prev.sleeping` → `wasDown && next.online && !next.sleeping`），listener 改用它；新增 6 个单测覆盖 offline→online / sleeping→online（H-1 回归用例）/ 入睡 / 冷启动等全组合。`mqtt/client.rs::disconnect` 与 `idle_watcher.rs` 模块注释的 LWT 错误断言已一并修正（注明干净 DISCONNECT 不触发 Will + rumqttd 实现依据）。前端 185 tests + tsc 全过。

---

### M-1【Medium】MQTT 发布链路与 Tauri 解码分支零测试（W3 规格偏差的 consequences）

ADR W3 规格：「通过 **fake MQTT broker** 验证事件 payload」。实施报告偏差说明 #2 承认用 trait sink 注入替代。后果：

- `watcher_set.rs::MqttFsEventSink::publish`（envelope 编码 + topic 拼装 + QoS1）无测试；
- `chat_mqtt.rs` 的 `WorkspaceFsChangeEvent` 解码分支 + `fs_change_kind_str` 无测试；
- `derive_mqtt_broker_host`（纯函数，最易测）无单测。

Rust ↔ 前端的**字段命名契约**（`kind`/`path`/`timestamp_ms`/`window_end_ms` 的 snake_case 字符串映射）完全靠人肉对齐，任一侧改名都无测试兜底。

> **✅ 2026-08-25 补记（评审后处置）**：已新增 `core/acowork-runtime/tests/fs_watcher_e2e.rs` —— **真实全链路 e2e**（真实 PollWatcher + 真实 rumqttd broker + 真实 `RuntimeMqttClient` + 订阅端按 Desktop 同款方式 decode `DataEnvelope`），覆盖：外部新建（同窗口两写合并为单条 Created）/外部修改/外部删除/CRUD 新增 workspace 后新目录推送，全部断言通过（含 agent_id/workspace_id/path 归一化/window_end_ms 契约）。本条核心缺口（MQTT 发布链路 + envelope 契约）已闭环；剩余小项（`derive_mqtt_broker_host`、`fs_change_kind_str` 纯函数单测）已于 `chat_mqtt.rs::adr058_tests` 补齐（5 个断言组，含 Rust↔TS 字符串契约防回归），M-1 全部闭环。

### M-2【Medium】ADR 风险表明确要求「测试覆盖此场景」的 `quickCreateAndRename` 冲突未覆盖

ADR 风险表最后一行（中风险）：右击新建 → watcher 检测到 `Created` → 回推 fetchTree 与 Rename input 状态的冲突，「确保 `renameTarget` 状态不丢失 — **测试覆盖此场景**」。当前无任何测试触及 `WorkspaceExplorer` 的该路径。至少应补一个「同一事件流中 Created 事件 + 主动 fetchTree 并发时 renameTarget 不丢失」的前端测试，或在实施报告明确记录为未完成项。

> **✅ 2026-08-25 补记（处置：记入待办）**：项目当前无组件级测试基础设施（12 个测试文件均为 store/lib 单测），为 `WorkspaceExplorer` 渲染树引入首套组件测试属独立工程决策，超出本次修复范围。已如实登记到实施报告「待办/后续」，用户实测（右击新建 → 重命名流程）已确认 rename 交互正常。

### M-3【Medium】clean 文件静默 reload 与用户编辑的竞态可丢失输入

`handleEditorConflicts`：clean 文件收到 modified → `await refreshFile(file.id)`。若 refresh 在途期间用户开始输入（tab 变 dirty），refresh 完成后 `content/originalContent/dirty=false` 三重写会**覆盖用户刚输入的内容**。VSCode 用 mtime 复核规避同类问题。低概率但后果是静默丢字。建议 refreshFile 落盘前复核 `file.dirty` 已变 true 则跳过（或改为对比 diskModified）。

> **✅ 2026-08-25 已修复**：`fileEditorStore.refreshFile` 新增可选参数 `opts.skipIfDirty`（默认行为不变，开闭兼容）；fs 事件触发的静默 reload 传 `{ skipIfDirty: true }`——fetch 在途期间 tab 变 dirty 则跳过覆盖（保留用户输入 + 清 loading 标志）。冲突 toast 的 Reload 按钮不传该参数（用户显式重载意图，覆盖 dirty 是预期）。新增 2 个测试：在途变 dirty 不被覆盖 / 手动 refresh 覆盖语义不变。

---

### L-1【Low】`refreshTreesForChanges` 注释与实现不符

注释声称 *"The root ("") entry is refreshed whenever the workspace has any cached tree"*，但代码只刷新「变更路径的 parent」且以 treeCache 命中为前提——嵌套路径变更时 root 并不会刷新（测试 `re-fetches the root when a top-level file changes` 通过只是因为顶层文件 parent 恰为 ""）。行为本身合理，注释误导，改注释即可。

> **✅ 2026-08-25 已修复**：注释已改为与实现一致（顶层变更 parent="" → 刷新 root；仅变更路径的已缓存 parent 被刷新）。

### L-2【Low】UI 自身操作的 deleted 回波无抑制

echo 抑制只覆盖 `modified`。用户在 UI 里删除文件（`DELETE /workspaces/file`）→ watcher 回推 `Deleted` → clean tab 若仍开着会弹 `File deleted: x` toast（用户自己刚删的，属噪声）。ADR 未要求抑制 deleted 回波，列为观察项，可在 closeFile 前比对「UI 已知删除」标记。

> **处置：保持观察**。用户实测未报告该噪声问题；引入「UI 已知删除」标记需要跨 store 状态传递，收益/成本比低。留待真实使用反馈后再决定。

### L-3【Low】`_prevAgentStatus` 模块级 Map 永不清理

agent 被移除后残留条目，量级极小，仅记录。修复 H-1 时可顺手处理。

> **✅ 2026-08-25 已修复（顺手）**：`disposeWorkspaceFsListener()` 现在会 `_prevAgentStatus.clear()`——recovery reload / re-init 后陈旧快照不再泄漏到新 listener 生命周期（否则 re-init 后首个 retained online 会把后续真实唤醒误判为「一直是 online」）。

### L-4【Low】`statDiskFile` 用 GET 全量拉取内容做元数据复核

大文件（二进制/大文本）dirty 冲突复核会整文件下载。ADR 已声明 HEAD 变体为后续项，此处仅确认该债务被如实继承而非恶化。

---

## 四、功能完整性对照（W0–W5）

| 项 | ADR 要求 | 实现情况 |
|----|---------|---------|
| W0 workspace 模块新建 | 新建 `workspace/`，不动 `security/fs_watcher.rs`，不收编既有代码 | ✅ 完全一致（`security/fs_watcher.rs` 零改动，diff 可证） |
| W1 proto 字段 38 + 三消息 | oneof 扩展 + `FsChange`/`FsChangeKind`/`WorkspaceFsChangeEvent` | ✅ 完全一致，含「字段号 38 永不复用」登记注释 |
| W1 聚合语义 | 同窗口多写合并 / Created+Modify→Created / Created+Delete 抵消 / rename 降级 | ✅ 全部实现且有对应单测 |
| W2 挂载 + 发布 | watcher_set 去重 + Phase C 钩子 + CRUD 钩子 + QoS1 非 retained | ✅ 完全一致（CRUD 三 handler 均挂 `sync_workspace_watchers`，handler 重构为「先 drop guard 再对账」语义等价，已逐行核对） |
| W3 集成测试 | fake MQTT broker 验证 payload | ⚠️ 偏差：trait sink 注入替代（见 M-1） |
| W4 Tauri 订阅/解包/emit | topic filter QoS1 + chat_mqtt 分支 + emit 独立通道 | ✅ 完全一致；broker host 从 Gateway URL 派生 + 失败回退 localhost，Remote 隧道方案成立 |
| W5 增量刷新 | per-parent-path fetchTree | ✅ 且优于 ADR 伪码：以 treeCache 命中为「可见性」过滤器，未展开目录零请求；不按 selectedAgent 过滤（后台 agent 的已缓存树也刷新），比 ADR 伪码更正确 |
| W5 编辑器冲突 UX | 四路径 + 同域时钟 echo 抑制 + dirty 弹窗前 modified+size 复核 | ✅ 四路径全实现全测试；`lastSavedAtMs` 用 Desktop 本地时钟（严格遵循 ADR 修订后的同域时钟约束） |
| W5 重连/唤醒兜底 | mqtt-status + agent status 双触发 → 全量 sync | ⚠️→✅ 重连触发 ✅（含 2s 去重）；唤醒触发原 ✗（H-1），**已修复闭环（isWakeTransition，见 §七处置表）** |
| 后端 write_file 回填 | ADR 假设 save 响应含 `modified`，实际缺失 | ✅ 如实补齐（`workspace_mutation_impl.rs` RFC3339 + size），Gateway 零改动 |
| `__agent_home__` 排除 | ADR 表格字面只要求 agent_workspaces.json 条目 | ✅ 合理偏差，已在实施报告声明理由（自留地写入噪声） |
| 手工验收 | 断线重连 / 唤醒兜底 / 冲突 toast | ❌ 未做（实施报告已列为待办；H-1 不修则唤醒验收必失败） |

## 五、架构评估（高内聚 / 低耦合 / 开闭）

**结论：三原则全部达标，对现有模块零污染。** 这部分是本次改动最亮眼的地方：

**高内聚 ✅**
- `workspace/fs_watcher.rs` 是纯聚合域逻辑：**零 MQTT 依赖**，通过 `WorkspaceFsEventSink` trait 输出（依赖倒置），测试注入 `CollectSink` 即可闭环。聚合规则（抵消/合并/越界过滤/路径归一化）全部内聚于单文件单结构体。
- MQTT 编码（envelope/topic/QoS）封闭在 `watcher_set.rs::MqttFsEventSink` 一处；前端事件处理封闭在 `lib/workspaceFsEvents.ts` 一处。

**低耦合 ✅**
- **Gateway 零改动**（ADR-009 红线守住，diff 无 gateway 文件）；
- `security/fs_watcher.rs`（audit_log 用途）零触碰，两条 watcher 线并行互不感知；
- `workspaceStore.ts` **零改动**——增量刷新复用其公共 API（fetchTree/treeCache），而非往 store 里塞 listener；
- `chatStore` 对 fs 事件零感知（独立 Tauri 通道 `acowork:workspace-fs-changed`，与 debug-event 同隔离模式）；
- 前端监听器放 `lib/` 而非 store 内 useEffect（偏离 ADR 伪码但更优）：无 React 生命周期纠缠、三处启动点幂等注册（dispose-first + initPromise 防重入）。

**开闭 ✅**
- proto 按规约**扩展** oneof 字段 38（不动既有消息，登记「永不复用」）；
- `chat_mqtt.rs` 在既有 match 上**追加**分支（oneof 扩展的必然触点）；
- CRUD handler 改动是行为保持重构 + 钩子追加，错误路径/response 形状逐行核对无变化；
- 未收编 `http/server.rs`/`usecases`/`workspace_resolver` 既有 workspace 代码（严格遵守 ADR「避免 W0 膨胀为模块重构」的修订）。

**关注点（不阻塞，记录在案）**

- **C-1**：`workspace/watcher_set.rs:29` 依赖 `crate::http::server::SharedMqttClientSlot`——域模块反向依赖 HTTP 层的类型。**非本次新增污染**：`intent_send.rs`、`tools/builtin/mod.rs`、`startup/*` 已有 5+ 处同样引用，本次只是沿袭惯例（该 slot 类型事实上是 crate 级共享设施，只是定义位置不佳）。建议后续独立小重构把 slot 类型下沉到 `mqtt` 模块，与本 ADR 解耦处理。
- **C-2**：`workspaceFsEvents.ts` 的 `statDiskFile` 自行拼 URL + fetch，绕过了 store 层既有的请求封装（fileEditorStore 内部也有同类拼装）——轻度重复，可后续抽 `lib/workspaceApi.ts`。

## 六、测试覆盖评估

**已覆盖（质量好）**：聚合全组合语义（含 Created+Deleted 抵消、Modify+Delete、元数据映射）、越界丢弃、flush 窗口重置、真实 PollWatcher e2e、watcher 去重/对账/排除内置 id、task 存活性、前端「仅刷已缓存父目录/根刷新/clean reload/echo 抑制/dirty 元数据复核跳过/dirty 真冲突 toast/clean 删除关 tab/dirty 删除保留/全量 sync/去重」10 例。

**缺口**（按价值排序）：唤醒转换检测（H-1 直接相关，先提取纯函数再测）> `derive_mqtt_broker_host`/`fs_change_kind_str` 纯函数 > Rust↔TS 字段契约 > quickCreateAndRename 场景（M-2）> MQTT envelope 发布路径（M-1）。

## 七、行动建议（按序）

> **✅ 2026-08-25 处置完毕**（用户已实测确认「打开文件让 LLM 修改，前端实时同步更新」正常）：
>
> | # | 项 | 状态 |
> |---|----|------|
> | 1 | H-1：`isWakeTransition` 纯函数提取 + sleeping→online 修复 + 6 单测 | ✅ 已修复 |
> | 2 | M-1：`derive_mqtt_broker_host` / `fs_change_kind_str` 单测（`chat_mqtt.rs::adr058_tests`）+ 全链路 e2e（`tests/fs_watcher_e2e.rs`） | ✅ 已补齐 |
> | 3 | M-2：quickCreateAndRename 场景测试 | 📋 记入实施报告待办（无组件测试基础设施） |
> | 4 | M-3：`refreshFile` skipIfDirty 防护 + 2 单测 | ✅ 已修复 |
> | 5 | L-1 注释修正 / L-3 `_prevAgentStatus` 清理 | ✅ 已修复（L-2 保持观察） |
> | 6 | 附带：`mqtt/client.rs` / `idle_watcher.rs` LWT 错误注释修正 | ✅ 已修正 |
>
> **回归验证（全绿）**：前端 vitest 185 passed（12 文件）+ `tsc --noEmit` 零错；Rust `clippy -p acowork-runtime --all-targets` 零告警、`fs_watcher_e2e` 1 passed、workspace 单测 12 passed；tauri `cargo test adr058_tests` 2 passed、`clippy --all-targets` 零告警。
>
> **剩余步骤**：按 ADR 6-commit 计划分批提交；手工验收重点补「Desktop 保持连接 + Runtime idle sleep 休眠→唤醒」路径（H-1 修复后的主验证场景）。

---

## 八、评审后修复明细（2026-08-25，供 commit 拆分参考）

| 文件 | 改动 |
|------|------|
| `apps/acowork-desktop/src/lib/workspaceFsEvents.ts` | 新增导出 `AgentStatusSnapshot` + `isWakeTransition`（H-1 核心修复）；listener 改用该函数；L-1 注释修正；dispose 清理 `_prevAgentStatus`（L-3）；clean reload 传 `skipIfDirty`（M-3） |
| `apps/acowork-desktop/src/stores/fileEditorStore.ts` | `refreshFile` 新增 `opts?: { skipIfDirty?: boolean }`（默认行为不变）；skip 分支保留内容并清 loading |
| `apps/acowork-desktop/src/lib/workspaceFsEvents.test.ts` | +8 测试：isWakeTransition 6 例（含 sleeping→online 回归用例）+ skipIfDirty 2 例 |
| `apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs` | 新增 `adr058_tests` 模块：`fs_change_kind_str` / `derive_mqtt_broker_host` 单测（M-1，Rust↔TS 契约防回归） |
| `core/acowork-runtime/src/mqtt/client.rs` | `disconnect` 文档注释修正：干净 DISCONNECT 不触发 LWT（rumqttd 实现依据 + 指向前端 isWakeTransition） |
| `core/acowork-runtime/src/agent/idle_watcher.rs` | 模块注释同上修正（删除「LWT 将替换为 offline」的错误断言） |
| `core/acowork-runtime/tests/fs_watcher_e2e.rs` | （上一轮新增）全链路 e2e |
