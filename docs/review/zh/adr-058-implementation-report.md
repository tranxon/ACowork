# ADR-058 实施报告：Workspace FS Watcher → MQTT → Desktop 自动刷新

**日期**：2026-08-25
**状态**：W0–W5 全部完成，全链路编译/测试/clippy 通过（未提交，待确认后按 6-commit 计划分批提交）
**ADR**：`docs/adr/zh/ADR-058-workspace-fs-watcher-mqtt-event.md`

---

## 交付清单

### W0+W1 — Runtime workspace 模块 + proto 契约

- **新建** `core/acowork-runtime/src/workspace/mod.rs` — 模块声明（既有 `security/fs_watcher.rs` 原样保留服务 audit_log）
- **新建** `core/acowork-runtime/src/workspace/fs_watcher.rs`（~450 行含测试）
  - `WorkspaceFsWatcher`：notify::PollWatcher 500ms 轮询 + 500ms 聚合窗口
  - 合并语义：同窗口多写合并 Modified；Created→Modified 保持 Created；Created→Deleted 抵消；rename 降级为 Delete+Create（无 inode 配对）
  - 越界过滤：`strip_prefix` 失败即丢弃，绝不泄露绝对路径；rel path 归一化 forward-slash
  - `WorkspaceFsEventSink` trait（解耦 MQTT，测试可注入 CollectSink）
  - 退出前 final flush，窗口内事件不丢
- **修改** `core/acowork-core/proto/mqtt_payload.proto`
  - `DataEnvelope.payload` oneof 新增字段 **38**：`WorkspaceFsChangeEvent workspace_fs_change_event`
  - 新增 `WorkspaceFsChangeEvent` / `FsChange` / `FsChangeKind`（proto3，注释说明字段号 38 永不复用）

### W2 — 挂载 + MQTT 发布

- **新建** `core/acowork-runtime/src/workspace/watcher_set.rs`
  - `WorkspaceWatcherSet`：`HashMap<workspace_id, WatcherHandle>` 单例去重；路径变化重启；优雅关闭（shutdown 信号 → final flush → 退出，abort 兜底）；`Drop` 停止全部
  - `MqttFsEventSink`：复用 `mqtt_client_slot`（与 HTTP server 同一 late-bind slot），`publish_envelope` QoS 1 / 非 retained，topic `acowork/agents/{id}/workspaces/{wid}/fs-changed`
  - `sync_from_resolver`：与 resolver 全量对账（仅 watch `agent_workspaces.json` 条目；`__agent_home__`/`__package_root__` 排除——前者是 runtime 自留地噪声源，后者是前者父目录会产生重复事件）
- **修改** `startup/subsystems.rs` — Phase C 完成后 `sync_from_resolver` 启动全部 watcher
- **修改** `startup/context.rs` / `startup/agent_init.rs` — `AgentBootContext.workspace_watcher_set` 新字段；watcher set 由 `RuntimeHttpServer::start` 内部创建（暴露 `pub workspace_watchers` 字段），boot ctx 与 HttpState 共享同一 Arc，避免触碰 ~25 个测试调用点
- **修改** `http/server.rs` — `HttpState.workspace_watchers`；`create/update/delete_workspace` 三个 CRUD handler 成功路径挂 `sync_workspace_watchers`（handler 重构为先 drop MutexGuard 再对账）

### W3 — 集成测试（Rust 侧 12 个，全部通过）

- fs_watcher 单测 9 个：rel path 归一化、越界丢弃、Create/Modify/Remove 映射、Created+Modified 合并、Created+Deleted 抵消、Modify+Deleted=Deleted、flush 批次与窗口重置、真实 PollWatcher 端到端（temp dir create/modify/delete → 聚合事件）
- watcher_set 测试 3 个：按 id+root 去重、sync 对账（增/删/换路径/排除内置 id）、watcher task 跨 sync 周期存活

### W4 — Desktop Tauri 订阅 + 解包 + emit

- **修改** `mqtt_client.rs` — `ALL_TOPIC_FILTERS` 新增 `acowork/agents/+/workspaces/+/fs-changed`（QoS 1，与 messages/# 同理由）
- **修改** `commands/chat_mqtt.rs`
  - `connect_mqtt`：broker host 从 Gateway base_url host 派生（Remote 隧道场景），port 用 `GATEWAY_MQTT_PORT` 常量；`derive_mqtt_broker_host` 解析失败回退 localhost
  - 新增 `WorkspaceFsChangeEvent` 分支：decode → `fs_change_kind_str`（i32→"created"/"modified"/"deleted"）→ emit `acowork:workspace-fs-changed`（独立通道，与 debug-event 同隔离模式）

### W5 — 前端 store + 重连/唤醒兜底

- **新建** `src/lib/workspaceFsEvents.ts`（核心，~350 行）
  - **增量刷新**：按 parent-path 分组，仅对 `treeCache` 中已存在的目录 re-fetch（未展开目录不浪费请求，保留展开状态）
  - **编辑器冲突 UX（VSCode 同款）**：
    - clean + modified → 静默 `refreshFile`
    - dirty + modified → 先 stat 磁盘 `modified`+`size` 复核（防 touch/chmod 误报，纯元数据变化静默采纳新基线）→ 真实变更弹 toast（Reload action；关闭 toast = Keep mine）+ `diskConflict='modified'` 标记
    - clean + deleted → 关 tab + toast
    - dirty + deleted → `diskConflict='deleted'` + toast（tab 保留）
  - **echo 抑制**：`lastSavedAtMs`（Desktop 本地时钟）vs `Date.now()`，窗口 1500ms；明确不用 Runtime 侧 `timestamp_ms`（Remote 跨机时钟偏移）
  - **重连/唤醒兜底（W5 验收项）**：`mqtt-status connected:true` + agent status 非 online→online 转换 双触发 → `invalidateTreeCache` + 全部已缓存 workspace root 重新 fetchTree("")；2s 去重防风暴
- **修改** `src/stores/fileEditorStore.ts` — `OpenFile` 新增 `diskModified`/`diskSize`/`lastSavedAtMs`/`diskDeleted`/`diskConflict`；`openFile`/`openPreview`/`refreshFile` 三处解析 `modified`+`size` 回填；`saveFile` 解析响应回填 + `lastSavedAtMs` + 清冲突标记；新增 `clearDiskConflict`
- **修改** `src/stores/workspaceStore.ts` — 无需改动（增量刷新通过既有 `fetchTree`/`treeCache` 实现，去重由 in-flight 集合天然保证）
- **修改** `App.tsx` + `SplashScreen.tsx`（3 处启动点）— 注册 `initWorkspaceFsListener`
- **新建** `src/lib/workspaceFsEvents.test.ts` — 10 个测试全部通过：仅刷新已缓存父目录、根目录刷新、clean 静默 reload、echo 抑制、dirty 元数据复核跳过、dirty 真实冲突弹 toast（Reload）、clean 删除关 tab、dirty 删除保留标记、全量 sync 失效+根重取、2s 去重

### 后端小补丁（支撑 ADR §3.3 契约）

- `usecases/workspace_mutation_impl.rs` `write_file` 响应新增 `modified`（RFC3339）+ `size` — ADR 假设 save 响应携带 `modified`，实际原本没有；补齐使 `saveFile` 回填 `diskModified` 成立。Gateway 零改动（纯透传）。

---

## 验证结果

| 检查 | 结果 |
|------|------|
| `cargo build -p acowork-core -p acowork-runtime -p acowork-gateway` | ✅ 通过 |
| `cargo clippy`（三个 crate，`-D warnings`） | ✅ 零告警 |
| `cargo test -p acowork-runtime` | ✅ 1062 passed（含新增 12） |
| `cargo test -p acowork-core` | ✅ 149 passed |
| Desktop tauri `cargo clippy --all-targets` | ✅（1 个 items_after_test_module 告警为预先存在，stash 验证） |
| 前端 `tsc --noEmit` | ✅ 零错误 |
| 前端 `vitest run` | ✅ 177 passed（12 文件，含新增 10） |

**预先存在的环境问题（与本次无关，stash 后复现）**：
- `acowork-embed` 链接失败（ort-sys 找不到 libonnxruntime，本机未配置 ONNX Runtime）
- `acowork-gateway` 单测 `lifecycle::process::tests::test_check_health_current_process` 失败（macOS 进程健康检查环境问题）

## 与 ADR 的偏差说明

1. **watcher 范围**：仅 watch `agent_workspaces.json` 条目，不含 `__agent_home__`（ADR 表格字面只要求 agent_workspaces.json；agent home 是 logs/conversations/memory 的持续写入源，watch 会造成事件噪声）。副作用：agent home 内的外部变更不推送（手动 refresh 仍可用）。
2. **W3 测试形态**：用 trait sink 注入替代 fake MQTT broker 验证 payload（MQTT slot 为空时 sink 丢弃，链路语义由单测 + e2e watcher 测试覆盖）。
3. **watcher set 传递方式**：ADR 建议 `RuntimeHttpServer::start` 新增参数；实际改为 start 内部创建 + 返回值暴露，避免修改 ~25 个测试调用点（该 codebase 已有"不动测试调用点"的先例注释）。
4. **write_file 响应**：补了 `modified`+`size`（ADR 假设已有，实际缺失）。

## 待办 / 后续

- [ ] 按 ADR 6-commit 计划分批提交（用户确认后执行）
- [ ] 手工验收：Runtime 启动 → 外部 `touch`/CLI 写文件 → FileTree 自动刷新；编辑器 tab 外部修改冲突 toast；断线重连全量 sync；idle sleep 唤醒兜底（**部分完成**：LLM 修改文件 → 打开的 tab 实时同步，用户 2026-08-25 实测通过；断连/唤醒路径待验收）
- [ ] `diskConflict` 的 tab 角标 UI 渲染（schema 已就位，渲染属 UI 层后续工作）
- [ ] `quickCreateAndRename` 与 watcher Created 事件回推的并发场景测试（ADR 风险表要求；项目暂无组件测试基础设施，引入后补 —— 评审报告 M-2）

## 评审后修复（2026-08-25，详见 `adr-058-code-review.md` §八）

- **H-1**：`workspaceFsEvents.ts` 提取 `isWakeTransition` 纯函数并修复 sleeping→online 唤醒转换漏检（+6 单测）——修复前「Desktop 全程连接 + Runtime 休眠唤醒」无兜底
- **M-3**：`refreshFile` 增 `skipIfDirty` 可选参数，fs 事件触发的静默 reload 不再覆盖在途用户输入（+2 单测）
- **M-1**：新增 `tests/fs_watcher_e2e.rs` 全链路 e2e（真实 broker）+ `chat_mqtt.rs::adr058_tests` 纯函数单测
- **L-1/L-3**：注释修正 + `_prevAgentStatus` dispose 清理；`mqtt/client.rs` / `idle_watcher.rs` LWT 错误注释修正
- 回归验证全绿：前端 185 tests + tsc 零错；Rust clippy 零告警 + e2e/单测通过
