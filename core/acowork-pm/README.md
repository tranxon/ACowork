# acowork-pm

项目管理服务（Project & Task Management）。

## 能力

- **项目管理**：CRUD、状态流转（active / archived / completed）
- **任务管理**：CRUD、看板视图、生命周期（pending → in_progress → submitted → approved / rejected）
- **父子任务树**：物理目录嵌套即权威，零冗余字段
- **依赖图**：跨树/跨项目显式声明，运行时计算 blocked 状态
- **附件管理**：图片/文件，缩略图生成（`image-thumb` feature），大小/MIME 白名单
- **REST API**：axum 提供，Gateway 反向代理
- **MCP tools**：HTTP Server 暴露 `pm_*` 工具，Agent 可直接调用

## 存储形态

**目录树 + 物理嵌套即权威**。一个项目 = 一棵完整目录树：

```
{data}/acowork-pm/projects/{pid}/
├── project.json
└── tasks/
    ├── {root_task_id}/
    │   ├── task.json
    │   ├── attachments/{att_id}/{original.{ext},thumb.jpg}
    │   └── children/{child_task_id}/...
    └── {another_root_task_id}/
```

**核心不变量**：
- 任务恒为目录（不用文件形态），避免二态分支
- 子任务强制放在父任务的 `children/` 子目录下
- `children/` 按需创建（首个子任务时 `mkdir`）
- 删除任务 = `rm -rf` 目录树，原子级（子树+附件+索引一并清理）
- Reparent = `mv` 目录，0 文件写

## 模块结构

| 模块 | 职责 |
|------|------|
| `types` | 核心领域类型（Project / Task / Attachment 等） |
| `error` | 统一错误类型 `PmError` |
| `config` | `PmConfig` 配置加载 |
| `store::tree` | `TreeTaskStore` + `ProjectStore` / `TaskStore` trait |
| `store::index` | 二级内存索引（按 project / assignee / 依赖反向图） |
| `store::atomic` | 原子写 + 路径校验工具 |
| `api` | axum 路由 + handlers |
| `mcp` | MCP HTTP Server + `pm_*` 工具 manifest |
| `server` | HTTP server 启动入口 |

## 设计引用

| 文档 | 角色 |
|------|------|
| [`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) | 服务端设计（数据模型、API、MCP） |
| [`docs/design/zh/22-pm-desktop-ui.md`](../../docs/design/zh/22-pm-desktop-ui.md) | Desktop UX 设计 |
| [`docs/plan/zh/pm-dev-plan.md`](../../docs/plan/zh/pm-dev-plan.md) | 开发计划 |
| [`docs/adr/zh/ADR-061-pm-storage-tree.md`](../../docs/adr/zh/ADR-061-pm-storage-tree.md) | ADR-061 存储选型决策记录 |

## 构建与运行

```bash
# 编译
cargo build -p acowork-pm

# 编译（含图片缩略图生成）
cargo build -p acowork-pm --features image-thumb

# 测试
cargo test -p acowork-pm

# 运行（需先启动 Gateway 反向代理）
cargo run -p acowork-gateway  # 内置 PM API 路由
```

PM 服务**不独立运行**——通过 Gateway 暴露，由 Gateway 监督生命周期。

## 依赖关系

```text
acowork-pm ──uses──▶ acowork-core    (共享错误/health 类型)
                ──▶ acowork-gateway  (运行时由 Gateway 监督 + 反向代理)
                ──▶ (无 storage crate 依赖，目录树直接走 tokio::fs)
```

PM 服务**不**依赖 acowork-memory / acowork-grafeo —— 存储层走目录树，独立演进。