# Gateway 组件详细设计

> 版本：v3.0 | 更新日期：2026-04-09

---

Gateway 是一个常驻的系统级进程（可表现为系统托盘应用），使用 Rust 实现。Gateway **不代理 Agent 的业务逻辑**（不代理 LLM 调用、不代理工具执行），只负责必须集中化的协调工作。

## 1. Package Manager

- **安装**：解压 `.agent` 到 `~/.local/share/agent-gateway/agents/<agent_id>/`，校验 manifest 完整性，记录版本。安装前必须验证包签名（详见 [02-agent-package.md](./02-agent-package.md)），签名无效或与已安装版本签名不一致则拒绝安装。
- **卸载**：删除对应目录，可选备份用户数据（含私有 Grafeo）。
- **升级**：保留 `data/` 和用户修改的 `config/`，替换其他文件。若 runtime_version 不兼容则提示用户。升级时校验新包签名证书指纹必须与已安装版本一致。
- **仓库支持**：可配置多个 HTTP 仓库源（类似 apt），定期检查更新。仓库提供的 .agent 包必须经过签名。

## 2. 生命周期管理器

**启动策略：**
- 按需启动：当收到匹配 trigger 的消息或用户显式调用时启动。
- 常驻：用户可标记某 Agent 开机自启。
- 定时启动：由 cron 表达式触发。

**进程管理：**
- 使用 `std::process::Command` 创建子进程，设置独立工作目录、环境变量。
- 启动参数注入：Agent 包路径、Gateway Socket 路径、Agent ID、工作区路径。
- API Key 分发：Agent Runtime 连接 Gateway 后，通过 Socket 传输 Key（不通过环境变量，避免 ps 泄露）。
- 健康检查：如果 Agent 进程退出，根据退出代码决定是否自动重启（可配置）。

**休眠与唤醒：**
- 采用杀死重启策略：空闲超时后直接杀死 Agent Runtime 进程，下次需要时重新 spawn。
- Agent 的状态通过私有 Grafeo 持久化，启动时从 Memory 恢复上下文。
- 不使用 SIGSTOP/SIGCONT（Windows 不支持、进程仍占内存、状态序列化困难）。
- Agent 可在 manifest 中声明 `startup_timeout_ms`，Gateway 据此判断是否需要预热（提前拉起）。

## 3. Intent Router

**输入源：**
- 用户界面（CLI/GUI）发出的请求。
- 定时任务触发器。
- 其他 Agent 通过 Gateway 转发的 Intent 消息（见 [06-communication.md](./06-communication.md)）。

**路由规则：**
- 根据消息中的 `target` 字段直接路由到目标 Agent。
- 若目标 Agent 未运行，则按需启动。
- 若未指定 target，则匹配已安装 Agent 的 manifest 中 `triggers.message.pattern`。

## 4. 沙箱配置器

Gateway 在启动 Agent Runtime 时根据 manifest 配置沙箱参数，之后由 OS 层面执行隔离。各平台实现方式不同，但隔离目标一致。

**跨平台隔离策略对照：**

| 隔离维度 | Linux | Windows | macOS | Android | iOS |
|---------|-------|---------|-------|---------|-----|
| **进程模型** | spawn 独立进程 | spawn 独立进程 | spawn 独立进程 | 单进程多线程 / Service | 单进程多线程 / Extension |
| **文件隔离** | bubblewrap `--bind` | 受限令牌 + ACL | App Sandbox | 系统沙箱 | 系统沙箱 |
| **网络隔离** | `--unshare-net` | Firewall API | Network Extension | 系统沙箱兜底 | 系统沙箱兜底 |
| **系统调用限制** | seccomp-bpf | 无（靠 Job Object） | sandbox-exec | 系统级 | 系统级 |
| **资源限制** | cgroups / rlimit | Job Object limits | rlimit | 系统级 | 系统级 |
| **WASM 引擎** | Wasmtime (JIT) | Wasmtime (JIT) | Wasmtime (JIT) | wasmi (解释器) | wasmi (解释器，iOS 禁止 JIT) |
| **数据目录** | XDG (`~/.local/share/`) | `%APPDATA%\AgentGateway\` | `~/Library/Application Support/AgentGateway/` | `context.getFilesDir()` | appSupportDir |

**路径解析统一接口：**

```rust
fn app_data_dir() -> PathBuf {
    // 各平台返回符合系统规范的路径
    // Linux:   ~/.local/share/agent-gateway/
    // Windows: C:\Users\<user>\AppData\Local\AgentGateway\
    // macOS:   ~/Library/Application Support/AgentGateway/
    // Android: /data/data/com.rollball.gateway/files/
    // iOS:     <appSupportDir>/AgentGateway/
}
```

**平台实现示例：**
```bash
bwrap \
    --ro-bind /usr /usr \
    --ro-bind /lib /lib \
    --ro-bind /bin /bin \
    --ro-bind /usr/lib/agent-gateway/agent-runtime /app \
    --bind <agent_workspace> /workspace \
    --dev /dev \
    --proc /proc \
    --unshare-net \              # 默认禁止网络（需网络时按 manifest 白名单配置）
    --die-with-parent \
    agent-runtime /workspace/agent-package --socket /tmp/gateway.sock
```

**Windows：**
- `CreateRestrictedToken` + Job Object + 文件系统 ACL

**macOS：**
- `sandbox-exec` 配置文件

## 5. Key Vault

集中管理所有 LLM API Key，加密存储：

```
~/.config/agent-gateway/vault/
├── openai_key.enc
├── anthropic_key.enc
└── vault.key               # 主密钥，用户密码派生
```

- Agent manifest 中用 `vault:openai_key` 引用 Key，不存明文。
- Agent Runtime 启动后通过 Gateway Socket 获取 Key（一次性传输，不通过环境变量）。
- Key 在 Rust 侧零拷贝/密封存储（使用 secrecy::SecretString），LLM Client 直接使用该 Secret 签名请求，WASM 插件层绝对没有 API 能读取到该字符串。

## 6. Budget Tracker

接收 Agent Runtime 上报的 LLM 用量，维护跨 Agent 的统计：

- 每个 Agent 有独立的日/月 Token 和费用限额。
- 超限时向 Agent 发送信号（stop / fallback / warn）。
- 提供预算查询接口供 Agent 本地预检。

## 7. Rate Limiter

协调多 Agent 对同一 LLM Provider 的并发请求，避免触发 API Rate Limit：

- Agent 调 LLM 前通过 Gateway 申请速率令牌（极轻量 RPC，< 0.1ms）。
- Gateway 基于 Provider 的 RPM/TPM 限制分配令牌。

## 8. 配置与数据存储

- **Gateway 自身配置**：`~/.config/agent-gateway/config.toml`（含 Vault 配置、仓库列表、默认 LLM 配置等）。
- **每个 Agent 的工作区**：`~/.local/share/agent-gateway/agents/<agent_id>/workspace/`：
  - `data/`：从包中复制，可读写。
  - `config/`：用户可修改的配置（初始来自包内 config）。
  - `memory/`：私有 Grafeo 数据库文件（`private.grafeo`）。
  - `runtime/`：临时文件（socket、pid）。
- **日志**：Gateway 收集所有 Agent 的 stdout/stderr，写入 `~/.local/share/agent-gateway/logs/`，支持按 Agent 过滤。
