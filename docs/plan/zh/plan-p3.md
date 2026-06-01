# Phase 3: 权限与工具安全 — 实施计划

> 版本：v2.0 | 更新日期：2026-04-25
> 前置条件：Phase 2（M5~M9）全部完成
> 预计周期：11~13 周
> 预计里程碑：M10~M14

---

## 背景与目标

Phase 1 交付了 MVP 执行链路，Phase 2 交付了 Grafeo 仿生记忆 + System Agent + Intent 路由 + 多 Provider。当前安全能力仅为策略级隔离（Runtime 路径白名单检查），存在 WASM 工具仅有声明无沙箱执行、无运行时权限请求机制、Shell 工具无风险分级等缺口。

Phase 3 的核心价值：从"能运行"升级到"安全运行"——

1. **权限框架**：声明 + 授权 + 运行时请求 + Approval Gate 全链路
2. **WASM 沙箱完整实现**：Wasmtime 集成 + WIT 组件模型升级
3. **Shell 安全分级**：FileProvenance + ShellRisk + 命令-文件关联分析
4. **离线巩固**：Phase 2 的即时提取对应的离线批量处理（三元组提取 + 经验泛化）
5. **质量评估**：LongMemEval 集成 + 检索质量可观测

### 关于进程级沙箱的决策（ADR-007）

原计划 S2（进程级沙箱隔离：bubblewrap + AppContainer）已**延后至 Phase 7**。理由：

1. **威胁模型不匹配**：当前 Rollball 是个人桌面 Agent 平台，用户跑的是自己选择/编写的 Agent。威胁主体是"Agent 被 Prompt Injection 利用"，不是"不可信第三方恶意 Agent"。权限框架 + Shell 分级 + Approval Gate 已构成完整的第一层防御。
2. **ROI 太低**：S2 占原 Phase 3 的 1/3 工期（5 周），解决的子进程越权问题在当前场景下极少发生。
3. **外部依赖负担**：bubblewrap 需要安装/分发策略、CI 需要 user namespace、AppContainer 需要管理员权限——这些在当前阶段不值得承担。
4. **替代方案可行**：对子进程越权问题，Shell 工具默认不提供（manifest 显式声明才启用）+ ShellRisk 分级 + Approval Gate 已构成足够防线。

Phase 7 将与 macOS Seatbelt 一起实现跨平台进程级沙箱，届时场景可能演进到企业/多租户。详见 `docs/08-security.md` §2.2 和 ADR-007。

---

## 阶段划分

### S1：权限声明与授权框架（3 周，8 项任务）

Phase 2 的 `rollball-core` 已定义 `Permission` 类型，但缺少完整的授权链路。S1 建立从声明到校验的完整权限框架。

**涉及 crate**：`rollball-core`、`rollball-gateway`、`rollball-runtime`

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S1.1 权限模型完善 | 扩展 `Permission` 枚举覆盖全部权限类型（filesystem/network/memory/intent/identity/shell/wasm）；定义 `PermissionGrant`（授权记录：who/what/when/scope）；定义 `PermissionPolicy`（默认/允许/拒绝/每次询问） | 12 | Permission 序列化/反序列化、grant 匹配逻辑 |
| S1.2 权限持久化存储 | Gateway 侧 `PermissionStore`（rusqlite），存储用户对每个 Agent 的授权决策；支持 CRUD + 按 agent_id 查询 | 8 | 存储/查询/撤销/过期授权 |
| S1.3 安装时权限审查 | Package Manager 安装流程增加权限审查步骤：解析 manifest 权限声明 → 对比已授权权限 → 新权限需用户确认 → 记录授权 | 6 | 安装时弹出权限列表、拒绝安装、部分授权 |
| S1.4 运行时权限校验器 | Runtime 侧 `PermissionChecker`：工具执行前校验权限，缓存已授权权限（启动时从 Gateway 获取）| 8 | 工具调用被权限拦截、缓存命中/miss |
| S1.5 运行时权限请求 | Runtime → Gateway `PermissionRequest` 消息类型；Gateway 向用户展示请求（CLI 交互 / 未来 Desktop App 弹窗）；用户响应后更新 PermissionStore 并回传 | 6 | 运行时请求 → 用户确认 → 授权生效 |
| S1.6 权限升级通知 | Agent 升级时，新版 manifest 新增权限 → Package Manager 检测差异 → 用户确认新增权限 | 4 | 升级检测权限差异、用户确认流程 |
| S1.7 权限撤销与重置 | Gateway CLI 命令 `rollball permission revoke/reset`；撤销后 Runtime 缓存失效机制 | 4 | CLI 撤销、Runtime 感知 |
| S1.8 权限集成测试 | 端到端：安装带权限声明的 Agent → 授权 → 运行时校验 → 运行时请求 → 撤销 | 6 | 全链路通过 |

**里程碑 M10：权限框架可用** — 安装时权限审查 + 运行时权限校验 + 运行时权限请求 全链路打通

预期测试合计：54 项

---

### S2：WASM 工具沙箱（3 周，6 项任务）

Phase 1 声明了 WASM 工具格式，Phase 3 实现完整的 Wasmtime 集成和 WIT 组件模型升级（对应 `docs/12-tool-system.md` §3 设计）。

**涉及 crate**：`rollball-runtime`（新建 `tools/wasm/` 模块）

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S2.1 Wasmtime 引擎集成 | 引入 wasmtime crate（LTS 版本）；`WasmEngine` 单例管理：预编译缓存、Cranelift 配置、Fuel metering 启用 | 6 | Engine 初始化、模块编译、Fuel 计量 |
| S2.2 WASM 实例管理 | `WasmToolInstance`：加载 .wasm → 编译 → 实例化 → 执行 → 销毁；线性内存分配/回收；execute() Host 函数注册 | 10 | 工具加载执行、内存限制、超时终止 |
| S2.3 WASI Preview 2 权限映射 | manifest 权限 → WASI capability 映射：filesystem 权限 → WASI 目录预开放（preopens）；network 权限 → WASI socket 能力；**WASI 权限是 WASM 工具的唯一文件系统/网络防线**（进程级沙箱延后至 Phase 7） | 8 | 权限映射正确、越权访问被拒绝 |
| S2.4 WIT 组件模型升级 | 定义 `rollball-tool.wit` 接口（替代 Phase 1 的手动 Host 函数）；工具输入/输出类型安全绑定；向后兼容 Phase 1 的 execute(ptr, len) 方式 | 8 | WIT 接口调用成功、旧格式兼容 |
| S2.5 rollball-tool-sdk | 发布 `rollball-tool-sdk` crate（wasm32-wasip2 目标）：`#[tool]` proc macro、自动 JSON 序列化、schema 导出 | 6 | SDK 示例工具编译运行 |
| S2.6 WASM 工具集成测试 | 编写示例 WASM 工具（image_filter）；端到端：LLM tool_call → WASM 沙箱执行 → 结果返回 | 4 | 示例工具在沙箱中执行成功 |

**里程碑 M11：WASM 沙箱可用** — WASM 工具在 Wasmtime 沙箱中隔离执行，支持 WIT 组件模型

预期测试合计：42 项

---

### S3：Shell 安全分级与 Approval Gate（3 周，7 项任务）

对应 `docs/08-security.md` §11 的完整设计：FileProvenance + ShellRisk + Approval Gate。

**涉及 crate**：`rollball-runtime`（`tools/builtin/shell.rs` 增强 + 新建 `security/` 模块）、`rollball-gateway`

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S3.1 FileProvenance 文件来源追踪 | `FileProvenance` 结构体 + `FileSource` 枚举（CreatedByTool/Downloaded/PreExisting/Unknown）；工作区启动扫描；file_write/network_fetch/shell 执行后自动更新来源；**rusqlite 持久化**（Agent 重启后恢复来源记录） | 10 | 来源记录正确、shell 后新文件标记 Unknown |
| S3.2 ShellRisk 风险分级引擎 | `ShellRisk` 四级分类（Low/Medium/High/Blocked）；命令解析器（提取可执行文件路径、检测危险模式）；基础命令白名单/黑名单 | 12 | 各等级命令分类正确、边界 case |
| S3.3 命令-文件关联分析 | `assess_shell_risk()` 函数：base_risk + FileProvenance 交叉查询；Downloaded/Unknown 文件被执行时提升到 High | 8 | 下载文件执行 → High、已知文件执行 → 维持原级 |
| S3.4 Approval Gate | `ApprovalGate` trait + CLI 实现：Medium/High 风险工具执行前暂停 → 向用户展示风险信息 → 等待确认/拒绝/始终允许；Gateway 侧 Approval 消息路由 | 8 | 暂停等待确认、拒绝后工具返回错误 |
| S3.5 工作区文件系统监控 | `FsWatcher`：基于 `notify` crate 的跨平台抽象层（不手动封装 inotify/FSEvents/ReadDirectoryChangesW）；监控新文件创建、权限变更、符号链接；异常模式检测 | 8 | 新可执行文件检测、符号链接告警 |
| S3.6 审计日志 | Shell 执行审计记录（command/risk_level/reason/approved_by/exit_code/files_created/files_modified）；结构化 JSON 日志写入 Agent 工作目录 | 4 | 审计记录完整、可回溯 |
| S3.7 Shell 安全集成测试 | 端到端场景：下载文件 → 尝试执行 → Approval Gate 拦截 → 确认后执行 → 审计日志 | 4 | 完整攻击场景拦截 |

**里程碑 M12：Shell 安全可用** — Shell 命令风险分级 + Approval Gate + 审计日志 全链路打通

预期测试合计：54 项

---

### S4：离线巩固与记忆质量（3 周，6 项任务）

Phase 2 实现了"即时提取"（每轮 tool_call），S4 实现对应的"离线巩固"——批量处理积累的 Episode，提取三元组、发现模式、经验泛化。同时建立记忆质量评估框架。

**涉及 crate**：`rollball-grafeo`、`rollball-runtime`

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S4.1 巩固调度器 | `ConsolidationScheduler`：定时触发（可配置间隔，默认 idle 30min 后）+ 手动触发 API；批次管理（未巩固 Episode 队列 → 分批处理 → 进度跟踪）| 6 | 定时触发、批次处理、进度持久化 |
| S4.2 三元组提取 | LLM 驱动的三元组提取：Episode 文本 → (subject, predicate, object) 三元组列表；去重（语义相似度阈值）；写入沉淀层 KnowledgeNode | 10 | 提取准确率、去重逻辑、写入正确 |
| S4.3 冲突分类与证据验证 | 新三元组与已有知识冲突时：Evolution（知识更新）/ Correction（错误修正）/ Ambiguous（保留双方）自动分类；LLM 证据验证 | 8 | 三种冲突类型分类正确、证据链记录 |
| S4.4 经验泛化（模式提炼） | 从多个 Episode 中提炼 ProceduralNode：识别重复行为模式 → 抽象为通用步骤 → 关联到 Skill；泛化置信度评估 | 8 | 模式识别、ProceduralNode 生成 |
| S4.5 质量评估框架 | `RetrievalMetrics`：precision@k、recall@k、MRR；LongMemEval 5 维评测（信息提取/跨会话推理/时序推理/知识更新/信息否定）集成 | 8 | 评测脚本可运行、基线指标记录 |
| S4.6 巩固集成测试 | 端到端：10 轮对话 Episode 积累 → 触发巩固 → 三元组提取 → 冲突处理 → 经验泛化 → 质量评估 | 4 | 巩固后检索质量提升可量化 |

**里程碑 M13：离线巩固可用** — Episode 批量巩固 + 三元组提取 + 经验泛化 + 质量评估

预期测试合计：44 项

---

### S5：集成验证与安全审计（1~2 周，4 项任务）

全系统集成验证，确保 S1~S4 各模块协同工作。

| 任务 | 内容 | 预期测试 | 验收标准 |
|------|------|---------|---------|
| S5.1 权限 + WASM 联动 | manifest 权限 → WASI capability 映射 → WASM 工具越权访问被拒绝 | 4 | WASM 工具越权被阻止 |
| S5.2 权限 + Shell 联动 | Shell 工具权限校验 → ShellRisk 分级 → Approval Gate → 审计日志 | 4 | 无 shell 权限时工具不可用，高风险操作需确认 |
| S5.3 安全红队测试 | 模拟攻击场景：Prompt Injection → 恶意 shell → 文件越权 → 网络渗透；验证应用层防御各层有效。**标注**：此为内部安全验证，不是正式安全审计；攻击场景覆盖 5 个定义场景，不承诺覆盖所有可能攻击 | 6 | 定义场景全部被拦截或告警 |
| S5.4 巩固效果验证 | 离线巩固后检索质量提升的量化验证 | 2 | 巩固后指标优于巩固前 |

**里程碑 M14：Phase 3 安全交付** — 全系统安全验证通过、定义攻击场景拦截率 100%

预期测试合计：16 项

---

## 总览

| 阶段 | 主题 | 任务数 | 预期测试 | 预计周期 |
|------|------|--------|---------|---------|
| S1 | 权限声明与授权框架 | 8 | 54 | 3 周 |
| S2 | WASM 工具沙箱 | 6 | 42 | 3 周 |
| S3 | Shell 安全分级与 Approval Gate | 7 | 54 | 3 周 |
| S4 | 离线巩固与记忆质量 | 6 | 44 | 3 周 |
| S5 | 集成验证与安全审计 | 4 | 16 | 1~2 周 |
| **合计** | | **31** | **210** | **11~13 周** |

**与原 v1.1 的差异**：

| 变更 | 原计划 | 新计划 | 理由 |
|------|--------|--------|------|
| 进程级沙箱 | S2（5 周，8 任务，64 测试） | 延后至 Phase 7 | ADR-007：当前威胁模型不需要内核级隔离，ROI 太低 |
| 阶段编号 | S1~S6 | S1~S5 | 去掉 S2 后重编号 |
| 里程碑 | M10~M15 | M10~M14 | 减少 1 个里程碑 |
| 总周期 | 15~19 周 | 11~13 周 | 减少进程沙箱 + 集成验证瘦身 |
| 总测试 | 278 | 210 | 进程沙箱 64 测试 + 集成验证 4 测试（沙箱联动）移除 |

---

## 依赖关系

```
S1（权限框架）──┬──→ S2（WASM 沙箱）──→ S5.1
               ├──→ S3（Shell 安全）──→ S5.2
               └──────────────────────→ S5.3
S4（离线巩固）──────────────────────→ S5.4
```

- S1 是 S2/S3 的前置（权限模型 + 校验器是 WASI 权限映射和 Shell 安全分级的基础）
- S2/S3 可并行（两条独立的安全能力线）
- S4 独立于 S1~S3（记忆系统增强，不依赖安全模块）
- S5 依赖 S1~S4 全部完成

---

## 关键技术决策点

| 编号 | 决策项 | 决策 | 理由 |
|------|--------|------|------|
| D1 | 进程级沙箱实施时机 | **延后至 Phase 7** | ADR-007：当前场景不需要，Phase 7 与 macOS Seatbelt 一起做 |
| D2 | WASM 沙箱的防御定位 | **WASI 是 WASM 工具的唯一文件系统/网络防线** | 进程沙箱延后，WASI 权限必须独立完整 |
| D3 | WASM 组件模型升级时机 | **S2 同步完成** | WIT 提供类型安全，越早切越少迁移成本 |
| D4 | 离线巩固 LLM 调用策略 | **远程 API 优先 + 本地可选** | 不依赖开发者硬件，与 Embedding 的 Local → Remote → Disabled 策略一致 |
| D5 | Approval Gate UI | **CLI + trait 抽象（Desktop 预留）** | Phase 5 Desktop App 需要接入，提前抽象 |
| D6 | FileProvenance 持久化 | **rusqlite** | Agent 重启后需恢复来源记录；权限数据不需要向量检索/图遍历，关系模型更简单 |
| D7 | 巩固触发策略 | **定时 + idle + 手动三种** | 定时保证基线，idle 利用空闲，手动给用户控制权 |
| D8 | FsWatcher 实现 | **基于 `notify` crate** | 不手动封装 inotify/FSEvents/ReadDirectoryChangesW，减少维护成本 |

---

## 与后续 Phase 的关系

- **Phase 4（通信与协调）**：Phase 3 的权限框架为 Intent 跨 Agent 通信提供权限校验基础
- **Phase 5（Desktop App）**：Approval Gate 的 CLI 实现升级为 Desktop App 弹窗；权限管理 UI
- **Phase 7（跨平台）**：进程级沙箱（bubblewrap / AppContainer / Seatbelt）+ `rollball-sandbox` crate 实现
- **Phase 6（云端生态）**：Agent 仓库安全扫描（`docs/08-security.md` §12）依赖 Phase 3 的安全基础设施

---

## 风险评估

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| Wasmtime LTS 版本 API 变更 | 低 | S2 编译失败 | 锁定具体版本、Cargo.lock 固定 |
| 三元组提取 LLM 调用成本 | 中 | S4 运行成本高 | 批量化、设置每日巩固上限、优先巩固高 importance Episode |
| Shell 命令解析的逃逸 | 高 | S3 安全分级被绕过 | 明确标注为"尽力检测"，Phase 7 进程沙箱作为底线防御 |
| FileProvenance 与并发 shell 的竞态 | 中 | 来源记录不准确 | FsWatcher 异步更新 + 乐观锁 |
| WASI Preview 2 生态不成熟 | 中 | S2 权限映射受限 | 降级到 WASI Preview 1，保留升级路径 |
| 无进程沙箱时子进程越权 | 中 | Shell 子进程绕过 Runtime 检查 | Shell 工具默认不提供（manifest 显式声明才启用）；Phase 7 进程沙箱根治 |

---

## 设计决策记录（ADR）

### ADR-007：进程级沙箱延后至 Phase 7

**状态**：已接受

**上下文**：

原 Phase 3 计划包含 S2（进程级沙箱隔离），涉及 bubblewrap（Linux）和 AppContainer（Windows）的集成。这是 `docs/08-security.md` §2.2 设计的 OS 级强制隔离实现。

**决策**：

将进程级沙箱从 Phase 3 延后至 Phase 7，与 macOS Seatbelt 一起实现。Phase 3 聚焦于应用层安全（权限框架 + WASM 沙箱 + Shell 安全分级）。

**理由**：

1. **威胁模型不匹配**：当前 Rollball 是个人桌面 Agent 平台，威胁主体是"Agent 被 Prompt Injection 利用"。权限框架 + Shell 分级 + Approval Gate 已构成完整的第一层防御。进程级沙箱是纵深防御的第二层，优先级低于第一层。
2. **ROI 太低**：S2 占原 Phase 3 的 1/3 工期（5 周 + 64 测试），解决的子进程越权问题在当前场景下极少发生。
3. **外部依赖负担**：bubblewrap 需要安装/分发策略、CI 需要 user namespace、AppContainer 需要管理员权限。这些在当前阶段不值得承担。
4. **替代方案可行**：Shell 工具默认不提供（manifest 显式声明才启用）+ ShellRisk 分级 + Approval Gate 已构成足够防线。WASM 工具已有 Wasmtime 沙箱隔离。

**后果**：

- **变得更容易**：Phase 3 工期缩短 4~6 周，无外部系统依赖，CI 简单
- **变得更难**：子进程越权问题在 Phase 7 之前无法从内核层阻止；Shell 命令解析标注为"尽力检测"；WASI 权限成为 WASM 工具的唯一防线，必须独立完整

### ADR-008：WASM 沙箱的防御定位调整

**状态**：已接受

**上下文**：

原计划中 WASM 沙箱（S3）和进程沙箱（S2）是嵌套关系——WASM 工具在 Agent 进程内，Agent 进程在 bwrap 沙箱内。进程沙箱延后后，WASM 沙箱的防御定位需要调整。

**决策**：

WASI 权限是 WASM 工具的**唯一文件系统/网络防线**，不假设进程沙箱存在。S2.3 权限映射必须独立完整，不能依赖"进程沙箱已经限制了"的假设。

**理由**：

纵深防御中，每一层都应能独立提供完整防护。如果 WASI 权限映射依赖进程沙箱，则进程沙箱不可用时 WASM 工具的隔离就出现缺口。

**后果**：

- WASI 权限映射设计必须更严格（不能假设"进程沙箱会兜底"）
- Phase 7 加入进程沙箱后，WASI 权限可进一步收窄（双重防护），但不依赖它

### ADR-P3-001：权限授权模型选型

**背景**：需要选择权限授权的粒度和交互模型。

**决策**：采用 Android 风格的"安装时声明 + 运行时请求"混合模型。

- 安装时：展示所有声明的权限，用户一次性确认
- 运行时：Agent 可请求额外权限，Gateway 弹出确认
- 权限粒度：按资源类型 + 路径/URL 模式匹配

**理由**：兼顾安全性（最小权限原则）和易用性（不频繁打扰用户）。

### ADR-P3-002：WASM 运行时选型

**背景**：需要选择 WASM 工具的运行时引擎。

**决策**：选择 Wasmtime（Bytecode Alliance），WASI Preview 2。

- Cranelift 编译器（默认，综合性能最优）
- 锁定 LTS 版本（如 v36.x）
- Phase 7 移动端备选 Wasmi（纯解释器，iOS 兼容）

**理由**：详见 `docs/12-tool-system.md` §3.1 选型对比。标准合规性最强，安全审计成熟，无厂商锁定。
