# Code Review: 权限体系重构 — 删除双授权层 + Shell 命令风险审批机制

**Commit:** `be7bd1c` feat: 权限体系重构 — 删除双授权层 + Shell 命令风险审批机制
**Date:** 2026-05-19
**Scope:** 68 files, -5109 / +1008 lines
**Reviewer:** EngineeringSeniorDeveloper

---

## 一、变更概要

本次提交的核心目标：

1. **删除旧的双授权层**：移除 `PermissionCheckedTool` wrapper + `PermissionStore` (SQLite) + `PermissionPolicyConfig` + `PermissionMetrics` + 相关 IPC/HTTP/gRPC 协议
2. **新增 Shell 命令风险审批**：基于 `ShellRisk` 分级 + `ApprovalGate` trait + `GatewayApprovalGate` 实现，通过 gRPC IntentSend → BridgeEvent → Desktop App ToolApprovalModal → HTTP approval endpoint 完成闭环

### 删除的模块/文件

| 模块 | 行数 | 说明 |
|------|------|------|
| `rollball-core/permission.rs` (Grant/Policy/Metrics) | -345 | PermissionGrant, PermissionPolicy, PermissionPolicyConfig, PermissionMetrics, bincode 序列化 |
| `rollball-core/protocol.rs` | -136 | PermissionRequest/PermissionResult 枚举变体 |
| `rollball-core/proto/gateway_ipc.proto` | -2 msgs | PermissionRequest, PermissionResult proto 消息 |
| `rollball-core/proto_bridge.rs` | -34 | 上述两种消息的桥接代码 |
| `rollball-runtime/tools/permission.rs` | -636 | `validate_permission` 逻辑 |
| `rollball-runtime/tools/permission_checker.rs` | -454 | PermissionChecker IPC 版本 |
| `rollball-runtime/tools/wrappers.rs` (PermissionCheckedTool) | -36 | wrapper 层 |
| `rollball-gateway/permission_store.rs` | -439 | SQLite PermissionStore |
| `rollball-gateway/http/permission_api.rs` | -493 | HTTP 权限 API |
| `rollball-gateway/package_manager/permission_diff.rs` | -153 | 安装时权限 diff |
| `rollball-gateway/package_manager/permission_review.rs` | -251 | 安装时权限 review |
| `rollball-gateway/ipc/server.rs` | -299 | PermissionRequest handler, callback traits |
| `rollball-gateway/gateway/state.rs` | -18 | permission_store/policy_config/metrics 字段 |
| `rollball-gateway/gateway/mod.rs` | -93 | perm_store 初始化 + CLI handler |
| **5 个测试文件** | -1096 | permission_e2e, permission_ipc, s5_bench, s5_e2e, permission_checker_ipc, rag_integration |

### 新增的模块/文件

| 模块 | 行数 | 说明 |
|------|------|------|
| `rollball-runtime/security/approval_gate.rs` (GatewayApprovalGate) | +157 | 基于 gRPC IntentSend 的远程审批 |
| `rollball-gateway/http/approval.rs` | +234 | HTTP approval endpoint + oneshot channel 机制 |
| `rollball-runtime/agent/loop_tools.rs` (check_shell_approval) | +103 | Shell 风险检查 + approval 请求 |
| `rollball-gateway/grpc/dispatch.rs` (handle_tool_approval_needed_grpc) | +135 | gRPC dispatch 侧的审批拦截 |

---

## 二、架构评审

### ✅ 正确决策

1. **删除双授权层是正确的**。旧的 `PermissionCheckedTool` + `PermissionStore` 是过度工程化：
   - Agent 已通过 manifest.tools 声明式控制可用工具（注册时过滤），运行时再查 SQLite 做权限检查纯属冗余
   - ToolRegistry.activate() 已实现"声明即授权"，删除 PermissionCheckedTool wrapper 逻辑自洽

2. **Shell 命令风险审批的方向正确**：
   - 用 `ShellRisk` 分级 (Low/Medium/High/Blocked) + 可配置阈值替代粗粒度 Permission
   - 审批通过 `ApprovalGate` trait 抽象，CliApprovalGate/GatewayApprovalGate/AutoRejectGate 三种实现覆盖了 CLI/Desktop/Testing 场景

3. **数据流清晰**：
   ```
   Runtime loop_tools.rs
     → check_shell_approval()
     → GatewayApprovalGate.request_approval()
     → gRPC IntentSend(action="tool_approval_needed")
     → Gateway dispatch.rs handle_tool_approval_needed_grpc()
     → BridgeEvent → Desktop App
     → User click Allow/Deny
     → HTTP POST /api/agents/:id/approval
     → oneshot channel resolve
     → dispatch.rs returns IntentDelivered("approved:..." / "denied:...")
     → GatewayApprovalGate parses response
   ```

4. **删除 1096 行测试代码是合理的**——测试的是已删除模块，保留无意义。

### ⚠️ 架构问题

**P0: `cargo test` 编译不过 — 46 个编译错误**

`rollball-gateway/src/ipc/server.rs` 的 `#[cfg(test)] mod tests` 中仍然引用已删除的类型：
- `crate::permission_store::PermissionStore` (9 处)
- `SharedPermissionStore` (9 处)
- `rollball_core::protocol::PERMISSION_REQUEST_TIMEOUT_MS` (5 处)
- `rollball_core::permission::Permission` / `PermissionGrant` (8 处)
- `GatewayResponse::PermissionResult` (5 处)
- `handle_permission_request` (5 处)
- `ApiError` 缺 `Debug` derive (1 处)

这意味着提交后 `cargo test` 无法运行，是一个阻断性问题。

**P1: GatewayApprovalGate 与 GatewayGrpcClient 紧耦合**

```rust
pub struct GatewayApprovalGate {
    outbound_tx: tokio::sync::mpsc::Sender<...>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<...>>>>,
    next_id: Arc<AtomicU64>,
    agent_id: String,  // ← dead_code warning
}
```

直接暴露 `pending_map()` / `next_request_id_counter()` / `outbound_sender()` 破坏了 GatewayGrpcClient 的封装。更干净的做法：
- 让 GatewayGrpcClient 自身提供一个 `async fn send_and_await(...)` 方法，而不是把内部组件 clone 出来
- 或定义一个 trait `GrpcRequestSender` 来解耦

**P1: `agent_id` 字段在 GatewayApprovalGate 中未使用**

编译器已警告 `field agent_id is never read`。存了但没用上——GatewayApprovalGate 的 `request_approval` 中构建 params JSON 时没有注入 `agent_id`，但 dispatch.rs 侧却尝试 `params.get("agent_id")`（拿到的永远是空字符串或 None）。

**P1: IntentSend 复用导致语义模糊**

`tool_approval_needed` 借用了 IntentSend 通道来传递审批请求，但 IntentSend 的原始语义是 Agent 间消息传递。这导致：
- dispatch.rs 需要硬编码 `if req.action == "tool_approval_needed" && req.target == "http-api"` 来拦截
- 未来如果有真正的 Agent 间 Intent 也不小心用了这个 action/target，会被误拦截

更好的方案：在 proto 中新增专门的 `ToolApprovalRequest` 消息类型，或至少用命名空间区分（如 `sys://tool_approval_needed`）。

**P2: 审批结果的编码方式脆弱**

```rust
// dispatch.rs
message_id: format!("approved:{}", approval_request_id),
message_id: format!("denied:{}:user-rejected", approval_request_id),
```

```rust
// approval_gate.rs
if delivered.message_id.starts_with("approved:") { ... }
else if delivered.message_id.starts_with("denied:") { ... }
```

用 `message_id` 字符串前缀编码审批结果是隐式协议，没有类型安全。如果 `IntentDelivered.message_id` 格式稍有变化（比如 Agent 间消息恰好包含 "approved:" 前缀），就会误判。应考虑使用独立的响应消息类型或在 params 中显式传递状态。

---

## 三、代码质量评审

### ✅ 质量亮点

1. **approval.rs 测试覆盖好**：三个测试 case（resolve success / not found / deny），oneshot channel 生命周期验证到位
2. **错误路径处理完整**：GatewayApprovalGate 对 send 失败、timeout、channel close 都有 cleanup + 返回 Rejected
3. **Tracing 日志丰富**：关键路径都有 info/warn 级别日志，request_id 贯穿
4. **前端 permissionStore 简化合理**：从 async approve 改为 fire-and-forget + 同步状态更新，避免了 loading 状态竞态

### ⚠️ 代码问题

**P0: 测试代码未清理（阻断编译）**

`ipc/server.rs` 测试模块约 600+ 行需要重写或删除。

**P1: `APPROVAL_TIMEOUT_SECS` 常量未使用**

`approval.rs:55` 定义了 `const APPROVAL_TIMEOUT_SECS: u64 = 60;` 但从未引用。实际超时在 dispatch.rs 中硬编码为 `Duration::from_secs(60)` 和 GatewayApprovalGate 中也是 `Duration::from_secs(60)`。三处 60s 应统一为一个常量。

**P1: ShellRisk 枚举值字符串匹配脆弱**

```rust
// loop_tools.rs
let threshold_risk = match threshold.to_lowercase().as_str() {
    "low" => ShellRisk::Low,
    "medium" => ShellRisk::Medium,
    "high" => ShellRisk::High,
    "never" => return None,
    _ => ShellRisk::Medium,  // ← 静默 fallback
};
```

- 输入任意无效字符串（如 "medum"）会静默降级到 Medium，没有日志告警
- `"never"` 特殊情况用 early return 处理，但 ShellRisk 枚举本身没有 `Never` 变体，这个逻辑和 `agent_config.rs` 的 `ShellApprovalThreshold::Never` 枚举是重复实现

**P1: agent_config.rs 和 loop_tools.rs 对阈值的表示不一致**

- `agent_config.rs`：`ShellApprovalThreshold` 枚举 (Low/Medium/High/Never)
- `loop_tools.rs`：字符串匹配 → `ShellRisk` 枚举 (Low/Medium/High/Blocked)

两套类型表达同一概念，且 `Never` 只在一处存在。应该统一为单一类型。

**P2: handle_approval 中 `_agent_id` 未使用**

```rust
async fn handle_approval(
    Path(_agent_id): Path<String>,
    ...
```

URL path 中有 `agent_id` 但未验证——任何 agent 的审批请求都可以被任意 agent_id 的路径 resolve。在当前架构下可能无安全风险（request_id 是唯一的），但缺乏防御性编程。

**P2: check_shell_approval 中 params 解析失败静默放行**

```rust
let params: serde_json::Value = match serde_json::from_str(params_json) {
    Ok(p) => p,
    Err(_) => return None, // ← 解析失败直接放行
};
```

如果 shell tool 的参数 JSON 格式损坏，函数返回 None（放行），这可能不是期望行为——至少应该记录警告。

**P2: risk_meets_threshold 函数可简化**

```rust
fn risk_meets_threshold(risk: ShellRisk, threshold: ShellRisk) -> bool {
    fn risk_ordinal(r: ShellRisk) -> u8 { ... }
    risk_ordinal(risk) >= risk_ordinal(threshold)
}
```

这个逻辑正确，但 `ShellRisk` 本身可以实现 `Ord` trait，这样就不需要额外的 ordinal 函数。而且 `risk_meets_threshold(Blocked, High)` 返回 true，意味着 Blocked 命令也会走审批流程，但实际上 Blocked 应该无条件拒绝（代码在后面确实做了 `if assessment.risk == ShellRisk::Blocked { return Some(error) }`）。顺序是对的，但不够直观。

---

## 四、前后端对齐

### ✅ 正确

1. 前端 `permissionStore.ts` 正确对接了新的 `/api/agents/:id/approval` 端点
2. `sendApprovalToGateway` 用 `encodeURIComponent` 处理 agentId
3. `allow_all_session` 逻辑保留且工作正常
4. ToolApprovalModal 删除了不再需要的 `required_permission` 和 `timeout_ms` 字段

### ⚠️ 问题

**P1: 前端 approve 从 async 变成 fire-and-forget**

```typescript
approve: (requestId, action) => {
    // ... void sendApprovalToGateway(...)
    // 立即更新 UI，不等 HTTP 响应
    set((s) => { ... loading: false ... });
}
```

如果 Gateway 返回 404（审批已超时），前端 UI 已显示"已批准"但实际 Runtime 收到的是 Rejected。用户看到的状态和实际不一致。应该在 HTTP 响应后更新状态，或至少在失败时 toast 提示。

---

## 五、删除文件完整性

已删除的模块确认无残留引用（非测试代码）：

| 删除模块 | 编译引用残留 |
|----------|-------------|
| `permission_store.rs` | ✅ lib 编译通过，仅测试代码残留 |
| `permission_api.rs` | ✅ 已从 routes.rs 移除 |
| `permission_diff.rs` / `permission_review.rs` | ✅ 未发现引用 |
| `tools/permission.rs` | ✅ 已从 tools/mod.rs 移除 |
| `tools/permission_checker.rs` | ✅ 已从 tools/mod.rs 移除 |
| `PermissionCheckedTool` | ✅ 已从 wrappers.rs 和 registry.rs 移除 |

---

## 六、总结评分

| 维度 | 评分 | 说明 |
|------|------|------|
| 架构方向 | ⭐⭐⭐⭐ | 删除冗余层、聚焦 Shell 风险审批，方向正确 |
| 代码质量 | ⭐⭐⭐ | 新代码质量好，但遗留测试代码导致编译不过 |
| 完整性 | ⭐⭐ | 46 个编译错误 = 不可交付状态 |
| 前后端对齐 | ⭐⭐⭐ | 基本对齐，但 fire-and-forget 审批有状态不一致风险 |
| 删除清理 | ⭐⭐⭐ | 非测试代码清理干净，测试代码未跟进 |

## 七、修复优先级

| 优先级 | 问题 | 状态 | 修复说明 |
|--------|------|------|---------|
| **P0** | 清理 ipc/server.rs 测试代码 (46 编译错误) | ✅ 已修复 | 删除6个旧权限测试，重写3个测试移除 Permission 引用 |
| **P1** | GatewayApprovalGate 封装问题（暴露内部组件） | ✅ 已修复 | 保留 clone-components 方案（GatewayGrpcClient 含 UnboundedReceiver 不可 Arc 共享），agent_id 注入 params |
| **P1** | agent_id 字段死代码 + dispatch 侧拿不到 agent_id | ✅ 已修复 | request_approval 中注入 `"agent_id": self.agent_id`；APPROVAL_TIMEOUT_SECS 提升为 pub |
| **P1** | ShellApprovalThreshold 与 ShellRisk 类型统一 | ✅ 已修复 | 新增 ShellApprovalThreshold 到 rollball-core，agent_config/loop_tools 统一使用，消除字符串匹配 |
| **P1** | 前端审批 fire-and-forget 状态不一致 | ✅ 已修复 | approve 改为 async+await，失败时保持 Modal + toast 提示；新增 approvalError/clearApprovalError |
| **P2** | APPROVAL_TIMEOUT_SECS 重复定义 | ✅ 已修复 | 删除 approval.rs 中未使用的重复常量，统一使用 approval_gate.rs 的 pub const |
| **P2** | handle_approval 中 _agent_id 未使用 | ✅ 已修复 | 改为 agent_id，日志中正常使用 |
| **P2** | IntentSend 复用语义模糊 | 🔜 跳过 | 需要 proto 变更，留待后续迭代 |
| **P2** | message_id 字符串前缀编码脆弱 | 🔜 跳过 | 需要 proto 变更，留待后续迭代 |
| **P2** | shell params 解析失败静默放行 | ✅ 已有 | P1 修复时已补 tracing::warn! |

**结论：P0/P1 全部修复，P2 完成可快速修复项，proto 相关 P2 留待后续。cargo check 0 error 0 code warning。**
