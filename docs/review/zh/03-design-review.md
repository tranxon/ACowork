# 设计文档 vs PRD 符合度审查报告（第三轮）

> 审查依据：PRD 文档 (00-prd.md v1.3) + 全部设计文档 (01~14) + 模块设计文档 (module-design/ 00~06)
>
> 审查日期：2026-04-17
>
> 审查范围：docs/ 下全部设计文档，重点对照 PRD 需求编号逐项核查覆盖度与一致性
>
> 审查工具：Claude Opus 4.6

---

## 一、审查概述

本轮 review 以 PRD (00-prd.md v1.3) 为权威基准，逐项核查所有设计文档（01~14 + module-design/00~06）是否完整覆盖产品需求，并识别跨文档不一致、优先级偏差和接口规格缺失。

**总体覆盖度：约 85-90%**。架构设计整体合理，Android 隐喻贯穿一致，ADR 记录完整。主要问题集中在：抽象层落地缺失、PRD 优先级与路线图阶段映射偏差、跨模块接口规格不足。

---

## 二、关键缺口（Critical — 阻塞 Phase 1 实现）

### Issue-01：rollball-memory crate 未定义

| 项目 | 说明 |
|------|------|
| 关联需求 | ADR-003、MEM-01~12、EXT-05 (MemoryStore trait) |
| 问题描述 | ADR-003 决定引入 `MemoryStore trait` + `MemoryManager` 作为记忆可扩展性核心抽象。`module-design/04-grafeo.md` 引用了 `rollball_memory::MemoryStore`，但 `module-design/00-overview.md` 的 workspace 成员列表中**没有 rollball-memory crate** |
| 影响 | Phase 1 记忆系统实现缺少 trait 定义所在的 crate，无法编译 |
| 建议 | 在 workspace 中新增 `rollball-memory` crate，创建 `module-design/07-memory.md` 详细定义 MemoryStore trait、MemoryManager、6 个生命周期阶段接口 |

### Issue-02：RAG 工具在 Runtime 设计中缺失

| 项目 | 说明 |
|------|------|
| 关联需求 | RAG-01~07 (均为 P1) |
| 问题描述 | `02-agent-package.md` manifest 中声明了 `type = "rag"` 的工具类型，PRD §1.13 详细定义了双通道检索模型和 RAG 工具接口。但 `module-design/02-runtime.md` 和 `12-tool-system.md` 的工具注册表中**没有 RAG 工具的实现设计** |
| 影响 | 企业 Agent 场景（PRD §5.5）无法实现 |
| 建议 | 在 `12-tool-system.md` 中补充 RAG 工具实现设计：标准查询接口（向量检索 + 混合关键词）、企业认证（API Key / OAuth 2.0 / Bearer Token）、离线降级（RAG-07：服务不可达时跳过通道不阻塞）、结果来源标注（RAG-05） |

### Issue-03：Approval Gate 缺少 HTTP API 端点

| 项目 | 说明 |
|------|------|
| 关联需求 | RUN-13 (P1)、DSK-02 |
| 问题描述 | `03-agent-runtime.md` §7.4 定义了高风险工具 Approval Gate 流程（暂停执行 → 请求用户确认 → 恢复/取消）。但 `04-gateway.md` §9 的 HTTP API 端点列表中**没有** approval 相关端点 |
| 影响 | Desktop App 无法接收确认请求，Approval Gate 功能无法落地 |
| 建议 | 在 `04-gateway.md` HTTP API 中补充：`POST /api/approval/pending`（Agent 提交确认请求）、`GET /api/approval/list`（Desktop App 拉取待确认列表）、`POST /api/approval/:id/respond`（用户响应确认/拒绝）；或通过 WebSocket 推送实时确认请求 |

---

## 三、重要不一致（High — 需尽快澄清）

### Issue-04：Skill 级联降级无实现规格

| 项目 | 说明 |
|------|------|
| 关联需求 | TOL-07 (P2)、PLT-05 (P2) |
| 问题描述 | PRD 要求工具不可用时 Skill 自动降级。`13-skill-system.md` §8.1 将此推迟到 Phase 4+。但 PLT-05 要求移动端优雅降级，且 `02-agent-package.md` 已定义 `target_platforms` 的 required/optional 模式，暗示 Phase 1 就需要基本降级支持 |
| 建议 | 至少在 Phase 2 定义 SKILL.md 中 `tool_deps` 的 required/optional 标记，运行时在 Skill Loader 阶段过滤不可用工具对应的 Skill |

### Issue-05：PRD 优先级与路线图阶段映射偏差

| 需求编号 | PRD 优先级 | 路线图实际安排 | 偏差程度 |
|----------|-----------|---------------|---------|
| RUN-13 (Approval Gate) | P1 (Phase 1-2) | Phase 3 | 延后一个阶段 |
| TOL-02 (WASM 自定义工具) | P1 | Phase 3 | 延后一个阶段 |
| SKL-03 (Skill 调试流程) | P1 | Phase 5.3 | 严重延后 |
| RAG-01~07 (企业 RAG) | P1 | 路线图中未出现 | 完全缺失 |

**建议**：要么调整路线图将这些 P1 需求提前到 Phase 2，要么在 PRD 中将其降级为 P2 并说明理由。当前状态下 PRD 与路线图存在承诺不一致。

### Issue-06：System Agent 启动顺序存在循环依赖

| 项目 | 说明 |
|------|------|
| 关联需求 | GTW-12、SYS-01~02 |
| 问题描述 | GTW-12 要求"启动 Agent 前向系统 Agent 查询 identity_deps 并注入"。但 System Agent 自身也需要被 Gateway 启动。多处文档（06-communication.md、07-system-agent.md、04-gateway.md）均提到此流程，但**没有任何文档定义 System Agent 的 bootstrap 特殊路径** |
| 建议 | 方案一：System Agent 启动时 skip identity injection（它不需要 identity_deps）；方案二：Gateway 内置 identity 缓存，System Agent 离线时使用缓存。需在 `07-system-agent.md` 中明确 |

### Issue-07：Intent 消息格式未定义

| 项目 | 说明 |
|------|------|
| 关联需求 | COM-01、GTW-04、SYS-02、SYS-05 |
| 问题描述 | `06-communication.md` 定义了 Capability Registry（HashMap 结构、O(1) 查找）和 Intent 路由机制（sync/async 模式、目标 Agent 自动启动）。但 **Intent 消息本身的完整 schema 未给出**——缺少 sender、target、action、params、response_type 等字段的形式化定义 |
| 建议 | 在 `06-communication.md` 或 `module-design/01-core.md` 中补充 IntentMessage struct 定义，包含路由字段、payload schema、超时配置、错误码 |

---

## 四、中等问题（Medium — 设计完善建议）

### Issue-08：权限匹配语义不明确

| 项目 | 说明 |
|------|------|
| 关联需求 | SEC-04、TOL-05 |
| 问题描述 | manifest 权限声明示例为 `"network:https://api.weather.com"`、`"filesystem:read:~/Documents"`，但无文档定义匹配规则：精确匹配？前缀匹配？通配符？子域名？子路径？ |
| 建议 | 在 `02-agent-package.md` manifest schema 部分增加权限 pattern 的形式化语法和匹配算法 |

### Issue-09：identity_deps 注入细节缺失

| 项目 | 说明 |
|------|------|
| 关联需求 | FMT-05、GTW-12 |
| 问题描述 | manifest 示例 `identity_deps = ["name", "city", "language"]`，但未定义：字段是 required 还是 optional？Gateway 查询 System Agent 后字段缺失时返回 null 还是空串？是否有格式校验（如 timezone 格式）？ |
| 建议 | 扩展 manifest schema，为每个 identity_dep 支持元数据：`{ field = "city", required = false, default = "unknown" }` |

### Issue-10：Tool Result 折叠算法模糊

| 项目 | 说明 |
|------|------|
| 关联需求 | RUN-11 (P1) |
| 问题描述 | PRD 要求保留最近 4 轮完整结果，更早的折叠为摘要。`03-agent-runtime.md` 提到"规则引擎折叠"和"借鉴 ZeroClaw fast_trim_tool_results"，但**未给出具体算法** |
| 建议 | 补充折叠策略：FIFO vs importance-based、摘要生成方式（规则裁剪 vs LLM 摘要）、折叠后 token 上限 |

### Issue-11：文档版本不同步

| 文档 | 当前版本 | 核心文档版本 |
|------|---------|------------|
| 01-overview.md | v3.0 | v3.4 |
| 07-system-agent.md | v3.0 | v3.4 |
| 08-security.md | v3.0 | v3.4 |

**建议**：统一所有设计文档到 v3.4，确保 ADR-002（PrivacyLevel 边界）、ADR-003（Memory Lifecycle）等决策在旧文档中得到反映。

### Issue-12：Capability Registry 更新与失效机制

| 项目 | 说明 |
|------|------|
| 关联需求 | GTW-04、DEV-10 |
| 问题描述 | `06-communication.md` §2.4 提到 Gateway 通过 `capability_update` 推送增量更新，但未定义：推送失败时是否重试？Agent 未收到更新时使用过期能力列表是否安全？多个 Agent 声明相同 action 时的优先级？ |
| 建议 | 补充更新的 delivery guarantee（at-least-once + 版本号去重）和冲突解决策略 |

---

## 五、超出 PRD 范围的设计

### Issue-13：仓库级安全扫描（08-security.md §12）

`08-security.md` §12 引入了仓库级安全扫描机制（manifest 合规扫描、prompt 注入分析、WASM 二进制扫描、Grafeo 记忆扫描），定位于 Phase 6-7。PRD 中**无对应需求编号**。

**建议**：如果确认纳入产品范围，应在 PRD §1.10 安全需求中补充 SEC-08~SEC-12。

### Issue-14：identity_store 工具

`12-tool-system.md` 新增第 14 个内置工具 `identity_store`，支撑 SYS-02（系统 Agent 身份管理）。设计合理，但 PRD TOL-01 只列了 13 个内置工具。

**建议**：更新 PRD TOL-01 为 14 个内置工具，或明确 `identity_store` 归属于系统 Agent 专用工具而非平台内置。

---

## 六、各文档覆盖度矩阵

| 设计文档 | 覆盖的 PRD 需求 | 关键缺口 |
|---------|----------------|---------|
| 01-overview.md | PLT-01, RUN-02/03, MEM-01/02, SEC-01, COM-02 | Zone vs PrivacyLevel 未反映 ADR-002；移动端降级机制未展开 |
| 02-agent-package.md | PKG-01~07, FMT-01~07, SYS-06, RAG-01 | 无 CRL 扩展点 (PKG-11)；RAG 认证方式优先级未定；manifest 版本演进策略缺失 |
| 03-agent-runtime.md | RUN-01~13, RXT-01~06 | MemoryStore trait 接口未定义；Skill 级联降级未集成；streaming tool_calls 解析细节不足 |
| 04-gateway.md | GTW-01~12, COM-02/03, SEC-07 | Approval Gate HTTP 端点缺失；Capability Registry 发现机制不完整；cron 时区存储位置未定 |
| 05-memory.md | MEM-01~12 (完整) | Phase 1 范围可能过大；离线巩固触发时是否阻塞主循环未明确 |
| 06-communication.md | COM-01~05 (基本完整) | Intent 消息 schema 缺失；同 action 多 Agent 冲突未处理；Debug Protocol 详情指向 10-debug-protocol.md |
| 07-system-agent.md | SYS-01~06 (完整) | 冷启动 bootstrap 顺序未解决；多 Agent 上报身份冲突的仲裁规则缺失 |
| 08-security.md | SEC-01~07, PKG-02/05, TOL-03/04, COM-05 | 仓库扫描超出 PRD 范围；Shell 子进程文件追踪标记为 Unknown 无法决策 |
| 09-roadmap.md | Phase 1~7 全局映射 | P1 需求延后严重（RUN-13, TOL-02, SKL-03, RAG-01~07）|
| 10-debug-protocol.md | COM-04, DEV-05~07 | 录制格式无版本号；Skill reload 时 Grafeo 经验层保留/丢弃未定 |
| 12-tool-system.md | TOL-01~10, RAG-01~07 (声明层) | RAG 执行层缺失；WASM Phase 1 范围与 PRD P1 不匹配 |
| 13-skill-system.md | SKL-01~05 | 级联降级推迟到 Phase 4+；SkillExperience 与 SKILL.md 冲突时优先级未定 |
| 14-desktop-app.md | DSK-01~08, DEV-05 | HTTP API 仅草图无完整 schema；DevMode 切换是否中断当前上下文未定 |
| module-design/00~06 | MNT-01, workspace 结构 | rollball-memory crate 缺失；RAG 工具未在 Runtime 工具注册表中 |

---

## 七、建议行动清单（按优先级排序）

| 优先级 | 行动 | 涉及文档 | 预估工作量 |
|--------|------|---------|-----------|
| **P0** | 定义 `rollball-memory` crate 并加入 workspace | module-design/00-overview, 新建 module-design/07-memory.md | 1~2 天 |
| **P0** | 补充 RAG 工具执行层设计 | 12-tool-system.md, module-design/02-runtime.md | 1 天 |
| **P0** | 补充 Approval Gate HTTP API 端点 | 04-gateway.md §9 | 0.5 天 |
| **P1** | 统一 PRD 优先级与路线图阶段映射 | 09-roadmap.md, 00-prd.md | 0.5 天 |
| **P1** | 定义 Intent 消息完整 schema | 06-communication.md 或 module-design/01-core.md | 0.5 天 |
| **P1** | 明确 System Agent bootstrap 流程 | 07-system-agent.md, 04-gateway.md | 0.5 天 |
| **P1** | 定义权限匹配语义 | 02-agent-package.md | 0.5 天 |
| **P2** | 同步所有文档到 v3.4 | 01-overview, 07-system-agent, 08-security | 1 天 |
| **P2** | 将超出 PRD 的设计反向补充到 PRD | 00-prd.md §1.10, §1.5 | 0.5 天 |
| **P2** | 补充 Tool Result 折叠算法 | 03-agent-runtime.md | 0.5 天 |
| **P2** | 补充 Capability Registry 更新保证 | 06-communication.md | 0.5 天 |

---

## 八、总结

设计文档整体质量高，架构决策清晰（5 个 ADR 覆盖了关键分歧点），Android 隐喻在各文档中保持一致。主要问题集中在三个层面：

1. **抽象层落地缺失**：`rollball-memory` crate 是 ADR-003 的核心产出物，但未出现在 workspace 设计中。这是 Phase 1 的编译级阻塞。

2. **PRD ↔ 路线图对齐偏差**：4 个 P1 需求（RUN-13, TOL-02, SKL-03, RAG-01~07）被推到 Phase 3 或更晚，与 PRD "P1 = Phase 1~2" 的承诺不一致。需要决策：是调整路线图还是降低 PRD 优先级。

3. **跨模块接口规格不足**：Intent schema、Approval Gate API、权限匹配规则、identity_deps 注入格式等跨模块接口缺少形式化定义，实现团队需要二次沟通确认。

**建议优先解决 P0 行动项（3 项）**，因为它们直接影响 Phase 1 的实现可行性。P1 行动项应在 Phase 1 开发启动前完成设计澄清。

---

## 九、架构师逐条评审（2026-04-17）

> 以下为项目架构师对 Claude Opus 4.6 审查结果的逐条回应，标注处置决策和理由。

### Critical 层

#### Issue-01：rollball-memory crate 未定义 — **误报**

Review 基于 `module-design/00-overview.md` 的旧快照做出判断，但实际上该文件已包含 `rollball-memory` crate：

- workspace 成员列表第 59 行：`"crates/rollball-memory"`
- 目录结构第 38 行：`rollball-memory/ # MemoryStore trait + 共享记忆类型（v3.4 新增）`
- `module-design/04-grafeo.md` 多处引用 `rollball_memory::MemoryStore`

Review 可能扫描了缓存版本。**无需修改**。

#### Issue-02：RAG 工具在 Runtime 设计中缺失 — **误报**

`12-tool-system.md` §4 已有完整的 RAG 工具设计（§4.1 声明、§4.2 执行流程、§4.3 降级与安全），包含：
- 工具声明格式（`type = "rag"`）
- 执行流程（Vault 取凭据 → 构造查询请求 → 解析结果 → 标注来源）
- 降级策略（离线降级返回空结果不阻塞）
- 与本地 Grafeo 的双通道检索对比表

Review 可能只扫描了 `module-design/02-runtime.md` 而忽略了 `12-tool-system.md`。**无需修改**。

#### Issue-03：Approval Gate 缺少 HTTP API 端点 — **有效，但优先级应降级**

确认 04-gateway.md 的 HTTP API 路由表中确实没有 approval 相关端点。但需要区分：

- **Phase 1 目标**：Approval Gate 在 `03-agent-runtime.md` §7.4 中定义为"CLI 模式降级为 manifest 配置的默认策略"，即 Phase 1 不需要 Desktop App 确认流程。
- **Phase 3 目标**：Approval Gate 接入 Desktop App 时才需要 HTTP API 端点。

Review 将 RUN-13 标为 P1（Phase 1-2）但路线图安排在 Phase 3，这个偏差确实存在（见 Issue-05 分析），但 HTTP 端点的缺失是路线图对齐的结果而非遗漏。

**处置**：标记为 Phase 3 待办，不阻塞 Phase 1。在 04-gateway.md 中补充注释说明 approval API 端点计划在 Phase 3 新增。

### High 层

#### Issue-04：Skill 级联降级无实现规格 — **有效，但 P2 不需要提前**

PRD TOL-07（Skill 级联降级）和 PLT-05（移动端降级）均标为 P2（Phase 3-4）。`13-skill-system.md` 推迟到 Phase 4+ 是与 PRD 一致的。

Review 暗示"Phase 1 就需要基本降级支持"缺乏依据——Phase 1 仅桌面端，不存在移动端降级场景。

**处置**：保持现状，Phase 4 前补充实现规格即可。如确需提前，可在 Phase 2 移动端适配时处理。

#### Issue-05：PRD 优先级与路线图阶段映射偏差 — **有效，需决策**

这是最有价值的发现。逐项分析：

| 需求 | PRD 标注 | 路线图实际 | 分析 |
|------|---------|-----------|------|
| RUN-13 (Approval Gate) | P1 (Phase 1-2) | Phase 3 | Phase 1 CLI 模式用 manifest 默认策略已够，但 P1 标注暗示 Phase 2 应有基本 GUI 确认能力。合理调整：Phase 2 实现 CLI approval，Phase 3 接入 Desktop App |
| TOL-02 (WASM 工具) | P1 | Phase 3 | WASM 工具是 Phase 1 内置 13 工具的扩展机制，不是 Phase 1 的必需品。P1 标注过高，应降为 P2 |
| SKL-03 (Skill 调试) | P1 | Phase 5.3 | Skill 调试依赖 Debug Protocol（Phase 5.2），无法提前。P1 标注过高，应降为 P2 |
| RAG-01~07 | P1 | 路线图未出现 | 企业 RAG 是"开发范式"而非平台核心功能（PRD §1.13 明确说明）。P1 标注过高，应降为 P2 |

**处置**：调整 PRD 优先级标注，使其与路线图一致。核心原则是"P1 = Phase 1-2 可交付"，超出此范围的需求降为 P2。

#### Issue-06：System Agent 启动顺序循环依赖 — **有效，需补充**

确认没有任何文档定义 System Agent 的 bootstrap 特殊路径。Review 建议的两个方案都可行：

- **方案一**（skip identity injection）：更简洁，System Agent 的 manifest 不声明 `identity_deps`，Gateway 对 system Agent 特殊处理跳过身份注入步骤
- **方案二**（Gateway 内置缓存）：更复杂但更通用，不推荐

System Agent 不需要 identity_deps（它是身份的提供者而非消费者），所以方案一更合理。

**处置**：在 `07-system-agent.md` 中补充 bootstrap 流程说明。

#### Issue-07：Intent 消息格式未定义 — **有效，但非阻塞**

`06-communication.md` 定义了路由流程、Capability Registry、Overview 推送，但 Intent 消息的完整 struct 确实没有给出。这在 `02-agent-package.md` manifest 示例中有部分体现（`intent:send:com.example.calendar` 权限声明），但缺少 sender、target、action、params、response_type 的形式化定义。

**处置**：在 `06-communication.md` §2 中补充 IntentMessage struct。Phase 1 开发前完成即可，不阻塞当前设计。

### Medium 层

#### Issue-08：权限匹配语义不明确 — **有效，低优先级**

确实缺少形式化的匹配规则。但 Phase 1 的权限数量有限（13 个内置工具的权限），匹配逻辑可以通过代码实现直接体现。形式化定义更适合在 API 文档阶段完成。

**处置**：Phase 2 前补充，在 `02-agent-package.md` 中增加权限 pattern 语法说明。

#### Issue-09：identity_deps 注入细节缺失 — **有效，低优先级**

字段 required/optional、缺失默认值等确实需要明确。但 Phase 1 的 identity_deps 字段有限（name/city/language/timezone），默认行为可在实现中硬编码。

**处置**：Phase 2 前补充 manifest schema 细节。

#### Issue-10：Tool Result 折叠算法模糊 — **误报**

`03-agent-runtime.md` 第 184 行已给出明确算法：
- 保留最近 4 轮完整 tool result
- 更早的折叠为单行摘要 `"[tool_name] 返回 {前200字符摘要}"`
- 参数化：`pruner.keep_full_results = 4`，`pruner.fold_summary_length = 200`
- Phase 1 用规则引擎，Phase 3 升级为 LLM 辅助压缩

Review 可能未注意到这段描述。**无需修改**。

#### Issue-11：文档版本不同步 — **误报**

Review 声称 `01-overview.md`、`07-system-agent.md`、`08-security.md` 仍为 v3.0，但实际扫描确认：
- `01-overview.md` 已是 v3.4
- `07-system-agent.md` 已是 v3.4
- `08-security.md` 已是 v3.6（本轮更新）

Review 基于过时快照做出判断。**无需修改**。

#### Issue-12：Capability Registry 更新与失效机制 — **有效，但可推迟**

推送失败重试、过期能力列表安全性等问题在 `06-communication.md` 的 Capability Overview 推送中未涉及。但 Phase 1 Agent 数量少（<10），过期能力列表的影响有限。

**处置**：Phase 4（Intent + Capability 完整实现时）补充 delivery guarantee 设计。

### 超出 PRD 范围

#### Issue-13：仓库级安全扫描超出 PRD — **已解决**

Review 基于 PRD v1.3 做出判断。PRD v1.4 已补充 SEC-09（仓库扫描）和 PKG-08a（上架扫描）。**无需修改**。

#### Issue-14：identity_store 工具超出 TOL-01 — **有效，需更新 PRD**

`12-tool-system.md` 新增第 14 个内置工具 `identity_store`，PRD TOL-01 仍写 13 个。需要更新。

**处置**：更新 PRD TOL-01 为 14 个内置工具，注明 `identity_store` 为系统 Agent 专用。

---

## 十、待执行行动项

| 优先级 | 行动 | 涉及文档 | 状态 |
|--------|------|---------|------|
| P1 | 调整 PRD 优先级标注（RUN-13 拆分为 13a/13b；TOL-02~04/08~09 降为 P2；SKL-03 降为 P2；RAG-01~05/07 降为 P2） | 00-prd.md | **已完成** v1.5 |
| P1 | 更新 PRD TOL-01 为 14 个内置工具（含 identity_store） | 00-prd.md | **已完成** v1.5 |
| P1 | 补充 System Agent bootstrap 流程说明 | 07-system-agent.md | **已完成** v3.5（新增 §8，含启动顺序、差异化逻辑、首次启动处理） |
| P2 | 补充 Intent 消息完整 schema | 06-communication.md | **已完成** v3.5（§2.1 重写，含 IntentMessage/IntentResponse struct、6 种 IntentStatus、JSON 示例、安全说明） |
| P2 | 补充 Approval Gate HTTP API 端点说明（Phase 3 计划） | 04-gateway.md | 待执行 |
| P2 | 补充 PRD SEC-08/09（Shell 安全 + 仓库扫描） | 00-prd.md | **已完成** v1.4 |
| P3 | 补充权限匹配语义 | 02-agent-package.md | **已完成** v3.3（新增 §3.2，含格式语法、通配符规则、匹配算法、Phase 1 权限列表） |
| P3 | 补充 identity_deps 注入细节 | 02-agent-package.md | **已完成** v3.3（新增 §3.1，含字段名约定、required vs optional 语义、字段缺失处理、默认值表） |
| P4 | 补充 Capability Registry delivery guarantee | 06-communication.md | 待执行 |
