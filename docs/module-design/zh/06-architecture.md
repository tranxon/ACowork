# 架构：依赖关系、数据流、路线图、编译产物、测试

## 1. 模块间依赖关系

```
                    ┌──────────────┐
                    │ rollball-core│ ← 共享类型层
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
    ┌─────────▼──────┐ ┌──▼────────┐ ┌─▼────────────┐
    │rollball-runtime│ │rollball-  │ │rollball-     │
    │                │ │gateway    │ │sign          │
    │    依赖:       │ │           │ │              │
    │  · core        │ │ 依赖:     │ │ 依赖:        │
    │  · grafeo      │ │ · core    │ │ · core       │
    │  · sign(验证)  │ │ · sign    │ │ (无运行时依赖)│
    └────────┬───────┘ │ · vault   │ └──────────────┘
             │         └───┬───────┘
             │             │
    ┌────────▼──────┐ ┌───▼──────────┐
    │rollball-grafeo│ │rollball-vault│
    │               │ │              │
    │ 依赖:         │ │ 依赖:        │
    │ · core(Memory │ │ · core       │
    │   trait)      │ │   (无)       │
    └───────────────┘ └──────────────┘
```

**关键约束**：
- `rollball-core` 不依赖任何其他内部 crate
- `rollball-grafeo` 仅依赖 `rollball-core` 的 Memory trait
- `rollball-runtime` 和 `rollball-gateway` 之间**没有直接依赖**，它们通过 IPC 通信
- `rollball-sign` 是独立工具 crate，不依赖运行时 crate

---

## 2. 数据流与通信

### 2.1 Agent 启动流程（数据视角）

```
Gateway CLI: "start com.example.weather"
    │
    ├─1→ PackageManager: 读取已安装 manifest → AgentManifest
    │
    ├─2→ Gateway UserProfile: 读取 identity → {name, city, language}
    │
    ├─3→ Vault: 获取 api_key_ref → SecretString
    │
    ├─4→ SandboxConfig: 从 manifest 生成沙箱参数
    │
    └─5→ LifecycleManager: spawn agent-runtime 进程
         参数: --package-path, --socket, --agent-id, 
               --workspace, --identity (JSON), --dev-mode
```

### 2.2 Agent 主循环（数据视角）

```
Agent Runtime 进程启动
    │
    ├─1→ PackageLoader: 解析 ZIP → (manifest, prompts, skills, config)
    │
    ├─2→ IPC Client: 连接 Gateway Socket → handshake
    │
    ├─3→ IPC Client: KeyRelease → SecretString (存入进程内存)
    │
    ├─4→ Grafeo::open(workspace/memory/private.grafeo)
    │
    └─5→ 主循环:
         每轮迭代:
         ├─ Context::build() → ChatMessage[]
         ├─ Provider::chat() → ChatResponse
         ├─ ToolDispatcher::dispatch() → ToolResult[]
         ├─ History::append()
         └─ IPC Client: UsageReport (异步)
```

### 2.3 Intent 跨 Agent 通信

```
天气 Agent → IPC Client → GatewayRequest::IntentSend
    │
    ▼
Gateway IntentRouter:
    ├─ 查找 target agent
    ├─ 若未运行 → LifecycleManager::start_agent()
    └─ 转发 Intent → Agent B 的 IPC 连接
    │
    ▼
日历 Agent ← GatewayResponse::IntentReceived
    ├─ 处理 Intent
    └─ 返回结果 → Gateway → 天气 Agent
```

---

## 3. 与路线图的映射

| Phase | 需实现的 Crate | 核心模块 |
|-------|--------------|---------|
| **Phase 1: MVP** | core, runtime, gateway, sign, vault | `core`: manifest + protocol + traits<br>`runtime`: agent/loop + package/loader + providers/openai + tools/builtin(核心17) + tools/memory(5) + tools/agent(intent_send, ask_user) + ipc/client<br>`gateway`: package_manager + lifecycle + ipc/server + vault<br>`sign`: keygen + sign + verify<br>`vault`: 加密存储 |
| **Phase 2: Memory** | + grafeo | `grafeo`: 全部模块（episodic + semantic + fulltext + hybrid + embedding）<br>`runtime`: memory/ 模块<br>`gateway`: system_agent/identity_injector |
| **Phase 2.5: DevFramework** | + runtime/debug | `runtime`: debug/ 全部模块<br>`gateway`: lifecycle 扩展（克隆 API） |
| **Phase 3: 安全沙箱** | + gateway/sandbox | `gateway`: sandbox/ 各平台实现<br>`runtime`: tools/wasm<br>`core`: permission 增强 |
| **Phase 4: 通信协调** | gateway 扩展 | `gateway`: intent/ + budget/ + rate/<br>`runtime`: tools/gateway 增强 |
| **Phase 5: 云端生态** | + desktop app | `apps/rollball-desktop`: Tauri 应用<br>`gateway`: package_manager/repository |

---

## 4. 编译产物

| 二进制 | 来源 Crate | 说明 |
|-------|-----------|------|
| `agent-runtime` | rollball-runtime | Agent 统一执行引擎 |
| `rollball-gateway` | rollball-gateway | Gateway 守护进程 |
| `rollball-keygen` | rollball-sign | 密钥对生成 |
| `rollball-sign` | rollball-sign | .agent 包签名 |
| `rollball-verify` | rollball-sign | .agent 包验签 |
| `rollball` | (CLI wrapper) | 统一 CLI 入口（聚合子命令） |

**统一 CLI 设计**（Phase 5 实现，Phase 1 先用独立二进制）：

```bash
# Phase 1: 独立二进制
agent-runtime /path/to/weather.agent --socket /tmp/gateway.sock
rollball-gateway start
rollball-keygen --alias my-key
rollball-sign --key my-key.pem --input weather.unsigned.agent
rollball-verify weather.agent

# Phase 5: 统一 CLI
rollball gateway start
rollball agent install weather.agent
rollball agent start com.example.weather
rollball keygen --alias my-key
rollball sign weather.unsigned.agent
rollball verify weather.agent
```

---

## 5. 测试策略

| 测试层级 | 位置 | 说明 |
|---------|------|------|
| 单元测试 | 各 crate `src/` 内 `#[cfg(test)]` | 每个 trait 实现都有测试 |
| 组件测试 | `tests/` per crate | 单 crate 内集成测试 |
| 集成测试 | workspace 根 `tests/` | 跨 crate 测试：Gateway + Runtime 交互 |
| 系统测试 | workspace 根 `tests/` | 完整流程：安装 Agent → 启动 → 对话 → 工具调用 → 停止 |
| Live 测试 | 手动 | 连接真实 LLM API 的端到端测试 |

---

> **下一步**：基于此模块设计，逐个模块细化接口定义。建议从 `rollball-core` 开始，因为它是所有 crate 的基础。
