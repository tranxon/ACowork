# ADR-072: 通用 MCP 安装检测模块 — 声明式 PackageSpec

**状态**：已接受
**日期**：2026-09-22
**决策者**：大鱼
**关联**：
- [ADR-069](./ADR-069-mcp-tool-level-optin.md)（MCP 工具级 opt-in——MCP 体系的上一个决策，本 ADR 补上"server 安装"一环）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（Node Agent——MCP 安装最终可能跨机器分发，本 ADR 的 PackageSpec 与机器无关）
- [docs/protocols/zh/04-mcp.md](../../protocols/zh/04-mcp.md)（MCP 协议参考——stdio 握手约定）
- LSP 安装脚本机制（`core/acowork-lsp-relay/src/install.rs` + `assets/lsp_install/`）——本 ADR 的**对照物**

**影响范围**：

**新增模块**：
- `core/acowork-core/src/protocol.rs`（新增 `McpPackageSpec` / `PackageKind` / `PypiRunner` / `McpInstallSpec` 类型 + `#[serde]` 序列化）
- `core/acowork-mcp/src/install.rs`（通用安装器：命令推导 / 依赖检测 / 健康检查 / 幂等 / 安全白名单）
- `assets/mcp_install/`（可选脚本兜底目录，与 `assets/lsp_install/` 同构）

**修改模块**：
- `core/acowork-gateway/src/http/mcp_catalog_api.rs`（新增 `GET/POST /api/mcp-catalog/install/{name}`；`do_probe` HTTP fallback 端口可配置）
- `apps/acowork-desktop/src/lib/types.ts`（`McpPresetDef` 增 `install?` 字段）
- `apps/acowork-desktop/src/lib/mcp-presets.ts`（preset 从 `command/args/installHint` 迁移到声明式 `package`）
- `apps/acowork-desktop/src/stores/mcpStore.ts`（增 `installMcp` action）
- `apps/acowork-desktop/src/components/harness/HarnessPage.tsx`（McpTab 增 Install 按钮 + dialog，UX 参考 LspTab）

---

## 背景

### 现状：内置 MCP preset 无"安装"能力

桌面 MCP 面板维护一份前端硬编码的 preset 列表（apps/acowork-desktop/src/lib/mcp-presets.ts），每个 preset 只有 `command` / `args` / `installHint`（一段说明文字）。用户点击"添加"后 Gateway 直接 `POST /api/mcp-catalog` 并 probe；**若 preset 假设的运行时不存在，用户没有任何工具自救**。

### 实例：docling preset 三重错误（2026-09-22 实测）

用户添加 docling 报 `failed to create transport ... program not found`。逐项核查（下载 PyPI wheel 验证）：

| # | 问题 | 事实 |
|---|------|------|
| 1 | 命令名错 | preset 写 `uvx docling-mcp`，但 docling-mcp v3.2.0 的 console_script 是 `docling-mcp-server`（entry_points.txt 实测），包名 ≠ 命令名 |
| 2 | 缺 stdio 标志 | `mcp_server.py` 默认 `transport=STREAMABLE_HTTP`（localhost:8000），ACowork stdio 握手收不到响应；需显式 `--transport stdio` |
| 3 | 缺运行时 | 机器无 `uvx`（`where uvx` 找不到），且 preset 无安装路径 |

probe 的 HTTP fallback 只试 `[3333, 3000, 8080]`，不含 docling 默认 8000，即使进程起来也探测不到。**结论：内置 preset 列表在"保证能安装成功"这一承诺上完全失守。**
### LSP 的对照

LSP 已有完整机制（`install_script` 字段 → `assets/lsp_install/{lang}.{ps1,sh}` → `GET/POST /api/lsp/install/{lang}` → LspTab 一键安装 + 实时输出）。但 LSP 是**命令式脚本**——每个语言一个脚本，把"包管理器 + 包标识 + PATH 处理 + 健康检查"全部硬编码。这对 LSP 成立（语言数量封闭稳定），对 MCP **不成立**：MCP 是开放生态，社区每天都在出新 server，照搬 LSP 会导致每个新 preset 都要写两个脚本，且自定义 MCP 完全用不上。

---

## 目标

1. **通用安装检测能力**：给定一个包标识，framework 自动推导安装命令、自动推导 spawn 命令、统一健康检查、自动 PATH 处理——**不绑定任何具体 MCP server**。
2. **开箱即用**：内置 preset 必须保证安装成功（或给出明确的引导路径），不能让用户脱离 ACowork 自行解决依赖。
3. **声明式优先，脚本兜底**：95% 的 MCP 用声明式 `McpPackageSpec` 表达；复杂场景（如 playwright 需额外装浏览器）才回退到 `install_script`。
4. **兼容自定义 MCP**：用户手动填写的 server（本地二进制 / HTTP URL）行为不变。
5. **安全**：install 端点只接受结构化 PackageSpec，拒绝任意 shell 字符串（防注入）。

---

## 可选方案

### 方案 A：命令式脚本（照搬 LSP）

为每个 preset 手写 `.ps1` / `.sh`（`assets/mcp_install/docling.{ps1,sh}` ...），每个脚本含 Install → Verify → Health-Check 三阶段。

- 优点：与 LSP 完全同构，实现最直接。
- 缺点：**耦合具体 server**——加一个新 preset = 写两个脚本（~200 行 × 2）；脚本把包管理器 / 包标识 / PATH / 健康检查揉在一起，换包名或改发布方式就要改脚本；自定义 MCP 用不上。**拒绝。**

### 方案 B：声明式 PackageSpec（推荐）

把"安装"拆成三个正交维度：**包管理器（kind）× 包标识（spec）× 启动方式（entry_point / spawn_args）**。preset 只声明事实，framework 负责全部流程（检测依赖 → 安装 → PATH 处理 → 健康检查 → 幂等）。

- 优点：加一个新 preset = 几行 JSON；自定义 MCP 可通过相同机制复用；健康检查 / 超时 / PATH 统一实现。
- 缺点：需要一个新的抽象层；无法表达的特例靠 `install_script` 兜底。

### 方案 C：混合（推荐范围内的精化）

以方案 B 为主；`PackageKind::Script` 保留 `install_script` 字段作为 escape hatch（playwright 装浏览器这类特例）。健康检查始终由 framework 统一执行（不在脚本里重复实现）。
---

## 决策

### 决策 1：声明式 `McpPackageSpec`（方案 B + C）

```rust
/// MCP 安装声明：包管理器 × 包标识 × 启动方式，三轴正交。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPackageSpec {
    /// 包分发方式，决定安装命令推导与依赖检测策略。
    pub kind: PackageKind,
    /// 包标识：npm name / pypi name / cargo crate / docker image / binary URL / git URL。
    pub spec: String,
    /// pypi 专属：uvx / pipx / pip，决定 runner 与 PATH 处理。
    pub runner: Option<PypiRunner>,
    /// 覆盖 spawn 命令名（缺省 = spec）。docling 需 "docling-mcp-server"。
    pub entry_point: Option<String>,
    /// 追加到 spawn 命令后的参数。docling 需 ["--transport", "stdio"]。
    pub spawn_args: Vec<String>,
    /// 完整覆盖 spawn（command + args）。filesystem 类需要传工作目录参数时使用。
    pub exec_override: Option<ExecOverride>,
}

pub enum PackageKind { Npm, Pypi, Cargo, Go, Docker, Binary, Script }
pub enum PypiRunner { Uvx, Pipx, Pip }
```

**关键判据**：spawn 命令**不能默认推导为"包名"**。docling 实测：包名 `docling-mcp`、entry point `docling-mcp-server`、且需 `--transport stdio`——三个名字不同。故 `entry_point` / `spawn_args` / `exec_override` 是必要字段。

### 决策 2：命令推导矩阵（framework 内统一实现）

| kind | 依赖检测 | 安装命令 | spawn 命令 |
|------|----------|----------|-----------|
| Npm | `npx` 存在 | `npx -y <spec>`（预热缓存） | `npx -y <spec> <spawn_args>` |
| Pypi+Uvx | `uvx` 存在 | `uvx --from <spec> <entry_point>` 预热 | `uvx --from <spec> <entry_point> <spawn_args>` |
| Pypi+Pipx | `pipx` 存在 | `pipx install <spec>` | `<entry_point> <spawn_args>`（已在 PATH） |
| Pypi+Pip | `pip` 存在 | `pip install <spec>` | `python -m <entry_point> <spawn_args>` |
| Cargo | `cargo` 存在 | `cargo install <spec>` | `<entry_point> <spawn_args>` |
| Go | `go` 存在 | `go install <spec>@latest` | `<entry_point> <spawn_args>` |
| Docker | `docker` 存在 | `docker pull <spec>` | `docker run -i --rm <spec> <spawn_args>` |
| Binary | — | 下载 → 解压 → 注册 PATH | `<entry_point> <spawn_args>` |
| Script | — | 执行 `install_script`（.ps1/.sh） | 按 `McpServerConfigDef.command/args` |

依赖检测失败（如缺 `uvx`）→ 返回结构化引导（"请先安装 uv：`pip install uv`"），**不自动装底层 runtime**（用户 agency 原则，已确认）；装完再点一次 Install。

### 决策 3：catalog 分层，`McpServerConfigDef` 不动

`McpServerConfigDef`（name/transport/url/command/args/env/headers）**原样保留**，是 spawn 的 wire format，兼容自定义 MCP。新增可选字段：

```rust
/// catalog 条目可选的安装上下文（preset 添加时一并写入）。
pub struct McpInstallSpec {
    pub package: McpPackageSpec,
    /// 安装状态（驱动前端 Install/Repair 按钮状态）
    pub state: InstallState,
}
```

- 自定义 MCP 不填 `install` → 走旧逻辑，行为不变。
- preset 添加 → spawn 配置 + install 上下文一起持久化。

### 决策 4：健康检查统一由 framework 执行

所有 kind 复用 `McpClient::initialize` JSON-RPC 握手（已在 `acowork-mcp` 实现），不在脚本里重复。docling 实测模型**懒加载**（`LocalDocumentConverter._converter = None`，首次转换才下载模型），故握手快速，`run_command_with_idle_timeout`（core/acowork-core/src/process.rs）60s idle 超时安全。

### 决策 5：`do_probe` HTTP fallback 端口可配置

`do_probe` 现硬编码 `[3333, 3000, 8080]`。改为从 `McpPackageSpec.spawn_args` 或额外字段读取默认端口（docling = 8000），并在 stdio 失败且 `spawn_args` 含 `--transport stdio` 时优先走 HTTP fallback。

### 决策 6：安全边界

`POST /api/mcp-catalog/install/{name}` 只接受 **catalog 中已注册条目的 `McpInstallSpec`** 或前端传回的**结构化 `McpPackageSpec`**（kind 白名单校验），拒绝任意 shell 字符串拼接。

### 决策 7：幂等

catalog 已有同名条目且 `state=installed` → 前端 Install 按钮隐藏或变 "Repair"（重新检测 + 重装）；安装成功才允许写入 catalog（阻断式，符合"开箱即用"承诺）。

### 决策 8：预设迁移

`mcp-presets.ts` 的 `McpPresetDef` 增 `install?: McpInstallSpec`。docling 作为首个用例直接修正三重错误：

```ts
{
  id: "docling",
  transport: "stdio",
  package: {
    kind: "pypi",
    spec: "docling-mcp",
    runner: "uvx",
    entry_point: "docling-mcp-server",
    spawn_args: ["--transport", "stdio"],
  },
  // spawn 配置由 framework 从 package 推导，不再手写 command/args
}
```

纯 npm 系（playwright / context7）迁移为 `{ kind: Npm, spec: "@playwright/mcp@latest" }`，行为不变。
---

## 后果

### 正面
- 内置 preset 做到"开箱即用"：声明式驱动安装 + 统一健康检查，失败有明确引导。
- 加一个新 MCP preset = 几行 JSON，不再手写双平台脚本；自定义 MCP 也可复用安装机制。
- PATH / 超时 / 幂等 / 健康检查统一在 framework 实现，测试集中。
- docling 的三重错误（命令名 / stdio / 缺 uv）一次修掉。

### 负面 / 成本
- 新增一层抽象（`McpPackageSpec` + 命令推导矩阵），首期实现量 ~4.5d。
- 命令推导矩阵无法表达的特例需 `install_script` 兜底（当前只预留机制，不实现特例）。
- 现有 catalog 条目无 `install` 字段 → 读取时 `Option` 默认空，无迁移代码（`deny_unknown_fields` 项目惯例）。

### 回滚
- `McpInstallSpec` 是 `McpServerConfigDef` 的可选扩展字段，删掉 `install` 字段即回退，spawn 行为不变。
- preset 迁移可逐条回退为旧 `command/args` 写法。

---

## 测试策略（P5）

- 单元测试：`PackageSpec → install/spawn 命令` 推导矩阵（纯函数，全 kind 覆盖）；依赖检测各分支。
- 集成测试：mock runner 模拟 `uvx` 存在/缺失两条路径；mock MCP server 验证健康检查握手。
- 端到端：真实 docling（`uvx --from docling-mcp docling-mcp-server --transport stdio`）安装 + probe + 加入 catalog。
