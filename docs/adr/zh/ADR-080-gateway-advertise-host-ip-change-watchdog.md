# ADR-080：Gateway advertise-host 漂移自愈（if-watch 驱动 pm / doc / embed 三类资源）

**状态**：已采纳
**日期**：2026-09-17
**决策者**：大鱼
**前置 ADR**：ADR-055 D3（Gateway advertise-host 解析）、ADR-064（pm/doc 独立进程）、ADR-073（instance_id 身份分层）
**对称机制**：§6.3.3 / Runtime `mqtt/client.rs` 已有的 **Node 端 → Runtime** endpoint 变化自愈

---

## 1.1 受影响范围（共性问题）

`advertise_host` 不仅拼接进 `pm_mcp_url` / `doc_mcp_url`,也拼接进 embed 进程的 publish URL——三者是同一类共性问题,**只需一个 watchdog 即可全部覆盖**:

| 子系统 | 用 advertise_host? | 走 MQTT retained? | 触发重发路径 | 是否受影响 |
|--------|---------------------|--------------------|--------------|------------|
| `acowork-pm` | ✅ `gw.pm_mcp_url` | ✅ `acowork/global/mcps` | `MqttPublisherTrigger::trigger()` → `publish_mcps()` | ✅ |
| `acowork-doc` | ✅ `gw.doc_mcp_url` | ✅ `acowork/global/mcps` | 同上(同一 payload) | ✅ |
| `acowork-embed` | ✅ `format!("http://{}:{}/v1", gw.advertise_host, eps.port)`(`mqtt/global_resources_builders.rs:267`) | ✅ `acowork/global/embedding_models` | 同 `trigger()` → `publish_embedding_models()` | ✅ |
| `acowork-lsp-relay` | ❌ 由 Node spawn,绑 `127.0.0.1`,Runtime 走 Node proxy | ❌ 走 Node control plane | — | ❌ 不受影响 |
| cloud embedding | ❌ `active_base_url` 来自 provider config | ✅ 但 endpoint 跟 advertise_host 无关 | — | ❌ 不受影响 |

关键复用点:`mqtt/global_resources_publisher.rs::LoopHelper::publish_all()` **一次性 publish 全部 5 个 topic**(providers / mcps / searches / embedding_models / user_profiles),watchdog 一次 `trigger()` 即可让 pm / doc / embed 三者 URL 同步漂移。

---

## 1. 问题

`Gateway::run` 在启动时通过 `config::resolve_advertise_host()` 探测一次本机 LAN IP（例如 `192.168.3.43`），随后该 IP 被烘焙进 `pm_mcp_url` / `doc_mcp_url` 并作为 retained `acowork/global/mcps` 推给每个 Runtime，Runtime 落盘到 `agent_mcp.json`。

**用户切换 Wi-Fi / VPN / 重启路由器后，本机 IP 变化，但 Gateway 进程没重启**，`advertise_host` 仍然指向旧 IP，所有 Runtime 的 `agent_mcp.json` 里的 `pm` / `doc` MCP URL 失效，`mcp_pm__*` 工具调用全部失败（错误：`HTTP request to MCP server failed`，transient）。唯一的恢复方式是手动重启 Gateway。

§6.3.3 已经在 Runtime 端做了对称修复（Node 改 endpoint → Runtime 重发 `http_endpoint`），但 **Gateway 端没有对应的对称机制**，这是本次修复要补齐的。

## 2. 决策

Gateway 进程内新增一个后台 watchdog 任务，订阅 OS 层的接口地址变化事件（Windows `NotifyAddrChange` / Linux `NETLINK_ROUTE` / macOS `SCDynamicStore`），封装由 `if-watch` crate（v3，tokio feature）提供。每次收到 `IfEvent::Up` / `IfEvent::Down`，重跑 `detect_non_loopback_ip()` UDP-trick 探测器，与当前 `gw.advertise_host` 对比：

- **一致**：no-op（避免无意义 republish）。
- **不一致**：原子更新 `gw.advertise_host` + 重新构造 `pm_mcp_url` / `doc_mcp_url` → 调 `MqttPublisherTrigger::trigger()` 触发 `acowork/global/mcps` 重发 → 每个订阅的 Runtime 自动收到新 retained 消息，落盘到 `agent_mcp.json`。

**无条件启动**：watchdog **始终启动**——即使操作者通过 `--advertise-host` 或 `gateway.toml` 显式 pin 了 `advertise_host`。pin 只决定**初始值**，不能阻止后续 IP 漂移（Wi-Fi / VPN / 路由器变更都会让 pin 的 IP 失效）；`reconcile()` 只在检测到的新 IP 与当前值**不同**时才改写 `gw.advertise_host`，因此"pin 且仍然有效"的地址不会被触碰，只有真正失效时才自愈。

## 3. 设计要点

### 3.1 不解析事件 payload，直接重跑探测器

事件触发代表「网络有变化」，但**哪个 IP 是「对外可达的最佳 IP」**这个判断逻辑已经在 `detect_non_loopback_ip()` 里编码（UDP connect 1.1.1.1 trick 选主路由出口）。重跑这个函数比解析事件 payload 后做 IP 优选简单、稳健、复用现有逻辑。

### 3.2 持锁粒度

`reconcile()` 内对 `GatewayState` 的写锁**只在状态变更路径上持有**，调 `trigger()` 前显式 `drop(gw)`，避免跨 `notify_one()` 持锁（`Notify::notify_one` 本身不阻塞，但 tokio 调度器切换可能导致锁持有时间膨胀）。

### 3.3 显式配置的尊重（初始值，非开关）

显式 pin（`--advertise-host` / `[network] advertise_host`）只决定 `resolve_advertise_host` 的**初始值**。watchdog 无条件启动；`reconcile()` 在检测到新 IP ≠ 当前值时改写 `gw.advertise_host` 并 republish。pin 且仍然有效的地址不会被触碰。

### 3.4 零 Runtime 改动

Runtime 端 `mqtt/client.rs::handle_global_mcps` 已经在监听 `acowork/global/mcps` 的 retained 消息并落盘到 `agent_mcp.json`。本次修复 **Runtime crate 零代码改动**——纯 Gateway 单边修复，验证了既有 retained 通道的契约。

### 3.5 事件 API 兼容性

`if-watch` crate 在 `tokio` feature 下重新 re-export `IfWatcher`（Windows: `if_watch::tokio::IfWatcher` 即 `win::tokio::IfWatcher` 的 alias；Linux: `linux::tokio` 同理）。`IfWatcher::new()` 返回 `Result<Self, std::io::Error>`（**同步**，不是 future），`impl Stream<Item = Result<IfEvent, std::io::Error>>`，通过 `tokio_stream::StreamExt::next()` 消费。

新增 `if_watch_api_compiles` 单元测试承担**上游 API drift 守门员**：任何 `if-watch` breaking change 都会让这个测试编译失败，在 CI 阶段被拦截。

## 4. 替代方案对比

| 方案 | 优点 | 缺点 | 是否采纳 |
|------|------|------|---------|
| **A. 周期性轮询** `detect_non_loopback_ip` | 实现最简单（无新依赖） | 粒度难取舍（快了占资源，慢了延迟高），且对「OS 已经知道但应用没问」的事件无感知 | ❌ 否 |
| **B. if-watch 事件驱动**（本方案） | 跨平台原生 API，零延迟，无 CPU 浪费 | 多一个新 crate 依赖（~10KB 编译产物） | ✅ 是 |
| **C. 各平台手写 syscalls** | 零依赖 | 150+ 行平台特定代码，编译矩阵复杂，维护成本高 | ❌ 否 |
| **D. 改 hostname / mDNS** 替代 IP | 一次到位解决多机 / 容器漂移 | 需要 DNS 解析一致性，mDNS 在某些网络环境不可用；改动面远超本次 bug 范围 | ❌ 否（后续 ADR 单独讨论） |
| **E. 加 5 分钟兜底重算** 作为 belt-and-suspenders | 双保险 | 事件驱动本身就是实时，加兜底是 over-engineering，掩盖设计问题 | ❌ 否（砍掉） |

## 5. 影响范围

**新增**：

- `core/acowork-gateway/src/lifecycle/advertise_watchdog.rs`（事件循环 + reconcile）
- `core/acowork-gateway/Cargo.toml`：新增 `if-watch = { version = "3", features = ["tokio"] }`

**修改**：

- `core/acowork-gateway/src/config.rs`：
  - `detect_non_loopback_ip` 由 `fn` 改 `pub(crate) fn`（供 watchdog 调用）
- `core/acowork-gateway/src/lifecycle/mod.rs`：注册新模块
- `core/acowork-gateway/src/gateway/mod.rs`：在 `MqttPublisherTrigger` 创建之后、`Some(trigger)` 返回之前 spawn watchdog

**零改动**：

- Runtime crate：`core/acowork-runtime/` 全树零改动（pm / doc / embed 三类 retained topic 已在 `available_cache.rs` / `mqtt/client.rs` 有现成订阅路径）
- Desktop app
- PM / Doc 子进程（它们走 `127.0.0.1:{port}` 跟 Gateway 通信，不感知 WAN IP）
- Node Agent + LSP relay（由 Node 自己 spawn / 绑 loopback，不走 advertise_host）

## 6. 测试

| 测试 | 位置 | 覆盖 |
|------|------|------|
| `if_watch_api_compiles` | `acowork-gateway/src/lifecycle/advertise_watchdog.rs` | 上游 API drift 守门 |
| `reconcile_keeps_state_when_no_ip_detected` | 同上 | 早返回路径（无 IP 时不破坏状态） |
| `test_build_available_mcps_*`（已有 4 个） | `acowork-gateway/src/mqtt/global_resources_builders.rs` | `build_available_mcps` 用 `gw.pm_mcp_url` 的契约，本次改动不破坏 |
| 完整 gateway 测试套件（478 个） | 整个 crate | 无回归（`cargo test -p acowork-gateway --lib` 全绿） |
| **手动 / 实机验证**（待） | 跑 debug gateway → 触发 Wi-Fi 切换 → 验证 `agent_mcp.json` 自动更新 | 端到端 |

## 7. 已知局限 / 后续工作

- **IPv6**：`detect_non_loopback_ip` 只看 IPv4；IPv6 场景下不会触发更新。后续如果 IPv6 变成主要 LAN 协议，需要扩展 detector。短期 LAN IP 在家用 / 开发环境仍以 IPv4 为主。
- **容器 / 跨主机漂移**：本方案只解决单机单进程场景。多机 / 容器编排层（k8s service / Docker network）的 endpoint 漂移是另一个量级的问题，需要独立的 discovery 机制（如 mDNS、Consul），不在本次范围。
- **首次事件可能丢失**：`if-watch` 在 Windows 上的实现可能错过启动瞬间的早期事件；解决方案是首次 reconcile 在 watchdog 启动后立即触发一次（当前代码未做，可作为小改进追加）。
