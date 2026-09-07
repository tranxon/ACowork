# Code Review：ADR-071（记忆蒸馏运行时配置与触发接线）

**Reviewer**: Senior Software Engineer
**日期**: 2026-09
**范围**: `e3ffe9f7`..`f4eaf46e`（设计前置 + W1–W5 共 6 个 commit；Rust 9 crates + desktop）
**对照文档**: [ADR-071](../adr/zh/ADR-071-distiller-runtime-config-and-trigger.md)

> 红/绿结论来自本机实测（macOS aarch64）。逐条对照 ADR-071 D1–D8 与验收矩阵。

---

## 1. 决策实现核对

| 决策 | 结论 | 证据 |
|---|---|---|
| D1 触发口径与 Pending 解耦 | ✅ | `MemoryProvider::count_unconsolidated_episodes`（memory provider.rs / grafeo provider_impl.rs / runtime test_support.rs / grafeo distiller TestProvider 同步）；`ConsolidationTimer::should_run_distill`（consolidation_bg.rs），主循环独立执行 distill 与 legacy lifecycle |
| D2 手动端点 + status 扩展 | ✅ | `POST /memory/distill` → `run_episodic_distill_once`（复用后台 `run_episodic_distiller_step_once`,force 语义,409 守 opt-in）;`GET /memory/consolidation/status` 返回 `distiller{enabled, episode_backlog, secs_since_distill, interval_secs, accumulation_threshold, idle_secs, last_run}` |
| D3 配置分层(manifest 初值 → agent_config.json) | ✅ | AgentConfig 5 字段 + 首次播种语义与 temperature/context_window 同构;ManifestDistillerConfig 初值字段;HTTP PUT `/agents/{id}/config` 接线 |
| D4 字段集 | ✅ | `distiller_enabled` / `distiller_model(CompactModelRef)` / `distiller_interval_minutes` / `distiller_accumulation_threshold` / `distiller_idle_minutes`,全 Option |
| D5 模型解析链 | ✅ | `resolve_distiller_model_id`:agent_config → manifest → `default_compact_model` → provider 首模型 |
| D6 prompt per-agent | ✅ | `OVERRIDABLE_PROMPTS` 5→7(`distiller-extraction.md`/`distiller-judge.md`);AgentCore 两槽;`DistillerConfig.extraction_prompt_override`/`judge_prompt_override`;grafeo run 消费(override 优先/None 回退常量);reload dispatch + HTTP PROMPT_ENTRIES +2 |
| D7 legacy「合并节点」退役 | ✅ | UI 底部按钮换「立即蒸馏」;memoryStore `consolidate` action 删除;legacy consolidate HTTP 路由保留(向后兼容,无 UI 入口) |
| D8 运行时热更新 | ✅(实现方式修订,见 §2) | `ConsolidationTimer.config` → `RwLock<SchedulerConfig>` + `update_config()`;后台 loop 每 tick 重读 |

## 2. 实现偏差(相对 ADR-071 初稿)

1. **D8 热更新方式**(初稿"重建 pipeline"→ 最终 **RwLock 热更**):agent_core `rebuild_consolidation_pipeline_if_running`(保留名)实际执行 `timer.update_config()`,不 abort/spawn 后台任务。理由:避免跨组件 timer 同步问题;idle/backlog/last-run 状态在换配置时保留。ADR-071 §D8 已就地修订为最终实现。
2. **W2 附带修复**:蒸馏 embedding 闭包原用 `Runtime::Handle::block_on` 在 async worker 内会 panic("Cannot start a runtime from within a runtime"),改为 `block_in_place` 桥接;手动端点与后台共用路径一并修复。
3. **distiller prompt 槽在 `distiller_scheduler_config()` 投影**:无 `[memory.distiller]` manifest 段但 package 带蒸馏 prompt 文件时仍产出 `Some(DistillerConfig)`(否则 override 静默丢失),而 `distiller_enabled` 保持 opt-in 关闭——D6 语义的边界情形,单测 `test_distiller_scheduler_config_override_without_manifest_section` 钉住。

## 3. 验证结果(本机实测)

- `acowork-memory` 30 passed;`acowork-grafeo` 290 passed(+2 `test_d7_prompt_overrides_*`);`acowork-runtime --lib` 1378 passed(+2 投影/热更测试;唯一失败预存基线 `restart_after_compression_preserves_todo_state`);
- clippy(memory/grafeo/runtime lib):0 新增(runtime 9 个 warning 全为 history.rs/loop_llm.rs 预存在);
- desktop:`tsc --noEmit` 0 error;`check:i18n` OK;vitest 357/358(唯一失败 `formatTime` 日期骨架,stash 复测基线即红,与本次无关)。

## 4. 遗留风险(沿用 ADR-068 review #32 §0.6 + 新增)

| # | 风险 | 级别 | 备注 |
|---|---|---|---|
| R1 | distiller prompt 无 golden 快照测试 | 高(R-R2) | 建议 `insta`/`expect-test` 钉住内置 `EXTRACTION_SYSTEM_PROMPT`/`JUDGE_SYSTEM_PROMPT` 与 override 拼接结果,防无意识改写 |
| R2 | MQTT `acowork/consolidation/event` 无订阅入口 | 中 | History 晋升/事件驱动缺触发面 |
| R3 | e2e 走 scripted mock LLM,未接真实 `ProviderLlmAdapter` | 中 | 蒸馏全链路(模型解析→LLM 适配→提取)未在真实 provider 验证 |
| R4 | autobio `key_hint` 空洞值(如 "the_agent")聚簇质量 | 低 | embedding 合并可缓解,仍建议词表护栏 |
| R5 | distiller_model 跨 provider 选择未落地 | 低 | `resolve_distiller_model_id` 注释明示:后台管道 LLM 走 agent 活跃 provider;选别家 provider 模型会失败 → UI 候选集来自 vault(实际可用的 provider+model),规避了该坑 |
| R6 | `POST /memory/consolidate`(legacy)HTTP 路由仍保留 | 低 | 无 UI 入口;建议下个 revision 删除并收敛 episodic cleanup 描述 |

## 5. 提交清单

| commit | 内容 |
|---|---|
| `e3ffe9f7` | 设计前置:ADR-071 + 05-memory §4.2 同步 |
| `ecad9cd7` | W1 触发修复(解耦 Pending;独立 distill 触发) |
| `5d8fc2e2` | W2 手动端点 + status 扩展 + embedding `block_in_place` 修复 |
| `abd32cbb` | W3 配置链路(AgentConfig/Manifest/Overrides/热更 RwLock) |
| `c8c8426b` | W4 prompt 覆盖(白名单 +2 / AgentCore 槽 / grafeo override) |
| `f4eaf46e` | W5 记忆面板 UI(蒸馏卡片 + 立即蒸馏 + legacy 退役) |

## 6. 二次 Review(2026-09,W1–W6 全量测试覆盖率补强)

首轮 review 发现测试覆盖与 commit 声明不符(W2 声称的 "adr071 memory e2e suite" 实际不存在),按优先级补齐。本次补强分两个 commit:

### 6.1 P0 — 新增 ADR-071 全链路 e2e(`6069f791`)

`core/acowork-runtime/src/memory/adr071_e2e.rs`(in-crate,因 `AgentCore::new` 为 `pub(crate)`,且需注入 `pub(crate)` 的 providers/timer;与 `prompts_reload_e2e` 文档化的限制一致):

- **E1**:真实 in-memory `GrafeoStore` 播种 2 Fact + 3 Preference(满足 ADR-068 Step 3 evidence gate)→ HTTP `POST /memory/distill` → 断言 DistillResponse(`scanned=5`/`promoted≥2`/`marked=5`)、`KnowledgeNode.promotion_metadata.promoted_by="episodic_distiller"` + evidence ids、episode 清理、`GET /memory/consolidation/status` `last_run` 摘要。LLM 为 scripted `MockProvider`,但走**真实 `ProviderLlmAdapter`**(R3 部分缓解——不再是 distiller 内部直调);`multi_thread` runtime 同时覆盖 W2 `block_in_place` embedding 桥。
- **E2**:manifest 无 `[memory.distiller]` → 409 + 零晋升 + episode 未 consolidated(opt-in 不变式)。
- 顺带:`distiller_scheduler_config()` 改 `pub(crate)`,harness 用生产投影构造 timer(status 反映 effective switch,与 `start_consolidation_pipeline` 一致)。

### 6.2 P1 — 单测补强(commit `102bdf8b`)

| 项 | 内容 |
|---|---|
| `usecases/agent_config_impl.rs` +4 | `ConfigField` 5 个 distiller 变体的 Set/Clear/类型错配/wire 翻译(`from_request_fields` 缺省→skip、null→Clear、值→Set) |
| `http/server.rs` +1 | `PUT /agents/{id}/config` 5 字段 → `agent_config.json` 落盘 → `GET /config` round-trip + partial-PUT 保留未发字段 |
| `consolidation_bg.rs` +3 | embedding 桥三路径:multi_thread=`Some` 且可调用、current_thread=`None` 降级、无 runtime=`None` |
| status `last_run` | 已由 E1 覆盖(不再单独补) |

### 6.3 新发现(记录,未修)

1. **`patch_typed` 语义歧义(预存,非 ADR-071 引入)**:`apply_field_patch` 无条件赋值 `cfg.x = patch_typed(...)`,而 `patch_typed` 对**类型错误的 `Set`** 返回 `None` → 字段被**清空**,与 impl 顶部注释 "leave on-disk alone" 矛盾。所有字段(含 distiller)共用此路径。建议后续 revision:区分 `Clear` 与 `Set-parse-failed`(tri-state),避免错误 JSON 造成数据丢失。P1a 测试按实际语义断言并标注。
2. **wire 层无显式清空**:`UpdateAgentConfigRequest` 用 `Option<serde_json::Value>`,serde 将 JSON `null` 与字段缺失折叠为同一 `None` → 显式清空需引入 presence-tracking wrapper。当前 UI「空输入=不发送该字段」规避了此坑(P1b 测试验证 partial-PUT 语义)。
3. **性能观察点(非阻塞)**:`count_unconsolidated_episodes` → `get_unconsolidated_episodes_by_subtype(None, i64::MAX)` 会全量物化 Episodic 层再数数;distiller enabled 时后台每 tick(60s)轮询。受 grafeo-engine 当前 GQL 限制(ORDER BY/WHERE 返回裸 ID),与 distiller Step 1 既有模式一致,但「数数」不应全量加载——建议引擎层提供 COUNT 或降频。
4. **W5 commit message 偏差**:称按钮 "only exposes while enabled",实际按钮常显、disabled 时 409(行为可接受,反馈更直接;doc 待同步)。

### 6.4 验证(本机实测)

- `adr071_e2e` 2/2 ✅;`agent_config_impl` +4 ✅;`server.rs` distiller roundtrip +1 ✅;`embedding_bridge_tests` +3 ✅;`consolidation_bg` 全量 18 ✅
- `acowork-runtime --lib` 1389 passed,唯一失败仍为预存基线 `restart_after_compression_preserves_todo_state`;clippy 0 新增
- 测试补强后影响面映射:触发(W1)✅ 单测;手动端点(W2)✅ e2e + 单测;配置链(W3)✅ 单测 + HTTP roundtrip;prompt(W4)✅ grafeo/agent_core 单测;embedding 桥(W2)✅ 三路径;status(W2)✅ e2e
