# ADR-053：Agent 级上下文压缩提示词 - prompts/summary.md 替代统一 COMPACTION_SYSTEM_PROMPT

**状态**：已定案
**日期**：2026-08-21
**决策者**：大鱼
**前置**：
- [ADR-011](./ADR-011-compaction-and-distillation.md)（上下文摘要与蒸馏统一策略）
- [ADR-014](./ADR-014-loop-module-decomposition.md)（Loop 模块分解）

---

## 1. 决策摘要

系统提示词（system prompt）早已是 **per-agent** 的（`.agent` 包内 `prompts/*.md` 声明式定义），但上下文压缩（compaction）与蒸馏（distillation）的系统提示词仍是**全局统一**的硬编码常量 `COMPACTION_SYSTEM_PROMPT`（`core/acowork-runtime/src/prompt.rs`）。不同 agent 类型对"什么信息必须保留"的需求差异巨大：软件工程师 agent 需要文件路径 / 函数名 / 技术决策，客服 agent 需要用户诉求与处理结论，文档 agent 需要结构与引用关系——一个统一的提示词无法表达这些差异。

本 ADR 将压缩提示词对齐 system prompt 的既有模式：**每个 agent 可在 `prompts/` 目录下声明自己的 `summary.md`**，作为该 agent 专属的压缩/蒸馏指令；缺失时回退到内置默认 `COMPACTION_SYSTEM_PROMPT`。

**核心决策**：

1. **包级声明**：`prompts/summary.md` = agent 专属压缩/蒸馏系统提示词。与 `system.md`（主对话身份）同级、同机制，符合"每个 agent 有独立 system prompt，也有独立 compact prompt"的对称性。
2. **优先级链**（自高到低）：
   ```
   AgentCore.compaction_prompt（prompts/summary.md 包声明）  >  COMPACTION_SYSTEM_PROMPT（内置兜底）
   ```
3. **消除语义混用**：compaction 路径**不再借用** `system_prompt_override`（agent_config.json 中用于覆盖主对话 system prompt 的字段）。压缩是独立的摘要任务，其指令属于包声明，不属于运行时配置。
4. **主 prompt 排除**：`prompt_builder` 组装主 system prompt 时**跳过 `summary.md`**，防止摘要元指令泄漏进每一轮 LLM 调用。
5. **全路径统一**：压缩主路径（`loop_context.rs`）与蒸馏路径（`episode_distill.rs` 的 `compact_messages`，以及预留的 `distill_on_session_end`）均使用同一解析结果；加载点收敛到 Phase A，Gateway 与 Standalone 两种模式行为一致。

---

## 2. 背景与动机

### 2.1 现状：统一提示词的问题

`COMPACTION_SYSTEM_PROMPT`（`prompt.rs:18`，~850 字符）定义了输出格式（`<summary>` + `<user_intent>` 两块）、语言规则与硬性约束。它适用于**通用**摘要，但无法回答：

> **2026-XX-XX 修订**：原 `<triples>` / `<entities>` 块已在 M3 改造中撤销（详见 ADR-057 §0.2 triples-removed 决策说明）；本文档历史描述保留作决策记录。

- 软件工程师 agent：摘要必须保留 `core/acowork-runtime/src/...` 这样的文件路径与函数名，否则下次会话无法续工。
- 项目管理 agent：摘要必须保留决策人、截止时间、风险项。
- 文档 agent：摘要必须保留文档结构与引用关系。

这些是**包作者**最清楚的需求，理应由 `.agent` 包声明，而非由 runtime 硬编码。

### 2.2 现状：`system_prompt_override` 的语义混用

`loop_context.rs` 压缩路径此前使用：

```rust
self.core.system_prompt_override
    .as_deref()
    .unwrap_or(crate::prompt::COMPACTION_SYSTEM_PROMPT)
```

`system_prompt_override` 的语义是**主对话** system prompt 的运行时覆盖（`agent_config.rs:158`、`session_init.rs` 注释明确 `None = "use compiled manifest prompt"`）。压缩借用它是历史遗留——它把"主对话覆盖"与"压缩指令"两个正交维度耦合在一起：

- 用户设置 `system_prompt_override` 的意图是改变对话身份，却意外改变了压缩行为；
- 用户想自定义压缩规则时，却必须通过"主对话覆盖"这个错位入口。

### 2.3 为什么现在做

- 项目未上线，无兼容负担，可以按最优设计直接重构（用户明确指示）。
- `prompts/*.md` 机制（`prompt_builder.rs`）已稳定，`summary.md` 是零成本的自然扩展。
- 蒸馏路径（tail distillation / session-end distillation）与压缩共用同一提示词，一并修正可避免"压缩用自定义规则、蒸馏用默认规则"的不一致。

---

## 3. 设计

### 3.1 文件约定

```
examples/senior-engineer-agent/
├── prompts/
│   ├── system.md        # 主对话身份（进入主 system prompt）
│   ├── constraints.md   # 行为约束（进入主 system prompt）
│   └── summary.md       # 压缩/蒸馏指令（新增，进入 compaction system prompt，不进入主 prompt）
├── skills/
└── manifest.toml
```

### 3.2 加载与解析链

```rust
// prompt_builder.rs
pub const COMPACTION_PROMPT_FILE: &str = "summary.md";

/// 读 prompts/summary.md，缺失或纯空白返回 None
pub fn load_compaction_prompt(package_dir: &Path) -> Option<String>;
```

- `AgentCore` 新增 `compaction_prompt: Option<String>` 字段（与 `system_prompt_override` 并列但语义独立）。
- **加载收敛到 Phase A**（`agent_init.rs` Step 3，system prompt 构建之后）：`load_compaction_prompt(&loaded.package_dir)` 只执行一次，结果存入 `AgentBootContext.compaction_prompt`。两种 AgentCore 构造入口都从该字段注入——Gateway 模式（`session_init.rs` Phase B 的 `Arc::get_mut`）与 Standalone 模式（`cli.rs` 直接构造 `AgentLoop` 后赋值）——保证同一 `.agent` 包在两种模式下解析出相同的压缩指令，不存在模式分裂。
- `loop_context.rs` 解析链：

```rust
self.core.compaction_prompt
    .as_deref()
    .unwrap_or(crate::prompt::COMPACTION_SYSTEM_PROMPT)
```

- 三条蒸馏入口（`compact_full_context` / `compact_messages` / `distill_on_session_end`）新增 `compaction_prompt: Option<&str>` 参数，由调用方从 `AgentCore` 传入。

### 3.3 主 prompt 排除

`build_system_prompt_with_mode` 在遍历 `prompts/*.md` 时按**精确文件名**（`summary.md`）跳过该文件。这是 `prompts/` "全量拼接"规则的唯一例外，理由：压缩指令是给摘要 LLM 的元指令，进入主对话 system prompt 会污染每一轮 LLM 调用。

排除与加载使用**同一匹配判据**（精确文件名 `summary.md`），保证"被排除出主 prompt"与"被加载为压缩指令"始终指向同一个文件；任何其他命名（`SUMMARY.md`、`summary.txt`）一律按普通 prompt 段处理，不静默丢失。

### 3.4 保留不变的

- 内置 `COMPACTION_SYSTEM_PROMPT` 作为兜底（无 `summary.md` 的包行为不变）。
- `build_compaction_system_prompt(base, identity_context)` 仍将用户身份（语言）指令追加到 base 之后——`summary.md` 作为 base 同样受益于语言规则。
- `COMPACT_PROMPT`（携带 `<conversation>` 正文的 user 消息模板）不变。

### 3.5 限制与预留

- **启动时静态加载**：`compaction_prompt` 在 Phase A 读取一次，之后不随包升级 / 热更新重载（与 system prompt 的启动时编译行为一致）。包作者更新 `summary.md` 后需重启 Runtime 生效。
- **`distill_on_session_end` 为预留路径**：该入口（session 关闭时的全量蒸馏）当前**无调用方**，签名已接入 `compaction_prompt` 参数但未激活；实际生效的蒸馏路径为 `loop_session.rs` 的 tail 蒸馏（`compact_messages`）与 `loop_context.rs` 的压缩主路径（`compact_via_llm`）。后续激活 session-close 蒸馏时需从 `AgentCore` 传入同一字段。

---

## 4. 影响

### 4.1 代码改动清单

| 文件 | 改动 |
|---|---|
| `core/acowork-runtime/src/package/prompt_builder.rs` | `load_compaction_prompt`（区分缺失/读取失败）+ 主 prompt 按精确文件名排除 `summary.md` + 7 个单元测试 |
| `core/acowork-runtime/src/agent/agent_core.rs` | 新增 `compaction_prompt` 字段 + new 初始化 + Clone impl |
| `core/acowork-runtime/src/startup/agent_init.rs` | Phase A 加载 `compaction_prompt` 并存入 `AgentBootContext` |
| `core/acowork-runtime/src/startup/context.rs` | `AgentBootContext` 新增 `compaction_prompt: Option<String>` 字段 |
| `core/acowork-runtime/src/startup/session_init.rs` | Phase B 从 ctx 注入（不再直接读文件） |
| `core/acowork-runtime/src/cli.rs` | Standalone 分支从 ctx 注入（消除模式分裂） |
| `core/acowork-runtime/src/agent/loop_context.rs` | 压缩解析链改用 `compaction_prompt`，移除 `system_prompt_override` 借用 |
| `core/acowork-runtime/src/episode_distill.rs` | 三个入口新增 `compaction_prompt: Option<&str>` 参数 |
| `core/acowork-runtime/src/agent/loop_session.rs` | tail 蒸馏传 `core.compaction_prompt` |
| `examples/senior-engineer-agent/prompts/summary.md` | 示范：工程类 agent 的压缩规则（保留文件路径 / 技术决策 / 验证证据） |

### 4.2 行为变化

| 场景 | 之前 | 之后 |
|---|---|---|
| 包无 `summary.md` | 用内置默认 | 用内置默认（不变） |
| 包有 `summary.md` | （无此概念） | 压缩/蒸馏用包声明指令 |
| 用户设置 `system_prompt_override` | 同时改变主对话与压缩 | 只改变主对话（语义澄清） |

### 4.3 验证

- `cargo build -p acowork-runtime`：通过。
- `cargo test -p acowork-runtime --lib`：888 passed（含 7 个 prompt_builder 新测试；`test_run_falls_back_to_user_message_when_raw_is_none` 为 pre-existing 时序性 flaky，与本次改动无关）。
- `cargo clippy -p acowork-runtime --all-targets -- -D warnings`：通过，零警告。
- 集成测试：`mqtt_e2e_full` / `mqtt_e2e` / `conversation_session_tokens` / `builtin_tools_mutation` 全绿；`shell_risk_e2e` 失败为 pre-existing（与本次改动无关，见会话记录）。

---

## 5. 备选方案

### 5.1 `system_prompt_override` 继续作为压缩覆盖入口（否决）

保留"运行时配置覆盖压缩指令"的能力看起来更灵活，但混淆了"主对话覆盖"与"压缩指令"两个维度；且运行时配置属于用户调优，包声明属于作者意图，应各自独立。

### 5.2 summary.md 放包根目录（否决）

避免 `prompt_builder` 排除逻辑，但破坏"所有 prompt 文件集中在 `prompts/`"的组织原则，且与 `system.md` 不对称。一行排除逻辑（精确文件名匹配）换来组织一致性，值得。

### 5.3 压缩指令也用 frontmatter 元数据（否决）

为单文件引入 YAML frontmatter 解析是过度设计；`.agent` 包目前无此机制，文件名约定已足够表达意图。
