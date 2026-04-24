# Memory Zone 与 NodeType 概念区分

> 更新日期：2026-04-24
> 问题：Zone 和 NodeType 概念混淆
> 状态：已解决（文档更新 + 代码注释）

## 问题描述

在 Phase 1-3 的设计中，`MemoryNode.zone` 字段被定义但未使用，而实际的节点类型区分通过 Grafeo LPG Label 系统实现。这导致了两个问题：

1. **设计层面的概念冲突**：zone 和 NodeType 都在做"分类"，但维度不同
2. **代码层面的不一致**：`MemoryStore::list_by_zone()` 方法已定义但 GrafeoStore 未实现

## 解决方案

### 1. 明确概念区分

| 维度 | NodeType（节点类型） | Zone（业务分区） |
|------|---------------------|-----------------|
| **问题** | "这是什么类型的记忆？" | "这个记忆属于哪个业务场景？" |
| **分类依据** | 认知功能分层 | 业务场景分区 |
| **实现方式** | Grafeo LPG Label | Node Property（Phase 4+） |
| **示例值** | Episodic, Knowledge, Procedural, Autobiographical | work, personal, system |
| **当前状态** | ✅ **已实现**（Phase 1-3） | ⚠️ **暂缓**（Phase 4+） |

### 2. 正交关系示例

```
一个 KnowledgeNode 可以同时属于：
  - NodeType: Knowledge（认知功能：语义记忆）
  - Zone: work（业务场景：工作相关）
  - 示例："用户的项目经理是王五" → Knowledge + work

一个 Episodic 可以同时属于：
  - NodeType: Episodic（认知功能：经历层）
  - Zone: personal（业务场景：个人生活）
  - 示例："用户提到周末去爬山" → Episodic + personal
```

### 3. 文档更新

更新了以下文档，明确区分两个概念：

- **[docs/05-memory.md](./05-memory.md)** §8
  - §8.1 节点类型（NodeType）— 认知功能分类
  - §8.2 Zone 概念 — 业务场景分区（暂缓实现）

- **[docs/module-design/04-grafeo.md](./module-design/04-grafeo.md)** §8
  - §8.1 节点类型（NodeType）— 认知功能分层
  - §8.2 Zone 概念 — 业务场景分区（暂缓实现）

### 4. 代码注释更新

在 `rollball-core/src/memory/traits.rs` 中添加了明确的注释：

```rust
/// Memory node with metadata
///
/// ⚠️ NOTE: The `zone` field is defined but NOT currently used in Phase 1-3.
/// Zone functionality is deferred to Phase 4+. Currently all nodes belong to
/// the `default` zone. See docs/05-memory.md §8.2 for details.
pub struct MemoryNode {
    pub id: String,
    pub content: String,
    pub metadata: Value,
    /// Business scenario zone (e.g., "work", "personal", "system").
    /// ⚠️ UNUSED in Phase 1-3. Reserved for Phase 4+.
    pub zone: String,
    pub privacy_level: PrivacyLevel,
}
```

## 设计决策

### 为什么暂缓 Zone 实现？

1. **Phase 1-3 聚焦认知分层架构**：当前优先级是建立完整的三层记忆架构（瞬态/经历/沉淀），业务分区需求尚未明确
2. **避免架构复杂度**：过早引入 zone 会增加检索和存储的复杂度
3. **按需启用**：Phase 4+ 根据实际使用场景再决定是否启用 zone 功能

### Zone 的未来实现方案（Phase 4+）

如果后续需要启用 zone 功能，建议方案：

1. **作为 Node Property 存储**（而非独立 Label）
2. **在各节点结构体中增加 `zone: String` 字段**
3. **检索时支持 zone 过滤**：
   ```rust
   let results = store.hybrid_search(&MemoryQuery {
       filters: MemoryFilters {
           zone: Some("work"),  // 只检索工作相关记忆
           ..Default::default()
       },
       ..Default::default()
   })?;
   ```

## 验收标准

- [x] 文档明确区分 NodeType 和 Zone 的概念
- [x] 代码注释说明 zone 字段暂未使用
- [x] 设计师和开发者不会再混淆两个概念
- [ ] Phase 4+ 时根据实际需求决定是否实现 zone 功能

## 参考资料

- [docs/05-memory.md](./05-memory.md) - Memory 仿生分层架构
- [docs/module-design/04-grafeo.md](./module-design/04-grafeo.md) - Grafeo 存储引擎设计
- [core/rollball-core/src/memory/traits.rs](../core/rollball-core/src/memory/traits.rs) - MemoryStore trait 定义
- [core/rollball-grafeo/src/types.rs](../core/rollball-grafeo/src/types.rs) - LPG 节点类型定义
