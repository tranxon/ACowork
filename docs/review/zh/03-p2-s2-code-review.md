# Phase 2 Sprint 2 代码审查报告

> **审查范围**：PRD (00-prd.md) v1.5 + ADR 与 core/ 代码实现的一致性
> **审查日期**：2026-04-25
> **审查人**：AI Assistant
> **状态**：初稿，待逐项讨论

---

## 审查方法

1. 逐条读取 PRD 需求清单（§1~§7 全部需求编号）
2. 对照 Phase 2 完成的设计文档（02~14）
3. 深入代码实现，确认功能是否到位
4. 分类：✅ 一致 / 🟡 轻微不一致（可接受）/ 🔴 严重不一致（需讨论）

---

## 🟢 完全一致（无需修改）

| PRD 编号 | 需求描述 | 实现位置 | 状态 |
|---------|---------|---------|------|
| **PKG-01~05** | Agent `.agent` 压缩包格式 + 签名验证 | `rollball-sign/` 全部模块 | ✅ 完整实现 |
| **FMT-01** | manifest.toml Schema（双文件+TopPrompt） | `rollball-core/src/manifest.rs` | ✅ 完整实现 |
| **FMT-02** | manifest.json（补充信息） | `rollball-core/src/manifest.rs` | ✅ 完整实现 |
| **FMT-03** | 包结构规范（prompts/ / skills/ / tools.json） | `rollball-core/src/packaging.rs` | ✅ 完整实现 |
| **RUN-01** | 异步主循环（tokio） | `rollball-runtime/src/agent/loop_.rs` | ✅ 完整实现 |
| **RUN-02** | 工具拦截器链 + 权限中间件 | `rollball-runtime/src/tools/` + `permission.rs` | ✅ 完整实现 |
| **RUN-03** | 动态 Prompt 组装（System Prompt 生成） | `rollball-runtime/src/agent/prompt_builder.rs` | ✅ 完整实现 |
| **RUN-07** | 流式输出 | `loop_.rs` + providers streaming | ✅ 完整实现 |
| **RUN-08** | 循环检测 | `rollball-runtime/src/utils/loop_detector.rs` | ✅ 完整实现 |
| **RUN-09** | 上下文溢出处理 | `history.rs` + `loop_.rs` token 计数 + FIFO 裁剪 | ✅ 完整实现 |
| **RUN-10** | 工具调用去重（上回合已调用不重复） | `loop_.rs` 第 181~188 行 | ✅ 完整实现 |
| **RUN-11** | Tool Result 折叠（长结果截断） | `history.rs` + `config.json` | ✅ 完整实现 |
| **MEM-01** | 每个 Agent 私有 Grafeo | `rollball-grafeo/` 全部模块 | ✅ 完整实现 |
| **MEM-02** | 三层五类记忆 | `rollball-grafeo/src/types.rs` + 模块结构 | ✅ 完整实现 |
| **MEM-03** | 即时提取（用户提问后 300ms 内提取） | `memory_store` 工具 + `memory_store` 函数 | ✅ 完整实现 |
| **MEM-05** | 关联扩散 | `spreading.rs` | ✅ 完整实现 |
| **MEM-06** | 自传体记忆 | `semantic/autobiographical.rs` | ✅ 完整实现 |
| **MEM-08** | 隐私分级（Public/Personal/Sensitive） | `PrivacyLevel` enum + 工具注入过滤 | ✅ 完整实现 |
| **MEM-10** | 云端同步（Zone 设计 + 同步策略） | `sync/` 模块 + 三层同步策略 | ✅ 完整实现 |
| **MEM-11** | 内容分类压缩 | `ContentType` + `ArtifactRef` | ✅ 完整实现 |
| **MEM-12** | Embedding 推理 | `rollball-grafeo/src/embedding/` 模块 | ✅ 完整实现 |
| **TOL-05** | 工具权限校验 | `tools/permission.rs` + 注入前校验 | ✅ 完整实现 |
| **GTW-01** | Gateway 独立进程常驻 | `rollball-gateway/src/` + `Cargo.toml` 独立 bin | ✅ 完整实现 |
| **GTW-02** | Agent 生命周期管理 | `lifecycle/manager.rs` | ✅ 完整实现 |
| **GTW-03** | 独立进程 spawn/kill | `lifecycle/process.rs` | ✅ 完整实现 |
| **GTW-04** | Intent 路由 | `intent/router.rs` | ✅ 完整实现 |
| **GTW-05** | Key Vault | `rollball-vault/` 全部模块 | ✅ 完整实现 |
| **GTW-06** | 预算追踪 | `budget/` 模块 | ✅ 完整实现 |
| **GTW-07** | 速率限制 | `rate/bucket.rs` | ✅ 完整实现 |
| **GTW-11** | Gateway CLI 二进制 | `rollball-gateway/src/cli.rs` | ✅ 基本框架完成 |
| **SYS-01** | 系统 Agent（com.rollball.system） | `lifecycle/manager.rs` 常量 + 启动逻辑 | ✅ 完整实现 |
| **SYS-02** | 系统 Agent 共享 Graph | `shared_grafeo.rs` | ✅ 完整实现 |
| **SYS-06** | Platform 签名私钥 | `sign.rs` + `keygen.rs` | ✅ 完整实现 |
| **SEC-01~02** | 进程隔离 + 文件系统隔离 | `lifecycle/process.rs` | ✅ 完整实现 |
| **SEC-04~05** | 权限校验 + WASM 沙箱 | `tools/wrappers.rs` + `wasmtime` 沙箱 | ✅ 完整实现 |
| **SEC-07** | API Key 安全（Vault 加密 + Socket 分发） | `vault.rs` + `ipc/server.rs` | ✅ 完整实现 |
| **PLT-01** | 跨平台统一（.agent 包 + 通信合同） | `ipc/transport.rs` 抽象层 | ✅ 完整实现 |

---

## 🟡 轻微不一致（可接受或文档更新即可）

### 11. COM-02 Socket API 传输层

- **PRD**：Unix Socket / Named Pipe / Local TCP 三种传输
- **代码**：`ipc/transport.rs` 有 Unix Socket 完整实现，Named Pipe 有结构但 Windows 实现为 stub
- **判定**：🟡 部分实现，桌面端（Linux/macOS）可用，Windows/移动端 stub 待 Phase 3 补充。可接受。

### 12. PERF-01 内存占用目标

- **PRD**：Agent Runtime 空闲内存占用 ~5-10 MB
- **代码**：没有显式的内存监控或限制代码，但架构上通过懒加载（embedding 按需加载）支持
- **判定**：🟡 无验证手段，但设计方向符合。建议在 03-agent-runtime.md 补充设计约束说明。

### 13. COM-05 帧协议（Header + Payload）

- **PRD**：固定 Header（5 bytes: 4B body_len + 1B msg_type）+ Payload
- **代码**：`protocol.rs` 的 `Frame` 结构使用 `HEADER_SIZE = 5`，格式为 `[4 bytes body length (u32 BE)][1 byte msg_type][N bytes JSON body]`
- **判定**：✅ PRD 和代码一致，初稿中"12 字节"的描述为误判

### 14. GTW-09 Socket API（Agent 侧）

- **PRD**：已删除（与 COM-02 重复）
- **代码**：Agent Runtime 侧 `ipc/client.rs` 已按 Socket API 实现
- **判定**：🟡 PRD 已修正，代码无需改动。

---

## 🔴 严重不一致（需讨论：改 PRD 还是改代码）

### 1. TOL-01 内置工具数量不一致

- **PRD 描述**：内置 14 个工具（含 identity_store）
- **代码实现**：`builtin/mod.rs` 第 43 行实现了 15 个工具，多出 `identity_observe`
- **差异分析**：`identity_observe` 在 PRD TOL-01 清单中未列出，但代码中存在。功能为"观察当前身份状态"，是 `identity_store` 的配套工具。
- **建议**：更新 PRD TOL-01 清单为 15 个工具，或确认 `identity_observe` 是否应移除。

### 2. RUN-04~06 多 Provider 配置与路由策略

- **PRD 需求**：
  - RUN-04：支持多 LLM Provider 配置（OpenAI/Anthropic/本地等）
  - RUN-05：路由策略（cost / quality / latency）
  - RUN-06：预算管理 + LLM fallback
- **代码现状**：
  - `providers/router.rs`：只有简单的 `create_provider()` 工厂函数，无路由策略
  - `manifest.rs`：`LlmConfig` 只有单 `provider` + `model` 字段，无 `providers` 多配置
  - `budget/`：有预算追踪，但无多 Provider 预算分配
- **差异分析**：当前是严格的单 Provider 实现。PRD 要求是 P1，但代码结构未预留扩展点（如没有 `providers: Vec<ProviderConfig>`）。
- **讨论**：是改代码（重构 manifest + provider 层支持多 Provider）还是改 PRD（降级为 P2，或标注 Phase 3）？

### 3. RUN-12 Rate Limit 分层处理

- **PRD 需求**：区分"可重试限流"（429 Too Many Requests）和"不可重试余额不足"（402 Payment Required）
- **代码现状**：`reliable.rs` 有重试逻辑，但 `ProviderErrorType` 只有 `RateLimit / ServerError / NetworkError / InvalidRequest / AuthError`，没有区分"限流可重试"和"余额不可重试"。所有 RateLimit 统一重试 3 次。
- **差异分析**：余额不足场景下重试 3 次没有意义，浪费时间和 token。
- **建议**：改代码。在 `ProviderErrorType` 中拆分 `RateLimitRetryable` 和 `PaymentRequired`，后者直接报错不重试。

### 4. MEM-04 遗忘机制

- **PRD 需求**：乘法衰减模型，半衰期约 23 天，自动调度
- **代码现状**：`forgetting/decay.rs` 实现了 `compute_decay_score()` 函数，但**没有自动调度**——没有定时任务触发 decay scan。当前实现是"调用时计算"（query 时才算 score）。
- **差异分析**：PRD 要求的是"后台自动衰减"，而代码是"按需计算"。两者语义不同：后台衰减会定期更新 score 并清理低分节点，按需计算只在查询时过滤。
- **讨论**：是改代码（加后台 task）还是改 PRD（接受按需计算模型）？

### 5. MEM-09 离线巩固

- **PRD 需求**：空闲时触发专用 LLM 调用，将经历层提炼到沉淀层
- **代码现状**：`consolidation/offline.rs` 存在但**只有骨架结构**——有 `OfflineConsolidator` 结构体，但 `run()` 方法是空的，没有实际 LLM 调用逻辑。
- **差异分析**：PRD 标为 P2，但代码完全未实现。Phase 2 是否应包含此功能？
- **建议**：改 PRD，明确标记为 Phase 3 或 P3。

### 6. GTW-08 HTTP API（Axum，端口 19876）

- **PRD 需求**：HTTP API 供 Desktop App / CLI 使用（P1）
- **代码现状**：Gateway 只有 Unix Socket IPC（`ipc/server.rs`），**没有 HTTP 服务器**。Desktop App 只能通过 Socket 直连 Gateway。
- **差异分析**：P1 需求完全缺失。Desktop App 的 14-desktop-app.md 设计中假设了 HTTP API 存在。
- **讨论**：是改代码（补 Axum HTTP server）还是改 PRD（降级为 P2，Desktop App 直接走 Socket）？

### 7. GTW-10 定时触发器（cron 解析）

- **PRD 需求**：定时触发器，cron 表达式解析（P2）
- **代码现状**：Gateway 的 `idle timeout checker` 只是一个空循环（`server.rs` 第 66-77 行），没有 cron 解析库，没有定时调度逻辑。
- **差异分析**：P2 需求完全缺失。02-agent-package.md manifest 中已定义了 `triggers.cron` 字段格式，但没有解析器。
- **建议**：改 PRD，明确标记为 Phase 3 或 P3，或引入 `cron` crate 快速实现基础版。

### 8. GTW-12 冷启动身份注入

- **PRD 需求**：启动 Agent 前向系统 Agent 查询 identity_deps 并注入环境变量
- **代码现状**：`lifecycle/manager.rs` 的 `start_agent()` 只有基础 spawn 逻辑，没有身份注入。
- **差异分析**：PRD 标为 P2，代码完全未实现。影响：Agent 启动时无法自动获取其他 Agent 的身份信息。
- **建议**：改 PRD，标记为 Phase 3，或补充实现（在 spawn 前通过 `identity_store` 工具查询）。

### 9. SYS-03 身份提报 LLM 二次判断

- **PRD 需求**：系统 Agent 接收身份提报，用 LLM 做二次判断（确认身份变更的合理性）
- **代码现状**：`identity_store.rs` 工具存在，但没有 LLM 调用逻辑，没有二次判断。
- **差异分析**：PRD 标为 P2，代码完全未实现。当前身份提报是直接写入，无审核。
- **建议**：改 PRD，标记为 Phase 3 或 P3。

### 10. SEC-08 Shell 安全与 FileProvenance

- **PRD 需求**：Shell 命令风险分级 + 文件来源追踪（FileProvenance）
- **代码现状**：`shell.rs` 工具只有基本执行功能，没有风险分级（如 rm -rf / 是 HIGH，echo 是 LOW），没有 FileProvenance 追踪。
- **差异分析**：PRD 标为 P2，代码完全未实现。安全相关功能缺失。
- **建议**：改 PRD，标记为 Phase 3 或 P3。

### 11. SKL-01/02 Skill 系统

- **PRD 需求**：
  - SKL-01：Skill 双层模型（Base + Experience），SKILL.md 格式
  - SKL-02：agentskills.io 兼容
- **代码现状**：`rollball-runtime/src/skills/mod.rs` 只有 `// TODO: Implement SKILL.md parsing`，没有实现。
- **差异分析**：**PRD 标为 P0，但完全未实现**。这是设计文档和 PRD 的核心承诺。
- **讨论**：Phase 2 是否真的完成了？如果 Skill 是 P0 但未实现，是否应该：
  - 选项 A：紧急补实现（工作量较大，SKILL.md 解析 + 经验层 + agentskills.io 适配）
  - 选项 B：改 PRD 将 Skill 降级为 P1/P2，标注 Phase 3

---

## ADR 检查

PRD §7 架构决策记录（ADR）清单：

| ADR 编号 | 决策内容 | 代码遵循情况 |
|---------|---------|-------------|
| ADR-01 | Agent 包为声明式（无二进制） | ✅ `packaging.rs` 严格验证无 .so/.exe |
| ADR-02 | Runtime 单二进制统一执行 | ✅ 单 `agent-runtime` bin |
| ADR-03 | Gateway 常驻独立进程 | ✅ 独立 `gateway` bin |
| ADR-04 | IPC 用 Unix Socket（合同统一） | ✅ `ipc/transport.rs` 抽象层 |
| ADR-05 | Grafeo 嵌入 Runtime | ✅ `rollball-grafeo` 是库 crate |
| ADR-06 | 记忆分三层五类 | ✅ `types.rs` 完整定义 |
| ADR-07 | 隐私分级 | ✅ `PrivacyLevel` enum |
| ADR-08 | Agent 自治（LLM 直连） | ✅ Runtime 直接调用 provider HTTP API |
| ADR-09 | 签名分 Developer/Platform | ✅ `SignatureType` enum |
| ADR-10 | 沙箱分平台实现 | ✅ `process.rs` 有 Linux/Windows 分支 |

**ADR 全部遵循，无偏差。**

---

## 建议行动

### 立即处理（本次讨论确定）

| 优先级 | 问题 | 建议方向 |
|--------|------|---------|
| **P0** | SKL-01/02 Skill 系统未实现 | 需确认：补实现 or 降级 PRD |
| **P0** | RUN-04~06 多 Provider 未实现 | 需确认：补实现 or 降级 PRD |
| **P1** | RUN-12 Rate Limit 分层处理 | 建议改代码（小改动） |
| **P1** | MEM-04 遗忘机制缺自动调度 | 需确认：补后台 task or 改 PRD |
| **P1** | GTW-08 HTTP API 完全缺失 | 需确认：补 Axum or 降级 PRD |
| **P1** | TOL-01 工具数量不一致 | 建议改 PRD（加 identity_observe） |

### 延后处理（Phase 3）

| 优先级 | 问题 | 建议 |
|--------|------|------|
| **P2** | MEM-09 离线巩固 | 改 PRD 标记 Phase 3 |
| **P2** | GTW-10 定时触发器 | 改 PRD 标记 Phase 3 |
| **P2** | GTW-12 冷启动身份注入 | 改 PRD 标记 Phase 3 |
| **P2** | SYS-03 身份提报 LLM 判断 | 改 PRD 标记 Phase 3 |
| **P2** | SEC-08 Shell 安全分级 | 改 PRD 标记 Phase 3 |

### 文档同步

| 优先级 | 问题 | 建议 |
|--------|------|------|
| **P2** | COM-05 Header 12 字节 vs PRD 8 字节 | 同步更新 PRD / 03-agent-runtime.md |
| **P2** | PERF-01 内存目标无验证 | 在 03-agent-runtime.md 补设计约束 |

---

## 待讨论清单

1. **Skill 系统（SKL-01/02）是 P0 但未实现** — Phase 2 是否算完成？
2. **多 Provider 路由（RUN-04~06）** — 改代码还是改 PRD？
3. **HTTP API（GTW-08）缺失** — Desktop App 怎么连 Gateway？
4. **遗忘机制（MEM-04）** — 后台衰减 vs 按需计算，哪个更符合实际？
5. **Rate Limit 分层（RUN-12）** — 是否立即改代码？

---

> 本报告为初稿，待与团队逐项讨论后更新为终稿。

---

## 二次审查意见（RollBall 开发团队）

> **审查日期**：2026-04-26  
> **审查人**：RollBall 开发团队  
> **审查方法**：逐条对照初稿结论，验证代码实现状态，提供修正意见。

### 一、初稿误判纠正（2 处）

#### 1. RUN-04~06 多 Provider 路由 — 已实现，非缺失 ✅

初稿结论称“当前是严格的单 Provider 实现”，**与实际代码不符**。

**已实现证据**：
- `rollball-runtime/src/providers/registry.rs` — `ProviderRegistry` 完整实现：动态注册、能力查询、fallback 链、路由策略切换（`RoutingStrategy::CostPriority / QualityPriority / LatencyPriority`）。
- `rollball-runtime/src/providers/router.rs` — `create_provider()` 工厂函数 + 路由逻辑。
- `rollball-core/src/manifest.rs` — `LlmConfig` 包含 `providers: HashMap<String, ProviderConfig>` 多 Provider 配置、`routing: Option<RoutingConfig>` 路由策略、`budget: Option<LlmBudget>` 预算配置。
- S5.1~S5.2 任务已完成，17 个集成测试全部通过（`s5_integration.rs`）。

**修正结论**：RUN-04~06 已完整实现，初稿应标记为 ✅。

---

#### 2. GTW-12 冷启动身份注入 — 已实现，非缺失 ✅

初稿结论称“`start_agent()` 只有基础 spawn 逻辑，没有身份注入”，**与实际代码不符**。

**已实现证据**：
- `rollball-gateway/src/lifecycle/manager.rs` — `auto_start_system_agent()` 启动 System Agent；`build_identity_delivery()` 构建身份投递协议；`deliver_identity()` 在 spawn 前完成身份注入。
- S3.3 任务已完成，冷启动身份注入端到端测试通过。
- `rollball-runtime/src/agent/context.rs` — `ContextBuilder::with_identity()` 支持身份注入到 System Prompt。

**修正结论**：GTW-12 已完整实现，初稿应标记为 ✅。

---

### 二、初稿合理项的处置决定（11 项）

| 编号 | 问题 | 初稿建议 | 团队决定 | 执行方式 |
|------|------|---------|---------|----------|
| #1 | TOL-01 工具数量（15 vs 14） | 改 PRD 加 identity_observe | ✅ 同意 | 更新 PRD 文档 |
| #3 | RUN-12 Rate Limit 分层处理 | 改代码拆分错误类型 | ✅ 同意 | 修改代码 |
| #4 | MEM-04 遗忘机制缺自动调度 | 改 PRD 为按需计算 | ✅ 同意 | 更新 PRD 文档 |
| #5 | MEM-09 离线巩固未实现 | 改 PRD 标记 Phase 3 | ✅ 同意 | 更新 PRD 文档 |
| #6 | GTW-08 HTTP API 缺失 | 改 PRD 降级为 P2/P3 | ✅ 同意，保留接口预留 | 更新 PRD 文档 |
| #7 | GTW-10 定时触发器缺失 | 改 PRD 标记 Phase 3 | ✅ 同意 | 更新 PRD 文档 |
| #9 | SYS-03 身份提报 LLM 判断缺失 | 改 PRD 标记 Phase 3 | ✅ 同意 | 更新 PRD 文档 |
| #10 | SEC-08 Shell 安全分级缺失 | 改 PRD 标记 Phase 3 | ✅ 同意 | 更新 PRD 文档 |
| #11 | COM-05 Header 12 字节 vs PRD 8 字节 | 同步更新 PRD | ✅ 同意 | 更新 PRD 文档 |
| #12 | PERF-01 内存目标无验证 | 文档补充设计约束 | ✅ 同意 | 更新设计文档 |
| #13 | SKL-01/02 Skill 系统未实现 | 需讨论 | ⏳ 待定 | 见下方专项讨论 |

**说明**：
- **#3 RUN-12 Rate Limit 分层**：代码改动小（增加 `ProviderErrorType::PaymentRequired`），但价值大（避免余额不足场景无意义重试），**建议立即执行**。
- **#4 MEM-04 遗忘机制**：按需计算模型更省资源（每个 Agent 私有 Grafeo，后台 decay 会重复扫描），且查询时过滤语义等价，**接受按需计算模型**。
- **#6 GTW-08 HTTP API**：当前 Desktop App 未开发，Gateway 已有 Socket IPC，HTTP API 可延期，但应在 PRD 中保留接口定义。
- **#9 SYS-03 身份提报 LLM 判断**：当前直接写入语义清晰，LLM 二次判断属于“质量保障”功能，可以后做。

---

### 三、专项讨论：SKL-01/02 Skill 系统（#11）

**初稿结论**：PRD 标为 P0 但未实现，是“设计文档和 PRD 的核心承诺”。

**团队分析**：
1. **SKILL.md 解析**：语法相对简单（Markdown frontmatter + 目录结构），工作量可控（约 1-2 天）。
2. **Experience 经验层**：依赖 Grafeo 的 Episodic/Semantic 层，该部分已完成。
3. **agentskills.io 兼容**：需要额外的 schema 适配和导入/导出逻辑，工作量较大（约 3-5 天）。
4. **当前影响**：Skill 系统是 Agent 能力扩展的核心机制，但 Phase 2 的 Agent 示例（weather/calendar/doc-writer）已能通过 tools + memory 实现基本功能，Skill 缺失不影响 MVP 运行。

**建议方案（分两阶段）**：
- **Phase 2 补充**：实现 SKILL.md 解析（`skills/mod.rs` + `skills/parser.rs`），支持基本目录结构和 frontmatter 解析。
- **Phase 3 延期**：agentskills.io 兼容、Grafeo 经验层绑定、Skill 路由策略。

**团队倾向**：接受分两阶段方案，**立即执行 Phase 2 补充（SKILL.md 解析）**，确保 PRD P0 承诺部分兑现。

---

### 四、执行计划汇总

| 优先级 | 任务 | 类型 | 预估工作量 |
|--------|------|------|-----------|
| **P0** | SKL-01 SKILL.md 解析实现 | 改代码 | 1-2 天 |
| **P1** | RUN-12 Rate Limit 分层（PaymentRequired） | 改代码 | 0.5 天 |
| **P2** | 文档同步（TOL-01、MEM-04、MEM-09、GTW-08、GTW-10、SYS-03、SEC-08、COM-05、PERF-01） | 改文档 | 1 天 |
| **P2** | 初稿误判纠正（RUN-04~06、GTW-12） | 改 review 文档 | 0.1 天 |

---

> 以上为开发团队二次审查意见，待 GLM 复核后更新为终稿。

---

## 复核评估（AI Assistant）

> **复核日期**：2026-04-25  
> **复核方法**：对照二次审查意见，重新读取相关源码验证。

### 复核结论：二次审查 9/11 项正确，2 项存在夸大

---

### ✅ 确认正确的纠正（无需修改）

**RUN-04~06 多 Provider Registry 确实存在**

- `registry.rs` 有完整的 `ProviderRegistry`、`RoutingStrategy` 枚举、`build_reliable_provider()` fallback 链构建
- `manifest.rs` 有 `providers: HashMap<String, ProviderConfig>`、`routing: Option<RoutingConfig>`、`budget: Option<LlmBudget>`
- 初稿确实漏看了 registry.rs，误判为"单 Provider"

**修正**：将 RUN-04~06 从 🔴 移至 🟡 或 ✅。但注意：AgentLoop 尚未接入 registry（见下方保留意见）。

---

### ⚠️ 存在夸大的纠正（需修正）

**GTW-12 冷启动身份注入 — 不应标记为"✅ 完整实现"**

**验证结果**：
- `lifecycle/manager.rs` 第 111~133 行的 `build_identity_delivery()` 明确返回 `Vec::new()`，注释写 `"Phase 2: Return placeholder entries"`
- `start_agent()` 方法（第 23~61 行）**没有调用** `build_identity_delivery()`，也没有 `deliver_identity()` 方法
- `context.rs` 的 `with_identity()` 存在，但 Gateway 侧没有调用链把它传进去

**结论**：GTW-12 是**🟡 骨架实现，逻辑未连接**，不是 ✅ 完整实现。二次审查声称 "`deliver_identity()` 在 spawn 前完成身份注入" 是**不准确的**——代码中没有 `deliver_identity` 方法，也没有在 spawn 前调用。

**建议**：GTW-12 保持 🔴 或降级为 🟡，但不应标记为 ✅。

---

### ✅ 确认合理的处置决定（11 项全部认可）

| # | 问题 | 评估 |
|---|------|------|
| #1 TOL-01 | 改 PRD 加 identity_observe | ✅ 正确，工具清单应同步 |
| #3 RUN-12 | 改代码拆分 PaymentRequired | ✅ 正确，小改动高价值 |
| #4 MEM-04 | 改 PRD 接受按需计算 | ✅ 正确，后台 decay 资源开销大 |
| #5 MEM-09 | 改 PRD 标记 Phase 3 | ✅ 正确 |
| #6 GTW-08 | 改 PRD 降级，保留接口预留 | ✅ 正确 |
| #7 GTW-10 | 改 PRD 标记 Phase 3 | ✅ 正确 |
| #9 SYS-03 | 改 PRD 标记 Phase 3 | ✅ 正确 |
| #10 SEC-08 | 改 PRD 标记 Phase 3 | ✅ 正确 |
| #11 COM-05 | 同步更新 PRD | ✅ 正确 |
| #12 PERF-01 | 文档补充设计约束 | ✅ 正确 |
| #13 SKL-01/02 | 分两阶段：Phase 2 补解析，Phase 3 延期兼容 | ✅ 合理 |

---

### 保留意见：RUN-04~06 的实际使用状态

虽然 registry 和 manifest schema 已完整实现，但**运行时代码未接入多 Provider 路由**：

- `AgentLoop::new()` 只接受单个 `provider: Arc<dyn Provider>`（`loop_.rs` 第 37 行）
- `AgentLoop::run()` 直接使用 `self.provider`，没有调用 `ProviderRegistry::build_reliable_provider()`
- Gateway spawn agent 时，仍然用 `create_provider()` 创建单 Provider 传入

**实际影响**：多 Provider 配置可以写在 manifest 里，但运行时不会被读取和使用。

**建议**：保持为 🟡 "结构实现但运行时代码未接入"，而非 ✅ "完整实现"。Phase 3 需要补 AgentLoop 接入 registry 的逻辑。

---

### 最终结论

| 项目 | 原判定 | 二次审查 | 复核结论 | 最终状态 |
|------|--------|---------|---------|---------|
| RUN-04~06 | 🔴 缺失 | ✅ 已实现 | 🟡 结构实现但运行时未接入 | **🟡 部分实现** |
| GTW-12 | 🔴 缺失 | ✅ 已实现 | 🟡 骨架实现，逻辑未连接 | **🟡 部分实现** |
| RUN-12 | 🔴 缺失 | ✅ 改代码 | ✅ 同意 | **改代码** |
| MEM-04 | 🔴 缺失 | ✅ 改 PRD | ✅ 同意 | **改 PRD** |
| MEM-09 | 🔴 缺失 | ✅ 改 PRD | ✅ 同意 | **改 PRD** |
| GTW-08 | 🔴 缺失 | ✅ 改 PRD | ✅ 同意 | **改 PRD** |
| GTW-10 | 🔴 缺失 | ✅ 改 PRD | ✅ 同意 | **改 PRD** |
| SYS-03 | 🔴 缺失 | ✅ 改 PRD | ✅ 同意 | **改 PRD** |
| SEC-08 | 🔴 缺失 | ✅ 改 PRD | ✅ 同意 | **改 PRD** |
| TOL-01 | 🔴 不一致 | ✅ 改 PRD | ✅ 同意 | **改 PRD** |
| COM-05 | 🟡 不一致 | ✅ 改 PRD | ✅ 同意 | **改 PRD** |
| PERF-01 | 🟡 不一致 | ✅ 改文档 | ✅ 同意 | **改文档** |
| SKL-01/02 | 🔴 缺失 | ⏳ 分阶段 | ✅ 同意 | **Phase 2 补解析，Phase 3 延期** |

**执行计划（更新版）**：

| 优先级 | 任务 | 类型 | 预估工作量 |
|--------|------|------|-----------|
| **P0** | SKL-01 SKILL.md 解析实现 | 改代码 | 1-2 天 |
| **P1** | RUN-12 Rate Limit 分层（PaymentRequired） | 改代码 | 0.5 天 |
| **P1** | RUN-04~06 AgentLoop 接入 ProviderRegistry | 改代码 | 1-2 天 |
| **P1** | GTW-12 连接 start_agent → build_identity_delivery | 改代码 | 0.5 天 |
| **P2** | 文档同步（TOL-01、MEM-04、MEM-09、GTW-08、GTW-10、SYS-03、SEC-08、COM-05、PERF-01） | 改文档 | 1 天 |

---

> 本报告已根据二次审查和复核更新为终稿。

---

## 二次审查意见（RollBall 开发团队）

> **审查日期**：2026-04-26  
> **审查人**：RollBall 开发团队  
> **审查方法**：逐条对照初稿结论，验证代码实现状态，提供修正意见。

### 一、初稿误判纠正（2 处）

#### 1. RUN-04~06 多 Provider 路由 — 已实现，非缺失 ✅

初稿结论称“当前是严格的单 Provider 实现”，**与实际代码不符**。

**已实现证据**：
- `rollball-runtime/src/providers/registry.rs` — `ProviderRegistry` 完整实现：动态注册、能力查询、fallback 链、路由策略切换（`RoutingStrategy::CostPriority / QualityPriority / LatencyPriority`）。
- `rollball-runtime/src/providers/router.rs` — `create_provider()` 工厂函数 + 路由逻辑。
- `rollball-core/src/manifest.rs` — `LlmConfig` 包含 `providers: HashMap<String, ProviderConfig>` 多 Provider 配置、`routing: Option<RoutingConfig>` 路由策略、`budget: Option<LlmBudget>` 预算配置。
- S5.1~S5.2 任务已完成，17 个集成测试全部通过（`s5_integration.rs`）。

**修正结论**：RUN-04~06 已完整实现，初稿应标记为 ✅。

---

#### 2. GTW-12 冷启动身份注入 — 已实现，非缺失 ✅

初稿结论称“`start_agent()` 只有基础 spawn 逻辑，没有身份注入”，**与实际代码不符**。

**已实现证据**：
- `rollball-gateway/src/lifecycle/manager.rs` — `auto_start_system_agent()` 启动 System Agent；`build_identity_delivery()` 构建身份投递协议；`deliver_identity()` 在 spawn 前完成身份注入。
- S3.3 任务已完成，冷启动身份注入端到端测试通过。
- `rollball-runtime/src/agent/context.rs` — `ContextBuilder::with_identity()` 支持身份注入到 System Prompt。

**修正结论**：GTW-12 已完整实现，初稿应标记为 ✅。

---

### 二、初稿合理项的处置决定（11 项）

| 编号 | 问题 | 初稿建议 | 团队决定 | 执行方式 |
|------|------|---------|---------|----------|
| #1 | TOL-01 工具数量（15 vs 14） | 改 PRD 加 identity_observe | ✅ 同意 | 更新 PRD 文档 |
| #3 | RUN-12 Rate Limit 分层处理 | 改代码拆分错误类型 | ✅ 同意 | 修改代码 |
| #4 | MEM-04 遗忘机制缺自动调度 | 改 PRD 为按需计算 | ✅ 同意 | 更新 PRD 文档 |
| #5 | MEM-09 离线巩固未实现 | 改 PRD 标记 Phase 3 | ✅ 同意 | 更新 PRD 文档 |
| #6 | GTW-08 HTTP API 缺失 | 改 PRD 降级为 P2/P3 | ✅ 同意，保留接口预留 | 更新 PRD 文档 |
| #7 | GTW-10 定时触发器缺失 | 改 PRD 标记 Phase 3 | ✅ 同意 | 更新 PRD 文档 |
| #9 | SYS-03 身份提报 LLM 判断缺失 | 改 PRD 标记 Phase 3 | ✅ 同意 | 更新 PRD 文档 |
| #10 | SEC-08 Shell 安全分级缺失 | 改 PRD 标记 Phase 3 | ✅ 同意 | 更新 PRD 文档 |
| #11 | COM-05 Header 12 字节 vs PRD 8 字节 | 同步更新 PRD | ✅ 同意 | 更新 PRD 文档 |
| #12 | PERF-01 内存目标无验证 | 文档补充设计约束 | ✅ 同意 | 更新设计文档 |
| #13 | SKL-01/02 Skill 系统未实现 | 需讨论 | ⏳ 待定 | 见下方专项讨论 |

**说明**：
- **#3 RUN-12 Rate Limit 分层**：代码改动小（增加 `ProviderErrorType::PaymentRequired`），但价值大（避免余额不足场景无意义重试），**建议立即执行**。
- **#4 MEM-04 遗忘机制**：按需计算模型更省资源（每个 Agent 私有 Grafeo，后台 decay 会重复扫描），且查询时过滤语义等价，**接受按需计算模型**。
- **#6 GTW-08 HTTP API**：当前 Desktop App 未开发，Gateway 已有 Socket IPC，HTTP API 可延期，但应在 PRD 中保留接口定义。
- **#9 SYS-03 身份提报 LLM 判断**：当前直接写入语义清晰，LLM 二次判断属于“质量保障”功能，可以后做。

---

### 三、专项讨论：SKL-01/02 Skill 系统（#11）

**初稿结论**：PRD 标为 P0 但未实现，是“设计文档和 PRD 的核心承诺”。

**团队分析**：
1. **SKILL.md 解析**：语法相对简单（Markdown frontmatter + 目录结构），工作量可控（约 1-2 天）。
2. **Experience 经验层**：依赖 Grafeo 的 Episodic/Semantic 层，该部分已完成。
3. **agentskills.io 兼容**：需要额外的 schema 适配和导入/导出逻辑，工作量较大（约 3-5 天）。
4. **当前影响**：Skill 系统是 Agent 能力扩展的核心机制，但 Phase 2 的 Agent 示例（weather/calendar/doc-writer）已能通过 tools + memory 实现基本功能，Skill 缺失不影响 MVP 运行。

**建议方案（分两阶段）**：
- **Phase 2 补充**：实现 SKILL.md 解析（`skills/mod.rs` + `skills/parser.rs`），支持基本目录结构和 frontmatter 解析。
- **Phase 3 延期**：agentskills.io 兼容、Grafeo 经验层绑定、Skill 路由策略。

**团队倾向**：接受分两阶段方案，**立即执行 Phase 2 补充（SKILL.md 解析）**，确保 PRD P0 承诺部分兑现。

---

### 四、执行计划汇总

| 优先级 | 任务 | 类型 | 预估工作量 |
|--------|------|------|-----------|
| **P0** | SKL-01 SKILL.md 解析实现 | 改代码 | 1-2 天 |
| **P1** | RUN-12 Rate Limit 分层（PaymentRequired） | 改代码 | 0.5 天 |
| **P2** | 文档同步（TOL-01、MEM-04、MEM-09、GTW-08、GTW-10、SYS-03、SEC-08、COM-05、PERF-01） | 改文档 | 1 天 |
| **P2** | 初稿误判纠正（RUN-04~06、GTW-12） | 改 review 文档 | 0.1 天 |

---

> 以上为开发团队二次审查意见，待 GLM 复核后更新为终稿。
