# Rollball Phase 1 开发计划

> 版本：v1.0 | 更新日期：2026-04-17
>
> 本计划基于 `docs/09-roadmap-and-scenarios.md` 和 `docs/00-PRD.md` v1.5，涵盖 Phase 1 所有任务的分解、排期和进度追踪。

---

## 1. 概述

### 1.1 Phase 1 目标

交付 **MVP（最小可行产品）**：一个完整的 Agent 执行链路——从 `.agent` 包安装、签名验证、Runtime 启动、LLM 对话、工具调用，到对话结束。

**核心交付物**：Linux 桌面端可运行 Demo，展示 Agent 通过 LLM + 工具完成天气查询任务。

### 1.2 阶段划分

| 阶段 | 名称 | 核心目标 |
|------|------|---------|
| **S1** | 基础层 | workspace 骨架 + rollball-core + rollball-sign + rollball-vault |
| **S2** | Runtime 核心 | Agent Runtime 主循环、内置工具、History/Loop/Budget |
| **S3** | Gateway | 包管理、生命周期、IPC、CLI |
| **S4** | 集成验证 | 示例 Agent、端到端测试 |

### 1.3 依赖关系图

```
rollball-core (S1)
    │
    ├── rollball-sign (S1) ←─┐
    │                        │
    ├── rollball-vault (S1) ←┘
    │
    ├── rollball-runtime (S2) ←─── rollball-sign + rollball-vault
    │
    └── rollball-gateway (S3) ←─── rollball-core + rollball-sign + rollball-vault
                                      │
                                      └── rollball-runtime (S2)
```

**原则**：S1 先行；S2 依赖 S1；S3 并行于 S2 但 Gateway 测试需等 Runtime 可运行；S4 收尾验证。

---

## 2. S1：基础层

### S1.1 任务：Workspace 骨架

**目标**：建立 7-crate workspace 结构，CI 就绪，每 crate 可独立编译。

| 任务 | 负责模块 | 验收标准 |
|------|---------|---------|
| S1.1.1 创建 workspace `Cargo.toml`，定义成员和 shared dependencies | - | `cargo check --all` 无报错 |
| S1.1.2 创建 7 个 crate 骨架目录 + `Cargo.toml` | - | 每个 crate 有 `lib.rs` / `main.rs` |
| S1.1.3 统一 `clippy` 和 `rustfmt` 配置（`.cargo/config.toml`） | - | CI lint 通过 |
| S1.1.4 编写 `dev/ci.sh`（`cargo check` + `cargo clippy --all-targets -- -D warnings`） | - | 脚本可执行 |

**说明**：7 个 crate 为 `rollball-core`、`rollball-memory`、`rollball-runtime`、`rollball-gateway`、`rollball-grafeo`、`rollball-vault`、`rollball-sign`。Phase 1 暂不深入 `rollball-memory` 和 `rollball-grafeo`（Phase 2），但目录结构先建立。

---

### S1.2 任务：rollball-core

**目标**：定义所有共享类型、协议消息、trait 接口。零重型依赖，只做类型定义。

#### S1.2.1 Manifest 类型 (`manifest.rs`)

- `AgentManifest` 结构（id, version, name, permissions, llm, tools, capabilities 等）
- `LlmConfig`、`ToolDeclaration`、`Permission` 等子类型
- TOML 解析测试（用 sample .agent manifest fixture）
- `Serialize`/`Deserialize` 测试

#### S1.2.2 协议消息 (`protocol.rs`)

- `GatewayRequest` 枚举（KeyRelease / IntentSend / BudgetQuery / UsageReport / RateAcquire / PermissionRequest）
- `GatewayResponse` 枚举（对应响应类型）
- `Frame` 传输帧结构（body_len + msg_type + body）
- JSON-RPC 兼容格式测试

#### S1.2.3 工具 Trait (`tools/traits.rs`)

- `Tool` trait 定义（`name()` / `spec()` / `execute()`）
- `ToolSpec`（name, description, input_schema JSON Schema）
- `ToolResult`（ok/err 结构，含 token 统计）
- JSON Schema 清洗逻辑（借鉴 ZeroClaw `schema.rs`）

#### S1.2.4 Provider Trait (`providers/traits.rs`)

- `ChatMessage`（role, content, name, tool_calls）
- `ChatRequest` / `ChatResponse` / `StreamEvent` 类型
- `Provider` trait（`chat()` / `chat_stream()` / `chat_token_count()`）
- 错误类型 `ProviderError`

#### S1.2.5 其他类型

- `Intent` 结构（target, action, params）
- `Permission` 枚举（Network / FilesystemRead / FilesystemWrite / Shell 等）
- `Budget` / `UsageReport` 类型
- `Identity` 结构
- 统一 `Error` 类型（`thiserror`）

**验收标准**：所有类型可被 `cargo check` 编译，单元测试覆盖所有 trait 方法。

---

### S1.3 任务：rollball-sign

**目标**：实现 `.agent` 包签名机制，提供 `rollball-keygen`、`rollball-sign`、`rollball-verify` 三个 CLI 命令。

#### S1.3.1 签名块数据结构 (`signing_block.rs`)

- `SigningBlock` / `Signer` / `Certificate` 结构
- 二进制序列化格式（CD 前插入 Signing Block）
- `SigningBlock::to_bytes()` / `from_bytes()` 测试

#### S1.3.2 密钥生成 (`keygen.rs`)

- Ed25519 密钥对生成
- X.509 自签名证书生成（Platform + Developer 两类）
- 输出到 `~/.config/agent-gateway/keys/` 目录
- CLI：`rollball-keygen --type developer --output-dir <path>` / `--type platform`

#### S1.3.3 签名 (`sign.rs`)

- 读取 .agent ZIP，计算各 section SHA-256 摘要
- 构建 Signing Block 并插入 ZIP（CD 之前）
- 输出签名后的 .agent 包
- CLI：`rollball-sign --input <pkg.agent> --key <keyfile> --output <signed.agent>`

#### S1.3.4 验签 (`verify.rs`)

- 从 ZIP 提取 Signing Block
- 校验各 section 摘要完整性
- 验证签名（Ed25519）
- CLI：`rollball-verify <pkg.agent>` → 输出 signer 信息或错误

#### S1.3.5 证书链验证 (`certificate.rs`)

- Developer 自签名验证
- Platform 证书链验证（Gateway 内置根证书）
- Phase 1 简化：仅 Developer 自签名 + Platform 根证书两种路径

**验收标准**：`rollball-sign sign + rollball-verify verify` 往返成功；签名前后 ZIP 内容一致（Signing Block 插入后不影响已有文件）；CLI 帮助信息完整。

---

### S1.4 任务：rollball-vault

**目标**：加密存储 LLM API Key，提供一次性分发接口。

#### S1.4.1 Vault 主结构 (`vault.rs`)

- Vault 目录结构（`~/.config/agent-gateway/vault/`）
- 密码派生主密钥（Argon2id → ChaCha20-Poly1305）
- `store(provider, key_name, api_key)` — 加密写入
- `retrieve(provider, key_name)` — 解密返回 `SecretString`
- `list(provider)` — 列出所有 key_name（不返回值）

#### S1.4.2 加密层 (`encryption.rs`)

- ChaCha20-Poly1305 AEAD 加密
- Nonce 生成（CSPRNG）
- 加密文件格式：`nonce (12B) + ciphertext + tag (16B)`

#### S1.4.3 Key 分发 (`distributor.rs`)

- 通过 IPC 一次性分发（`KeyRelease` 消息）
- 不暴露在环境变量或命令行参数

**验收标准**：密码解锁后 `store` + `retrieve` 往返成功；无密码/错误密码无法解密；Vault 目录权限正确（600）。

---

## 3. S2：Runtime 核心

### S3.1 任务：Agent Runtime CLI + 配置

**目标**：Runtime 可独立运行，接受 CLI 参数。

| 任务 | 验收标准 |
|------|---------|
| S3.1.1 CLI 入口（clap）：`--agent-id`, `--manifest-path`, `--work-dir`, `--gateway-socket`, `--dev-mode` | `--help` 输出正确 |
| S3.1.2 配置加载（manifest + 环境变量/CLI 覆盖） | manifest.toml 正确解析 |
| S3.1.3 日志初始化（`tracing-subscriber`） | `--log-level debug` 生效 |

---

### S3.2 任务：.agent 包加载器

**目标**：解析 .agent ZIP 包，校验 manifest。

| 任务 | 验收标准 |
|------|---------|
| S3.2.1 ZIP 解压（使用 `zip` crate） | 可读取 .agent 包内所有文件 |
| S3.2.2 manifest.toml 解析 + 校验（必填字段、版本兼容） | 无效 manifest 报错 |
| S3.2.3 prompt 组装（`prompts/` + `SKILL.md` → system prompt） | system prompt 格式正确 |
| S3.2.4 签名验证委托（调用 `rollball-sign` 验签） | 未签名/无效包拒绝加载 |

**说明**：包加载器不内联签名逻辑，而是调用 `rollball-sign` crate 的 API。

---

### S3.3 任务：LLM Provider 实现

**目标**：支持 OpenAI Compatible API 和 Ollama。

| 任务 | 验收标准 |
|------|---------|
| S3.3.1 OpenAI Provider 实现（`reqwest` + streaming） | 流式输出正常 |
| S3.3.2 Ollama Provider 实现 | Ollama 本地调用正常 |
| S3.3.3 Provider 路由（根据 manifest `llm.provider` 选择） | 配置路由生效 |
| S3.3.4 重试 + fallback 链（`reliable.rs`） | Provider 故障时自动切换 |

**说明**：Anthropic Provider Phase 2 再加。

---

### S3.4 任务：主循环（Agent Loop）

**目标**：实现 Runtime 9步主循环 + 2个子步骤。

```text
① 预算预检
② 构建上下文
  ②.5 Preemptive Trim（上下文超限时）
③ 调用 LLM
④ 解析响应（text / tool_calls / streaming）
⑤ 工具调度
  ⑤.1 权限校验
  ⑤.2 HashSet 去重
⑥ 结果追加历史
⑦ 用量上报（异步）
⑧ 循环检测
⑨ DevMode 控制
```

| 任务 | 验收标准 |
|------|---------|
| S3.4.1 Budget Guard（本地 token 预估校验） | 超出限额阻止调用 |
| S3.4.2 上下文构建（system prompt + history + identity） | prompt token 数量准确 |
| S3.4.3 Streaming + tool_calls 并发处理（检测到 tool_calls 立即中断 streaming） | streaming 正确截断 |
| S3.4.4 Loop Detector（Exact Repeat / Ping-Pong / No Progress + 三级响应） | 死循环可检测和中断 |
| S3.4.5 Preemptive Trim（步骤②.5，在 context.build 前触发） | 上下文超限前主动裁剪 |
| S3.4.6 Reactive Recovery（Emergency History Trim） | 溢出后渐进恢复 |
| S3.4.7 Rate Limit 分层处理（可重试 / 不可重试） | 429 响应正确处理 |

---

### S3.5 任务：内置工具（Phase 1 范围）

**目标**：实现 13 个内置工具。

#### S3.5.1 核心 Builtin（Phase 1 必做）

| 工具 | 文件 | 验收标准 |
|------|------|---------|
| `shell` | `shell.rs` | 命令执行，权限校验 |
| `file_read` | `file_read.rs` | 工作区内文件读取 |
| `file_write` | `file_write.rs` | 工作区内文件写入 |
| `file_edit` | `file_edit.rs` | 精确字符串替换 |
| `glob_search` | `glob_search.rs` | glob 模式搜索 |
| `content_search` | `content_search.rs` | 正则搜索文件内容 |
| `calculator` | `calculator.rs` | 算术计算 |
| `http_request` | `http_request.rs` | HTTP GET/POST |
| `web_fetch` | `web_fetch.rs` | 网页抓取转纯文本 |
| `web_search` | `web_search.rs` | Brave/SearXNG 搜索 |
| `weather` | `weather.rs` | wttr.in 天气查询 |
| `identity_query` | `identity_query.rs` | 向系统 Agent 查询身份（IPC） |

#### S3.5.2 Memory 工具（Phase 1 简化版）

| 工具 | 文件 | 验收标准 |
|------|------|---------|
| `memory_store` | `memory_store.rs` | 存储单条记忆（JSON payload） |
| `memory_recall` | `memory_recall.rs` | 关键词检索记忆 |

**说明**：Phase 1 Grafeo 后端用 SQLite mock 替代（图数据库 + 遗忘机制在 Phase 2）。`memory_store` 写入 SQLite，`memory_recall` 做 LIKE 搜索。Phase 2 再迁移到真正的 Grafeo 图数据库。

#### S3.5.3 工具注册表 + 权限校验

| 任务 | 验收标准 |
|------|---------|
| S3.5.3.1 工具池注册（按 manifest `tools[]` 过滤激活） | 权限外工具不可调用 |
| S3.5.3.2 权限校验（PathGuardedTool / RateLimitedTool 装饰器） | 无权限工具调用被拒绝 |
| S3.5.3.3 工具 JSON Schema 清洗（借鉴 ZeroClaw） | Schema 格式兼容 LLM |

---

### S3.6 任务：History Manager

**目标**：对话历史 FIFO 裁剪 + Tool Result 折叠。

| 任务 | 验收标准 |
|------|---------|
| S3.6.1 Token 计算（`cl100k_base` tiktoken 近似估算） | Token 计数误差 < 5% |
| S3.6.2 对话历史 FIFO 裁剪（动态 N 计算） | 超出上下文上限时裁剪 |
| S3.6.3 Tool Result 折叠（保留最近 4 轮完整结果，更早的折叠为摘要） | 折叠后 token 数符合预期 |

---

### S3.7 任务：IPC 客户端

**目标**：Runtime → Gateway Socket 通信。

| 任务 | 验收标准 |
|------|---------|
| S3.7.1 传输层抽象（`transport.rs`：`UnixSocketTransport` for Linux） | 连接建立成功 |
| S3.7.2 Frame 编解码（JSON 序列化） | 双向消息往返正常 |
| S3.7.3 KeyRelease 请求 / UsageReport 上报 | 收到 KeyReleaseResult |

**说明**：Named Pipe（Windows）和 Local TCP（移动端）在 Phase 7 再加。

---

## 4. S3：Gateway

### S4.1 任务：Gateway CLI

**目标**：`rollball-gateway` 二进制，支持守护进程模式和 CLI 命令。

| 任务 | 验收标准 |
|------|---------|
| S4.1.1 CLI 入口（clap）：`--daemon`, `install`, `uninstall`, `start`, `stop`, `list` 子命令 | `--help` 输出正确 |
| S4.1.2 守护进程模式（后台常驻） | `rollball-gateway --daemon` 常驻 |
| S4.1.3 Gateway 配置（`~/.config/agent-gateway/gateway.toml`） | 配置正确加载 |

---

### S4.2 任务：IPC 服务端

**目标**：Gateway 接收 Runtime 的 Socket 请求。

| 任务 | 验收标准 |
|------|---------|
| S4.2.1 Unix Socket 服务端（`tokio::net::UnixListener`） | 接受 Runtime 连接 |
| S4.2.2 Frame 解码 + 路由到对应 Handler | 所有 6 种 Request 类型可处理 |
| S4.2.3 响应序列化 + 发送 | 响应正确返回 Runtime |
| S4.2.4 连接会话管理（Session struct） | 多 Runtime 并发连接 |

---

### S4.3 任务：包管理器

**目标**：.agent 包的安装、卸载、升级。

| 任务 | 验收标准 |
|------|---------|
| S4.3.1 安装流程（ZIP 解析 → 签名验证 → manifest 校验 → 解压到安装目录） | 签名无效拒绝安装 |
| S4.3.2 卸载（删除安装目录，可选备份 Grafeo） | 目录清理干净 |
| S4.3.3 升级（签名一致性校验：作者指纹必须一致） | 恶意包覆盖被拒绝 |
| S4.3.4 已安装 Agent 列表管理（`~/.local/share/agent-gateway/packages/`） | 列表准确 |

---

### S4.4 任务：生命周期管理器

**目标**：Agent 进程 spawn / kill / health-check。

| 任务 | 验收标准 |
|------|---------|
| S4.4.1 进程 spawn（`tokio::process::Command`，传入 socket path 等参数） | Agent 进程启动 |
| S4.4.2 进程 kill（正常停止 Agent） | 进程被终止 |
| S4.4.3 健康检查（Socket 连接状态检测） | 不健康 Agent 标记 |
| S4.4.4 空闲超时管理（`idle_timeout` 配置） | 超时 Agent 自动停止 |

**说明**：Phase 1 暂不实现沙箱隔离（bubblewrap 等，Phase 3），用 `--work-dir` 目录隔离代替。

---

### S4.5 任务：Key Vault 集成

**目标**：Gateway 通过 IPC 分发 API Key。

| 任务 | 验收标准 |
|------|---------|
| S4.5.1 Vault 解锁（Gateway 启动时解锁，用户输入密码一次） | 解锁成功/失败反馈 |
| S4.5.2 KeyRelease Handler（验证 Runtime 身份，分发对应 Provider 的 Key） | Key 到达 Runtime |
| S4.5.3 Key 分发后立即从内存清除（一次性分发） | 同一 Key 不重复分发 |
| S4.5.4 UsageReport Handler（接收用量数据，更新 Vault 中的 key metadata） | 用量统计准确 |

---

## 5. S4：集成验证

### 5.1 任务：示例 Agent（天气查询）

**目标**：构建一个真实可运行的天气查询 Agent，验证全链路。

| 任务 | 验收标准 |
|------|---------|
| 5.1.1 编写 `weather-agent.agent` 包（manifest.toml + prompts/default.md + prompts/system.md） | 包结构符合规范 |
| 5.1.2 用 `rollball-sign` 签名 | 签名成功 |
| 5.1.3 Gateway CLI 安装：`rollball-gateway install weather-agent.agent` | 安装成功 |
| 5.1.4 启动天气 Agent：`rollball-gateway start com.example.weather` | Runtime 进程启动 |
| 5.1.5 发送对话消息，验证 LLM 调用 + 工具调用 + 响应返回 | 完整往返成功 |
| 5.1.6 停止 Agent：`rollball-gateway stop com.example.weather` | 进程终止 |

**weather-agent 能力设计**：

- `llm.provider = "openai"`
- `tools = ["weather", "memory_store", "memory_recall"]`
- `permissions = ["network:wttr.in"]`
- 行为：查询天气 → 写入 memory（城市偏好）→ 下次直接使用偏好城市

---

### 5.2 任务：端到端测试套件

**目标**：用 `tests/` 目录下的集成测试验证核心路径。

| 测试 | 验证内容 |
|------|---------|
| `test_sign_and_verify_roundtrip` | sign + verify 往返 |
| `test_vault_store_retrieve` | Vault 加密存储往返 |
| `test_manifest_parse` | manifest.toml 解析 |
| `test_runtime_main_loop_step` | 单步主循环（mock LLM） |
| `test_gateway_install_package` | 安装流程 |
| `test_gateway_spawn_agent` | spawn + IPC 通信 |
| `test_e2e_weather_agent` | 完整天气查询流程（需要真实 API Key）|

**说明**：需要真实 LLM API 的测试用环境变量跳过（`ROLLBALL_TEST_SKIP_LIVE_LLM=1` 跳过）。

---

## 6. 进度追踪

### 6.1 状态定义

| 状态 | 含义 |
|------|------|
| **待开始** | 尚未开始开发 |
| **进行中** | 正在开发 |
| **待测试** | 代码完成，等待单元测试 |
| **完成** | 代码 + 测试通过 |
| **阻塞** | 等待其他任务完成 |

### 6.2 任务总表

| ID | 任务 | 模块 | 阶段 | 状态 | 备注 |
|----|------|------|------|------|------|
| S1.1 | Workspace 骨架 | - | S1 | 待开始 | |
| S1.2 | rollball-core 类型定义 | rollball-core | S1 | 待开始 | |
| S1.3 | rollball-sign 签名工具链 | rollball-sign | S1 | 待开始 | |
| S1.4 | rollball-vault 密钥存储 | rollball-vault | S1 | 待开始 | |
| S3.1 | Runtime CLI + 配置 | rollball-runtime | S2 | 待开始 | |
| S3.2 | .agent 包加载器 | rollball-runtime | S2 | 待开始 | |
| S3.3 | LLM Provider 实现 | rollball-runtime | S2 | 待开始 | |
| S3.4 | Agent 主循环 | rollball-runtime | S2 | 待开始 | |
| S3.5 | 内置工具（13个） | rollball-runtime | S2 | 待开始 | |
| S3.6 | History Manager | rollball-runtime | S2 | 待开始 | |
| S3.7 | IPC 客户端 | rollball-runtime | S2 | 待开始 | |
| S4.1 | Gateway CLI | rollball-gateway | S3 | 待开始 | |
| S4.2 | IPC 服务端 | rollball-gateway | S3 | 待开始 | |
| S4.3 | 包管理器 | rollball-gateway | S3 | 待开始 | |
| S4.4 | 生命周期管理器 | rollball-gateway | S3 | 待开始 | |
| S4.5 | Key Vault 集成 | rollball-gateway | S3 | 待开始 | |
| S5.1 | 示例天气 Agent | examples/ | S4 | 待开始 | |
| S5.2 | 端到端测试套件 | tests/ | S4 | 待开始 | |

---

## 7. ZeroClaw 代码复用准则

Rollball 开发中优先复用 ZeroClaw 的代码，避免重复造轮子。

**复用原则**：
- 只复用"相对独立"的代码——完整模块、完整函数、边界清晰的 trait
- 需要大幅修改才能适配 Rollball 架构的代码，不直接复用
- 不复用与 ZeroClaw 单进程模式深度耦合的部分（如 Runtime 内嵌的 HTTP Server）

**Phase 1 重点复用领域**：

| 领域 | ZeroClaw 对应文件 | Rollball 落地位置 | 说明 |
|------|-----------------|-----------------|------|
| Tool trait | `src/tool.rs` | `rollball-core/src/tools/traits.rs` | 直接复用 trait 定义 |
| Provider trait | `src/provider.rs` | `rollball-core/src/providers/traits.rs` | 直接复用 trait 定义 |
| Schema 清洗 | `src/schema.rs` | `rollball-runtime/src/tools/schema.rs` | adaptation |
| Streaming 解析 | `src/streaming.rs` | `rollball-runtime/src/providers/` | adaptation |
| Loop Detector | `src/agent/loop_detector.rs` | `rollball-runtime/src/agent/loop_detector.rs` | adaptation（三种模式 + 三级响应） |
| History Manager | `src/agent/history.rs` | `rollball-runtime/src/agent/history.rs` | adaptation（token 计算 + FIFO 裁剪） |
| RateLimitedTool | `src/security.rs` | `rollball-runtime/src/tools/wrappers.rs` | adaptation |
| PathGuardedTool | `src/security.rs` | `rollball-runtime/src/tools/wrappers.rs` | adaptation |
| JSON-RPC frame | `src/protocol.rs` | `rollball-core/src/protocol.rs` | adaptation |
| Vault 加密 | `src/security/secrets.rs` | `rollball-vault/src/encryption.rs` | 借鉴加密逻辑 |

**复用要求**：
- 注释标明来源：`// Adapted from zeroclaw/src/xxx.rs`
- 显著偏离时注明原因：`// Rollball deviation: <reason>`
- 不通过 workspace 依赖引用 zeroclaw crate，而是 adaptation 后复制到对应 crate

**不直接复用的部分**（需要重新设计）：
- ZeroClaw 的单一巨型 schema.rs（572KB）→ Rollball 按 crate 拆分
- ZeroClaw 的单进程 Agent Loop → Rollball 需要 IPC 层
- ZeroClaw 的文件系统沙箱（bubblewrap 集成方式）→ Rollball Phase 3 重新实现
- ZeroClaw 的配置驱动机制 → Rollball 改用 manifest 声明驱动

## 8. 开发约定

### 7.1 代码风格

- Rust edition 2024，rust-version 1.87
- 所有 crate 通过 `cargo clippy --all-targets -- -D warnings`
- 模块内按 `lib.rs` → 子模块顺序组织
- 错误处理用 `thiserror`，传播用 `?`

### 7.2 测试策略

- **单元测试**：每个模块 `.rs` 文件同级 `mod tests`
- **集成测试**：`tests/` 目录下，按功能分组
- **Mock 策略**：LLM Provider 用 mock 实现（`Provider::mock()` trait method）
- **跳过条件**：真实 API 调用在 CI 中跳过（`ROLLBALL_TEST_SKIP_LIVE_LLM=1`）

### 7.3 提交约定

```
<type>(<scope>): <subject>

Types: feat / fix / test / refactor / docs / chore
Scope: core / sign / vault / runtime / gateway / cli / docs
```

### 7.4 里程碑

| 里程碑 | 完成标志 |
|--------|---------|
| **M1: 基础就绪** | S1 全部完成；`cargo check --all` 通过 |
| **M2: Runtime 可运行** | S2 全部完成；Runtime 可加载 manifest 并调用 mock LLM |
| **M3: Gateway 可管理** | S3 全部完成；Gateway 可 install/start/stop Agent |
| **M4: MVP 交付** | S4 全部完成；天气 Agent 端到端运行 |

---

## 8. 附录：Phase 1 不包含的内容

以下内容在 Phase 2+ 实现，Phase 1 刻意不做：

| 内容 | 原因 | 目标 Phase |
|------|------|-----------|
| Grafeo 图数据库 | Phase 2 仿生记忆核心 | Phase 2 |
| System Agent | 依赖 Runtime + Gateway 基础 | Phase 2 |
| Intent 路由 | 依赖 System Agent | Phase 2 |
| WASM 工具沙箱 | Phase 1 内置工具足够 | Phase 3 |
| bubblewrap 文件系统隔离 | Phase 1 用 `--work-dir` 替代 | Phase 3 |
| Desktop App | Phase 5 | Phase 5 |
| DevMode Debug Protocol | Phase 5 | Phase 5 |
| Multi-provider routing | Phase 1 单 Provider 足够 | Phase 2 |
| macOS / Windows 适配 | Linux 优先 | Phase 7 |
| RAG 集成 | Phase 3 企业场景 | Phase 3 |
