# 记忆写入入口决策摘要（Memory Write-Entrypoint Decisions）

> 本文档是代码注释的权威引用；完整分析报告见本地归档
> `docs/_internal/archive/review/memory-write-entrypoints-garbage-analysis.md`
> （gitignored，不入库）。

## 核心决策

> **低质量的数据还不如没有数据。** 只有确信有质量的数据才能写入长期记忆。
> 自动化、无价值闸门的写入通道一律不建；事件流（日志、工具失败、构建结果）
> 不进入长期记忆；所有写入必须回答「这条信息未来会被检索吗？是否具备复用价值？」；
> **死代码不保留**——无调用者的写入通道直接删除，避免日后被误用。

## 已废弃通道（本变更删除）

| # | 通道 | 写入目标 | 废弃原因 |
|---|------|---------|---------|
| B | `record_tool_failures` / `record_procedural_from_failure` | Procedural（fail_count） | 工具失败是运行时异常/待修 bug，非长期记忆；首行 80 字截断无复用价值 |
| B2 | `run_self_evaluation` | Autobiographical Limitation（批量） | 统计口径残缺（success_count 恒 0）→ 系统性假阳性；能力边界非工具计数器可推导 |
| G | `record_turn` / `ConversationRecord`（死代码） | Episodic | 无调用者；留档接口有被误用风险 |
| H | grafeo `auto_generate_limitation_nodes` | Autobiographical Limitation（批量） | 与 B2 同一逻辑的重复实现 |

## 有效写入入口

| # | 入口 | 写入目标 | 触发方式 |
|---|------|---------|---------|
| A | `memory_store` 工具 | Knowledge / Procedural / Autobiographical | LLM 主动调用 |
| C | 会话蒸馏 `write_summary_to_provider` | Episodic + Knowledge triples | 自动 / compaction |
| D | manifest 引导 | Autobiographical Identity/Capability | 启动时 |
| E | HTTP 管理 API | 任意节点 | 外部客户端 |
| F | consolidation 后台流水线 | Generalized / Resolved Conflict 节点 | 自动 / idle 或累计阈值 |

## 影响面

- `core/acowork-memory/src/manager.rs`：删除 `record` / `record_turn` /
  `record_tool_failures` / `record_procedural_from_failure` / `run_self_evaluation`，
  `ConversationRecord` 类型移出公共 re-export。
- `core/acowork-runtime/src/agent/loop_memory.rs`：删除 `record_tool_failures_to_memory`；
  `retrieve_and_inject_memories` 不再返回 node IDs（原消费者已删）。
- `core/acowork-runtime/src/agent/loop_.rs`：删除工具失败记录调用与
  `retrieved_memory_ids` 传递链；`execute_single_iteration` 移除未使用参数。
- `core/acowork-grafeo/src/consolidation/offline.rs`：删除
  `auto_generate_limitation_nodes`（步骤 7），步骤重编号。
- `core/acowork-runtime/src/agent/session_state.rs`：删除只增不读的 `turn_counter`。
