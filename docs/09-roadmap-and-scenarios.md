# 实现路线图与使用场景

> 版本：v3.0 | 更新日期：2026-04-09

---

## 1. 实现路线图

### Phase 1: 基础框架 + LLM 交互（MVP）

- 定义 manifest v1 规范，实现 ZIP 解析。
- 实现 .agent 包签名机制：密钥对生成（`rollball-keygen`）、签名（`rollball-sign`）、验证（`rollball-verify`）。
- 实现 Agent Runtime 核心：加载 .agent 包、组装 prompt、LLM 主循环、内置工具（memory, http, shell）。
- Gateway 基础功能：安装（含签名验证）、卸载、启动/停止进程、Unix Socket 通信。
- Key Vault 基础功能：加密存储、一次性分发。
- 实现一个示例 Agent（天气查询，能调 LLM + 用工具）。
- 本地目录隔离（不使用命名空间，仅 `--work-dir`）。

### Phase 2: Memory 分层 + 系统 Agent

- Agent Runtime 内嵌 Grafeo：私有 Memory 初始化、情景记忆写入/检索、语义记忆图操作。
- 系统 Agent 实现：身份管理 ContentProvider、默认交互入口、身份提报接收与 LLM 判断、observe 通知机制。
- 冷启动身份注入：Gateway 启动 Agent 前向系统 Agent 查询 identity_deps 并注入。
- Embedding 生成：集成 ONNX Runtime，本地向量生成。
- 离线工作：所有 Memory 操作本地完成，不依赖网络。

### Phase 3: 权限与沙箱

- 集成 bubblewrap（Linux）实现文件系统隔离。
- 实现权限声明和用户授权对话框（CLI 或简单 GUI）。
- 运行时权限请求机制。
- 资源限制（cgroups 或 rlimit）。
- WASM 工具沙箱（Wasmtime 集成）。
- Prompt Guard 和 approval 机制。

### Phase 4: 通信与协调

- Intent 跨 Agent 消息转发 + Capability Registry。
- Budget Tracker（用量上报 + 超限信号 + 本地预检）。
- Rate Limiter（速率令牌分配）。
- 定时触发器（cron 解析）。

### Phase 5: 云端与生态

- Memory Sync Service（云端增量同步、冲突解决）。
- 远程仓库支持（添加仓库、更新、自动下载）。
- 图形化管理界面（Tauri 或 egui）。
- Agent 商店原型。

### Phase 6: 跨平台适配

- Windows 适配：Named Pipe 传输、Job Object 隔离、Windows 路径规范。
- macOS 适配：App Sandbox 隔离、macOS 路径规范。
- 移动端适配（Android/iOS）：SingleProcess 运行模式、Local TCP 传输、wasmi WASM 引擎、移动端路径规范。
- 注意：.agent 包格式和 Gateway Service API 合同无需修改，适配仅在实现层。

## 2. 使用场景示例

**场景**：用户安装"天气 Agent"和"日历 Agent"。每天早上 7 点，天气 Agent 自动获取当地天气，并通过 Intent 调用日历 Agent 创建提醒"今天带伞"。

**流程：**

1. Gateway 的 cron 触发器 spawn 天气 Agent 的 Agent Runtime 进程（若未运行）。启动前，Gateway 先向系统 Agent 查询 identity_deps，将用户身份信息注入启动参数。
2. Agent Runtime 加载天气 Agent 的 .agent 包，从 Vault 获取 API Key，连接 Gateway Socket。
3. 天气 Agent 从私有 Grafeo 读取用户上次保存的城市（情景记忆），调 LLM 规划查询天气。
4. LLM 返回 tool_call: `http_get("https://api.weather.com/...?city=Beijing")`，权限校验通过，执行。
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
