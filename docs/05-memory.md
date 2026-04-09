# Memory 分层架构

> 版本：v3.0 | 更新日期：2026-04-09

---

Memory 采用**本地优先（Local-First）**设计，以 Grafeo 图数据库为存储引擎，按归属和生命周期分为三层。每个 Agent 拥有完全独立的私有 Memory，不存在 Gateway 维护的公共数据库。跨 Agent 的数据共享通过 Intent 查询和系统 Agent 服务实现，而非共享存储。

```
┌────────────────────────────────────────────┐
│           第一层：工作记忆                   │
│  Agent Runtime 进程内存                     │
│  当前对话历史、上下文窗口                    │
│  生命周期：单次会话                         │
├────────────────────────────────────────────┤
│           第二层：私有记忆                   │
│  Agent 进程内嵌 Grafeo                      │
│  情景记忆 (HNSW 向量索引)                   │
│  语义记忆 (LPG 知识图谱)                    │
│  全文检索 (BM25 倒排索引)                   │
│  生命周期：数据持久化到磁盘，进程级隔离      │
├────────────────────────────────────────────┤
│           第三层：云端同步                   │
│  Memory Sync Service                       │
│  跨设备增量同步、冲突解决 (CRDT/LWW)        │
│  联邦共享（可选，需授权）                   │
│  生命周期：永久                             │
└────────────────────────────────────────────┘
```

**设计原则**：每个 Agent 是一个独立的"数字人"，保有对用户完全独立的个性化记忆。不同 Agent 对同一用户的认知可以不同——天气 Agent 记住你住北京，日历 Agent 记住你常去上海出差——这是符合仿生设计的自然结果。基础身份信息（姓名、语言等）的一致性通过系统 Agent 的 ContentProvider 服务保障，而非共享数据库。

## 1. 私有 Memory（Agent 内嵌 Grafeo）

每个 Agent Runtime 进程内嵌一个独立的 Grafeo 实例，数据文件存储在 Agent 工作区：

```
~/.local/share/agent-gateway/agents/<agent_id>/workspace/memory/private.grafeo
```

**核心能力：**
- **情景记忆（HNSW 向量索引）**：存储 Agent 与用户的交互片段，支持语义相似性检索。
- **语义记忆（LPG 知识图谱）**：存储从交互中提取的结构化知识（事实、偏好、关系）。
- **全文检索（BM25 倒排索引）**：支持对记忆内容的精确关键词搜索。
- **混合搜索**：融合向量检索 + 全文检索，通过 Reciprocal Rank Fusion (RRF) 排序。
- **Embedding 生成**：Grafeo 内置 ONNX Runtime，可在本地生成向量（如 all-MiniLM-L6-v2），无需外部 embedding 服务。

**隔离保证：**
- 数据隔离：每个 Agent 的 Grafeo 文件在独立工作区，沙箱层面文件系统隔离。
- 进程隔离：一个 Agent 的 Grafeo 崩溃不影响其他 Agent。
- OS 级保证：Agent A 的沙箱内无法访问 Agent B 的 Grafeo 文件。

## 2. 跨 Agent 知识共享

不同 Agent 之间不共享数据库，知识共享通过两种机制实现：

**路径 1：Intent 查询（推荐，主路径）**

Agent A 需要某项知识，直接向拥有该知识的 Agent B 发送 Intent 查询：

```json
{
  "type": "intent",
  "target": "com.example.weather",
  "action": "query_user_city",
  "params": {},
  "id": "msg-123"
}
```

天气 Agent 从自己的私有 Grafeo 查到结果并返回。这是最小权限方式——日历 Agent 只拿到了需要的那个事实。

**路径 2：系统 Agent ContentProvider（身份与偏好）**

用户身份和偏好等系统级信息由系统 Agent（`com.rollball.system`）统一管理，其他 Agent 通过 Intent 查询。详见 [07-system-agent.md](./07-system-agent.md)。

**路径 3：云端 Memory Sync 同步**

云端作为知识同步层，Agent 写入的知识可按规则广播给订阅了该信息的其他 Agent，各 Agent 的本地 Grafeo 各自更新。
