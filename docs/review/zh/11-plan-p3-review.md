# Phase 3 计划评审报告

> 评审对象：`docs/plan/plan-p3.md` v1.1
> 评审日期：2026-04-25
> 评审角色：软件架构师
> **2026-04-25 后续更新**：plan-p3.md 已升级为 v2.0，进程级沙箱（原 S2）延后至 Phase 7（ADR-007）。以下评审意见中与 S2 相关的条目已标注处置结果。

---

## 总体评价

Phase 3 计划结构清晰，任务拆分粒度合理，与设计文档（08-security、12-tool-system、05-memory）的映射关系明确。但存在以下**架构层面的问题**需要解决后才能进入实施。

---

## P0：必须修复（阻塞实施）

### P0-1：S5 离线巩固与 Phase 3 主题"权限与沙箱"不一致

**问题**：Phase 3 的核心主题是"权限与沙箱"（标题和背景部分明确定义），但 S5（离线巩固与记忆质量）是记忆系统的增强，与安全主题无直接关系。这导致：

1. Phase 3 的概念内聚性被打破——"安全交付"的里程碑 M15 包含了非安全功能
2. S5 的前置条件（Phase 2 S2.0~S2.8 记忆基础设施）与 S1~S4 的前置条件不同，混合在同一 Phase 增加了并行管理的复杂度
3. S5 的 3 周工期与 S2~S4 并行时，如果记忆基础设施未完成，S5 无法启动

**建议**：将 S5 拆出 Phase 3。选项：
- **A**：S5 独立为 Phase 3.5（或在 Phase 2 末尾作为 S6 追加），Phase 3 只聚焦安全
- **B**：S5 推迟到 Phase 4，与通信协调一起做（记忆巩固可以跟 Intent 跨 Agent 协调产生协同）
- **C**：保持现状，但将 Phase 3 标题改为"权限、沙箱与记忆巩固"，明确这不是纯安全 Phase

**我的推荐**：方案 A。离线巩固是 Phase 2 记忆系统的自然延伸，放到 Phase 2 末尾或紧接 Phase 2 的独立小阶段更合理。Phase 3 回归纯安全主题。

> **v2.0 处置**：plan-p3.md 已将标题改为"权限与工具安全"，S5（离线巩固）保留在 Phase 3 内作为 S4，承认 Phase 3 包含非安全功能。这是务实的折中——离线巩固体量不大（3 周），单独成 Phase 太轻，与 Phase 4 合并又增加 Phase 4 复杂度。

---

### P0-2：S2 沙箱依赖 bubblewrap 但缺少安装/分发策略

**问题**：S2.2 要求 `BubblewrapSandbox` 实现，但 bubblewrap 是外部系统依赖（不像 Rust crate 可以写进 Cargo.toml）。计划没有回答：

1. bubblewrap 如何安装？包管理器？内置？用户手动？
2. bwrap 不存在时的行为是什么？S2.8 提到了降级策略，但 S2.2~S2.6 的测试怎么跑？
3. CI/CD 环境中 bwrap 如何配置？风险表提到了"Docker privileged"，但不是所有 CI 环境都支持

**建议**：
- 在 S2.1（Sandbox 抽象层）任务中增加"沙箱依赖检测"子任务：运行时检测 bwrap / AppContainer 可用性，输出结构化的 `SandboxAvailability` 报告
- 明确 bwrap 的最低版本要求
- 在 S2.8 降级策略之前，先明确"开发环境如何安装 bwrap"的操作文档
- CI 策略：Linux CI 用 `--cap-add SYS_ADMIN --security-opt apparmor=unconfined` 运行 Docker，或在无 bwrap 环境中 skip 沙箱测试（`#[cfg(feature = "sandbox-linux")]` feature gate）

> **v2.0 处置**：**已解决——整个 S2 延后至 Phase 7**（ADR-007）。此 P0 不再阻塞 Phase 3 实施。Phase 7 实现进程沙箱时需重新考虑此问题。

---

### P0-3：S1 权限框架的 IPC 消息设计缺失

**问题**：S1.5（运行时权限请求）定义了 Runtime → Gateway `PermissionRequest` 消息，但没有定义：

1. 消息格式（与现有 Gateway Service API 的 Frame 协议如何对齐？）
2. Gateway 向用户展示的 CLI 交互阻塞问题：如果 Gateway 主循环在等待用户输入，其他 Agent 的 IPC 请求是否被阻塞？
3. 权限响应的异步性：用户可能不会立即响应，Runtime 在等待期间的行为是什么？

**建议**：
- S1.5 增加"PermissionRequest/PermissionResponse 消息协议设计"子任务
- 明确 Gateway 侧的权限请求处理不阻塞 IPC 主循环（使用独立 task 或 channel）
- 定义 Runtime 侧的超时策略（默认 60s 超时后拒绝工具调用）

> **v2.0 处置**：**仍需在 S1 实施时解决**。此 P0 保留有效。

---

## P1：建议修复（不阻塞但有风险）

### P1-1：S3 WASM 沙箱与 S2 进程沙箱的关系模糊

**问题**：S2 是进程级沙箱（隔离整个 Agent 进程），S3 是 WASM 沙箱（隔离工具执行）。两者是**嵌套关系**——WASM 工具已经在 Agent 进程内，而 Agent 进程又在 bwrap 沙箱内。但计划没有明确：

1. WASM 工具是否需要双重隔离？（进程沙箱已经限制了文件系统/网络）
2. WASI 权限映射（S3.3）与 manifest 权限 → SandboxConfig（S2.7）的权限是同一套还是独立的？
3. 如果进程沙箱已经阻止了文件系统访问，WASM 工具的 WASI 文件预开放还有意义吗？

**建议**：
- 在 S3 开头增加"与进程沙箱的关系说明"段落
- 明确：在沙箱化进程中，WASM 工具的 WASI 权限是**二次防护**（纵深防御），不依赖进程沙箱存在
- S3.3 权限映射应定义：**无进程沙箱时**（降级模式）WASI 权限是唯一防线；**有进程沙箱时**WASI 权限进一步收窄

> **v2.0 处置**：**已解决**。S2 延后后，WASI 权限是 WASM 工具的**唯一文件系统/网络防线**（ADR-008），plan-p3.md S2.3 已明确标注。Phase 7 加入进程沙箱后 WASI 成为二次防护。

### P1-2：S4.5 FsWatcher 跨平台实现复杂度被低估

**问题**：S4.5 计划 8 个测试覆盖 inotify / FSEvents / ReadDirectoryChangesW 三个平台 API。但：

1. FSEvents 是 macOS API——Phase 3 明确不覆盖 macOS（D2 决策）
2. ReadDirectoryChangesW 在 Windows 上有已知的缓冲区大小和通知丢失问题
3. Rust 生态有成熟的跨平台 FS watch 库（`notify` crate），为什么不用？

**建议**：
- 使用 `notify` crate 替代手动封装三平台 API，减少维护成本
- 删除 FSEvents 实现（与 D2 决策一致：macOS 留 Phase 7）
- S4.5 改为"基于 notify crate 的 FsWatcher"，降低任务复杂度

> **v2.0 处置**：**已采纳**。plan-p3.md D8 决策使用 `notify` crate，S3.5 任务描述已更新。

### P1-3：S6.4 安全红队测试缺少具体场景定义

**问题**：S6.4 提到"模拟攻击场景：Prompt Injection → 恶意 shell → 文件越权 → 网络渗透"，但没有定义：

1. 每个攻击场景的具体输入是什么？
2. "拦截率 100%"的验收标准如何量化？是一次通过就行，还是需要覆盖攻击变体？
3. 谁来设计攻击场景？开发者自测还是独立安全审计？

**建议**：
- 在 S6.4 中列出至少 5 个具体的攻击场景（含输入示例和预期拦截点）
- "100% 拦截率"限制定义为"定义的攻击场景列表中全部被拦截"，不承诺覆盖所有可能攻击
- 标注此为"内部安全验证"，不是正式安全审计

> **v2.0 处置**：**已采纳**。plan-p3.md S5.3 已标注"此为内部安全验证，不是正式安全审计"，验收标准改为"定义场景全部被拦截或告警"。

### P1-4：S1.2 PermissionStore 使用 rusqlite，但 Grafeo 迁移的教训未吸收

**问题**：Phase 2 已经将 Grafeo 存储后端从 rusqlite 迁移到 grafeo-engine。S1.2 又引入一个新的 rusqlite 存储（PermissionStore）。这意味着同一进程中可能同时存在两个存储引擎（Grafeo + rusqlite）。

**建议**：
- 评估 PermissionStore 是否可以复用 Grafeo 的 LPG 存储（Permission 作为节点，Grant 作为边）
- 如果坚持 rusqlite，明确为什么权限数据不适合 Grafeo（可能是：权限数据不需要向量检索/图遍历，关系模型更简单）
- 无论选哪个，在 D 列表中增加 ADR 记录决策理由

> **v2.0 处置**：**部分采纳**。plan-p3.md D6 保留 rusqlite 决策，理由已写入（权限数据不需要向量检索/图遍历，关系模型更简单）。如需正式 ADR，在 S1 实施时补充。

### P1-5：缺少 `rollball-sandbox` crate 与 workspace 集成的说明

**问题**：D1 决策新建 `rollball-sandbox` crate，但计划没有说明：

1. 这个 crate 放在 `core/` 目录下吗？（按现有结构应该如此）
2. `Cargo.toml` workspace members 需要更新
3. 依赖关系：`rollball-sandbox` 依赖 `rollball-core`（SandboxConfig）和平台特定 crate，被 `rollball-gateway` 依赖

**建议**：
- 在 S2.1 任务中增加"创建 `rollball-sandbox` crate 骨架 + 更新 workspace Cargo.toml"
- 明确依赖链：`rollball-core` → `rollball-sandbox` → `rollball-gateway`

> **v2.0 处置**：**已解决——`rollball-sandbox` crate 随 S2 一起延后至 Phase 7**。Phase 3 不需要此 crate。

---

## P2：建议改进（优化项）

### P2-1：测试数量估计偏高

S1~S6 总计 278 个测试，按 15~19 周工期算，平均每周需交付 ~15 个测试。结合 Phase 1（262 测试 / ~12 周 ≈ 22/周）和 Phase 2 的经验，建议：

- S2（进程沙箱）的 64 个测试中，跨平台测试在 CI 中可能需要条件编译
- S4.5 FsWatcher 的 8 个测试可能需要异步等待文件系统事件，测试运行时间较长
- 建议标注哪些测试是"CI 必须"vs"本地验证"，避免 CI 超时

> **v2.0 处置**：S2 的 64 测试随 S2 延后。v2.0 总测试 210（减少 68），按 11~13 周工期 ≈ 16~19/周，合理。

### P2-2：ADR 编号与项目约定不一致

计划使用了 `ADR-P3-001/002/003` 编号，但项目已有的 ADR 使用 `ADR-001/006` 格式（见 MEMORY.md 中的 ADR-006 跨平台 IPC）。建议统一为 `ADR-007/008/009`。

> **v2.0 处置**：**已采纳**。v2.0 使用 `ADR-007`（进程沙箱延后）、`ADR-008`（WASM 防御定位调整）。

### P2-3：依赖图缺少 S2 内部的串行依赖

S2 的 8 个任务看似可以流水线执行，但实际上：
- S2.1（Sandbox trait）是 S2.2~S2.8 全部的前置
- S2.2（bubblewrap）和 S2.4（AppContainer）可以并行
- S2.5（网络隔离）和 S2.6（资源限制）依赖 S2.2 或 S2.4
- S2.7（集成）依赖 S2.2~S2.6

建议在 S2 内部画出任务依赖图，避免实施时发现前置未完成。

> **v2.0 处置**：S2 整体延后至 Phase 7，此问题在 Phase 7 实施时再解决。

### P2-4：D4（离线巩固 LLM 策略）假设本地有 GPU

D4 决策"本地 qwen 9B 优先"，但：
1. 不是所有开发者都有 2080Ti
2. qwen 9B 需要什么推理框架？Ollama？llama.cpp？candles？
3. Phase 3 应该是平台级代码，不应依赖开发者硬件

**建议**：D4 的"本地优先"应改为"远程 API 优先 + 本地可选"，与 Embedding 的"Local → Remote → Disabled"策略一致。

> **v2.0 处置**：**已采纳**。v2.0 D4 改为"远程 API 优先 + 本地可选"。

### P2-5：缺少性能基准定义

Phase 3 引入了沙箱（进程启动额外开销）和 WASM（工具执行额外开销），但没有定义性能基线：

1. 沙箱化启动比裸启动慢多少？上限是多少？
2. WASM 工具执行比内置工具慢多少？
3. 权限校验的 P99 延迟是多少？

**建议**：在 S6（集成验证）中增加性能基准测试子任务。

> **v2.0 处置**：进程沙箱延后，第 1 点不再适用。WASM 工具和权限校验的性能基准仍有价值，可在 S5 集成验证中补充。

---

## 与设计文档的一致性检查

| 检查项 | plan-p3.md | 对应设计文档 | 一致？ | 备注 |
|--------|-----------|-------------|--------|------|
| 沙箱实现范围 | Linux + Windows | 08-security §2.2 列出 Linux/macOS/Windows | ⚠️ | macOS 推迟合理，但 08-security §11.6 分阶段表中 Phase 2 对应 Linux，Phase 3 对应 Windows。plan-p3 把 Linux 和 Windows 合在一起了 |
| Shell 安全分阶段 | Phase 3 全部实现 | 08-security §11.6 Phase 1 做 Shell 安全，Phase 2 做内核级 | ⚠️ | plan-p3 延后了，与 08-security 原计划不一致。08-security §11.6 的 Phase 1 包含"Shell 命令风险分级 + approval gate + 审计日志"，但实际 Phase 1/2 都没做 |
| WASM WIT 升级 | Phase 3 同步完成 | 12-tool-system §3.4 "Phase 3+ 升级路径" | ✅ | 一致 |
| PermissionRequest 消息 | S1.5 定义 | 12-tool-system §4 Gateway Tools 表有列出 | ✅ | 一致 |
| FileProvenance 持久化 | D6 选择 rusqlite | 08-security §11.2 只有内存 HashMap | ⚠️ | plan-p3 增加了持久化，设计文档未更新 |
| 离线巩固 | S5 完整实现 | 05-memory §4 定义，plan-p2 S2.6.4 标记为 Phase 3 | ✅ | 一致 |

**需要同步更新的设计文档**：
1. `08-security.md` §11.6 — 分阶段表与实际进度不一致 ✅ **已更新**
2. `08-security.md` §11.2 — FileProvenance 结构需补充持久化说明 ⏳ **待 S3 实施时补充**
3. `09-roadmap-and-scenarios.md` Phase 3 描述 — 需要与 plan-p3 对齐 ✅ **已更新**

---

## 评审结论

| 级别 | 数量 | 是否阻塞 | v2.0 状态 |
|------|------|---------|-----------|
| P0 | 3 | 是，修复后方可进入实施 | P0-1 保留（已折中）、P0-2 已解决（S2 延后）、P0-3 仍需解决 |
| P1 | 5 | 否，但建议在 S1 启动前确认 | P1-1 已解决、P1-2 已采纳、P1-3 已采纳、P1-4 部分采纳、P1-5 已解决 |
| P2 | 5 | 否，优化项 | P2-1 已解决、P2-2 已采纳、P2-3 延后、P2-4 已采纳、P2-5 部分采纳 |

**v2.0 结论**：P0-2（bubblewrap 分发）和 P0-1（S5 拆出）两个核心阻塞项已解决。剩余 P0-3（权限请求 IPC 消息设计）需在 S1 实施时补充。
