# ADR-058 架构评审报告（开发前置审查）

**评审对象**：`docs/adr/zh/ADR-058-workspace-fs-watcher-mqtt-event.md`
**评审方式**：逐条源码实证（非纸面推演）
**日期**：2026-08-25
**结论**：**架构方向正确，修订 4 处事实性问题后即可进入开发。**

---

## 一、断言核查矩阵

ADR 中所有关键链路断言已逐一对照源码验证：

| # | ADR 断言 | 核查结果 | 证据 |
|---|---------|---------|------|
| 1 | 现有 `FsWatcher` 为 PollWatcher 500ms，仅服务 audit_log | ✅ 属实 | `security/fs_watcher.rs:55` `FS_POLL_INTERVAL=500ms`；无任何 MQTT 推送路径 |
| 2 | proto 字段 38 空闲（SessionState=37） | ✅ 属实 | `mqtt_payload.proto:58` `SessionState = 37`；38 未占用；30s 段确为 Session 命名空间 |
| 3 | Gateway 是纯反代，Runtime 是 workspace 权威所有者 | ✅ 属实 | `proxy.rs:107-110` 注释原文吻合 |
| 4 | `GET /workspaces/file` 已返回 `modified` 字段 | ✅ 属实 | `proxy.rs:722-724` 注释 + handler 存在 |
| 5 | Desktop `ALL_TOPIC_FILTERS` 存在于 `mqtt_client.rs:199-230`，clean_session=true | ✅ 属实 | `mqtt_client.rs:199` 起；注释明确 clean_session 语义 |
| 6 | Desktop broker 地址硬编码 `127.0.0.1:19875` | ✅ 属实 | `mqtt_client.rs` `connect_default` |
| 7 | Runtime 有通用 envelope 发布通道 | ✅ 属实 | `mqtt/client.rs:964` `publish_envelope(topic, envelope, qos, retain)` — 新事件类型零新增管道 |
| 8 | Phase C 完成钩子存在 | ✅ 属实 | `startup/subsystems.rs` `phase_c_spawn_subsystems` |
| 9 | `fileEditorStore.saveFile` 成功分支不解析响应 JSON；`openFile` 只解 `{content,size,mimeType}` | ✅ 属实 | `fileEditorStore.ts:465-471` / `:254` |
| 10 | Runtime 侧 workspace CRUD 存在、可挂钩子 | ✅ 属实 | `http/server.rs` + `usecases/workspace_mutation_impl.rs` + `tools/workspace_resolver.rs` |
| 11 | **"既有 `MQTT_CONNECTED → invalidateTreeCache + fetchTree("")` 重连兜底"** | ❌ **不成立** | 见问题 P1 |
| 12 | **"Runtime 重启后 `ready=true` 重发触发 Desktop 兜底"** | ❌ **不成立** | 见问题 P2 |
| 13 | idle sleep 进程语义是"开放问题" | ⚠️ 已有明确答案 | 见问题 P4 |
| 14 | ADR 文件清单写 `workspace/mod.rs` "注册新模块" | ⚠️ 表述误导 | 见问题 P3 |

## 二、必须修订的问题

### P1（高）：重连全量 sync 兜底机制**不存在**，不是"既有机制"

ADR 在 §2.1、§3.4、风险表中三次声称该兜底"既有、不新增机制"。实证结果：

- `invalidateTreeCache` 全前端唯一调用点是 `WorkspaceExplorer.tsx:470`（手动 refresh 按钮）
- 前端不存在任何 `MQTT_CONNECTED` 处理；实际事件是 `mqtt-status`（`chatStore.ts:766`），其 `connected:true` 分支**只更新 chatStore 状态**，不触发任何 workspace 同步

**影响**：断连重连兜底是本方案数据一致性的关键路径（QoS1 + clean_session=true + 非 retained = 断线期间事件必然丢失），它必须作为**新开发项**列入 W5，而非引用现有设施。工作量估算需调整。

**修订建议**：W5 增加"监听 `mqtt-status` connected:true → invalidateTreeCache + fetchTree('')"；§3.4 和风险表措辞改为"需新增"。

### P2（高）：Desktop 未订阅 `agents/+/ready` 主题

ADR 声称 Runtime 重启/idle sleep 后靠 `ready=true` 重发触发兜底。实证：`ALL_TOPIC_FILTERS` 中无 `ready` 条目，Desktop 全代码库无该主题订阅。Runtime 侧确实发布（`client.rs:1003`，retained）。

**修订建议**：二选一，写入 W4：
- a) `ALL_TOPIC_FILTERS` 增加 `("acowork/agents/+/ready", AtLeastOnce)`，并在 chat_mqtt.rs 分发中触发 workspace 兜底；
- b) 复用已订阅且 retained 的 `agents/+/status`（`online` 转换）触发兜底 —— 推荐，零新增订阅。

### P3（中）：`core/acowork-runtime/src/workspace/` 模块不存在

ADR 文件清单的表述（"workspace/mod.rs 注册新模块"）暗示模块已存在。实际 runtime 的 workspace 逻辑分散在 `http/server.rs`、`usecases/workspace_mutation*.rs`、`tools/workspace_resolver.rs` 三处。

**影响**：W0 实际是"从零新建 workspace 模块"而非"在既有模块中注册"。更重要的是 **watcher 启停钩子（workspace CRUD 后 ensure/stop_watcher）的真实挂载点是 `http/server.rs` 的 CRUD handler**，ADR 未指明。建议 ADR 明确：新模块 `workspace/` 仅收编 watcher（+watcher_set），CRUD 钩子留在现有 handler 中调用，**不要**借机重构 workspace 三处既有代码（避免 W0 膨胀）。

### P4（中）：idle sleep 语义已有源码答案，且比 ADR 假设的更严重

ADR 把"进程存活 vs 退出"列为开放问题。实证（`idle_watcher.rs:48-49, 448-461`）：

```text
On expiry: publish "sleeping" → RuntimeMqttClient::disconnect → process::exit(0)
```

**idle sleep = 进程退出**。即：agent 自动休眠期间 watcher 必然停摆、事件必然丢失。这不是边缘场景——idle sleep 是常规路径。这使 P1/P2 的兜底从"保险"升级为**主路径的一部分**：唤醒 → 兜底全量 sync 必须可靠工作，否则"外部变更感知"在每次休眠后都静默失效。建议 ADR 删除"开放问题"措辞，直接写入已确认语义，并把"唤醒后兜底"列为 W5 验收项。

## 三、设计建议（不阻塞开发）

1. **echo 抑制的时钟偏移风险（Remote 模式）**：`change.timestamp_ms`（Runtime 机器 wall clock）与 `file.lastSavedAtMs`（Desktop `Date.now()`）跨机器比较，偏移 >1.5s 时抑制失效或误伤。建议抑制判断改为"事件到达时间 - 本地 save 时刻"（同一时钟域），或直接对比 `diskModified`。
2. **`touch`/chmod 误报**：`FsChangeKind::Modified` 未区分内容变化与纯 mtime 变化。dirty 文件收到 Modified 事件即弹 toast，一次 `touch` 就会误报。建议 dirty 分支先 re-GET metadata 比对 `modified`+`size` 再决定是否弹（VSCode 同款思路，成本低）。
3. **W4 的真实改动位置**：Desktop 的 topic 分发/解包/emit 在 `src-tauri/src/commands/chat_mqtt.rs`（非 `mqtt_client.rs` 的 on_message 内联），ADR 的文件清单建议修正。
4. §3.2 伪代码 `flush_window` 后未重置 `window_started`（示意代码瑕疵，实现时注意）。

## 四、总体评估

**架构判断全部成立**：watcher 放 Runtime（ADR-009/048 合规）、主题 Owner 一致（mqtt.md §3.2）、DataEnvelope 扩展 oneof 字段 38、500ms 聚合、rename 降级、per-parent 增量刷新、QoS1 + 非 retained + 兜底——这套组合与既有基础设施的贴合度经源码验证是真实的，`publish_envelope` 的存在使 Runtime 侧改动确实很小。

**开发就绪度**：修订 P1–P4（约半天文档工作量，W4/W5 范围各加一条）后即可按 W0–W5 推进。W0–W3（Runtime 侧）可立即开始，不受 P1/P2 影响；W4/W5 开工前应先落实修订。

| 修订项 | 归属 | 阻塞范围 |
|--------|------|---------|
| P1 重连兜底改为新开发项 | W5 + §3.4 + 风险表 | W5 |
| P2 ready/status 兜底触发 | W4 | W4/W5 |
| P3 模块表述 + 钩子挂载点 | §A.4 | W0 |
| P4 idle sleep 语义写实 | §2.1 | 无（认知修正） |
| 建议 1/2（时钟偏移、touch 误报） | §3.3 | 无（可实施中处理） |
