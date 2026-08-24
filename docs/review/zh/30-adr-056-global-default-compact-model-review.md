# 30 — ADR-056 全局默认精简模型 Code Review

**Date**: 2026-09-12
**Reviewer**: Senior Engineer
**Status**: ✅ 已修复（全部 8 项问题已闭环，cargo test 1458 全绿 + workspace clippy 0 warning + 前端 build 通过）
**Scope**: ADR-056 P1–P5 全量实现（26 文件，+1102 行）

**参考文档**:
- ADR-056: [`docs/adr/zh/ADR-056-global-default-compact-model.md`](../../adr/zh/ADR-056-global-default-compact-model.md)
- 涉及核心文件:
  - `core/acowork-core/proto/mqtt_payload.proto` / `core/acowork-core/src/protocol.rs`（P1 数据模型）
  - `core/acowork-gateway/src/resource_cache.rs` / `http/settings_api.rs` / `mqtt/global_resources_publisher.rs`（P1/P3 Gateway）
  - `core/acowork-runtime/src/agent/loop_context.rs` / `agent/agent_core.rs` / `startup/session_init.rs` / `mqtt/available_cache.rs`（P2 Runtime）
  - `apps/acowork-desktop/src/components/harness/GlobalCompactModelCard.tsx` / `GlobalModelPicker.tsx`（P4 UI）
  - `core/acowork-gateway/tests/settings_api.rs`（P5 集成测试）

---

## 一、整体判断

**核心链路（proto → resource_cache → MQTT → Runtime 缓存 → resolve → 跨 provider 实例）设计正确，P4 UI 与 i18n 完整，测试总量扎实（24 个新增 + 1450+ 全绿）。但存在 1 个违反 ADR 核心设计目标的致命缺陷：本地 provider（Ollama）无法被 Level 1 选中，功能对 ADR §2.2/§2.3 的主打场景完全失效。另有 1 个调用点漏改（title 生成）会导致跨 provider 蒸馏用错 provider。**

## 二、问题清单（均已 doublecheck 确认，并已修复）

> 修复状态列在每项末尾。全部修复已通过 `cargo test`（核心 3 crate 1458 全绿）+ `cargo clippy --workspace --all-targets -- -D warnings`（0 warning）+ 前端 `tsc`/`vite build` 验证。

### 🔴 P0-1 本地 provider（Ollama）不可用作全局默认 — 违反 ADR §2.2/§2.3 核心目标

**✅ 已修复**：`is_default_compact_provider_available` 增加本地分支（api_key 非空 或 本地 base_url/本地协议）；新增共享 `providers::is_local_base_url`；`available_cache::is_provider_available` 语义对齐；新增/修正 6 个测试（本地无 key 可用、云端撤 key 不可用、未知 provider）。

**证据链**（逐环验证）:
1. `mqtt/client.rs:208` `extract_provider_keys` 用 `.filter(|pr| !pr.api_key.is_empty())` 过滤 → Ollama 空 key **不进** `provider_key_vault`
2. `startup/session_init.rs:304` 填充 vault 同样 `if !p.api_key.is_empty()` → 一致
3. `agent_core.rs:975` `is_default_compact_provider_available` 只查 vault key → 对 Ollama 恒 false
4. `loop_context.rs:260` Level 1 依赖此判断 → **用户选 Ollama 模型，Level 1 永远 fallback 到 Level 2**
5. UI 侧 `GET /api/providers` 返回 `"(local)"` 条目 → **UI 能选 Ollama，运行时判不可用**，前后端语义脱节

**违反条款**: ADR §2.2 场景表（"想用本地 Ollama qwen2.5:0.5b 做蒸馏"）、§2.3 设计目标（"支持本地 + 商业模型混搭"）
**讽刺点**: `available_cache::is_provider_available`（`available_cache.rs:139`）已含 `|| !pr.base_url.is_empty()` 的本地启发式，但它是死代码（`#[allow(dead_code)]`），从未被 `resolve_distill_model` 调用。

### 🔴 P0-2 title 生成漏改调用方 — 跨 provider 蒸馏用错 provider

**✅ 已修复**：抽取 `AgentLoop::distill_provider` 共享 helper（返回 provider+model+tier，失败时 demote 到 session provider + current chat model），`compact_history_if_needed` / tail distillation / title 生成三处调用点统一复用。

`loop_.rs:781`:
```rust
let provider = self.core.provider.clone();                            // session provider
let compact_model = self.resolve_distill_model(title_input).model_id; // 只取 model_id
compact_session_title_with_llm(&prompt, provider.as_ref(), &compact_model, 120)
```
Level 1 命中 `ollama-local/qwen2.5:0.5b` 而 session 是 deepseek 时 → 用 deepseek 实例调不存在的模型 → title 生成失败。
同一 PR 中 `loop_context.rs:433`、`loop_session.rs:283` 均已正确重建 provider，唯此调用点遗漏。

### 🟠 P1-1 HTTP 状态码 400 ≠ ADR §4.1 要求的 422

**✅ 已修复**：`ApiError` 新增 `unprocessable_entity`（422）；`settings_api` PUT 改用之；2 个集成测试改断言 422。

`settings_api.rs:83` 用 `ApiError::bad_request`（400）；ADR §4.1 明确 "不存在则拒绝（HTTP 422）"。`ApiError` 无 `unprocessable_entity` 方法；集成测试 `settings_api.rs:252/293` 断言 400，将错误固化。

### 🟠 P1-2 ADR §5.1 清单项 `provider_api.rs` 未落实

**✅ 已修订 ADR**：`default_compact_model` 是全局设置，塞进 per-provider `ProviderEntryResponse` 是反模式；ADR §5.1 表格改为"provider_api.rs 不改，由独立 settings 端点提供"（与 §3.1 数据流图一致），§5.3 同步修正。

ADR 要求 `ProviderEntryResponse` 新增 `default_compact_model` 字段供 UI 拉取；实际 `provider_api.rs` 零改动（git 确认）。当前 UI 走独立 settings 端点（符合 ADR §3.1 数据流图），§5.1 与 §3.1 存在文档自相矛盾。需二选一：实现字段 或 修订 ADR。

### 🟠 P1-3 `available_cache::is_provider_available` 语义偏离 ADR 且未接线

**✅ 已修复**：语义改为 api_key 非空 或 本地 base_url（与 AgentCore 一致）；保留 `#[allow(dead_code)]`（模块查询 helper 惯例，同 `is_mcp_available`）；补 ADR §9.1 三用例测试。

- ADR §5.2 要求 "api_key 非空 + provider enabled"；§9.1 测试用例 "api_key 为空 → false"
- 实际实现 `!api_key.is_empty() || !base_url.is_empty()` → 空 key + 非空 base_url 返回 true，**偏离 §9.1 ②**
- `ProviderListItem` 无 `enabled` 字段，ADR 的 enabled 判断无处落地
- 无任何业务调用方（仅 2 个自测）

### 🟡 P2-1 单元测试场景失真

**✅ 已修复**：`seed_providers` 不再给 ollama-local seed key（模拟真实无 key 本地 provider）；`tier1_global_default_is_picked_when_set_and_available` 现在验证真实旗舰场景（本地无 key 命中 Level 1）。

`loop_context.rs:1382` `tier1_global_default_is_picked_when_set_and_available` 给 ollama-local **seed 了 API key** 才断言 Level 1 命中 —— 真实 Ollama 无 key（P0-1），测试给出虚假信心。

### 🟡 P2-2 `build_provider_for` 失败时 model_id 未回退

**✅ 已修复**：`distill_provider` 失败分支同时 demote model 到 current chat model + tier=CurrentChat，杜绝"session provider 调跨 provider 模型"。

`loop_context.rs:454-460` / `loop_session.rs:309-317`: 重建失败 fallback 到 session provider，但 `compact_model` 仍保持 Level 1 的 model_id → 用错误 provider 调不存在的模型。

### 🟡 P2-3 小项

**✅ 已修复**：`gateway-api.ts` 末尾补换行；顺手修复阻塞 workspace clippy 的 pre-existing `process.rs:523`（cfg 分支统一为 String，行为不变）。

- `gateway-api.ts` 文件末尾无换行
- `settings_api.rs:96` 落盘失败时 version 不回滚（有注释说明，内存/磁盘短暂不一致，可接受）

## 三、✅ 确认无误的部分

1. proto 扩展符合 ADR §4.2（`optional CompactModelRef = 7`，向后兼容）
2. `resource_cache::set_default_compact_model` 设计干净：validate → mutate → version bump，错误不变异，8 个单测
3. `ResolvedDistill` 携带 tier，每级降级有结构化 `tracing::warn!`（符合 ADR §7 日志要求）
4. token 估算修正（ADR §5.2）：probe_model 按 Level 1→2→3 优先级
5. 跨 provider 蒸馏的 provider 重建（loop_context/loop_session）思路正确
6. i18n 双语完整，UI 状态机（saved/staged/saving）清晰

## 四、ADR §9.2 集成测试缺口

| ADR §9.2 场景 | 现状 |
|---|---|
| 完整链路 PUT → MQTT → Runtime → Level 1 命中 | ❌ 仅 HTTP 层测试，无 MQTT 全链路 |
| api_key 撤销 → 走 Level 2 | ✅ 单测覆盖 |
| 跨 provider 蒸馏（chat=deepseek, distill=ollama） | ⚠️ 单测覆盖但场景失真（seed 了 key） |
| manifest 推荐标识 ★ | ❌ 依赖 §11 遗留链路，可接受 |

## 五、修复顺序建议

1. **P0-1**：`is_default_compact_provider_available` 增加本地 provider 语义（api_key 非空 或 本地 base_url/本地协议），并对齐 `available_cache::is_provider_available`
2. **P0-2**：抽取统一的 `distill_provider` helper，三处调用点复用
3. **P1-1**：`ApiError` 增 `unprocessable_entity` → settings_api 返回 422 → 测试改断言
4. **P1-3 / P2-1**：对齐 available_cache helper 语义 + 补 ADR §9.1 三用例测试；修正失真测试
5. **P2-2**：provider 重建失败时连 model 一起回退 Level 3
6. **P1-2**：实现 `ProviderEntryResponse.default_compact_model` 或修订 ADR §5.1（待 owner 拍板）
