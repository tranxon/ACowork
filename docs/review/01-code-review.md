# Rollball Phase 1 源码审查报告

> 审查日期：2026-04-20
> 审查范围：crates/ 下所有 7 个 crate 的 Phase 1 实现源码
> 对照标准：docs/plan/plan-p1.md、docs/module-design/01~05

---

## 一、总体评价

Phase 1 代码完成度高，S1~S3 全部任务已标记"完成"，7-crate workspace 结构清晰，核心数据类型完整，单元测试覆盖了主要路径。代码风格统一，thiserror + ? 传播使用规范，ZeroClaw 借鉴标注到位。

但审查中也发现了一些需要关注的问题：安全关键路径存在简化不足（签名验证未真正集成到安装流程、VaultFacade 未接入 rollball-vault）、路径遍历防护有绕过风险、unsafe 使用缺乏合理性论证、主循环流式处理缺失等。以下按严重度分类。

---

## 二、严重度定义

| 级别 | 含义 |
|------|------|
| **P0** | 安全漏洞或数据丢失风险，必须在合入前修复 |
| **P1** | 设计合规性偏差或逻辑缺陷，影响核心功能正确性，Phase 1 交付前应修复 |
| **P2** | 代码质量/可维护性问题，建议修复但不阻塞交付 |

---

## 三、各 Crate 审查详情

### 3.1 rollball-core

**设计合规性**：与 01-core.md 高度一致。AgentManifest 所有字段齐全，Protocol 6 种 Request/Response 完整，Tool/Provider trait 定义准确，MemoryStore trait 已为 Phase 2 预留。

**优点**：
- Permission 的 matches() 实现了宽→窄匹配逻辑（Network(None) 匹配 Network(Some)），设计精巧
- Frame 线格式有完整的边界检查和长度校验
- Schema 清洗逻辑正确处理了 allOf/oneOf/anyOf 递归
- 单元测试充分（33 tests per plan-p1.md）

**问题**：

1. **[P1] RollballError 过于宽泛** — 所有错误变体都是 `String` 类型，没有结构化错误码。Provider 的 `Provider(String)` 无法区分 429/401/500 等 HTTP 状态码，导致 ReliableProvider 不得不做字符串匹配判断是否可重试（`msg.contains("429")`），非常脆弱。建议 Provider 错误至少增加 `status_code: u16` 字段。

2. **[P2] Permission::FilesystemRead/Write 缺少 None→Some 匹配注释** — `matches` 方法已正确实现宽→窄，但 `Permission::Network(None)` 匹配 `Permission::Network(Some("evil.com"))` 意味着声明了 `network` 权限就等于放行所有 URL。这是设计意图还是需要区分 URL 白名单？建议在代码注释中明确。

3. **[P2] Identity 结构过于简单** — 仅有 6 个字段，设计文档 v3.4 中 Identity 的 Zone/PrivacyLevel 概念未体现。Phase 1 可接受，但建议加 `TODO(Phase 2)` 注释。

---

### 3.2 rollball-sign

**设计合规性**：与 05-vault-sign.md 基本一致。SigningBlock 二进制格式完整，三 CLI 工具可用，sign+verify 往返测试通过。

**优点**：
- 签名块格式有 magic 校验和 size prefix/suffix 双重校验，防篡改设计良好
- 篡改包验证测试（test_verify_tampered_package）验证了摘要不匹配会被检测
- 自定义 hex 编解码避免了额外依赖

**问题**：

1. **[P0] 签名块存储方式与设计文档不一致** — 设计文档明确要求"Signing Block 插入在 Central Directory 之前"（APK v2 思路），但实际实现将签名块作为 ZIP entry `META-INF/SIGNING.BLOCK` 存储。这意味着签名块可以被轻易删除/替换而 ZIP 结构仍合法。虽然 verify 会检测缺失，但**设计文档的安全模型是基于二进制级别的不可分割性**，当前实现降低了篡改门槛。且安装流程中签名验证入口名大小写不一致（sign.rs 用 `META-INF/SIGNING.BLOCK`，install.rs 用 `META-INF/signing.block`）。

2. **[P1] 证书验证 verify_chain() 实质上全部通过** — `verify_chain` 对 Developer 和 Platform 证书都返回 `Ok(true)`，没有任何真实验证。Phase 1 文档要求"Developer 自签名验证"至少应该验证证书的公钥与签名中的公钥一致，而不是只要能 JSON 解析就通过。

3. **[P1] install.rs 签名验证未真正委托给 rollball-sign** — `install.rs` 第 23-29 行检查 `META-INF/signing.block` 存在后仅打日志，没有调用 `rollball_sign::verify::verify_package()`。设计文档明确要求"签名验证委托（调用 rollball-sign 验签）"，且 Phase 1 的核心安全主张是"未签名/无效包拒绝加载"。

4. **[P2] SelfSignedCert 使用 JSON 而非 X.509** — Cargo.toml 引入了 `x509-cert` 依赖但未使用。keygen.rs 注释提到"Full X.509 support in Phase 2"，但当前 JSON 格式没有防伪造保护（任何人都可以手写一个 JSON 证书声称是 Platform 类型）。

---

### 3.3 rollball-vault

**设计合规性**：与 05-vault-sign.md 一致。Vault open/unlock/store/retrieve/list API 完整，Argon2id + ChaCha20-Poly1305 加密正确。

**优点**：
- 加密层实现正确：nonce 随机生成、密钥长度校验、tampered data 检测
- Argon2id 参数（64MB/3 iterations/4 parallelism）是保守安全的选择
- Vault::lock() 正确零化主密钥，Drop trait 也调用 lock()
- 错误密码解密失败测试通过

**问题**：

1. **[P1] VaultFacade 未接入 rollball-vault crate** — `rollball-gateway/src/vault/mod.rs` 的 VaultFacade 是一个纯内存 HashMap，`unlock()` 方法接受密码但直接设 `unlocked = true`，完全没有调用 `rollball_vault::Vault`。这意味着：
   - API Key 以明文存储在内存 HashMap 中，无加密保护
   - Gateway 重启后所有 Key 丢失
   - rollball-vault 的完整加密存储能力未被使用

   rollball-gateway 的 Cargo.toml 已声明 `rollball-vault` 依赖，但代码中没有 `use rollball_vault`。

2. **[P2] master_key 用 Vec<u8> 而非 SecretString** — 设计文档要求 Key "不暴露在环境变量或命令行参数"，Vault::retrieve 正确返回 SecretString，但 Vault 内部的 `master_key: Option<Vec<u8>>` 未用 secrecy 保护。虽然 lock() 时做了零化，但 Vec 的零化不能保证编译器不会优化掉 dead store。建议使用 `zeroize::Zeroize` 或 `secrecy::Secret<Vec<u8>>`。

3. **[P2] KeyRelease 响应中 api_key 是明文 String** — `GatewayResponse::KeyReleaseResult { api_key: String }` 将 Key 以明文 JSON 传输。设计文档说"一次性分发"，但 IPC Socket 传输中 Key 以 String 形式存在于 serde_json Value 中，无法保证消费后被零化。

---

### 3.4 rollball-runtime

**设计合规性**：与 02-runtime.md 大体一致。9 步主循环、LoopDetector 三模式三级、History FIFO+折叠、13 内置工具、Provider OpenAI+Ollama+Router+Reliable 全部实现。

**优点**：
- AgentLoop 主循环步骤①~⑨与设计文档一一对应
- LoopDetector 三种检测模式（ExactRepeat/PingPong/NoProgress）+三级响应（Warning/Block/Break）完整实现
- HistoryManager 三种裁剪策略（FIFO/ToolResult折叠/紧急裁剪）齐全
- 工具注册表 activate() 的三层装饰器（PermissionChecked→PathGuarded→RateLimited）架构清晰
- OpenAI Provider 的流式处理通过 mpsc channel + ChannelStream 实现了 Stream trait
- ReliableProvider 正确区分了 retryable vs balance_exhausted

**问题**：

1. **[P0] PathGuardedTool.validate_path() 存在路径遍历绕过** — 使用简单的 `starts_with` 字符串前缀检查，未做路径规范化。攻击方式：
   - `path = "/tmp/agent-workdir/../../etc/passwd"` — 前缀匹配通过，但实际解析到 `/etc/passwd`
   - `path = "/tmp/agent-workdir-eval/secret"` — 前缀匹配通过，但指向不同目录
   
   修复建议：使用 `std::fs::canonicalize()` 或 `path-clean` crate 规范化后再比较，并拒绝包含 `..` 的路径。

2. **[P1] 主循环缺少流式处理（③ Streaming）** — 设计文档要求"检测到 tool_calls 立即中断 streaming"，但当前 `AgentLoop::run()` 仅调用 `provider.chat()` 非流式接口。虽然 OpenAI Provider 实现了 `chat_stream()`，但主循环未使用。这意味着用户无法看到逐步生成的文字，体验较差。

3. **[P1] ⑦ Usage Report 未实际发送** — 第 246 行仅打日志 "Usage report would be sent here (Phase 1: log only)"。设计文档要求"用量上报 → ipc_client.send(UsageReport) // 异步不阻塞"。虽然 IPC 客户端已实现，但主循环没有持有 `GatewayClient` 引用。Phase 1 至少应该通过 IPC 发送一个简单的 UsageReport。

4. **[P1] ⑨ DevMode 控制未实现** — 设计文档要求主循环最后一步是 "DevMode 控制 → debug.step(iteration)"，当前完全跳过。虽然 Phase 1 暂不需要完整的 Debug Protocol，但步骤⑨的位置应该至少预留一个 `// TODO(Phase 5): DevMode step control` 占位。

5. **[P1] 主循环缺少 ③ Reactive Recovery** — 设计文档要求当 LLM 调用返回上下文溢出错误时触发 "Reactive Recovery（Emergency History Trim）"。当前 `loop_.rs` 第 115-121 行 LLM 错误直接返回 Err，没有尝试 `history.emergency_trim()` 后重试。

6. **[P2] BudgetGuard 用 session_tokens 代替 daily_tokens** — BudgetGuard 用 `session_tokens` 累加，但检查的是 `daily_tokens` 限额。单次会话的 token 数不可能达到日限额（如 100K），导致预算检查形同虚设。Phase 1 应至少在 Gateway 侧维护真实的日/月累计用量。

7. **[P2] Token 估算过于粗糙** — `estimate_tokens()` 使用 4 字符/token 的固定比例，对中文（约 1.5 字符/token）和代码（约 3 字符/token）误差较大。设计文档要求"Token 计数误差 < 5%"，当前可能达到 50%+。建议至少区分 CJK 字符。

8. **[P2] LoopDetector.check_exact_repeat() 重置后 count 归零** — 第 174-175 行检测到循环后重置 `count = 0, last_signature = None`，导致同一工具下次调用从 count=1 重新计数，需要再 3 次才触发。这意味着"三级渐进响应"实际上永远停在 Warning 级别（因为 hit_counts 虽然累加，但 state 每次重置后需要 3 次连续相同调用才触发下一次检测）。Escalation 测试通过是因为它用 9 次连续调用绕过了重置逻辑。

---

### 3.5 rollball-gateway

**设计合规性**：与 03-gateway.md 基本一致。CLI 子命令、IPC 6 种 Handler、包安装/卸载/升级、生命周期管理均实现。

**优点**：
- IPC Server 的 6 种 Request Handler 全部实现路由
- Process spawn/kill 跨平台处理（Unix process_group / Windows taskkill）
- 健康检查跨平台实现（Linux /proc / Windows tasklist / macOS ps）
- 包安装器正确检查重复安装和缺失 manifest
- SessionManager 管理连接会话

**问题**：

1. **[P0] Gateway.run() 使用裸指针 unsafe** — 第 52 行 `let state_ptr = &mut self.state as *mut GatewayState;`，第 78 行 `let state = unsafe { &mut *state_ptr };`。这段 unsafe 完全不必要——`run()` 方法已有 `&mut self`，可以直接使用 `&mut self.state`。裸指针的唯一"理由"是 `ipc_server.run(state)` 需要 `&mut GatewayState`，但完全可以用 `self.state` 直接传递。这个 unsafe 在多线程环境下可能导致未定义行为。

2. **[P1] GatewayState 无并发保护** — `GatewayState` 包含 `HashMap<String, AgentInfo>` 和 `VaultFacade`，在 IPC server 处理连接时被 `&mut` 引用，但 idle timeout checker 通过 `tokio::spawn` 在另一个 task 中运行，理论上需要访问 state。虽然当前 idle checker 只是打日志，但 Phase 2 真正实现时会遇到数据竞争。建议现在就用 `Arc<Mutex<GatewayState>>`。

3. **[P1] install.rs 未拒绝未签名包** — 第 27-29 行，当 ZIP 没有 signing block 时仅 `tracing::warn` 并继续安装。设计文档明确要求"签名无效拒绝安装"，Phase 1 至少应在非 dev-mode 下拒绝未签名包。

4. **[P1] IPC Server 是同步阻塞的** — `IpcServer::run()` 是同步循环，一次只处理一个连接。设计文档要求"多 Runtime 并发连接"，当前实现是串行处理，第二个 Agent 必须等第一个断开。这对 Phase 1 的单 Agent 场景可接受，但需要在代码中明确标注限制。

5. **[P2] 升级缺少签名一致性校验** — `upgrade.rs` 应校验升级前后签名者指纹一致（设计文档："签名一致性校验：作者指纹必须一致"），但当前实现只是删除旧包再安装新包，没有指纹比对。

---

### 3.6 rollball-memory / rollball-grafeo

**设计合规性**：Phase 1 预期是骨架占位。

**问题**：

1. **[P2] rollball-memory/store.rs 仅 107 字节** — `store.rs` 只有一行 `unimplemented!()` 占位，但 rollball-runtime 的 Cargo.toml 依赖了 `rollball-memory`。建议至少提供一个 InMemoryStore 的 Phase 1 实现，否则 memory_store/memory_recall 工具无法正常工作。

2. **[P2] Grafeo 全部 unimplemented** — grafeo.rs/graph.rs/decay.rs/retrieval.rs 全部是占位符，这符合 Phase 2 规划，但 Runtime 的 memory 工具依赖 Grafeo 后端，Phase 1 至少需要一个 stub 实现。

---

## 四、跨 Crate 问题

### 4.1 [P0] 签名验证链路断裂

这是最严重的跨 Crate 问题：

1. `rollball-sign` 实现了完整的签名/验签逻辑
2. `rollball-gateway` 的 Cargo.toml 声明了 `rollball-sign` 依赖
3. 但 `install.rs` 的签名验证只是检查 entry 是否存在，没有调用 `rollball_sign::verify::verify_package()`
4. 且 entry 名大小写不一致（sign.rs 用 `SIGNING.BLOCK`，install.rs 检查 `signing.block`）

修复方案：install.rs 应调用 `rollball_sign::verify::verify_package()` 并在验证失败时拒绝安装。

### 4.2 [P1] Vault 集成链路断裂

1. `rollball-vault` 实现了完整的加密存储
2. `rollball-gateway` 的 Cargo.toml 声明了 `rollball-vault` 依赖
3. 但 VaultFacade 是纯内存 HashMap，未使用 rollball-vault

修复方案：VaultFacade 应内部持有 `rollball_vault::Vault` 实例，unlock() 调用 `vault.unlock(password)`，store/get 委托给 vault。

### 4.3 [P1] Runtime IPC 客户端未与主循环集成

1. `rollball-runtime/src/ipc/client.rs` 实现了 GatewayClient
2. 但 AgentLoop 没有持有 GatewayClient 引用
3. KeyRelease、UsageReport、BudgetQuery 都未通过 IPC 实际调用

---

## 五、Top 5 关键问题（按严重度排序）

| # | 严重度 | 问题 | 位置 |
|---|--------|------|------|
| 1 | **P0** | PathGuardedTool 路径遍历绕过 | runtime/tools/wrappers.rs:92-114 |
| 2 | **P0** | Gateway.run() 不必要的 unsafe 裸指针 | gateway/gateway/mod.rs:52,78 |
| 3 | **P0** | 签名验证未集成到安装流程 | gateway/package_manager/install.rs:23-29 |
| 4 | **P1** | VaultFacade 未接入 rollball-vault 加密 | gateway/vault/mod.rs |
| 5 | **P1** | 主循环缺少流式处理和 Reactive Recovery | runtime/agent/loop_.rs |

---

## 六、Top 5 亮点

| # | 亮点 | 说明 |
|---|------|------|
| 1 | 签名块二进制格式设计精良 | magic + size prefix/suffix 双校验，防篡改能力强 |
| 2 | 工具安全装饰器架构清晰 | 三层装饰器（Permission→Path→RateLimit）可组合、可扩展 |
| 3 | LoopDetector 三模式三级设计完整 | ExactRepeat/PingPong/NoProgress + Warning/Block/Break，远超简单循环检测 |
| 4 | Vault 加密实现专业 | Argon2id 参数保守、ChaCha20-Poly1305 AEAD、SecretString 返回、Drop 零化 |
| 5 | 跨平台进程管理考虑周全 | Unix process_group 隔离、Windows taskkill、/proc/tasklist/ps 三平台健康检查 |

---

## 七、修复优先级建议

### Phase 1 交付前必须修复（P0）

1. **PathGuardedTool 路径遍历修复**：使用 `canonicalize` 或拒绝 `..` 路径
2. **移除 unsafe 裸指针**：直接传 `&mut self.state`
3. **签名验证集成到安装流程**：调用 `rollball_sign::verify::verify_package()` 并统一 entry 名

### Phase 1 交付前建议修复（P1）

4. **VaultFacade 接入 rollball-vault**：持有 Vault 实例，委托加密存储
5. **主循环补充 Reactive Recovery**：LLM 返回上下文溢出错误时 emergency_trim + 重试
6. **Usage Report 通过 IPC 发送**：AgentLoop 持有 GatewayClient，异步上报

### 可延至 Phase 2（P2）

7. Token 估算精度优化
8. BudgetGuard 真实日/月累计
9. RollballError 结构化错误码
10. rollball-memory 提供 InMemoryStore stub

---

## 八、测试覆盖评估

| Crate | 计划测试数 | 实际测试情况 | 缺失关键测试 |
|-------|-----------|-------------|-------------|
| rollball-core | 33 | 覆盖充分 | Protocol 全 6 种 Request 序列化往返 |
| rollball-sign | 21 | 覆盖充分 | 签名块在 ZIP 中的完整性（非 entry 模式） |
| rollball-vault | 20 | 覆盖充分 | 并发 store/retrieve 安全性 |
| rollball-runtime | - | 核心模块有测试 | AgentLoop 端到端（需 mock Provider） |
| rollball-gateway | - | 基础功能有测试 | IPC 端到端、多 Agent 并发 |

最大的测试缺口是 **端到端集成测试**（plan-p1.md S4.2），当前 `tests/` 目录下只有一个空的 `integration_test.rs`。

---

## 九、结论

Phase 1 代码在架构设计和模块完整性上表现优秀，7-crate workspace 结构清晰，核心数据流路正确。但有三个 P0 级问题（路径遍历、unsafe 滥用、签名验证断裂）需要在交付前修复，否则安全声称无法兑现。P1 级问题（Vault 未集成、Reactive Recovery 缺失）影响功能完整性但不阻塞基本流程。

总体评估：**架构 9/10，实现 7/10，安全 5/10**。修复 P0 问题后可达交付标准。
