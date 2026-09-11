# Code Review：Gateway ↔ Agent Runtime 边界违规审计

> 评审对象：`core/acowork-gateway` 中所有触碰 agent runtime 私有文件系统 / 私有数据结构的代码
> 对照基准：[ADR-009](../adr/en/ADR-009-gateway-workspace-isolation.md)（Gateway Workspace Isolation，**仅英文版**）+ [ADR-040](../adr/zh/ADR-040-remove-grpc-mqtt-http-only.md)（gRPC 移除，runtime 自有 HTTP）+ [ADR-055](../adr/zh/ADR-055-remote-runtime-node-topology.md)（远程 Node 拓扑，gateway/runtime **可跨机器**）
> 评审日期：2026-09-10 | 评审人：Senior Software Engineer (ponytail)
> 触发事件：用户报告"聊天框选 skill 后上下文用量弹窗显示 skills=0、Debug Panel 快照无 Skill Instructions 行"，根因链分析发现 gateway 在 `core/acowork-gateway/src/http/skills_api.rs` 自己解析 SKILL.md，而 runtime 的解析器要求 `triggers` 非空——同一份数据两套语义，触发本次 bug 链

---

## 1. 结论摘要

**总体判定：🔴 ADR-009 原则存在但执行不彻底；ADR-055 之后部分违规已从"代码气味"升级为"硬正确性故障"。**

- ADR-009（Accepted）的 5 条原版违规（V1–V5）有 3 条已清理，但 **V3（读 prompts）以 `#[allow(dead_code)]` 形式残留**——下次有人加新 gateway 需求时极易复用死代码复活违规。（**已修**：Phase 2 直接删除，见 §7.0）
- 本次 Skill 链 bug 暴露了一条**未被 ADR-009 覆盖的同型违规**：gateway 在 `http/skills_api.rs` 自己实现了一份 SKILL.md 解析器。Gateway 与 Runtime 的解析器语义漂移（gateway 宽松 / runtime 严格）是 bug 的直接成因。
- 审计还发现 **2 条新违规**和 1 处**架构性遗漏**：ADR-009 没有中文版，导致中文团队查不到这条规则；`core/acowork-gateway/src` 内**没有 CI lint 阻止下一次同类违规**。

**严重度分级：**

| 等级 | 数量 | 说明 |
|---|---|---|
| 🔴 跨机器必坏 | 2 | V-A（skills）、V-B（avatar）——ADR-055 部署下 gateway 读不到 runtime 机器上的 `install_path`，直接 5xx |
| 🟡 遗留/隐患 | 2 | V-C（workspace 文件浏览，需复核）、V-D（dead code 未删） |
| 🟢 合规 | 3 | V-E/F/G（`.agent` 包元数据、embed sidecar，符合 ADR-009 install-time exception） |

> ⚠️ **第二轮复审（同日下午，见 §7）推翻了本节的严重度排序**：本表只覆盖了「读」，
> 完全漏审了「写」；而且 Phase 1/2 的 avatar 读反代**引入了新的停止态回归**。
> 以 §7 为准。

---

## 2. 背景：本次 bug 链（已修复，作为评审入口）

用户在聊天框选中 `ponytail-review` skill 并发送消息，上下文用量弹窗显示 skills 占比 0，Debug Panel 上下文快照无 Skill Instructions 行。

**第一次修复**（已落地，HEAD）：

- `core/acowork-runtime/src/agent/inbound.rs`：给 `InboundMessage::ChatMessage` 加 `command: String` 字段（原本结构体根本没有，doc 注释却写"handler 可提取 command"——注释与实现不一致）。
- `core/acowork-runtime/src/startup/gateway_loop.rs`：`control_action_to_inbound` 不再 `command: _`，传给 `InboundMessage`；`dispatch_inbound` ⑭ 分支新增解析：command 非空且 `params_json.skill_instructions` 未显式设置时，调用 `SessionManager::resolve_skill_instructions(&command)`。
- `AgentCore` 新增 `skill_registry: Arc<SkillRegistry>` 字段，Phase A 加载后注入（之前 `let _skill_registry` 直接丢弃）。

**修复后实测**（ponytail agent 最新日志 `~/.acowork/acowork-node/packages/com.acowork.ponytail/.../logs/20260910_172152.log`）：

```
17:21:53 WARN  startup_phase_a  skills::parser
   Failed to parse skill from ".../skills/ponytail-review/SKILL.md": Empty triggers list in SKILL.md

17:24:22 WARN  startup_phase_d  gateway_loop
   ChatMessage: command did not match any loaded skill, ignoring
   session_id=20260910_164755_f5e768 command=ponytail-review
```

**第一次修复证实链路通了**（command 顺利到达 dispatch_inbound 的解析分支），但**暴露了第二个独立问题**：runtime 的 SKILL.md 解析器要求 `triggers` 至少一个，否则整份 SKILL.md 拒收。

**第二次修复**（已落地，HEAD）：把 `parser.rs:169` 的硬拒改为 `tracing::warn!` 不拒，测试断言同步更新。

---

## 3. 违规审计矩阵

下表覆盖 `core/acowork-gateway/src` 内所有触碰 agent runtime 私有数据（filesystem / 数据结构）的代码点。

### 3.1 🔴 高危：跨机器必坏

| ID | 文件:行 | 操作 | 路径 / 内容 | ADR-009 判定 | ADR-055 行为 |
|---|---|---|---|---|---|
| **V-A** | `http/skills_api.rs:172, 187` + `list_skills/get_skill_detail/import_skill/get_skill_execution_history` (215/264/313/...) | 读 + 解压 + 解析 | `{install_path}/skills/` 目录、`SKILL.md` frontmatter + Markdown body | ❌ **未声明允许**——运行时私有数据 | 🔴 跨机器时 `install_path` 在 gateway 这边不存在，端点 5xx |
| **V-B** | `http/agents.rs:1075`（avatar 端点）| `std::fs::read(&canonical_path)` | `{install_path}/assets/avatar*.{png,jpg,jpeg,gif,webp,svg}` | ❌ 未声明 | 🔴 跨机器时 `install_path/assets` 在 runtime 机器，gateway 读不到 |

### 3.2 🟡 中危：遗留代码 / 需复核

| ID | 文件:行 | 操作 | 路径 / 内容 | 处置 |
|---|---|---|---|---|
| **V-C** | `http/agents.rs:1119–1137`（workspace 文件浏览）+ `http/agents.rs:513`（avatar 早期实现）| 读 | `{install_path}/.../*`（用户路径）| ADR-058 §11.2 明确"workspace 文件 API 全部走 Gateway→Runtime 反代"——需复核现有 endpoint 是否真在反代 |
| **V-D** | `http/agents.rs:2888` `read_system_prompt` <br> `http/agents.rs:2933` `read_manifest_tools` <br> `http/agents.rs:2952` `write_manifest_tools` | 读 + 写 | `{install_path}/prompts/*.md`、`manifest.toml` | ADR-009 V3 的处理方式是 `#[allow(dead_code)]` 留死代码——下次有人加新需求极易复用。**直接删除**比留 `#[allow(dead_code)]` 安全 |

### 3.3 🟢 合规（ADR-009 install-time exception）

| ID | 文件:行 | 操作 | 路径 | ADR-009 允许依据 |
|---|---|---|---|---|
| **V-E** | `gateway/mod.rs:1475–1494` | 读 | `sa_packages_dir/manifest.toml`（系统 agent 列表）| "Agent listing: reading agent.yaml / manifest.toml for metadata during list_agents" 显式例外 |
| **V-F** | `http/agents.rs:1940` `extract_manifest_from_package` <br> `gateway/node_manager.rs:725–810` | 读 `.agent` ZIP | `.agent` 安装包内 `manifest.toml` | "Package manager: install, uninstall, upgrade, clone, publish" 显式例外。`.agent` 包本身在 gateway `{packages_dir}`（gateway 权威），但后续用元数据时**不得再读 runtime 私有文件** |
| **V-G** | `embedding_providers.rs`（全文）| 读 + 写 | gateway 自有 embed sidecar 配置 | 不是违规——embed 是 `global scope` 在 gateway 机器运行（ADR-055 §3 拓扑），跟 runtime 的 per-agent embedding 不是一回事 |

---

## 4. 架构根因（不是 bug，是 ADR-055 之后的角色错位）

### 4.1 时间线

```
ADR-009（Accepted）          时代背景：Gateway + Runtime 同一进程（gRPC）
   ↓                         Gateway 直接读 runtime 文件"能跑就算了"
ADR-040（拆进程）             Gateway → Runtime 改 MQTT + HTTP
   ↓                         但 Gateway 内的 file-access 代码**没清理**
ADR-055（跨机器）             Gateway 与 Runtime 可在不同机器
   ↓                         同地址访问变跨网络 fs——本次 skill bug 的根本语境
现状                          部分违规从"代码气味"升级为"硬正确性故障"
```

### 4.2 错位本质

ADR-009 时代 Gateway 的角色是"通信 + 资源管理 + **必要文件协调**"，但实现里它还在**兼职 agent workspace owner**。ADR-040 拆开 runtime 和 ADR-055 跨机器部署都没做系统性清理——留下了**两类问题**：

1. **行为类**：gateway 内部自己解析 SKILL.md、自己读 prompts、自己读 avatar。两份 parser 漂移 → 用户能踩到的 bug。
2. **拓扑类**：跨机器后 `install_path` 在 gateway 这边根本无权访问——这类违规在单进程时代不暴露，跨机器后**直接 5xx**。

### 4.3 ADR-009 自身的不完整

| 不完整点 | 影响 |
|---|---|
| 仅英文版（zh 未译）| 中文团队下次做 gateway 需求**查不到这条规则**——只能踩雷再修 |
| 没有 CI lint 兜底 | `core/acowork-gateway/src/**` 内散落 7–8 个文件含 `std::fs::` 调用，未来新人加 endpoint 时无人提醒，违规会继续生长 |
| V3 处置 = `#[allow(dead_code)]` | 死代码是下一次违规的温床——下次有人加新 gateway 需求会复用 |

---

## 5. 根治方案（分阶段，最小破坏）

### 5.1 Phase 1：Skill 全部反代（根治本次 bug 链）

| 步骤 | 改动 | 风险 / 收益 |
|---|---|---|
| 1 | Runtime HTTP `core/acowork-runtime/src/http/server.rs` 加 5 个端点：`GET /agents/{id}/skills`、`GET /agents/{id}/skills/{name}`、`POST /agents/{id}/skills/import`、`GET /agents/{id}/skills/{name}/history`（用现有 `SkillRegistry` + 新增 `SkillExecutionStore`）| 低——Runtime 本就是 skill 权威 |
| 2 | **删除** `acowork-gateway/src/http/skills_api.rs`（5 路由 + "Minimal SKILL.md parser" 整套一起删）| **消灭第二份 parser 的唯一办法** |
| 3 | `acowork-gateway/src/http/proxy.rs`（已有 ADR-058 workspace 反代框架）加 `/api/agents/{id}/skills*` → runtime HTTP 反代规则 | 低 |
| 4 | Desktop **零改动**——路径前缀保持 `/api/agents/{id}/skills*`，反代透传 | 前端无感 |

**效果**：单一 parser、单一权威、跨机器自动正确——三件事同时解决。

### 5.2 Phase 2：剩余违规清理

| ID | 处置 |
|---|---|
| V-B（avatar）| Avatar 文件随 `.agent` ZIP 进入 gateway `{packages_dir}`（gateway 权威），但**解包后被复制到 runtime 机器的 `install_path/assets/`**——gateway 读不到。处置：avatar 端点改为反代到 runtime HTTP（runtime 自有 assets 路径）。 |
| V-C（workspace 文件浏览）| 对照 ADR-058 §11.2.A 22a–22h 复核现有 endpoint；漏反代的补上 |
| V-D（dead code）| **直接删除** `read_system_prompt` / `read_manifest_tools` / `write_manifest_tools` 三个函数（约 80 行），不留 `#[allow(dead_code)]` 纪念品 |

### 5.3 Phase 3：让规则本身不可绕过（举一反三的根治）

| 改动 | 作用 |
|---|---|
| **翻译** `docs/adr/zh/ADR-009-gateway-workspace-isolation.md`，顺手合并 ADR-009 v2 散落（ADR-034 §11.2.A、ADR-043、ADR-058 注释里的内容）| 让中文团队查得到这条规则 |
| **CI lint**：在 `dev/ci.sh` 加 grep 检查——`core/acowork-gateway/src/**` 里 `std::fs::read\|read_to_string\|read_dir\|File::open` 只能出现在白名单：`config.rs`、`budget/store.rs`、`cron/store.rs`、`embedding_providers.rs`、`vault/`、`gateway/state.rs`、`http/proxy.rs`（反代配置例外）。其他文件命中即 PR fail | 物理上阻止下一次同类违规 |
| **AGENTS.md / CONTRIBUTING** 增加一条规则："Gateway = 通信 + 资源管理 + 反代。Agent runtime 私有数据（skills、prompts、conversations、memory、embedding-of-agent-data）的任何读写只能通过 runtime HTTP" | 让规则在 onboarding 阶段可见 |

### 5.4 Phase 4：扩展检查（标记工单，本次不修）

| 项目 | 说明 |
|---|---|
| `list_agents` 是否每次请求重读 `manifest.toml`？| ❌ **已证伪，非工单**。`list_agents`（`http/agents.rs:283`）只读内存 `gw.installed_agents`，零 fs IO。`manifest` 的**内容**由 Node 的 retained `InstalledAgentInfo.manifest_toml` 报文随 MQTT 送达（`gateway/state.rs:481 upsert_installed_from_node`），`install_path` 也只是报文里的字符串。Gateway 从不读 Node 磁盘上的 manifest.toml。原审计（含 ADR-009 zh §5.5 初稿）此处判断错误 |
| Embedding sidecar API（gateway 提供）vs per-agent embedding（runtime 内部）的边界有无文档化？| ADR-055 §3 拓扑图提到 global scope，但**文字契约缺失**，值得补一份短 ADR |
| `agents/{id}/prompts/reload`（ADR-063 §3.7.6）| 确认是 MQTT push → runtime reload，不是直接写文件 |

---

## 6. 实施顺序建议

```
Phase 1（本次优先）         Phase 2            Phase 3            Phase 4
─────────────────         ──────────────     ──────────────     ──────────────
Runtime HTTP 加 5 端点     Avatar 反代        翻 ADR-009 zh     list_agents 缓存
删 gateway skills_api    删 V-D dead code   CI lint            embedding 边界
Proxy 加 skills 反代      V-C 复核补漏       CONTRIBUTING       reload 端点确认
Desktop 零改动
```

**最小破坏原则**：Phase 1 完成后 **Desktop 零改动**；Phase 2 删除的是死代码（无外部影响）；Phase 3 是规则层；Phase 4 是体检。

> ⚠️ **Phase 1–4 已于 2026-09-10 实施完毕，但复审（§7）发现 Phase 1/2 的 avatar 读反代引入了停止态回归（V-P），且第一轮完全漏审了写侧越界（V-H…V-K）。Phase 5 以 §7.5 为准。**

### 6.1 Phase 5（第二轮新增，本次已留档未实施）

> 以 **§7.5 终态路由归属表** 与 **§7.6 工单表** 为准（下表为概览，已按 §7 结论修正）。

```
工单 1（先行）              工单 2/3/4（一批做）              工单 5            工单 6/7/8…
────────────────────       ─────────────────────────       ──────────────     ──────────────
proto 加 overrides_json     Runtime 写 overrides 文件        Node 加只读        publish 归属拍板(V-R)
Node 原样搬运并发布         Gateway 加 overrides 字段        资产端点           bundled 改 dispatch(V-O)
（不解析）                  删 avatar_cache 全套              Gateway 读路由     删 runtime avatar 读端点
                           live 变更事件（不带数据）          改反代 Node         display_name 移除本地态(V-Q)
```

**依赖链**：1 →（2 → 3 → 4）→ 5；6 待用户拍板后与 7 一批；8 依附 2/3；9 收尾；10 全部定型后。

---

## 7. 第二轮复审（2026-09-10 下午，同一评审人）

第一轮（§1–§6）已按 Phase 1–3 实施完毕。本节记录实施后的复审结果：
**第一轮只审了「读」，写侧越界全部漏审；而且 Phase 1/2 的 avatar 读反代引入了一个新回归。**

### 7.0 已实施状态（P1–P3）

| 阶段 | 内容 | 状态 |
|---|---|---|
| P1 | Runtime 新增 `http/skills.rs`（3 读端点，复用唯一 parser）；删 Gateway 本地 parser + 3 读端点；`proxy.rs` 加反代 | ✅ 已实施，`cargo check --all-targets` 通过 |
| P2 | Runtime 新增 `http/avatar.rs`（3 读端点）；V-D 死代码 `read_system_prompt` / `read_manifest_tools` / `write_manifest_tools` 直接删除；V-C（workspace 浏览）复核**合规** | ✅ 已实施 |
| P3 | ADR-009 中文版（含 §5 复审）；`dev/ci.sh` 加 `run_gateway_fs_redline`；`AGENTS.md` 加 Gateway 边界规则 | ✅ 已实施 |

### 7.1 🔴 最高优先级：读侧反代回归（V-P，Phase 1/2 引入）

| 项 | 内容 |
|---|---|
| 现象 | **停止态 agent 的自定义/打包头像在侧边栏失效，静默回落到随机内置图标** |
| 证据 | [proxy.rs:1925-1931](../../core/acowork-gateway/src/http/proxy.rs#L1925)：Runtime HTTP 端点未注册时返回 `404 "is not running (no Runtime HTTP endpoint registered)"`。停止态无 Runtime 进程 → `GET /api/agents/{id}/avatar` 与 `/avatar-file` 必然失败 |
| 前端路径 | [AgentList.tsx:502](../../apps/acowork-desktop/src/components/agent-list/AgentList.tsx#L502) 对**所有** agent（含停止态）渲染 `AgentAvatar`，`avatarUrl={agent.avatar}` 取自 `list_agents`（[agents.rs:360](../../core/acowork-gateway/src/http/agents.rs#L360) `avatar: eff_avatar`，停止态同样返回）。`AgentAvatar` 走 `CustomAgentAvatar` → [avatar.ts:43](../../apps/acowork-desktop/src/lib/avatar.ts#L43) `resolveAgentAvatarFileUrl` → `GET /avatar-file` |
| 用户可见后果 | 头像加载 `onError` → [AgentAvatar.tsx:121](../../apps/acowork-desktop/src/components/common/AgentAvatar.tsx#L121) 回落 `DeterministicBuiltinAvatar`。**无报错、无提示**，启动后又变回来，表现为"头像自己乱跳" |
| 性质 | Phase 1/2 之前该端点由 Gateway 本地 `std::fs::read` 服务，单机下停止态可用；改为反代后**单机也坏了** |

#### 7.1.1 为什么 Phase 1/2 的方向必须修正（三条决定性证据）

**证据 1：内嵌 broker 是纯内存的，Gateway 一重启 retained 全丢。**

[broker.rs:142-172](../../core/acowork-gateway/src/mqtt/broker.rs#L142) 的 rumqttd 配置模板里**没有 `[persistence]` 段**（`grep persistence core/acowork-gateway/src/mqtt/broker.rs` → 0 命中）。retained 消息只活在内存里，而 rumqttd 随 Gateway 进程启停。后果：

```
Gateway 重启 → 内嵌 broker 全新启动 → retained 全部清零
                     │
    ┌────────────────┴──────────────────┐
活着的 Node / Runtime 重连             停止态的 Runtime
on-connect 自动重发 retained ✅        永远不会再连接 → 永远不重发 ❌
```

→ 任何「头像真相只存在于 Runtime 的 retained meta」的设计，**在每次 Gateway 重启后，所有停止态 agent 的头像/名字集体回出厂**。这不是边缘场景，是必现。

**证据 2：ADR-017 的 `avatar_cache.json` 就是为这件事加的，我判错了它的性质。**

[agent_config.rs:8-11](../../core/acowork-gateway/src/http/agent_config.rs#L8) 注释原文：

> `The Gateway maintains a lightweight avatar cache file ({data_dir}/avatar_cache.json) so that list_agents can return the current avatar without a gRPC roundtrip, **even when the agent is stopped**.`
> `The cache file survives Gateway restarts and is the source of truth for list_agents when the agent is stopped.`

它冗余的只是**写入方**（不该由 Gateway 写），**持久位本身是必需的**。Phase 1/2 若照原计划删掉它而无替代，等于拆掉唯一的持久位。

**证据 3：Gateway 的聚合路径会反复冲掉 manifest 派生的字段。**

[dispatch.rs:844](../../core/acowork-gateway/src/mqtt/dispatch.rs#L844) `upsert_installed_from_node` **重建** `AgentInfo`，把 manifest 派生的 `avatar` 重置回包内默认值；紧随其后的 `cache.get(&aid)` 覆盖才是补丁。触发时机 = Node 每次启动 / 安装 / 卸载 / clone / upgrade。

→ **任何「只存在 Gateway 内存」的方案都会被这一步冲掉。** 能撑住的只有「一个常驻进程，手里有磁盘，且会自动重连重发」——**那个进程只能是 Node**。

#### 7.1.2 已决策设计（用户 2026-09-10 拍定）

**核心区分：`manifest.*` 是「包内容」，`overrides.*` 是「用户偏好」。两者不得混存。**

| | `manifest.avatar` / `manifest.display_name` | `overrides.avatar` / `overrides.display_name` |
|---|---|---|
| 语义 | 包作者默认值，**可分发** | 用户对**这个 instance** 的偏好，**不可分发** |
| 写者 | Runtime（运行态改）/ Node（publish 烘焙） | **Runtime 唯一写者**（HTTP） |
| 位置 | `{instance}/manifest.toml` | `{agent_id}/{instance_id}.overrides.json`（**兄弟文件**，不在包目录内 → upgrade 不冲掉） |

> 为什么不把用户偏好直接写进 `manifest.toml`：publish/build 会把用户个人头像打成可分发包，且 upgrade 会冲掉。这也是 ADR-017 当初避开 manifest.toml 的理由。

**链路（三通道各司其职）：**

```
写    PUT /api/agents/{id}/avatar-config  →  Gateway 纯反代  →  Runtime（唯一写者）
      Runtime 写 {agent_id}/{instance_id}.overrides.json  +  发布变更事件（事件不带数据）

durable   Node 原样读入该文件 → InstalledAgentInfo.overrides_json（不透明 string）
          Node 常驻 → 每次重连自动重发 retained inventory  →  Gateway 始终能恢复

live  Runtime 变更事件（MQTT，仅通知）  →  Gateway 重新拉取 overrides  →  刷新列表

消费  list_agents 里唯一一处合并点：overrides（用户偏好） > manifest（出厂默认）
      ⚠️ overrides 必须存进 AgentInfo 的**独立字段**，不得写回 manifest ——
         否则被 upsert_installed_from_node 冲掉（证据 3）
```

**Node 的角色定性（修正 §7.2 的原规则）：**

> **Node = 包的搬运工 + 只读资产服务 + 通信中转。**
> ✅ 可以：把包目录里的文件**原样**读出来搬运（`manifest_toml` 已有先例，见下），不做任何解释
> ❌ 不可以：承担任何**写/业务语义** —— 不解析 SKILL.md、不解析 overrides、不解析头像优先级、不改 manifest
> **解析与归属判定的权力只能在唯一一处**（否则重演 V-A 的 parser 漂移 bug）

先例现成 —— [mqtt_payload.proto:1265](../../core/acowork-core/proto/mqtt_payload.proto#L1265)：

```protobuf
/// Full manifest.toml content. The Gateway parses this to rebuild its
/// `AgentManifest` (capability registration, cron triggers, avatar).
string manifest_toml = 5;
```

Node 已经在做「读文件全文 → 原样搬运 → Gateway 解析」这件事。新增 `overrides_json` 是**同一模式的复刻，不是新机制**。

**新增 proto 字段（`InstalledAgentInfo`）：**

```protobuf
/// 用户对该 instance 的偏好覆盖（avatar / builtin_avatar / display_name）。
/// 由 Runtime 写入 {agent_id}/{instance_id}.overrides.json；
/// Node 原样搬运、**不解析**；Gateway 消费并在 list 时优先于 manifest。
string overrides_json = 7;
```


### 7.2 写侧越界（V-H…V-K，第一轮完全漏审）

第一轮把 avatar 写端点判为「留 Gateway 因为牵涉 Gateway 自有缓存与 publish 流程」（§5.2），**该判断被用户裁定推翻**：

> **边界规则（用户裁定 2026-09-10，7.1.2 修订版为准）**
> Gateway = 通信 + 资源管理 + 反代；Node = 通信中转 + Runtime 生命周期（装 / 卸 / 启 / 停）。
> **两者都不得触碰 Agent Runtime 内部业务逻辑**（头像管理即典型）。
> 数据读写一律走 HTTP（不变量：初始数据 + 增删改查全部 HTTP）；MQTT 只承载状态变化事件通知与控制命令。
>
> **7.1.2 修订**：Node 追加「包的搬运工 + 只读资产服务」角色 —— 允许**原样**搬运包内文件（`manifest_toml` 已有先例），
> 但仍**不得解析、不得写、不得承担业务语义**。

| ID | 文件:行 | 操作 | 问题 | 跨机器行为 |
|---|---|---|---|---|
| **V-H** | [agents.rs:510](../../core/acowork-gateway/src/http/agents.rs#L510) `update_agent_manifest_avatar`（550 读 / 573 写）| 写 `{install_path}/manifest.toml` | Gateway 写 Node 本机包目录 | 🔴 **publish/build 已委托 Node**（[publish_api.rs:98](../../core/acowork-gateway/src/http/publish_api.rs#L98) "delegated to the node"），Gateway 写的是**本机**路径，Node 从**自己机器**的 packages_dir 构建 → 头像选择**静默丢失** |
| **V-I** | [agents.rs:952](../../core/acowork-gateway/src/http/agents.rs#L952) `upload_agent_file`（1048 写）| 写文件进 `{install_path}` | 同上 | 🔴 5xx |
| **V-J** | [agents.rs:879](../../core/acowork-gateway/src/http/agents.rs#L879) `delete_avatar_file`（905 删）| 删 `{install_path}` 文件 | 同上 | 🔴 5xx |
| **V-K1** | [agents.rs:704](../../core/acowork-gateway/src/http/agents.rs#L704) `get_avatar_config` / [739](../../core/acowork-gateway/src/http/agents.rs#L739) `update_avatar_config` | 读写 Gateway 自有 `{data_dir}/avatar_cache.json`（[agent_config.rs:178-231](../../core/acowork-gateway/src/http/agent_config.rs#L178)）+ 改内存 manifest | ADR-017 / gRPC 时代遗留，与 runtime 的 `agent_config.json` 双份真相 | 单机下"看起来对"，跨机器不体现 runtime 真实状态 |
| **V-K2** | 同上 | — | 🔴 **功能实际失效（与拓扑无关）**：Runtime 侧 `UpdateAgentConfigRequest` **明确排除** `avatar` / `builtin_avatar`（[server.rs:2312](../../core/acowork-runtime/src/http/server.rs#L2312) 注释："They keep flowing through `RuntimeConfigUpdate` over MQTT"），而 `RuntimeConfigUpdate` 已随 gRPC 在 ADR-033 被移除（Gateway 侧注释亦承认 "no longer supported"。`agent_config.rs:171` 的 `avatar` 字段**没有任何代码路径去写** → Runtime `resolve_effective_avatar()` 永远只读得到 install-time 默认值 | 换头像**永远不生效**，重启也不生效 |

**已核实：两条 UI 入口的可达性不同，必须分开处理**（更正先前的笼统判断）

| 入口 | 停止态可达？ | 证据 | 结论 |
|---|---|---|---|
| 右面板 **Setup tab**（头像选择器 / `avatar-config`）| ❌ 不可达 | [AppLayout.tsx:442-472](../../apps/acowork-desktop/src/components/layout/AppLayout.tsx#L442)：`RightNavBar hides the workspace / memory / setup / tools / debug buttons once the agent stops`，且 `activeTab` 被强制弹回 `status` | 该路径属**用户偏好**，写者 = Runtime，**只在运行态发生** → 反代给 Runtime 安全，无停止态要求 |
| 侧边栏右键 **Publish 向导**（`manifest/avatar`、`manifest/file` 上传）| ✅ **可达** | [AgentList.tsx:418-431](../../apps/acowork-desktop/src/components/agent-list/AgentList.tsx#L418) 的 `publish` 菜单项**没有任何 running 门控**（同菜单只有 `uninstall` 有系统 agent 门控）。其注释原文：`publish prepare/execute **and avatar upload** are instance-scoped routes` | ⚠️ **该路径必须在停止态可用** → **不能**反代给 Runtime（无进程）→ 它属于 **Node 的 publish 域**（见 V-R） |

→ 因此 [avatar.ts:57](../../apps/acowork-desktop/src/lib/avatar.ts#L57) 与 ADR-017 中 "works when agent is stopped" 对 **Setup 写路径**是**过期注释**，应清理。
→ **读路径**（侧边栏渲染）与 **publish 路径**仍受停止态约束（V-P / V-R）。
→ 这两条路径改写的**对象不同**：`avatar-config` 写的是用户偏好（overrides），`manifest/*` 写的是包内容（manifest）—— 见 7.1.2。

### 7.2.1 追加发现

| ID | 位置 | 事实 | 影响 |
|---|---|---|---|
| **V-Q** | [agentStore.ts:64](../../apps/acowork-desktop/src/stores/agentStore.ts#L64) `STORAGE_KEY = "acowork-agent-profiles"` → **localStorage** | 用户的 per-agent `displayName` 覆盖**只存在前端**，服务端一无所知 | ① 侧边栏 [AgentList.tsx:539](../../apps/acowork-desktop/src/components/agent-list/AgentList.tsx#L539) 用 `profile.displayName ?? agent.display_name ?? agent.name`（本地赢），但同文件 [413](../../apps/acowork-desktop/src/components/agent-list/AgentList.tsx#L413)/[427](../../apps/acowork-desktop/src/components/agent-list/AgentList.tsx#L427) 的右键菜单用 `agent.display_name ?? agent.name`（服务端）→ **改名后两处显示不同名字**；② 违反 AGENTS.md `Desktop App = ... no state persistence`；③ 两台 Desktop 连同一 Gateway 各说各话。**用户裁定：这是 bug，必须修 → `display_name` 迁到服务端 overrides（走 7.1.2 同一条路）** |
| **V-R** | publish 域归属（承接 V-L）| publish 向导停止态可达（本节上表），且 [publish.rs:46](../../core/acowork-node/src/package/publish.rs#L46) prepare 已在写 `manifest.toml`、`build` 在打包；Gateway 侧 `build_publish` 是 stub（[gateway/mod.rs:1745](../../core/acowork-gateway/src/gateway/mod.rs#L1745)）| `POST /manifest/avatar`、`POST /manifest/file` 应归 **Node 的 publish 域**，与 `PublishPrepare`/`PublishBuild` 同侧。⚠️ **但通道有矛盾**：现有 prepare/build 走 MQTT 控制命令，而 `/manifest/file` 是 **multipart 图片字节**——数据不能走 MQTT。建议 publish 的 Node 侧整体开 HTTP 面（Node 已有 HTTP server + reverse-proxy router），MQTT 只保留"开始/结果通知"。**待用户拍板** |

### 7.3 Node 边界复查（V-L / V-O）

| ID | 位置 | 事实 | 判定 |
|---|---|---|---|
| **V-L** | Node [publish.rs:314-376](../../core/acowork-node/src/package/publish.rs#L314) `prepare_publish(clean)` | Node **写** `manifest.toml`（`dev=false`）、`remove_dir_all(recordings/)`、重置配置；`build_publish` 打包该目录 | ⚠️ **越界嫌疑**：按 7.2 规则"Node 不碰 runtime 内部业务逻辑"，清理/改写 runtime 包内容属 Runtime 业务。但 publish 是「打包」，归属需要拍板。关联事实：Gateway 侧 `build_publish` 已是 stub（[gateway/mod.rs:1745](../../core/acowork-gateway/src/gateway/mod.rs#L1745) `"not available in the node topology yet"`） |
| **V-O** | [gateway/mod.rs:225](../../core/acowork-gateway/src/gateway/mod.rs#L225) `auto_install_bundled_agents` → [320](../../core/acowork-gateway/src/gateway/mod.rs#L320) `install_agent_from_dir` → [42-143](../../core/acowork-gateway/src/gateway/mod.rs#L42) `install_bundled_agent_to_disk`；[1752](../../core/acowork-gateway/src/gateway/mod.rs#L1752) `ensure_dirs` | dev_mode 下 Gateway **直接往 Node 的包目录写文件**（`config.packages_dir` 默认 = `{ACOWORK_NODE_HOME}/packages`，见 [config.rs:1071 test 注释](../../core/acowork-gateway/src/config.rs#L1071)）；`ensure_dirs` 还 `create_dir_all` 该目录 | ❌ **越界**：按「Node 管理生命周期」应改为 dispatch install。Node 侧目前**没有** bundled-agent 概念，需新增或改走 registry + URL（对齐 `install_agent` 既有做法） |
| 更正 | [gateway/mod.rs:1475-1494](../../core/acowork-gateway/src/gateway/mod.rs#L1475) | 第一轮 §3.3 V-E 把它判为「`list_agents` 元数据例外」，**判错**：它是系统 Agent 自动安装的兜底路径，与 `list_agents` 无关，应归入 V-O 同类 | — |

### 7.4 待补的接线缺口（V-M / V-N）

| ID | 位置 | 事实 | 处置 |
|---|---|---|---|
| **V-M** | Gateway [mqtt/client.rs:27-60](../../core/acowork-gateway/src/mqtt/client.rs#L27) `PERSISTENT_SUBSCRIPTIONS` | **没有** `acowork/agents/+/meta`。而 Runtime 的 retained meta 报文**已带** `avatar` / `builtin_avatar`（[mqtt/client.rs:1137](../../core/acowork-runtime/src/mqtt/client.rs#L1137)）。Gateway 当前靠 V-K1 的私有缓存反推头像 | 删掉 avatar_cache 后，`list_agents` 的头像需改为：Gateway 订阅 meta → 存 `AgentInfo` → 按 `config.avatar > config.builtin > manifest.*` 解析。**数据走 HTTP，变更通知走 MQTT**，符合 7.2 规则 |
| **V-N** | [dispatch.rs:514](../../core/acowork-gateway/src/mqtt/dispatch.rs#L514) / [1283](../../core/acowork-gateway/src/mqtt/dispatch.rs#L1283) / [1370](../../core/acowork-gateway/src/mqtt/dispatch.rs#L1370) / [agents.rs:1751](../../core/acowork-gateway/src/http/agents.rs#L1751) | `{install_path}/workspace` 被拼成字符串塞进 `RunningAgentInfo.workspace` | ✅ **纯展示/日志字段，不触碰文件系统**，拓扑无害。但会被 §5.4 的 ceiling lint 计入，属误报——保留作为"路径不得当真用"的哨兵 |

### 7.5 终态路由归属表（target state）

| 端点 | 现状 | **目标归属** | 通道 |
|---|---|---|---|
| `GET /api/agents/{id}/avatar` | 反代 Runtime（V-P 回归）| **Node**（只读资产服务）| HTTP 反代 |
| `GET /api/agents/{id}/avatar-file` | 反代 Runtime（V-P 回归）| **Node** | HTTP 反代 |
| `GET /api/agents/{id}/manifest/avatar-assets` | 反代 Runtime（V-P 回归）| **Node** | HTTP 反代 |
| `GET/PUT /api/agents/{id}/avatar-config` | Gateway 私有缓存（V-K1）| **Runtime**（唯一写者，写 overrides）| HTTP 反代 |
| `POST /api/agents/{id}/manifest/avatar` | Gateway 写 manifest（V-H）| **Node**（publish 域，包内容）| HTTP（V-R 待拍）|
| `POST /api/agents/{id}/manifest/file`（multipart）| Gateway 写文件（V-I）| **Node**（publish 域，包内容）| HTTP（**必须**，字节不能走 MQTT）|
| `DELETE /api/agents/{id}/avatar-file` | Gateway 删文件（V-J）| 拆：删**包内**文件 = Node；清**用户偏好** = Runtime | HTTP |
| `display_name` 覆盖（V-Q）| Desktop localStorage | **Runtime**（写 overrides）| HTTP 反代 |

> **读侧统一走 Node** 的关键收益：运行态/停止态**同一条路，无分支**。双路径（先试 Runtime 失败再回落）正是这轮 review 的病根。
> 副作用：Phase 2 加在 runtime 的 3 个 avatar 读端点变冗余，**应删除**（不留埋雷）。

### 7.6 新增工单（Phase 5）

| # | 工单 | 依赖 | 优先级 |
|---|---|---|---|
| 1 | **proto + Node 搬运**：`InstalledAgentInfo` 加 `overrides_json = 7`（不透明）；Node 从 `{agent_id}/{instance_id}.overrides.json` 原样读入并随 retained inventory 发布（**不解析**）| — | 🔴 最高 |
| 2 | **Runtime：用户偏好写路径**：`PUT /avatar-config` 落 `{agent_id}/{instance_id}.overrides.json`（兄弟文件，**不在包目录内** → upgrade 不冲掉）；补 `display_name` 支持；修复 V-K2（权重入 `agent_config.json` 的断链）| 1 | 🔴 最高 |
| 3 | **Gateway 消费**：`AgentInfo` 加**独立** `overrides` 字段（**不得写回 manifest**，否则被 `upsert_installed_from_node` 冲掉）；`list_agents` 单点合并 `overrides > manifest`；删除 `avatar_cache.json` 全套（agent_config.rs:178-231、agents.rs:831/928、dispatch.rs:847）| 1,2 | 🔴 高 |
| 4 | **live 变更通道**：Runtime 写 overrides 后发布变更事件（**事件不带数据**）；Gateway 收到后拉取刷新 | 2,3 | 🔴 高 |
| 5 | **读侧改走 Node**：Node 加只读资产端点（`assets/*` 读取 + 目录列表）；Gateway 3 条 avatar 读路由改反代 Node；**删除** Phase 2 加的 runtime `http/avatar.rs` 读端点 | 1 | 🔴 高 |
| 6 | **V-R（publish 域）**：`manifest/avatar` + `manifest/file` 归 Node publish 域；确定 Node 侧走 HTTP 还是控制命令；multipart 必须 HTTP | **用户拍板** | 🟡 中 |
| 7 | **V-O**：bundled agent 安装改走 dispatch（对齐 `install_agent`）；`ensure_dirs` 不再创建 Node 包目录 | 6 | 🟡 中 |
| 8 | **V-Q 前端**：删除 `agentStore` 的 localStorage `displayName` 覆盖，改读服务端；修掉侧边栏/右键菜单两处名字不一致 | 2,3 | 🟡 中 |
| 9 | **V-N / lint**：ceiling 加注释说明误报；改造后收紧 `ADR009_FS_CEILING`（当前 `gateway/mod.rs:2 http/agents.rs:4 mqtt/dispatch.rs:3`）| 3,7 | 🟢 低 |
| 10 | 文档：清理 ADR-017 "works when agent is stopped" 过期注释；**重写 ADR-009 zh §5.5**（前两稿方向均错误）| 全部定型 | 🟡 中 |
| 11 | **存疑待查**：`avatar_update` 控制命令（[control/mod.rs:432](../../core/acowork-node/src/control/mod.rs#L432) `not_implemented until ADR-055 Phase 2c`）—— 若它指**用户偏好**则该删（数据不该走 MQTT）；若指 **publish 烘焙包内容**则可能是合法的控制命令。与工单 6 一起定 | 6 | 🟢 低 |

> **状态（2026-09-11 早）**：工单 **5–8 + 11 已实施**（详见 §7.9）。剩下 9（lint ceiling）、10（ADR-009 §5.5 重写）属于收尾文档，可批量做。


### 7.7 本节的证据边界（未验证项）

- 7.1 的「停止态 503」由代码路径推定（`proxy.rs` 的 endpoint 缺失分支 + 前端无条件渲染）。**未做运行时验证**（当前 runtime exe 被占用，按要求未编译）。工单 1 动手前应先用一次实机复现确认。
- V-K2 的「换头像永远不生效」同样是静态推定（`UpdateAgentConfigRequest` 字段排除 + 无写入路径）。建议实机验证一次：running 态改头像 → 重启 agent → 看头像是否回退。

### 7.8 Phase 5 实施状态（2026-09-10 晚，工单 1–4）

主线工单 1→2→3→4 已落地；5 及之后未动（见 §7.6 表）。详见 §7.9 工单 5/6/7/8/11 实施。

| # | 工单 | 状态 | 落点 |
|---|---|---|---|
| 1 | proto + Node 搬运 | ✅ | [mqtt_payload.proto](../../core/acowork-core/proto/mqtt_payload.proto) `InstalledAgentInfo.overrides_json = 7`（不透明）；[package/mod.rs](../../core/acowork-node/src/package/mod.rs) `read_overrides_json` 原样读入；[uninstall.rs](../../core/acowork-node/src/package/uninstall.rs) 一并删兄弟文件 |
| 2 | Runtime 用户偏好写路径 | ✅ | 新增 [agent_overrides.rs](../../core/acowork-core/src/agent_overrides.rs)（`AgentOverrides` + `overrides_path`）；[http/avatar.rs](../../core/acowork-runtime/src/http/avatar.rs) `PUT /agents/{id}/avatar-config` 是**唯一写者**，含 `display_name`；`agent_config.json` 的 `avatar` / `builtin_avatar` 死字段删除（V-K2 断链清除） |
| 3 | Gateway 消费 | ✅ | [state.rs](../../core/acowork-gateway/src/gateway/state.rs) `GatewayState.agent_overrides`（**独立于 `AgentInfo.manifest`**，故 `upsert_installed_from_node` 冲不掉）；[agents.rs](../../core/acowork-gateway/src/http/agents.rs) `list_agents` + `get_agent_detail` 单点合并 `overrides > manifest`；`avatar_cache.json` 全套删除（agent_config.rs / dispatch.rs / agents.rs） |
| 4 | live 变更通道 | ✅ **方案收敛**（见下） | [proxy.rs](../../core/acowork-gateway/src/http/proxy.rs) `forward_avatar_config_put`：PUT 成功后把 Runtime 返回的 `overrides` **原样镜像**进 `GatewayState` |

#### 7.8.1 工单 4 为什么没有走 MQTT 事件

工单 4 原表述是「Runtime 写 overrides 后发布变更事件（事件不带数据）；Gateway 收到后拉取刷新」。实施时收敛为 **HTTP 写入路径上的镜像**，理由：

1. **Gateway 本来就在写路径上**——overrides 的**唯一**写入者是 Runtime 的 `PUT /agents/{id}/avatar-config`，而前端只经 Gateway 反代到达它。Gateway 手里已经有权威结果，再让 Runtime 发一条不带数据的 MQTT 事件、Gateway 收到后再发 HTTP 回查 Runtime，是**多一个来回干同一件事**。
2. **镜像值不由 Gateway 猜**——存的是 Runtime 落盘后返回的 `overrides` 原文，所以「Node retained 来源」与「live 来源」结构上不可能不一致（这两个来源分歧正是本方案要防的 bug）。
3. MQTT 主题**零新增**，不触碰 broker 的 retained 语义，也不用新增订阅。

**已知天花板（可接受，有升级路径）**：若将来出现**不经过 Gateway** 的 overrides 写入方（例如 Runtime 内部自我改名），Gateway 的镜像会**陈旧到下一次 Node 重发 retained inventory**（Node 重启 / 装卸 / 升级）才纠正。届时升级路径 = Runtime 发一条 `acowork/agents/{id}/overrides-changed`（无 payload）事件，Gateway 收到后按 7.2 规则发 HTTP 拉取。

#### 7.8.2 本轮验证

- `cargo check -p acowork-core -p acowork-node -p acowork-gateway -p acowork-runtime`：**通过，零 error / 零 warning**。
- 单元测试（`cargo test --lib`，独立 target dir 以避开被占用的 `target/debug/*.exe`）：**2213 passed / 0 failed**（core 210 + node 448 + gateway 142 + runtime 1413）。其中新增：`agent_overrides::tests::{path_is_a_sibling_of_the_instance_dir, missing_or_malformed_json_degrades_to_no_overrides}`、`http::avatar::tests::{traversal_escapes_are_rejected, extension_whitelist_is_case_insensitive}`。
- **未做**端到端实机验证（runtime exe 被占用，按要求未编译 exe）。因此 §7.1 的停止态头像回归（V-P）**仍然存在**——它由工单 5（读侧改走 Node）解决，本轮没动。改头像 → 重启 → 头像是否保留，仍需一次实机确认。
---

### 7.9 Phase 5 实施状态（2026-09-11 早，工单 5–8 + 11）

#### 7.9.1 工单 5：读侧改走 Node（V-P 回归修复）

| 改动 | 落点 |
|---|---|
| 新增 Node 只读资产端点 | [assets.rs](../../core/acowork-node/src/assets.rs)（~300 行）`/agents/{id}/avatar`、`/agents/{id}/avatar-file`、`/agents/{id}/manifest/avatar-assets` |
| 复用 Phase 5a auth | `proxy/mod.rs` `authorize()` 验证 `X-ACowork-Node-Token` |
| 挂载到 Node 同一 :19900 | [control/mod.rs:1216](../../core/acowork-node/src/control/mod.rs) node HTTP listener |
| Gateway 改反代 | [proxy.rs](../../core/acowork-gateway/src/http/proxy.rs) 三条 avatar 读路由改反代 Node |
| 删除 Runtime 冗余读端点 | [http/avatar.rs](../../core/acowork-runtime/src/http/avatar.rs) 3 读端点 → 只保留 `GET/PUT /avatar-config` |

**关键收益**：运行态/停止态走同一条路，无 fallback 分支。`list_agents` 拿不到镜像时就缺失（**确定性 fallback**），不再随机回落图标。

#### 7.9.2 工单 6：publish 域写端点搬到 Node

| 改动 | 落点 |
|---|---|
| Node 写端点 | [package_http.rs](../../core/acowork-node/src/package_http.rs)（~600 行）：`PUT /agents/{id}/manifest/avatar`、`POST /agents/{id}/manifest/file`（10 MB cap + 路径遍历 guard + 扩展名白名单）、`DELETE /agents/{id}/avatar-file` |
| Gateway 改反代 | [proxy.rs](../../core/acowork-gateway/src/http/proxy.rs) 三路由改反代 Node |
| Gateway 侧旧 handler 保留但改为反代 | [agents.rs](../../core/acowork-gateway/src/http/agents.rs) `update_agent_manifest_avatar` / `upload_agent_file` / `delete_avatar_file` 路由仍注册（前端 Tauri command 不动）；handler 改反代 Node，不再本地写文件 |
| 决定 | **HTTP，不走 MQTT**：multipart 字节流不能走控制平面（V-R §5）；用户最终拍板 |

**为什么不动 Tauri command**：前端几十处调用点，搬运成本远大于「Gateway 多转一道」。

#### 7.9.3 工单 7：bundled agent 安装改走 dispatch（V-O）

| 改动 | 落点 |
|---|---|
| 删除 Gateway 内置 fs 写入 | [gateway/mod.rs](../../core/acowork-gateway/src/gateway/mod.rs) 删除 `install_bundled_agent_to_disk` / `copy_dir_recursive` / `has_two_level_instance` / `install_agent_from_dir` / `auto_install_bundled_agents`（旧 boot-time 写盘） |
| 新增 `dispatch_bundled_agent_install` | 把 bundled dir 打成 `.agent` ZIP 写到 **Gateway 自己拥有的** `package_registry_dir()`，再发 `install_agent_by_url(NodeInstallDispatch { ensure: true })` 给 node |
| System-Agent 启动路径改造 | `gateway/mod.rs` 的 SA bootstrap task 改为：等 node retained → 没有就 dispatch bundled install → 等 inventory 出现 → start。`packages_dir` 字符串引用删除（sa_packages_dir） |
| `GatewayConfig::ensure_dirs` 移除 `packages_dir` | 该目录归 node 所有（[config.rs](../../core/acowork-gateway/src/config.rs)）；Gateway `ensure_dirs` 只建 vault + data + package-registry |

**结果**：Gateway 在 bundled 路径上**零** fs 写包目录。所有路径都走 node control plane → node 本地 install → retained publish → Gateway 镜像。

#### 7.9.4 工单 8：前端 display_name 清理（V-Q）

| 改动 | 落点 |
|---|---|
| 删除 localStorage 覆盖 | [agentStore.ts](../../apps/acowork-desktop/src/stores/agentStore.ts) `AgentProfileSettings.displayName` 字段删除；`loadAllProfiles` / `normalizeProfile` / `DEFAULT_PROFILE` 同步清理 |
| 全部 UI 读服务端 | [AgentList.tsx](../../apps/acowork-desktop/src/components/agent-list/AgentList.tsx)、[AppLayout.tsx](../../apps/acowork-desktop/src/components/layout/AppLayout.tsx)、[ChatPanel.tsx](../../apps/acowork-desktop/src/components/chat/ChatPanel.tsx)、[chatStore.ts](../../apps/acowork-desktop/src/stores/chatStore.ts) 全部去掉 `?? profile?.displayName` 兜底 |
| 改名为 HTTP | [AgentSetupTab.tsx](../../apps/acowork-desktop/src/components/right-panel/AgentSetupTab.tsx) 输入框 commit 走 `updateAvatarConfig({display_name})`（Runtime PUT 已支持），onBlur/Enter 触发，`nameDraft` 暂存未提交值 |
| type 同步 | [types.ts](../../apps/acowork-desktop/src/lib/types.ts) `AvatarConfigResponse.display_name` 字段已存在；`UpdateAvatarConfigRequest.display_name` 新增 |

**结果**：sidebar / 右键菜单 / 头像 tooltip 全部读 `agent.display_name`（来自 node retained inventory 的 overrides_json），server-side source of truth。

#### 7.9.5 工单 11：`avatar_update` MQTT 控制命令删除

判定：**用户偏好走 HTTP**（V-R §5 规则），该命令无消费者。删除：
- [mqtt_payload.proto](../../core/acowork-core/proto/mqtt_payload.proto) `NodeAvatarUpdate` 消息体 + `node_control_command::Command::AvatarUpdate` 字段 16 注释为 retired
- [control/mod.rs](../../core/acowork-node/src/control/mod.rs) `agent_lifecycle_instance_id` 的 AvatarUpdate 分支 + `handle_command` 的 `not_implemented` 分支
- [node_control.rs](../../core/acowork-gateway/src/mqtt/node_control.rs) `command_name` 的 AvatarUpdate 分支

**proto 字段保留为 retired 注释**，避免新代码复用字段 16。

#### 7.9.6 本轮验证

- `cargo check --workspace --all-targets`：**通过，零 error / 零 warning**
- `cargo clippy --workspace --all-targets -- -D warnings`：**通过**（含一处历史 e2e 测试的 `while_let_loop` 加了 `#[allow]` 注释，理由：timeout-only 的 break 分支无法被 `while let` 表达）
- 单元测试（`CARGO_TARGET_DIR=target-test cargo test --workspace --lib --exclude acowork-embed`）：**core 210 + node 98 + gateway 447 + runtime 215 = 970 passed / 0 failed**（lsp-relay 5 个 pre-existing 失败因 exe 锁 + embed 跳过）
- 桌面 `npx tsc --noEmit`：**通过**
- `dev/ci.sh` 红线：`Gateway filesystem red line: OK`、`acowork-node dependency red line: OK`、`MQTT ErrorKind red line: OK`
- `dev/ci.sh all` 中的 `cargo build --workspace --bins` 与 smoke 因 runtime / doc / embed exe 被占用跑不动（按要求未编译 exe），静态路径全绿
- **未做**实机验证（exe 锁未释放）：改头像 → 重启 → 头像是否保留、setting 进得去仍需一次完整 runtime 启动确认（V-P）

#### 7.9.7 已知天花板

- **bundled 安装依赖 Node 在 SA bootstrap 前启动**：本机单 machine OK；多 machine 拓扑下 SA 也只能装在 `local_node` 上，与 `install_agent` 既有路径一致。
- **`dispatch_bundled_agent_install` 直接调 `nc.install_agent_by_url` 而非经过 `OperationRecord`**：bundled 不需要操作追踪，retry 靠 `ensure=true` 在 node 端幂等。
- **桌面 `nameDraft` commit 失败时静默**：与原 `setProfile` 行为对齐（不弹 toast）；日志走 `log.warn`。


---

## 8. 关联工单与参考

- 本次 bug 链的运行时侧修复：commit 在 HEAD（`inbound.rs` + `gateway_loop.rs` + `agent_core.rs` + `agent_init.rs` + `session_init.rs` + `session_manager.rs` + `skills/parser.rs` + `startup/context.rs`）
- 上下游相关 ADR：
  - [ADR-009 Gateway Workspace Isolation](../adr/en/ADR-009-gateway-workspace-isolation.md)（英文，本评审主线）
  - [ADR-040 Remove gRPC, MQTT/HTTP Only](../adr/zh/ADR-040-remove-grpc-mqtt-http-only.md)
  - [ADR-055 Remote Runtime Node Topology](../adr/zh/ADR-055-remote-runtime-node-topology.md)
  - [ADR-058 Workspace FS Watcher → MQTT Event](../adr/zh/ADR-058-workspace-fs-watcher-mqtt-event.md)

---

## 附录

### A.1 本次 bug 链完整证据

**前端发送（`apps/acowork-desktop/src/stores/chatStore.ts:1308` 起）**：

```typescript
sendMessage: async (content, agentId, command, attachedItems) => {
  // command = activeSkill?.name（只传技能名）
  invoke("mqtt_publish_control", {
    instanceId: agentId,
    command: "chat_message",
    payloadJson: { session_id, message_id, content, command: command ?? "", params_json },
  });
}
```

**MQTT proto（`core/acowork-core/proto/mqtt_payload.proto:926–931`）**：

```protobuf
message ChatMessage {
  string session_id = 2;
  string message_id = 3;
  string content = 4;
  /// Optional slash command prefix (e.g. "/commit", "/review-pr")
  string command = 5;     // ← 技能名走这里
  ...
}
```

**修复前的断点（`core/acowork-runtime/src/startup/gateway_loop.rs`）**：

```rust
// 之前：command 被丢弃
ControlAction::SendMessage {
    session_id, message_id, content,
    command: _,           // ← 第一次断链
    params_json,
} => Some((session_id, InboundMessage::ChatMessage {
    content, message_id, params_json,
}))
```

**修复后的链路（HEAD）**：

```rust
// 之后：command 透传 + 运行时解析
InboundMessage::ChatMessage {
    content, message_id, command, params_json,  // command 透传
} => {
    // 解析 params_json
    let mut skill_instructions = ...;

    // 运行时按 command 找 SkillRegistry（运行时权威）
    if skill_instructions.is_none() && !command.is_empty() {
        if let Some(instructions) = session_manager
            .lock().await
            .resolve_skill_instructions(&command)
        {
            skill_instructions = Some(instructions);
        }
    }
    // → SessionMessage.skill_instructions → ContextBuilder.set_skill_instructions
    //   → 系统提示词包含 "## Skill Instructions" 段
    //   → compute_section_sizes 产出 skill_instructions section
    //   → onContextBuilt 事件含 skills 占比 → 弹窗 > 0
    //   → Debug Panel 出现 Skill Instructions 行
}
```

### A.2 grep 审计原始命令

```bash
# 1. 文件系统读
grep -rn "std::fs::\|tokio::fs::\|read_dir\|read_to_string\|File::open" \
  core/acowork-gateway/src/

# 2. agent package 内部布局字符串
grep -rn 'join("skills")\|join("prompts")\|join("manifest")\|"skills/"\|"prompts/"\|"manifest\.toml"\|SKILL\.md\|\.agent"' \
  core/acowork-gateway/src/

# 3. install_path / work_dir / package_dir
grep -rn "install_path\|work_dir\|package_dir\|package_path" \
  core/acowork-gateway/src/

# 4. ADR-009 / ADR-055 引用点（确认原则已落地但执行不彻底）
grep -rn "ADR-009\|ADR-040\|ADR-055" core/acowork-gateway/src/
```

