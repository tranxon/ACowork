# ADR-071: 记忆蒸馏运行时配置与触发接线(EpisodicDistiller 可运维化)

**状态**:已实现(2026-09;W1–W5 落地,提交见实施路径表;W6 文档收尾)
**日期**:2026-09
**决策者**:大鱼
**前置**:
- [ADR-068](./ADR-068-memory-layer-promotion-two-axis-orthogonal.md)(两轴正交与 EpisodicDistiller 引擎;本 ADR 补全其 M4/M7 的调度触发接线与配置面)
- [ADR-063](./ADR-063-package-level-prompt-override.md)(package-level prompt override;本 ADR 将蒸馏两件套纳入白名单)
- [ADR-053](./ADR-053-agent-specific-compaction-prompt.md)(summary.md 覆盖先例,Debug 界面 PromptList 范式)
- [05-memory.md §4.2](../design/zh/05-memory.md)(离线蒸馏设计基线)

---

## 背景与问题(均为代码级事实)

| # | 问题 | 事实 |
|---|------|------|
| P1 | **后台调度触发断链** | ADR-068 后没有任何路径再创建 `Pending` 沉淀层节点(memory_store 只写 Episode;蒸馏晋升直写 Active),而 `ConsolidationBgTask` 的 `should_run()` 触发条件统计 **Pending 节点数**(`get_pending_for_consolidation` 扫 `KNOWLEDGE` label 的 `status=Pending`)→ 真实部署下 `run_episodic_distiller_step` **永不执行**。单测/e2e 仅直调 `distiller.run()` 验证,M7 验收未覆盖 scheduler→distiller 全链路 |
| P2 | **配置只读一次,无热更新** | distiller 开关/参数在 consolidation pipeline 启动时从 manifest 快照一次(`agent_core.rs start_consolidation_pipeline`);运行时不可改;Desktop 无写 manifest 通道 |
| P3 | **蒸馏模型硬编码** | 蒸馏 LLM = `global_provider_list` 第一个模型的 id;无模型配置项;未复用摘要模型(`default_compact_model`)机制 |
| P4 | **蒸馏 prompt 不可 per-agent 覆盖** | `EXTRACTION_SYSTEM_PROMPT` / `JUDGE_SYSTEM_PROMPT` 写死于 grafeo 常量;ADR-068 revision 把 grafeo 三件套(extraction/conflict-classification/generalization)移出 override 白名单,蒸馏两件套从未可配 |
| P5 | **UI 无入口 + legacy 按钮名不副实** | 记忆面板无 distiller 设置;"合并节点"按钮(consolidate)在 ADR-068 后只做 episodic cleanup,不合并、不蒸馏、不晋升 |

---

## 决策

### D1 触发口径与 legacy Pending 解耦

蒸馏后台触发不再依赖 `Pending` 节点计数。provider trait 新增 `count_unconsolidated_episodes()`(统计 `EPISODIC` label 中 `consolidated=false` 且未被 distiller skip 的数量)。

蒸馏独立触发条件(AND):

```
周期到点(distiller_interval_minutes,默认 60 分钟)
  ∧ (未 consolidated episode 积压 ≥ distiller_accumulation_threshold(默认 50)
      ∨ 对话空闲 ≥ distiller_idle_minutes(默认 30 分钟))
```

legacy offline(生命周期 cleanup)维持原 Pending 条件;两条管道各自独立触发,互不阻塞。

### D2 手动蒸馏端点

- 新增 `POST /memory/distill`(runtime localhost):绕过周期直接跑一次 `run_episodic_distiller_step`(force 语义),与后台共用同一实现
- `GET /memory/consolidation/status` 扩展返回:distiller_enabled、effective 配置、上次运行 `DistillerResult`(晋升计数/时间)

### D3 配置分层(沿用平台既有惯例)

- `manifest [memory.distiller]` = **包作者初值**(.agent 声明式,新增 model/interval/accumulation/idle 字段)
- `{work_dir}/config/agent_config.json` = **运行时层**:首次运行 agent_config.json 无 distiller 参数时,按 manifest 默认初始化一次;此后 Desktop 界面只读写 agent_config.json。字段全 `Option`,`None` = 回落 manifest → 系统默认(与 temperature/context_window 等既有字段同构)

### D4 字段集

`AgentConfig` 新增(`agent_config.json`):

| 字段 | 类型 | 默认(manifest 初值缺省时) |
|------|------|------|
| `distiller_enabled` | `Option<bool>` | false(保持 ADR-068 opt-in) |
| `distiller_model` | `Option<CompactModelRef{provider_id, model_id}>` | 无(见 D5 解析链) |
| `distiller_interval_minutes` | `Option<u64>` | 60 |
| `distiller_accumulation_threshold` | `Option<usize>` | 50 |
| `distiller_idle_minutes` | `Option<u64>` | 30 |

`ManifestDistillerConfig` 新增同名初值字段(enabled 已有;model 用 provider_id/model_id 双字段,与 `CompactModelRef` 对应;其余 snake_case 同名)。

### D5 模型选择与解析链

- **UI 逻辑完全复用摘要模型下拉**(`GlobalCompactModelCard`:vault keys → `provider::model` options、乐观更新、失败回滚);但存储独立字段(`distiller_model`),**不隐式复用** `default_compact_model` 字段——蒸馏质量/成本特性与摘要不同,允许分开选择
- effective 解析链:`agent_config.json` → manifest `[memory.distiller].model` → `default_compact_model`(全局兜底)→ provider 列表第一模型(现状保底)

### D6 蒸馏 prompt 纳入 ADR-063 覆盖(per-agent)

- `OVERRIDABLE_PROMPTS` 增加两项:`distiller-extraction.md`(Step 2a 结构化提取)、`distiller-judge.md`(Step 4 Judge 仲裁)
- AgentCore 增加两个 `Arc<RwLock<Option<String>>>` 槽;`reload_prompts_into_core` 一并刷新;`DefaultEpisodicDistiller::run` 增加 override 参数(grafeo 常量保留为内置默认)
- Debug 面板 PromptList 从服务端枚举,白名单新增后**自动可见**,无需前端改动
- **不恢复** `conflict-classification.md` / `generalization.md`(其生产者已下线)
- 注:本决策部分反转 ADR-068 revision 的"移除 grafeo override"条目——仅限蒸馏两件套,原移除理由(grafeo 蒸馏路径不经 prompts overlay)因本 ADR 将蒸馏路径纳入 per-agent 配置面而不再成立

### D7 legacy「合并节点」入口退役

- 手动入口改为「立即蒸馏」(D2);记忆面板 consolidate 按钮(zh locale「合并节点」)退役
- episodic cleanup 并入周期 consolidation 自动执行(现状即如此),不设人工入口(如后续需要人工清理,再加显式"清理"次级按钮)

### D8 运行时热更新

- `RuntimeConfigOverrides`(RuntimeConfigUpdate)增加 D4 字段 → gateway PUT `/api/agents/{id}/config` 全链路透传 → `AgentCore.apply_runtime_config` 应用
- **最终实现(与初稿不同)**:distiller 配置变更不重建后台任务。`ConsolidationTimer` 将调度策略保存在内部 `RwLock<SchedulerConfig>`,`update_config()` 换值后 **后台 loop 每 tick 重读**(≤60s 生效);timer 的 idle/backlog/last-run 状态保留,配置变更不会误触发或推迟蒸馏。仅在 pipeline 尚未启动时,下一次 `start_consolidation_pipeline`(agent 启动 / 手动重建)使用新配置
- prompt 槽走既有 reload(不重建 pipeline;`distiller_scheduler_config()` 每次组装时读取当前槽值)
- `AgentCore.rebuild_consolidation_pipeline_if_running()` 为保留的兼容入口名,实际行为即上述 `update_config` 热更

---

## 实施路径

| # | 工作项 | 内容 | 状态 |
|---|--------|------|------|
| W1 | 触发修复 | provider `count_unconsolidated_episodes` + grafeo 实现;`ConsolidationTimer`/`run_consolidation` 解耦触发口径;周期/积压/空闲判定 | ✅ `ecad9cd7` |
| W2 | 手动端点 | `POST /memory/distill` + `consolidation/status` 扩展(共享 `run_episodic_distiller_step`);修复 embedding 闭包 `block_on` panic → `block_in_place` 桥接 | ✅ `5d8fc2e2` |
| W3 | 配置链路 | `AgentConfig` + `ManifestDistillerConfig` 新增字段;`RuntimeConfigOverrides` 透传;`apply_runtime_config`;`ConsolidationTimer.config` → `RwLock` 热更(D6) | ✅ `abd32cbb` |
| W4 | prompt 覆盖 | 白名单 +2(`distiller-extraction.md`/`distiller-judge.md`);AgentCore 槽;`DistillerConfig` override 字段;`Distiller::run` 消费;reload;Debug PROMPT_ENTRIES +2 | ✅ `c8c8426b` |
| W5 | UI | 记忆面板"记忆蒸馏"卡片(开关/模型下拉/周期/立即蒸馏/上次运行);consolidate 按钮替换(legacy action 删除) | ✅ `f4eaf46e` |
| W6 | 测试与文档 | 调度触发(积压/空闲/手动)单测;配置热更单测;prompt override 单测;ADR-071 状态收尾 | ✅ 本提交 |

## 验收矩阵

| 验收项 | 方法 | 状态 |
|--------|------|------|
| 周期触发 | `should_run_distill` interval gate;consolidation_bg 单测 `test_distiller_trigger_*` | ✅ |
| 空闲触发 | idle ≥ 阈值且积压不足 → distill 触发(独立分支) | ✅ |
| 手动触发 | `POST /memory/distill` → `run_episodic_distill_once`(force,仍守 opt-in);409 disabled | ✅ |
| 默认关闭 | 无配置 → `SchedulerConfig::default().distiller_enabled=false`;后台不跑蒸馏 | ✅ |
| 配置热更新 | PUT agent config → RwLock `update_config`,≤60s 生效;`test_distiller_trigger_live_config_update_d6` | ✅ |
| 模型选择 | `resolve_distiller_model_id` 四层解析链单测;UI 下拉 → `CompactModelRef` | ✅ |
| prompt 覆盖 | grafeo `test_d7_prompt_overrides_*`(到达 LLM 调用点/None 回退);agent_core 投影测试;Debug PROMPT_ENTRIES 可见 | ✅ |
| legacy 退役 | UI consolidate 按钮→"立即蒸馏";store 层 `consolidate` action 删除;episodic cleanup 仍随周期执行 | ✅ |
| 回归 | memory 30 / grafeo 290 / runtime lib 1378(1 预存基线失败 `restart_after_compression_preserves_todo_state`)/ desktop tsc + vitest 357/358(1 预存 formatTime 失败);clippy 0 新增 | ✅ |

## 与现有 ADR 的关系

| ADR | 关系 |
|-----|------|
| ADR-068 | **补全/修订** — M4/M7 调度触发接线(修复 P1);配置面扩展(P2–P5);prompt override 移除条目部分反转(仅蒸馏两件套,D6) |
| ADR-063 | **扩展** — `OVERRIDABLE_PROMPTS` +2 |
| ADR-053 | **依赖先例** — summary.md 覆盖与 Debug PromptList 范式 |
| ADR-062 | **无冲突** — keyword sanitize 等质量门禁在 LLM 边界不变 |
