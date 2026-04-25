# 实现路线图与使用场景

> 版本：v3.1 | 更新日期：2026-04-14

---

## 1. 实现路线图

### Phase 1: 基础框架 + LLM 交互（MVP）

- 定义 manifest v1 规范，实现 ZIP 解析。
- 实现 .agent 包签名机制：密钥对生成（`rollball-keygen`）、签名（`rollball-sign`）、验证（`rollball-verify`）。
- 实现 Agent Runtime 核心：加载 .agent 包、组装 prompt、LLM 主循环（含 Preemptive Trim、Streaming + tool_calls 状态机）、内置工具（memory, http, shell）。
- History Manager：对话历史 FIFO 裁剪、动态 N 计算、Tool Result 折叠（规则引擎版 HistoryPruner，保留最近 4 轮完整 tool result）。
- Loop Controller：循环检测三种模式（Exact Repeat / Ping-Pong / No Progress）+ 三级渐进响应（Warning → Block → Break）。
- Tool Call 单轮去重（HashSet）。
- Budget Manager：本地预算预检 + 上下文 token 预估校验。
- Rate Limit 分层：区分可重试限流 / 不可重试余额不足，解析 retry_after，不计入 LLM 重试次数。
- Context Exceeded Reactive Recovery：Tool Result 折叠 → Emergency History Trim 渐进式恢复。
- Gateway 基础功能：安装（含签名验证）、卸载、启动/停止进程、Socket 通信。
- Key Vault 基础功能：加密存储、一次性分发。
- Gateway CLI 二进制：命令行管理 Agent（install/uninstall/start/stop/list）。
- 实现一个示例 Agent（天气查询，能调 LLM + 用工具）。
- 本地目录隔离（不使用命名空间，仅 `--work-dir`）。

### Phase 2: Memory 分层 + 系统 Agent

- Agent Runtime 内嵌 Grafeo：三层五类仿生分层（瞬态/经历/沉淀）、情景记忆写入/检索、语义记忆图操作、乘法衰减遗忘机制（decay_score = importance × activity_signal）、Dormant/Purge 差异化策略。
- 即时提取 Tool Call 机制：memory_store 工具加入内置列表，LLM 自主判断是否调用，支持 Fact/Preference/Relation/Procedural/Autobiographical 五种类型，带 importance 和 privacy 参数。
- 关联扩散检索：hybrid_search（HNSW + BM25）+ graph_expand（1-2 跳），经历层↔沉淀层通过 source_episode 反向查询建立跨层关联。
- AutobiographicalNode：从 manifest.toml 自动派生 Identity/Capability，注入 System Prompt 头部（上限 200 token），History 超过 10 条自动摘要压缩。
- Episode 内容分类压缩：信息性内容原样存储，工件性内容（代码/文件/命令输出）压缩为摘要 + ArtifactRef 引用，零 LLM 调用，纯 Runtime 确定性逻辑。
- PrivacyLevel（Public/Personal/Sensitive）按 Zone 强制执行。
- 系统 Agent 实现：身份管理 ContentProvider、默认交互入口、身份提报接收与 LLM 判断、observe 通知机制。
- 冷启动身份注入：Gateway 启动 Agent 前向系统 Agent 查询 identity_deps 并注入。
- Embedding 生成：集成 ONNX Runtime，本地向量生成。
- 离线工作：所有 Memory 操作本地完成，不依赖网络。

### Phase 3: 权限与工具安全

- 实现权限声明和用户授权对话框（CLI 或简单 GUI）。
- 运行时权限请求机制。
- WASM 工具沙箱（Wasmtime 集成 + WIT 组件模型）。
- Prompt Guard 和 approval 机制（Approval Gate：高风险工具需用户确认，Gateway → Desktop App 确认流程）。
- Shell 命令风险分级 + 文件来源追踪（FileProvenance）+ 审计日志。
- 离线巩固（三元组提取 + 经验泛化 + 记忆质量评估）。

> **2026-04-25 调整（ADR-007）**：进程级沙箱（bubblewrap / AppContainer / Seatbelt）延后至 Phase 7。Phase 3 聚焦应用层安全防线。

### Phase 4: 通信与协调

- Intent 跨 Agent 消息转发 + Capability Registry。
- Budget Tracker（用量上报 + 超限信号 + 本地预检）。
- Rate Limiter（速率令牌分配）。
- 定时触发器（cron 解析）。

### Phase 5: Desktop App + 开发框架

Desktop App 和开发调试能力在同一阶段交付，因为它们共享 Debug Protocol 基础设施。

#### 5.1: Desktop App 基础（用户模式）

- Gateway HTTP API：Axum HTTP Server，供 Desktop App 和 CLI 使用。
- Tauri v2 Desktop App 骨架：Rust backend + React frontend。
- 系统托盘：Gateway 状态指示、快捷操作。
- Agent 管理界面：安装、卸载、启停、Agent 列表。
- 对话界面：消息收发、流式输出、工具调用展示。
- 设置页面：Gateway 连接配置、Provider 管理、Vault API Key 管理。
- 首次启动引导流程。

#### 5.2: 开发框架（Debug Protocol + 开发者模式）

- Agent Runtime DevMode：`--dev-mode` 启动参数、Debug Protocol Server（WebSocket）。
- Debug Protocol 实现：执行控制（Pause/Step/Resume）、状态查询、断点系统。
- 消息快照与回滚机制。
- Agent 克隆 API（Gateway HTTP API 侧）。
- 从零创建 Agent 向导。
- Desktop App 开发者模式：调试面板、单步执行详情、断点管理。

#### 5.3: 开发框架高级

- 消息编辑与重执行。
- Skill 热加载 + Desktop App Skill 编辑器。
- Provider 动态切换（调试面板内）。
- Manifest 编辑器。
- 录制回放引擎（JSONL 格式）、自动/手动回放。

#### 5.4: 发布工具链

- 发布向导（Desktop App 内）：检查 → 清理 → 打包 → 签名 → 分发。
- Gateway 发布 API（prepare / build / install-locally / export）。

### Phase 6: 云端与生态

- Memory Sync Service（云端增量同步、冲突解决）。
- 远程仓库支持（添加仓库、更新、自动下载）。
- Agent 商店原型。
- 付费 Agent 许可证验证。

### Phase 7: 跨平台适配 + 进程级沙箱

- 进程级沙箱：Linux（bubblewrap + seccomp-bpf）、Windows（AppContainer + Job Objects）、macOS（Seatbelt）。
- Windows 适配：Named Pipe 传输、Windows 路径规范。
- macOS 适配：App Sandbox 隔离、macOS 路径规范。
- 移动端适配（Android/iOS）：SingleProcess 运行模式、Local TCP 传输、wasmi WASM 引擎、移动端路径规范。
- 注意：.agent 包格式和 Gateway Service API 合同无需修改，适配仅在实现层。

## 2. 使用场景示例

**场景**：用户安装"天气 Agent"和"日历 Agent"。每天早上 7 点，天气 Agent 自动获取当地天气，并通过 Intent 调用日历 Agent 创建提醒"今天带伞"。

**流程：**

1. Gateway 的 cron 触发器 spawn 天气 Agent 的 Agent Runtime 进程（若未运行）。启动前，Gateway 先向系统 Agent 查询 identity_deps，将用户身份信息注入启动参数。
2. Agent Runtime 加载天气 Agent 的 .agent 包，从 Vault 获取 API Key，连接 Gateway Socket。
3. 天气 Agent 从私有 Grafeo 读取用户上次保存的城市（情景记忆），调 LLM 规划查询天气。
4. LLM 返回 tool_call: `http_request({"method":"GET","url":"https://api.weather.com/...?city=Beijing"})`, 权限校验通过，执行。
5. 天气 Agent 将结果写入私有 Grafeo（情景 + 语义记忆）。
6. 天气 Agent 通过 Gateway 发送 Intent：

```json
{"type":"intent", "target":"com.example.calendar", "action":"create_event", "params":{"summary":"带伞","time":"07:30"}}
```

7. Gateway 查找日历 Agent，若未运行则 spawn（同样先注入身份信息），转发 Intent。
8. 日历 Agent 的 Agent Runtime 加载包、接收 Intent、调 LLM 处理。
9. 日历 Agent 调用本地日历 API 创建事件，返回成功。
10. Gateway 将响应返回给天气 Agent（可选），天气 Agent 空闲超时后被杀死。

**补充场景：身份信息更新**

用户对天气 Agent 说"我搬到上海了"：
1. 天气 Agent 从对话中识别到居住地变更，向系统 Agent 发送身份更新 Intent：`{"action": "identity:update", "params": {"updates": {"city": "Shanghai"}, "evidence": "用户说我搬到上海了", "confidence": 0.9}}`
2. 系统 Agent 的 LLM 二次判断确认"搬家"语义，更新私有 Grafeo 中的用户城市。
3. 系统 Agent 通知所有订阅了 city 变更的 Agent（如日历 Agent），日历 Agent 更新本地缓存。
