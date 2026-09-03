# acowork-pm 实现 Review 报告

> 评审对象：`core/acowork-pm`（服务端）+ `apps/acowork-desktop/src/views/pm`（Desktop）+ Gateway 集成
> 对照基准：`docs/design/zh/21-pm-project-management.md`（v1.0 设计）+ `docs/plan/zh/pm-dev-plan.md`（v0.3 计划）
> 评审日期：2026-09-02 | 评审人：Software Architect

---

## 1. 结论摘要

**总体判定：✅ 完整复现设计 v1.0，无功能性缺口。**

- 设计文档 v1.0 的 **P0–P4 全部里程碑已实现并测试通过**（`cargo test -p acowork-pm` 全绿：23 handlers_e2e + 3 remote_e2e + 5 smoke + 17 mcp 单测 + store 单测）。
- 服务端数据模型、状态机、REST API、MCP 工具、权限模型、存储形态与设计文档**逐条对齐**。
- Desktop 视图（看板/详情/编辑/审核/离线降级/可访问性/虚拟化）完整落地。
- 发现的差异均为**计划级（v0.3）与设计级（v1.0）之间的演进**或**文档标注为"可选/后置"的功能**，不构成对设计 v1.0 的违背。

---

## 2. 覆盖矩阵（设计 → 实现）

### 2.1 服务端 `core/acowork-pm`

| 设计章节 | 要求 | 实现 | 状态 |
|---------|------|------|------|
| §2.1 组件形态 | 独立进程（ADR-064）、Gateway supervisor + 反代、启动失败非致命（503） | [main.rs](core/acowork-pm/src/main.rs)（独立二进制）+ [pm_supervisor.rs](core/acowork-gateway/src/lifecycle/pm_supervisor.rs) + [pm_proxy.rs](core/acowork-gateway/src/http/pm_proxy.rs) | ✅ |
| §2.2 数据目录 | 目录树、物理嵌套、`children/` 按需创建、`.trash/` | [README.md](core/acowork-pm/README.md) + [tree.rs](core/acowork-pm/src/store/tree.rs) | ✅ |
| §2.3 配置 | `[pm]` 小节 10 项 | [config.rs:18](core/acowork-pm/src/config.rs#L18) `PmConfig` | ✅（1 项默认值偏差，见 §4.3） |
| §3.2/3.3 数据模型 | Project/Task 全字段、零冗余 | [types.rs:206](core/acowork-pm/src/types.rs#L206) `Project` / [types.rs:250](core/acowork-pm/src/types.rs#L250) `Task` | ✅ |
| §3.4 父子树 | 物理嵌套、深度≤5、reparent=mv、删父级联 | [tree.rs:840](core/acowork-pm/src/store/tree.rs#L840) `reparent_task`（DFS 防环 + 深度校验） | ✅ |
| §3.5 依赖图 | `depends_on` 显式存、派生字段仅 API 返回、claim 阻塞 | [tree.rs:1052](core/acowork-pm/src/store/tree.rs#L1052) `compute_blocked_by` | ✅ |
| §3.6 附件 | 元数据入 JSON、缩略图、10MB/20 个限制 | [attachments.rs](core/acowork-pm/src/api/attachments.rs)（upload/download/delete 全实现） | ✅ |
| §4 状态机 | 六态 + 流转图 | [tree.rs:409](core/acowork-pm/src/store/tree.rs#L409) `validate_transition` 与状态图逐条一致 | ✅ |
| §5 REST API | 17 条路由 | [routes.rs:49](core/acowork-pm/src/api/routes.rs#L49) `pm_router` 全量 | ✅ |
| §6 MCP | 12 工具 + HTTP transport | [manifest.rs](core/acowork-pm/src/mcp/manifest.rs)（12 工具断言）+ [tools.rs](core/acowork-pm/src/mcp/tools.rs) | ✅ |
| §9.1 指派校验 | assignee 必须存在于 Agent 目录 | [agent_dir.rs](core/acowork-pm/src/mcp/agent_dir.rs) `HttpAgentDirectory`（HTTP 查询 Gateway `/api/agents`，启动拉全量 + 周期刷新 + 即时兜底） | ✅ |
| §9.2/9.3 身份 | `X-MCP-Actor`、-32001/-32002、匿名只读 | [mcp/mod.rs](core/acowork-pm/src/mcp/mod.rs)（e2e 测试覆盖） | ✅ |
| §10.1 存储语义 | 原子写、零双写 | [atomic.rs:15](core/acowork-pm/src/store/atomic.rs#L15) `atomic_write_json` | ✅ |
| §10.2 内存索引 | by_id/by_project/by_assignee/blocked_by | [index.rs:22](core/acowork-pm/src/store/index.rs#L22) `TaskIndex`（+by_status/by_attachment） | ✅ |
| §10.3 启动重建 | walkdir 重建索引 | [server.rs:53](core/acowork-pm/src/server.rs#L53) `rebuild_index` | ✅ |
| §10.5 路径安全 | 白名单 ID + canonicalize 防穿越 | [atomic.rs:50](core/acowork-pm/src/store/atomic.rs#L50) `canonicalize_within` | ✅ |

### 2.2 Gateway 集成

| 设计要求 | 实现 | 状态 |
|---------|------|------|
| `/api/pm/*` 反向代理（ADR-064） | [pm_proxy.rs](core/acowork-gateway/src/http/pm_proxy.rs) `pm_proxy_routes`（剥离 `/api/pm` 前缀 → `127.0.0.1:{pm_port}`；未就绪 503 + `Retry-After`） | ✅ |
| PM 独立进程 supervisor | [pm_supervisor.rs](core/acowork-gateway/src/lifecycle/pm_supervisor.rs)（spawn / `/health` 探活 / 指数退避重启） | ✅ |
| 身份注入（ADR-064 Phase 3） | [pm_proxy.rs](core/acowork-gateway/src/http/pm_proxy.rs) `build_trusted_headers`（REST 覆盖 `X-Actor: human`；MCP 校验 `X-MCP-Actor` ∈ `installed_agents`） | ✅ |
| PM 启动失败非致命 | [gateway/mod.rs:618](core/acowork-gateway/src/gateway/mod.rs#L618) `Err` 分支仅告警 | ✅ |
| MCP 自动注入 `acowork/global/mcps` | [global_resources_builders.rs:163](core/acowork-gateway/src/mqtt/global_resources_builders.rs#L163) `pm_mcp_url` 注入 + 单测 | ✅ |
| advertise endpoint 构造 | [gateway/mod.rs:599](core/acowork-gateway/src/gateway/mod.rs#L599) `http://{advertise_host}:{port}{mcp_http_path}` | ✅ |

### 2.3 Desktop `apps/acowork-desktop`

| 计划任务 | 要求 | 实现 | 状态 |
|---------|------|------|------|
| T2-1 视图壳 | ProjectsView + 三态 | [ProjectsView.tsx](apps/acowork-desktop/src/views/ProjectsView.tsx) | ✅ |
| T2-2 Sidebar | 列表/新建/删除/计数徽章 | [ProjectSidebar.tsx](apps/acowork-desktop/src/views/pm/ProjectSidebar.tsx) | ✅ |
| T2-3/8 看板 + 拖动 | 4 列 + dnd-kit + 乐观更新回滚 + 键盘拖动 | [KanbanBoard.tsx](apps/acowork-desktop/src/views/pm/KanbanBoard.tsx) | ✅ |
| T2-4 Header | inline 编辑 + 统计 + 菜单 | [ProjectHeader.tsx](apps/acowork-desktop/src/views/pm/ProjectHeader.tsx) | ✅（见 §4.2） |
| T2-5 TaskCard | 类型/优先级/负责人/截止/子任务计数/阻塞 | [TaskCard.tsx](apps/acowork-desktop/src/views/pm/TaskCard.tsx) | ✅（见 §4.2） |
| T2-6 DetailDrawer | 多 Tab + 子任务树 + 附件 | [TaskDetailDrawer.tsx](apps/acowork-desktop/src/views/pm/TaskDetailDrawer.tsx) | ✅（见 §4.2） |
| T2-7 EditDialog | 类型/优先级/指派/截止/父任务/依赖 | [TaskEditDialog.tsx](apps/acowork-desktop/src/views/pm/TaskEditDialog.tsx) | ✅ |
| T2-9 Reparent | 拖到卡片 → 子任务 + 防环 | [KanbanBoard.tsx:154](apps/acowork-desktop/src/views/pm/KanbanBoard.tsx#L154) | ✅ |
| T2-11 审核 UI | inline 批准/拒绝 + RejectDialog | [TaskCard.tsx:162](apps/acowork-desktop/src/views/pm/TaskCard.tsx#L162) + [RejectDialog.tsx](apps/acowork-desktop/src/views/pm/RejectDialog.tsx) | ✅ |
| T2-13 离线降级 | 30s 健康轮询 + Banner + 写禁用 | [healthStore.ts](apps/acowork-desktop/src/stores/pm/healthStore.ts) + [ServiceOfflineBanner.tsx](apps/acowork-desktop/src/views/pm/ServiceOfflineBanner.tsx) | ✅ |
| T2-14 可访问性 | 键盘导航 + ARIA + 焦点管理 | 各组件（Drawer 焦点恢复、aria-label、role） | ✅ |
| T2-15 性能 | 列表虚拟化 + 懒加载 | [KanbanColumn.tsx:71](apps/acowork-desktop/src/views/pm/KanbanColumn.tsx#L71) `@tanstack/react-virtual` + `loading="lazy"` | ✅ |

---

## 3. 测试验证

`cargo test -p acowork-pm` 全绿（本次实测）：

| 套件 | 数量 | 覆盖 |
|------|------|------|
| `tests/handlers_e2e.rs` | 23 | 项目/任务 CRUD、生命周期、reparent、children、附件 roundtrip、错误格式 |
| `tests/remote_e2e.rs` | 3 | 远程 Agent 全链路、非 assignee 拒绝、匿名只读 |
| `tests/smoke.rs` | 5 | ID roundtrip、配置校验、store 构造 |
| `src/mcp/mod.rs` | 17 | 握手、tools/list、全生命周期、鉴权（-32001/-32002）、依赖阻塞、防环 |
| `src/store/tree.rs` 等 | 若干 | 状态机、防环、级联删除、索引 |

---

## 4. 发现的差异 / 缺口

### 4.1 计划级 → 设计级演进（非缺口，符合设计 v1.0 / ADR-064）

| 项 | 计划 v0.3 | 设计 v1.0 / 实现 | 判定 |
|----|----------|------------------|------|
| P0 部署形态 | 独立子进程 + supervisor + 端口分配（18082→…） | ~~内嵌 Gateway（D-10 定稿）~~ → **ADR-064 恢复独立进程**（supervisor + 反代） | 实现正确跟随 ADR-064 |
| 状态管理 | `@tanstack/react-query` | **zustand**（设计 §7 未强制） | 合理替代，非缺口 |
| Agent 目录同步 | 周期拉取 + 即时兜底 | ~~共享 state 即时校验（`GatewayAgentDirectory`）~~ → **ADR-064 恢复 HTTP 周期拉取 + 即时兜底**（`HttpAgentDirectory`） | 实现正确跟随 ADR-064 |

### 4.2 计划级功能未实现（设计标注为"可选/后置"）

| 项 | 计划要求 | 现状 | 严重度 |
|----|---------|------|--------|
| **附件灯箱**（T2-10） | AttachmentLightbox 原图放大 + 上一张/下一张 | 图片点击 `target="_blank"` 新标签打开，无灯箱 | 🟡 低（设计 §7 未强制） |
| **提升子任务为顶层**（T2-4） | 删除菜单含"提升子任务为顶层"可选语义 | 仅级联删除（设计 D-5 标注"可选高级操作"） | 🟡 低 |
| **TaskCard 附件/依赖计数**（T2-5） | 卡片显示子任务/附件/依赖计数 | 仅子任务计数 | 🟢 极低 |
| **备注 Tab**（T2-6） | 6 Tab 含备注 | 5 Tab 无备注（设计 §5 明确"备注功能后置 P2+"） | 🟢 已文档化后置 |

### 4.3 代码卫生问题（建议清理）

| 项 | 位置 | 问题 | 建议 |
|----|------|------|------|
| **`addNote` 死代码** | [pm-api.ts:156](apps/acowork-desktop/src/lib/pm-api.ts#L156) | 定义但从未调用，指向不存在的 `/notes` 端点 | 删除 |
| **`start_dev` 陈旧占位** | [server.rs:91](core/acowork-pm/src/server.rs#L91) | 已标记 `@deprecated`，由 `PmService::serve` 替代（ADR-064 独立进程） | ✅ 已解决 |
| **`index_rebuild_on_start` 默认值** | [config.rs:121](core/acowork-pm/src/config.rs#L121) | 实现默认 `true`，设计 §2.3 写 `false`（增量加载） | 对齐设计或更新设计文档 |
| **schemas README 附件状态** | [schemas/README.md:54](core/acowork-pm/schemas/README.md#L54) | 标注附件"🔶 P1 待完成"，实际已完整实现 | ✅ 已更新为 ✅ |

---

## 5. 架构质量评估

对照设计评审清单：

| 维度 | 评估 |
|------|------|
| **正确性** | 状态机、依赖阻塞、防环、级联删除均有测试覆盖；乐观更新 + 失败回滚 |
| **简洁性** | 目录树存储零冗余字段，无过度抽象；`TaskStore` trait 为 P5+ SQLite 切换预留 |
| **边界** | PM 与 Gateway 通过 `AgentDirectory` trait 解耦；`Router<()>` 与 `Router<AppState>` 用反代（`pm_proxy.rs`）隔离 |
| **契约** | OpenAPI 3.1 + 存储 schema + 错误码表三份契约资产齐全 |
| **可靠性** | 原子写、启动重建、非致命启动、离线降级 |
| **安全** | 路径穿越防护、ID 白名单、MCP 身份校验、匿名只读 |
| **可逆性** | 存储层 trait 化，SQLite 切换为 P5+ 预留；无不可逆决策 |

---

## 6. 建议（按优先级）

1. **P1（建议做）**：删除 `pm-api.ts` 的 `addNote` 死代码；更新 `schemas/README.md` 附件状态为 ✅。
2. **P2（可选）**：对齐 `index_rebuild_on_start` 默认值（实现 `true` vs 设计 `false`）——二选一：改实现为 `false` 或更新设计文档为 `true`（当前 `true` 更安全但启动慢）。
3. **P3（可选）**：`start_dev` 要么 serve 完整 router（便于独立调试），要么删除并更新注释。
4. **P4（产品决策）**：附件灯箱、提升子任务为顶层、卡片附件/依赖计数——若产品需要再补，当前不阻塞任何流程。

---

## 7. 结论

**acowork-pm 完整复现了设计文档 v1.0 的全部要求**，P0–P4 里程碑交付物齐备、测试全绿、契约资产完整。发现的差异均为计划级（v0.3）与设计级（v1.0）之间的合理演进，或设计文档明确标注为"可选/后置"的功能。无功能性缺口，可进入下一阶段（P5+ 或产品迭代）。

---

## 8. 架构风险跟进（2026-09-02 定案 → 2026-09-02 落地）

> 评审后架构评审发现：PM **内嵌于 Gateway**（设计 v1.0 D-10）违反"Gateway 零业务"铁律（ADR-019/055 方向），且 X-Actor 由客户端自报可伪造。

**决策**：PM 迁出为独立进程，Gateway 仅保留 supervisor + 反向代理 + 身份注入。详见 [ADR-064](../adr/zh/ADR-064-pm-standalone-process.md)。

**落地状态（ADR-064 Phase 0–4 已完成）**：
- ✅ Phase 0：`acowork-pm` 独立二进制 + `/health` + 端口分配 + 独立数据目录 `$HOME/.acowork/acowork-pm/`
- ✅ Phase 1：Gateway 反代 `pm_proxy.rs` 替换 `nest_service`；`pm_api.rs` 删除；`prepare_pm_data_dir` 删除；Gateway Cargo.toml 移除 `acowork-pm` 依赖
- ✅ Phase 2：`pm_supervisor.rs`（spawn / `/health` 探活 / 指数退避重启）
- ✅ Phase 3：`HttpAgentDirectory`（HTTP 查询 Gateway `/api/agents`）+ `pm_proxy.rs` 身份注入（REST 覆盖 `X-Actor: human`；MCP 校验 `X-MCP-Actor` ∈ `installed_agents`）
- ✅ Phase 4：设计文档 / ADR-061 / README / schemas README 收口

**影响**：本报告 §2.1"服务端"与 §2.2"Gateway 集成"已按 ADR-064 更新为独立进程形态（`nest_service`、`PmService::with_agent_directory`、`GatewayAgentDirectory` 均已失效并替换）。
