<h1 align="center">ACowork.AI — 和你的 agent 同事一起工作</h1>

<p align="center">
  <img src="assets/brand-mark.svg" alt="ACowork" width="360">
</p>

<p align="center">
  🏗️ <strong>声明式 Agent 平台 · 去中心化 · 高安全 · 可扩展</strong><br>
  ⚡️ <strong>Easy to build an agent colleague.</strong><br>
  ⚡️ <strong>Easy to share an agent colleague.</strong><br>
  ⚡️ <strong>Easy to deploy agent colleagues.</strong>
</p>

<p align="center">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/language-Rust-ff6600" alt="Language" /></a>
  <a href="./docs/design/zh/"><img src="https://img.shields.io/badge/docs-design-brightgreen" alt="Docs" /></a>
  <a href="./apps/acowork-desktop/"><img src="https://img.shields.io/badge/status-alpha-orange" alt="Status" /></a>
</p>

<p align="center">
  <a href="README.md">English</a>
</p>

---

<p align="center">
  <table>
    <tr>
      <td width="50%" align="center" valign="top">
        <img src="./assets/1.jpg" alt="多 Agent 协作与记忆系统" width="100%" />
        <br />
        <em>与多位 AI 同事协作——每位 Agent 拥有独立记忆、实时上下文感知和工具执行能力。</em>
      </td>
      <td width="50%" align="center" valign="top">
        <img src="./assets/2.jpg" alt="调试面板与上下文快照" width="100%" />
        <br />
        <em>全链路开发框架：迭代调试、Token 追踪、上下文快照，深入洞察 AI 推理过程。</em>
      </td>
    </tr>
  </table>
</p>

---

## ACowork.AI 是什么？

ACowork.AI 是一个**去中心化、高安全、可扩展的 AI Agent 运行时平台**，对标 Android 的设计哲学。
它不只是一套开发框架，而是让你创造 **AI 同事**的平台——每个 Agent 都是拥有独立记忆、工作区和个性的自主数字存在，
各有所长，彼此协作。

每个 Agent 都是独立的**"数字伙伴"**：拥有自己的运行时进程、私有记忆、工作区和配置——完全独立的个性化认知。
就像身边有一支 AI 专家团队——质量分析师、项目经理、高级工程师——各司其职，通过平台的 Intent 机制沟通协调。

**调优 Prompt、Tools、Memory = 构建 AI 同事。** Personal/Sensitive 数据在打包时自动剥离，
你可以自由分享 Agent 的能力，而不必担心泄露私有记忆。

---

## ✨ 核心亮点

| | |
|---|---|
| 🧩 **声明式 Agent** | `.agent` 包只包含 manifest + prompts + skills——**无可执行代码**，签名后在安装时强制验证。 |
| ⚙️ **统一 Runtime** | 单一 Rust 二进制加载任意 `.agent` 包；Agent 直连 LLM API——不经 Gateway 代理，零额外延迟。 |
| 🔒 **进程级隔离** | 每个 Agent 作为独立 OS 进程运行，自带文件系统、Grafeo DB 与沙箱化的工具执行。 |
| 🧠 **仿生记忆** | 每个 Agent 拥有私有 Grafeo 图数据库，三层五类分层记忆 + HNSW/BM25 混合检索 + 关联扩散。 |
| 🛡️ **三层安全** | 包签名 + 操作系统进程沙箱 + Wasmtime 工具沙箱。 |
| 💬 **Intent 协作** | Agent 通过 Capability Registry 声明能力，Gateway 作为 broker 路由请求/订阅，支持同步/异步。 |
| 🌐 **分布式原生** | Gateway 作为单一控制面入口；Node Agent 把 Runtime 派到任意机器（GPU 机 / 工位 / 云主机），MQTT + HTTP 反代——单机与多机走同一条协议路径。 |
| 🛠️ **全链路开发** | Desktop App（Tauri v2）内置 DevMode：对话调试、Skill 热加载、断点、录制回放、发布向导。 |

---

## 🏛️ 架构

ACowork 把每个 Agent 视作**手机上的一个应用**。`.agent` 包就是完整自包含的应用（如 APK），通用 Runtime 是操作系统，
Gateway 是云端控制面，每台机器上的 **Node Agent** 负责托管 Agent 进程，并作为本地鉴权 / 网络暴露边界。

### Android 类比

| Android         | ACowork                              | 作用                                                                                                |
| --------------- | ------------------------------------ | --------------------------------------------------------------------------------------------------- |
| ART             | Agent Runtime                        | 通用执行引擎（平台唯一二进制，loopback-only）                                                       |
| APK             | `.agent` 包                          | 声明式打包（config + prompts + skills，无可执行代码）                                               |
| APK Signature   | Signing Block                        | 包签名，验证完整性和来源                                                                            |
| AMS             | Gateway                              | 单点入口：MQTT broker 宿主 + HTTP 统一入口 + 全局资源权威（providers / MCP / budget / cron …）       |
| **OEM Service** | **Node Agent**（`acowork-node`）     | **节点级 Runtime 父进程：进程生命周期 + 本地 package 管理 + 节点反代 `:19900`**                     |
| Binder IPC      | MQTT + HTTP 反向代理                 | 进程间通信（实时事件 + 大数据查询反代）                                                              |
| ContentProvider | 系统 Agent                           | 系统级数据服务（身份、偏好）                                                                        |

单机模式下 Gateway 自动 spawn 一个本机 Node（`local`）；分布式模式下用户在目标机器执行 `acowork-node start`。
**Gateway 侧代码完全无「本地 / 远程」分支**——任何场景都走同一条协议路径。完整设计权衡见
[`docs/adr/zh/ADR-055-remote-runtime-node-topology.md`](./docs/adr/zh/ADR-055-remote-runtime-node-topology.md)。

### 系统架构

<p align="center">
  <img src="./assets/architecture.svg" alt="ACowork.AI 系统架构" width="100%" />
</p>

---

## 🚀 快速开始

跨平台编译脚本位于 [`dev/`](./dev/)，统一处理 ONNX Runtime 探测、profile 切换、资源 staging——优先使用脚本而非直接调用 `cargo`。

### 前置依赖

| 工具         | 版本           | 说明                                                                                                              |
| ------------ | ------------- | ----------------------------------------------------------------------------------------------------------------- |
| Rust         | **nightly**   | `rustup default nightly`                                                                                          |
| Node.js      | >= 18         | Desktop App 与 Tauri CLI                                                                                          |
| PowerShell   | 7.x           | Windows 必需（`.ps1` 脚本）；推荐 `pwsh`                                                                          |
| ONNX Runtime | 自动管理       | 由 `dev/setup_ort.*` 安装到 `.ort/onnxruntime-<plat>-<arch>-<ver>/`                                               |

```bash
git clone https://github.com/tranxon/ACowork.git
cd ACowork
```

### Step 1 — 安装 ONNX Runtime（一次性）

```bash
# Windows PowerShell
.\dev\setup_ort.ps1

# macOS / Linux / WSL / Git Bash
./dev/setup_ort.sh
```

### Step 2 — 编译并启动后端（Gateway + Runtime + Node）

```bash
# Windows —— release 构建，然后启动 Gateway
.\dev\build_core.ps1 -Start

# macOS / Linux —— release 构建并启动
./dev/build_core.sh

# Debug profile
.\dev\build_core.ps1 -Debug -Start      # Windows
./dev/build_core.sh --debug              # bash
```

macOS Apple Silicon 用户也可使用 `./dev/build_macos.sh` 一键编译（自动启用 CoreML）。

### Step 3 — 启动 Desktop App

Desktop App 是 Tauri v2 壳——React/TS 前端通过 HTTP 与 Gateway 对话，Rust 侧负责系统托盘与订阅实时事件的 MQTT 客户端。

```bash
cd apps/acowork-desktop
npm install

# 浏览器模式 dev server
npm run dev                # → http://localhost:5173

# 完整 Tauri 桌面窗口
npm run tauri dev
```

### ✍️ 30 秒写出第一个 Agent

```toml
# examples/qa-agent/manifest.toml
[package]
id = "com.example.qa-agent"
name = "Quality Assurance"
display_name = "QA-Tom"
role = "QA"
version = "1.0.0"

[llm]
provider = "deepseek"
model = "deepseek-v4-flash"

[permissions]
tools = ["web_search", "read_file", "write_file"]
```

```markdown
<!-- prompts/system.md -->
你是一个 QA Agent，擅长帮助用户做质量管理与代码审查。
```

构建并签名：

```bash
./dev/build-agent.sh examples/qa-agent   # 产出 com.example.qa-agent.agent
```

> **当前状态**：ACowork 处于 **Alpha 阶段**。Gateway、Runtime、Grafeo 记忆引擎与 Desktop App 都在积极开发中。
> 详见 [路线图](#-路线图) 了解当前已交付与下一步计划。

打包安装包、签名、CI、远程节点接入等更多内容请参见 [`docs/design/zh/`](./docs/design/zh/)。

---

## 🧪 Agent 开发流程

```
① 编写       manifest.toml + prompts/ + skills/SKILL.md + 可选 tools/*.wasm
② 签名       acowork-keygen → acowork-sign  （Developer 私钥 + APK 风格签名）
③ 调试       Desktop App DevMode → 对话调试、SKILL.md 热加载、断点、录制回放
④ 发布       发布向导 → 远程仓库，或直接分享 .agent 文件
```

开发者通过**调优声明式配置**来构建 Agent——系统提示词、工具能力、记忆行为——而非编写命令式代码。
从编写到发布的完整链路，平台均提供工具支撑。

---

## 📈 路线图

| 阶段    | 内容                                                                                                       | 状态     |
| ------- | ---------------------------------------------------------------------------------------------------------- | -------- |
| Phase 1 | 基础框架 + LLM 交互（MVP）：包解析、签名验证、Runtime 主循环、Gateway 基础                                  | ✅ 已完成 |
| Phase 2 | Memory 分层 + 系统 Agent：Grafeo 仿生分层、即时提取、关联扩散                                              | 🚧 进行中 |
| Phase 3 | 权限与沙箱：文件系统隔离、WASM 沙箱（Wasmtime）、Approval Gate                                             | 🚧 部分实现 |
| Phase 4 | 通信与协调：Intent、Budget Tracker、Rate Limiter、Cron                                                     | 🚧 部分实现 |
| Phase 5 | Desktop App + 开发框架：Debug Protocol、Skill 热加载、录制回放；MQTT 协议栈重构                            | 🚧 进行中 |
| Phase 6 | 云端与生态：Memory Sync、远程 `.agent` 仓库、Agent 商店                                                    | 🔮 规划中 |
| Phase 7 | 跨平台适配：Windows / macOS / Android / iOS                                                                | 🔮 规划中 |

---

## 📚 文档

- 架构设计：[`docs/design/zh/`](./docs/design/zh/)
- 模块级设计：[`docs/module-design/zh/`](./docs/module-design/zh/)
- 架构决策记录（ADR）：[`docs/adr/zh/`](./docs/adr/zh/)（ADR-009 → ADR-058+）
- 开发者约定：[`AGENTS.md`](./AGENTS.md)

---

## 🧪 参考与致谢

ACowork.AI 的设计深受以下开源项目启发：

- [ZeroClaw 🦀](https://github.com/zeroclaw-labs/zeroclaw) — Trait 驱动运行时、安全装饰器、流式解析
- [Grafeo](https://github.com/GrafeoDB/grafeo) — HNSW 向量索引、BM25 全文检索、混合搜索
- [Mem0](https://github.com/mem0ai/mem0) — 多层级记忆、用户/会话/Agent 状态
- [HippoRAG](https://github.com/OSU-NLP-Group/HippoRAG) — 神经生物学启发长时记忆、关联扩散
- [LightMem](https://github.com/zjunlp/LightMem) — 轻量级记忆压缩
- [OpenCode](https://github.com/anomalyco/opencode) — 多 Agent 协作、Provider 无关设计

---

## 🤝 贡献

项目处于 **Alpha 实现期**。欢迎提交代码、设计反馈与评审意见：

- 通过 issue 提交 bug 报告、提案或设计反馈
- 提 PR 前请先阅读 [`AGENTS.md`](./AGENTS.md) 了解项目约定

---

## 📄 License

Apache-2.0 —— 详见 [`LICENSE`](./LICENSE)。

---

<p align="center">
  <b>ACowork.AI — 与你的 AI 同事协作</b><br>
  <i>像组建团队一样构建和协作 AI 伙伴。</i>
</p>
