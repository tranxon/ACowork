//! `TreePmStore`：基于目录树的 PM 存储实现。
//!
//! ## 关键 API
//!
//! - `new(config)` —— 构造 + 创建根目录
//! - `rebuild_index()` —— 从文件系统 walkdir 重建内存索引（启动时 / 崩溃恢复）
//! - `PmStore` trait 方法 —— 全部走 `Arc<TreePmStore>`，handler 通过 axum `State<ApiState>` 访问
//!
//! ## 完整实现分阶段交付
//!
//! - **P0**：trait 定义 + 构造 + 索引骨架
//! - **P1（本文件）**：`rebuild_index` walkdir 实现 + 全部 CRUD / 子树 / reparent /
//!   状态机 / 附件实现
//! - **P3**：依赖图深层运算（`compute_blocked` 已实现；跨项目环检测增强）
//! - **P5+**：可选 SQLite 后端（替换 trait impl）
//!
//! ## 物理布局
//!
//! ```text
//! {data}/acowork-pm/projects/{pid}/
//! ├── project.json
//! └── tasks/
//!     ├── {root_task_id}/
//!     │   ├── task.json
//!     │   ├── attachments/{att_id}/{original.{ext},thumb.jpg}
//!     │   └── children/{child_task_id}/...
//!     └── {another_root_task_id}/
//! ```
//!
//! **核心不变量**（与 [`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §3.1 对齐）：
//! - 任务恒为目录（叶子任务也是目录）
//! - 子任务强制放在父任务 `children/` 子目录下
//! - `task.json` **不**冗余 `parent_id` / `subtask_ids`——物理嵌套即权威
//! - 删除 = `rm -rf` 目录树（子树 + 附件原子级清理）
//! - Reparent = `mv` 目录（0 文件写），DFS 防环 + 深度限制

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::RwLock;
use tokio::fs;

use crate::config::PmConfig;
use crate::error::{PmError, Result};
use crate::types::{
    AttachmentId, AttachmentMeta, CreateProject, CreateTask, DependencyKind, Priority, Project,
    ProjectId, ProjectStatus, ReparentTask, ReviewStatus, Task, TaskFilter, TaskId, TaskResult,
    TaskSort, TaskStatus, UpdateProject, UpdateTask,
};

use super::atomic::{
    atomic_write_json, check_not_reserved, read_json, rename_or_fallback, validate_id_format,
};
use super::index::{TaskEntry, TaskIndex};

// ────────────────────────────────────────────────────────────────────────────
// PmStore trait
// ────────────────────────────────────────────────────────────────────────────

/// PM 存储抽象。
///
/// 业务层（HTTP handlers / MCP tools）只依赖此 trait，**不**直接引用 [`TreePmStore`]。
/// 未来 P5+ 切 SQLite 仅需替换实现，业务代码 0 改动。
#[async_trait]
pub trait PmStore: Send + Sync {
    // ── Project operations ─────────────────────────────────────────

    async fn create_project(&self, input: CreateProject, created_by: &str) -> Result<Project>;
    async fn get_project(&self, id: &ProjectId) -> Result<Option<Project>>;
    async fn list_projects(&self) -> Result<Vec<Project>>;
    async fn update_project(&self, id: &ProjectId, input: UpdateProject) -> Result<Project>;
    async fn delete_project(&self, id: &ProjectId, cascade: bool) -> Result<()>;

    // ── Task operations ─────────────────────────────────────────────

    async fn create_task(&self, project_id: &ProjectId, input: CreateTask, created_by: &str)
        -> Result<Task>;
    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>>;
    async fn find_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>>;
    async fn update_task(&self, id: &TaskId, input: UpdateTask) -> Result<Task>;
    async fn delete_task(&self, id: &TaskId, cascade: bool) -> Result<()>;
    async fn reparent_task(&self, id: &TaskId, input: ReparentTask) -> Result<()>;
    /// 列出任务的直接子任务（按物理 `children/` 目录）。
    async fn list_children(&self, parent: &TaskId) -> Result<Vec<Task>>;

    // ── Lifecycle operations ────────────────────────────────────────

    async fn claim_task(&self, id: &TaskId, agent_id: &str) -> Result<Task>;
    async fn submit_task(
        &self,
        id: &TaskId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        agent_id: &str,
    ) -> Result<Task>;
    async fn review_task(&self, id: &TaskId, approved: bool, reviewer: &str) -> Result<Task>;

    // ── Dependency graph ────────────────────────────────────────────

    /// 计算阻塞本任务的依赖（kind=Blocks 且未完成）。
    /// 用于 API 响应附带 `blocked_by` / `is_blocked` 字段。
    async fn compute_blocked_by(&self, task_id: &TaskId) -> Result<Vec<TaskId>>;

    // ── Attachment operations ──────────────────────────────────────

    async fn list_attachments(&self, task_id: &TaskId) -> Result<Vec<AttachmentMeta>>;
    async fn get_attachment(&self, att_id: &AttachmentId) -> Result<Option<AttachmentMeta>>;
    async fn register_attachment(&self, task_id: &TaskId, meta: AttachmentMeta) -> Result<()>;
    async fn delete_attachment(&self, att_id: &AttachmentId) -> Result<()>;
}

// ────────────────────────────────────────────────────────────────────────────
// TreePmStore 结构 + 构造
// ────────────────────────────────────────────────────────────────────────────

/// 目录树存储实现。
///
/// 内部状态：
/// - `projects_dir` = `{data_dir}/projects`
/// - `trash_dir` = `{data_dir}/.trash`
/// - `index` = 内存二级索引（`Arc<RwLock<TaskIndex>>`）
#[derive(Debug)]
pub struct TreePmStore {
    pub(crate) config: PmConfig,
    pub(crate) projects_dir: PathBuf,
    pub(crate) trash_dir: PathBuf,
    pub(crate) index: Arc<RwLock<TaskIndex>>,
}

impl TreePmStore {
    /// 构造存储实例（**不**重建索引——调用方决定）。
    ///
    /// 创建根目录 `projects/` 和 `.trash/`。
    pub async fn new(config: PmConfig) -> Result<Self> {
        let projects_dir = config.projects_dir();
        let trash_dir = config.trash_dir();

        fs::create_dir_all(&projects_dir).await?;
        fs::create_dir_all(&trash_dir).await?;

        Ok(Self {
            config,
            projects_dir,
            trash_dir,
            index: Arc::new(RwLock::new(TaskIndex::new())),
        })
    }

    /// 从文件系统 walkdir 重建内存索引。
    ///
    /// 启动时由 [`crate::server::PmService::new`] 调用（如果配置开启）。
    /// 崩溃后调用幂等——从 `task.json` 全量重建，**无修复逻辑**（物理是权威，无冗余可漂移）。
    pub async fn rebuild_index(&self) -> Result<()> {
        let mut index = TaskIndex::new();
        let mut projects = fs::read_dir(&self.projects_dir).await?;

        while let Some(proj) = projects.next_entry().await? {
            if !proj.file_type().await?.is_dir() {
                continue;
            }
            let name = proj.file_name().to_string_lossy().into_owned();
            if !name.starts_with("p-") {
                continue;
            }
            let project_id = parse_project_id(&name)?;
            let tasks_dir = proj.path().join("tasks");
            if !fs::try_exists(&tasks_dir).await? {
                continue;
            }
            self.walk_tasks(&tasks_dir, &project_id, 0, &mut index)
                .await?;
        }

        let count = index.len();
        *self.index.write() = index;
        tracing::info!(count, "PM index rebuilt");
        Ok(())
    }

    /// 迭代扫描 `tasks/`（或 `children/`）目录，填充索引。
    ///
    /// 用显式栈做 DFS（避免递归 `async fn` 的 E0733 无限大小 future）；
    /// 项目 ID 整树不变，栈仅携带目录路径与深度。
    async fn walk_tasks(
        &self,
        root_dir: &Path,
        project_id: &ProjectId,
        root_depth: u8,
        index: &mut TaskIndex,
    ) -> Result<()> {
        let mut stack: Vec<(PathBuf, u8)> = vec![(root_dir.to_path_buf(), root_depth)];
        while let Some((dir, depth)) = stack.pop() {
            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                if !entry.file_type().await?.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with("t-") {
                    continue;
                }
                let task_id = parse_task_id(&name)?;
                let task_path = entry.path();
                let json_path = task_path.join("task.json");
                if !fs::try_exists(&json_path).await? {
                    tracing::warn!(task = %name, "task dir missing task.json, skipping");
                    continue;
                }
                let task: Task = read_json(&json_path).await?;

                index.insert(
                    task_id.clone(),
                    TaskEntry {
                        project_id: project_id.clone(),
                        status: task.status,
                        assignee: task.assignee.clone(),
                        depth,
                        dir_path: task_path.clone(),
                    },
                );
                for att in &task.attachments {
                    index.register_attachment(att.id.clone(), task_id.clone());
                }
                for dep in &task.depends_on {
                    if dep.kind == DependencyKind::Blocks {
                        index.add_dependency(task_id.clone(), dep.task_id.clone());
                    }
                }

                // 子任务目录入栈
                let children_dir = task_path.join("children");
                if fs::try_exists(&children_dir).await? {
                    stack.push((children_dir, depth + 1));
                }
            }
        }
        Ok(())
    }

    /// 迭代收集目录下所有直接子任务 ID（不含自身）。
    ///
    /// 显式栈 DFS，避免递归 `async fn` 的 E0733。
    async fn collect_subtree_ids(&self, root_dir: &Path, out: &mut Vec<TaskId>) -> Result<()> {
        let mut stack: Vec<PathBuf> = vec![root_dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let children = dir.join("children");
            if !fs::try_exists(&children).await? {
                continue;
            }
            let mut entries = fs::read_dir(&children).await?;
            while let Some(e) = entries.next_entry().await? {
                if !e.file_type().await?.is_dir() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with("t-") {
                    continue;
                }
                let tid = parse_task_id(&name)?;
                out.push(tid.clone());
                stack.push(e.path());
            }
        }
        Ok(())
    }

    /// 迭代收集被移动子树的索引更新项（**不持锁**）。
    ///
    /// 返回 `(task_id, 新 dir_path, 新 depth)` 列表；调用方在物理 mv 之后
    /// 一次性加锁批量应用，避免 parking_lot guard 跨 `.await`（非 Send）。
    /// 显式栈 DFS，避免递归 `async fn` 的 E0733。
    async fn collect_subtree_updates(
        &self,
        root_id: &TaskId,
        new_root_dir: &Path,
        new_root_depth: u8,
        out: &mut Vec<(TaskId, PathBuf, u8)>,
    ) -> Result<()> {
        let mut stack: Vec<(TaskId, PathBuf, u8)> =
            vec![(root_id.clone(), new_root_dir.to_path_buf(), new_root_depth)];
        while let Some((rid, rdir, rdepth)) = stack.pop() {
            out.push((rid.clone(), rdir.clone(), rdepth));
            let children = rdir.join("children");
            if !fs::try_exists(&children).await? {
                continue;
            }
            let mut entries = fs::read_dir(&children).await?;
            while let Some(e) = entries.next_entry().await? {
                if !e.file_type().await?.is_dir() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with("t-") {
                    continue;
                }
                let cid = parse_task_id(&name)?;
                stack.push((cid, e.path(), rdepth + 1));
            }
        }
        Ok(())
    }

    /// 优雅停机（当前无 pending write，保留接口供未来 flush）。
    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!("TreePmStore shutdown complete");
        Ok(())
    }

    /// 计算任务的物理路径（不含 `task.json` 后缀）——**仅根任务**。
    ///
    /// 嵌套任务必须经索引 `TaskEntry::dir_path` 定位（O(1)）。
    pub(crate) fn task_dir(&self, project_id: &ProjectId, task_id: &TaskId) -> PathBuf {
        self.projects_dir
            .join(project_id.as_str())
            .join("tasks")
            .join(task_id.as_str())
    }

    /// 计算项目目录。
    pub(crate) fn project_dir(&self, project_id: &ProjectId) -> PathBuf {
        self.projects_dir.join(project_id.as_str())
    }

    /// 计算项目 `project.json` 路径。
    pub(crate) fn project_json_path(&self, project_id: &ProjectId) -> PathBuf {
        self.project_dir(project_id).join("project.json")
    }

    /// 当前索引任务数（供健康检查）。
    pub fn indexed_task_count(&self) -> usize {
        self.index.read().len()
    }

    /// 内部辅助：从索引读取任务条目。
    pub fn index_entry(&self, task_id: &TaskId) -> Option<TaskEntry> {
        self.index.read().by_id.get(task_id).cloned()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// ID 解析辅助
// ────────────────────────────────────────────────────────────────────────────

fn parse_project_id(s: &str) -> Result<ProjectId> {
    validate_id_format(s, "p-", "project")?;
    ProjectId::parse(s).map_err(|_| PmError::InvalidId(format!("project: {}", s)))
}

fn parse_task_id(s: &str) -> Result<TaskId> {
    validate_id_format(s, "t-", "task")?;
    check_not_reserved(s)?;
    TaskId::parse(s).map_err(|_| PmError::InvalidId(format!("task: {}", s)))
}

/// 状态流转合法校验（对齐设计文档 §4 状态机图）。
fn validate_transition(from: TaskStatus, to: TaskStatus, task_id: &TaskId) -> Result<()> {
    let allowed = match from {
        TaskStatus::Pending => matches!(to, TaskStatus::InProgress | TaskStatus::Cancelled),
        TaskStatus::InProgress => matches!(
            to,
            TaskStatus::Pending | TaskStatus::Submitted | TaskStatus::Cancelled
        ),
        TaskStatus::Submitted => matches!(to, TaskStatus::Done | TaskStatus::Rejected),
        TaskStatus::Done => matches!(to, TaskStatus::InProgress),
        TaskStatus::Rejected => {
            matches!(to, TaskStatus::Pending | TaskStatus::InProgress | TaskStatus::Cancelled)
        }
        TaskStatus::Cancelled => false,
    };
    if !allowed {
        return Err(PmError::InvalidStateTransition {
            task_id: task_id.to_string(),
            from: from.board_column().to_string(),
            to: to.board_column().to_string(),
        });
    }
    Ok(())
}

fn priority_rank(p: Priority) -> u8 {
    match p {
        Priority::Urgent => 0,
        Priority::High => 1,
        Priority::Normal => 2,
        Priority::Low => 3,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// PmStore impl for TreePmStore
// ────────────────────────────────────────────────────────────────────────────

#[async_trait]
impl PmStore for TreePmStore {
    // ── Projects ───────────────────────────────────────────────────

    async fn create_project(
        &self,
        input: CreateProject,
        created_by: &str,
    ) -> Result<Project> {
        let id = ProjectId::generate();
        let now = Utc::now();
        let project = Project {
            id: id.clone(),
            title: input.title,
            description: input.description,
            status: ProjectStatus::Active,
            created_by: created_by.to_string(),
            created_at: now,
            updated_at: now,
            metadata: input.metadata,
        };
        let dir = self.project_dir(&id);
        fs::create_dir_all(dir.join("tasks")).await?;
        atomic_write_json(&self.project_json_path(&id), &project).await?;
        tracing::info!(project_id = %id, "created project");
        Ok(project)
    }

    async fn get_project(&self, id: &ProjectId) -> Result<Option<Project>> {
        let path = self.project_json_path(id);
        if !fs::try_exists(&path).await? {
            return Ok(None);
        }
        let project = read_json(&path).await?;
        Ok(Some(project))
    }

    async fn list_projects(&self) -> Result<Vec<Project>> {
        let mut result = Vec::new();
        let mut entries = fs::read_dir(&self.projects_dir).await?;
        while let Some(e) = entries.next_entry().await? {
            if !e.file_type().await?.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("p-") {
                continue;
            }
            let json_path = e.path().join("project.json");
            if !fs::try_exists(&json_path).await? {
                continue;
            }
            match read_json::<Project>(&json_path).await {
                Ok(p) => result.push(p),
                Err(err) => {
                    tracing::warn!(project = %name, error = %err, "skipping unreadable project.json");
                }
            }
        }
        result.sort_by_key(|p| p.created_at);
        Ok(result)
    }

    async fn update_project(&self, id: &ProjectId, input: UpdateProject) -> Result<Project> {
        let path = self.project_json_path(id);
        if !fs::try_exists(&path).await? {
            return Err(PmError::ProjectNotFound(id.to_string()));
        }
        let mut project: Project = read_json(&path).await?;
        if let Some(t) = input.title {
            project.title = t;
        }
        if let Some(d) = input.description {
            project.description = d;
        }
        if let Some(s) = input.status {
            project.status = s;
        }
        if let Some(m) = input.metadata {
            project.metadata = m;
        }
        project.updated_at = Utc::now();
        atomic_write_json(&path, &project).await?;
        Ok(project)
    }

    async fn delete_project(&self, id: &ProjectId, _cascade: bool) -> Result<()> {
        let dir = self.project_dir(id);
        if !fs::try_exists(&dir).await? {
            return Err(PmError::ProjectNotFound(id.to_string()));
        }
        let ts = Utc::now().format("%Y%m%dT%H%M%SZ");
        let trash_path = self.trash_dir.join(format!("{}.archived-{}", id, ts));
        rename_or_fallback(&dir, &trash_path).await?;
        tracing::info!(project_id = %id, trash = %trash_path.display(), "project archived");

        // 清理该项目全部任务索引（含附件 / 依赖引用）
        let ids: Vec<TaskId> = {
            let index = self.index.read();
            index
                .by_id
                .iter()
                .filter(|(_, e)| &e.project_id == id)
                .map(|(tid, _)| tid.clone())
                .collect()
        };
        let mut index = self.index.write();
        for tid in ids {
            index.remove(&tid);
            index.remove_dependent_refs(&tid);
            index.remove_blocker(&tid);
            index.remove_task_attachments(&tid);
        }
        Ok(())
    }

    // ── Tasks ──────────────────────────────────────────────────────

    async fn create_task(
        &self,
        project_id: &ProjectId,
        input: CreateTask,
        created_by: &str,
    ) -> Result<Task> {
        // 校验项目存在
        if !fs::try_exists(self.project_json_path(project_id)).await? {
            return Err(PmError::ProjectNotFound(project_id.to_string()));
        }

        let task_id = TaskId::generate();
        let now = Utc::now();

        // 确定目标目录 + 深度
        let (dir, depth) = if let Some(parent_id) = &input.parent_task_id {
            parse_task_id(parent_id.as_str())?;
            let parent = self
                .index
                .read()
                .by_id
                .get(parent_id)
                .cloned()
                .ok_or_else(|| PmError::TaskNotFound(parent_id.to_string()))?;
            if &parent.project_id != project_id {
                return Err(PmError::InvalidId(format!(
                    "parent {} is not in project {}",
                    parent_id, project_id
                )));
            }
            let new_depth = parent.depth + 1;
            if new_depth > self.config.max_task_depth {
                return Err(PmError::MaxDepthExceeded {
                    depth: new_depth,
                    max: self.config.max_task_depth,
                });
            }
            // 子任务数量限制（每任务直接子任务 ≤ 1000）
            let children_dir = parent.dir_path.join("children");
            if fs::try_exists(&children_dir).await? {
                let mut count = 0usize;
                let mut rd = fs::read_dir(&children_dir).await?;
                while let Some(_e) = rd.next_entry().await? {
                    count += 1;
                    if count >= 1000 {
                        return Err(PmError::TooManyChildren(parent_id.to_string()));
                    }
                }
            }
            fs::create_dir_all(&children_dir).await?;
            (children_dir.join(task_id.as_str()), new_depth)
        } else {
            let dir = self.task_dir(project_id, &task_id);
            fs::create_dir_all(&dir).await?;
            (dir, 0)
        };

        fs::create_dir_all(dir.join("attachments")).await?;

        // 依赖自环预检（浅层；深层环检测 P3）
        for dep in &input.depends_on {
            parse_task_id(dep.task_id.as_str())?;
            if dep.task_id == task_id {
                return Err(PmError::DependencyCycle(task_id.to_string()));
            }
        }

        // 人类创建 → 直接生效（NotRequired）；Agent 创建 → 待审核（Pending）
        let review_status = if created_by == "human" {
            ReviewStatus::NotRequired
        } else {
            ReviewStatus::Pending
        };

        let task = Task {
            id: task_id.clone(),
            project_id: project_id.clone(),
            title: input.title,
            description: input.description,
            task_type: input.task_type,
            status: TaskStatus::Pending,
            review_status,
            priority: input.priority,
            assignee: None,
            due_at: None,
            depends_on: input.depends_on,
            attachments: vec![],
            result: None,
            created_by: created_by.to_string(),
            created_at: now,
            updated_at: now,
            claimed_at: None,
            submitted_at: None,
        };
        atomic_write_json(&dir.join("task.json"), &task).await?;

        // 更新索引
        let mut index = self.index.write();
        index.insert(
            task_id.clone(),
            TaskEntry {
                project_id: project_id.clone(),
                status: task.status,
                assignee: task.assignee.clone(),
                depth,
                dir_path: dir,
            },
        );
        for dep in &task.depends_on {
            if dep.kind == DependencyKind::Blocks {
                index.add_dependency(task_id.clone(), dep.task_id.clone());
            }
        }
        tracing::info!(task_id = %task_id, project_id = %project_id, depth, "created task");
        Ok(task)
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>> {
        let entry = match self.index.read().by_id.get(id) {
            Some(e) => e.clone(),
            None => return Ok(None),
        };
        let path = entry.dir_path.join("task.json");
        if !fs::try_exists(&path).await? {
            return Ok(None);
        }
        let task = read_json(&path).await?;
        Ok(Some(task))
    }

    async fn find_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>> {
        let candidates: Vec<TaskId> = {
            let index = self.index.read();
            index
                .by_id
                .iter()
                .filter(|(_, e)| {
                    if let Some(pid) = &filter.project_id {
                        if &e.project_id != pid {
                            return false;
                        }
                    }
                    if let Some(st) = filter.status {
                        if e.status != st {
                            return false;
                        }
                    }
                    if let Some(a) = &filter.assignee {
                        if e.assignee.as_deref() != Some(a.as_str()) {
                            return false;
                        }
                    }
                    true
                })
                .map(|(id, _)| id.clone())
                .collect()
        };

        let mut result = Vec::new();
        for id in candidates {
            if filter.only_blocked {
                let blocked = self.compute_blocked_by(&id).await?;
                if blocked.is_empty() {
                    continue;
                }
            }
            if let Some(t) = self.get_task(&id).await? {
                result.push(t);
            }
        }

        match filter.sort.unwrap_or_default() {
            TaskSort::CreatedAt => result.sort_by_key(|t| t.created_at),
            TaskSort::UpdatedAt => result.sort_by_key(|t| t.updated_at),
            TaskSort::DueAt => {
                result.sort_by_key(|t| t.due_at.unwrap_or(chrono::DateTime::<Utc>::MAX_UTC))
            }
            TaskSort::Priority => result.sort_by_key(|t| priority_rank(t.priority)),
        }
        Ok(result)
    }

    async fn update_task(&self, id: &TaskId, input: UpdateTask) -> Result<Task> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        if let Some(t) = input.title {
            task.title = t;
        }
        if let Some(d) = input.description {
            task.description = d;
        }
        if let Some(t) = input.task_type {
            task.task_type = t;
        }
        if let Some(s) = input.status {
            validate_transition(task.status, s, id)?;
            task.status = s;
        }
        if let Some(p) = input.priority {
            task.priority = p;
        }
        if input.assignee.is_some() {
            task.assignee = input.assignee.clone().flatten();
        }
        if input.due_at.is_some() {
            task.due_at = input.due_at.clone().flatten();
        }
        if let Some(deps) = input.depends_on {
            for dep in &deps {
                parse_task_id(dep.task_id.as_str())?;
                if dep.task_id == *id {
                    return Err(PmError::DependencyCycle(id.to_string()));
                }
            }
            task.depends_on = deps;
        }
        task.updated_at = Utc::now();
        atomic_write_json(&path, &task).await?;

        // 重建索引条目（状态 / 责任人变化）+ 重建依赖图
        let mut index = self.index.write();
        index.remove(id);
        index.remove_dependent_refs(id);
        index.insert(
            id.clone(),
            TaskEntry {
                project_id: entry.project_id,
                status: task.status,
                assignee: task.assignee.clone(),
                depth: entry.depth,
                dir_path: entry.dir_path,
            },
        );
        for dep in &task.depends_on {
            if dep.kind == DependencyKind::Blocks {
                index.add_dependency(id.clone(), dep.task_id.clone());
            }
        }
        Ok(task)
    }

    async fn delete_task(&self, id: &TaskId, _cascade: bool) -> Result<()> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;

        // 收集子树任务（物理嵌套，级联删除）
        let mut to_remove = vec![id.clone()];
        self.collect_subtree_ids(&entry.dir_path, &mut to_remove).await?;

        // rm -rf 任务目录（子树 + 附件原子级清理）
        fs::remove_dir_all(&entry.dir_path).await?;

        let mut index = self.index.write();
        for tid in to_remove {
            index.remove(&tid);
            index.remove_dependent_refs(&tid);
            index.remove_blocker(&tid);
            index.remove_task_attachments(&tid);
        }
        tracing::info!(task_id = %id, "deleted task (with subtree)");
        Ok(())
    }

    async fn reparent_task(&self, id: &TaskId, input: ReparentTask) -> Result<()> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let old_dir = entry.dir_path.clone();

        // 计算目标目录 + 新深度
        let (target_dir, new_depth) = match &input.new_parent {
            None => {
                // 提升为项目根任务
                let dir = self.task_dir(&entry.project_id, id);
                fs::create_dir_all(dir.parent().expect("tasks dir")).await?;
                (dir, 0)
            }
            Some(parent_id) => {
                parse_task_id(parent_id.as_str())?;
                let parent = self
                    .index
                    .read()
                    .by_id
                    .get(parent_id)
                    .cloned()
                    .ok_or_else(|| PmError::TaskNotFound(parent_id.to_string()))?;
                if &parent.project_id != &entry.project_id {
                    return Err(PmError::InvalidId(format!(
                        "parent {} is not in project {}",
                        parent_id, entry.project_id
                    )));
                }
                // DFS 防环：目标父不能位于被移动子树内部
                if parent.dir_path.starts_with(&old_dir) {
                    return Err(PmError::CycleDetected {
                        task_id: id.to_string(),
                        parent_id: parent_id.to_string(),
                    });
                }
                let new_depth = parent.depth + 1;
                if new_depth > self.config.max_task_depth {
                    return Err(PmError::MaxDepthExceeded {
                        depth: new_depth,
                        max: self.config.max_task_depth,
                    });
                }
                let children = parent.dir_path.join("children");
                fs::create_dir_all(&children).await?;
                (children.join(id.as_str()), new_depth)
            }
        };

        if target_dir == old_dir {
            return Ok(());
        }

        // mv 目录（0 文件写）
        rename_or_fallback(&old_dir, &target_dir).await?;

        // 更新子树索引（dir_path + depth）
        //
        // 先收集更新项（**不持锁**，纯 fs 遍历），再一次性加锁批量应用——
        // 避免 parking_lot guard 跨 `.await`（非 Send）。
        let mut updates = Vec::new();
        self.collect_subtree_updates(id, &target_dir, new_depth, &mut updates)
            .await?;
        {
            let mut index = self.index.write();
            for (tid, dir, depth) in updates {
                if let Some(e) = index.by_id.get_mut(&tid) {
                    e.dir_path = dir;
                    e.depth = depth;
                }
            }
        }
        tracing::info!(task_id = %id, new_dir = %target_dir.display(), "reparented task");
        Ok(())
    }

    async fn list_children(&self, parent: &TaskId) -> Result<Vec<Task>> {
        let entry = self
            .index
            .read()
            .by_id
            .get(parent)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(parent.to_string()))?;
        let children = entry.dir_path.join("children");
        let mut result = Vec::new();
        if !fs::try_exists(&children).await? {
            return Ok(result);
        }
        let mut entries = fs::read_dir(&children).await?;
        while let Some(e) = entries.next_entry().await? {
            if !e.file_type().await?.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("t-") {
                continue;
            }
            let cid = parse_task_id(&name)?;
            if let Some(t) = self.get_task(&cid).await? {
                result.push(t);
            }
        }
        result.sort_by_key(|t| t.created_at);
        Ok(result)
    }

    // ── Lifecycle ──────────────────────────────────────────────────

    async fn claim_task(&self, id: &TaskId, _agent_id: &str) -> Result<Task> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        validate_transition(task.status, TaskStatus::InProgress, id)?;

        // 依赖未满足 → 409
        let blocked = self.compute_blocked_by(id).await?;
        if let Some(b) = blocked.first() {
            return Err(PmError::DependencyNotSatisfied {
                task_id: id.to_string(),
                blocker: b.to_string(),
            });
        }

        task.status = TaskStatus::InProgress;
        task.claimed_at = Some(Utc::now());
        task.updated_at = Utc::now();
        atomic_write_json(&path, &task).await?;
        self.refresh_entry(&entry, id, task.status, task.assignee.clone());
        Ok(task)
    }

    async fn submit_task(
        &self,
        id: &TaskId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        agent_id: &str,
    ) -> Result<Task> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        validate_transition(task.status, TaskStatus::Submitted, id)?;

        let now = Utc::now();
        task.status = TaskStatus::Submitted;
        task.submitted_at = Some(now);
        task.result = Some(TaskResult {
            text: text.to_string(),
            attachment_ids,
            submitted_by: agent_id.to_string(),
            submitted_at: now,
        });
        task.updated_at = now;
        atomic_write_json(&path, &task).await?;
        self.refresh_entry(&entry, id, task.status, task.assignee.clone());
        Ok(task)
    }

    async fn review_task(&self, id: &TaskId, approved: bool, _reviewer: &str) -> Result<Task> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        let to = if approved {
            TaskStatus::Done
        } else {
            TaskStatus::Rejected
        };
        validate_transition(task.status, to, id)?;

        task.status = to;
        task.review_status = if approved {
            ReviewStatus::Approved
        } else {
            ReviewStatus::Rejected
        };
        task.updated_at = Utc::now();
        atomic_write_json(&path, &task).await?;
        self.refresh_entry(&entry, id, task.status, task.assignee.clone());
        Ok(task)
    }

    // ── Dependency graph ───────────────────────────────────────────

    async fn compute_blocked_by(&self, task_id: &TaskId) -> Result<Vec<TaskId>> {
        let task = self
            .get_task(task_id)
            .await?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;

        let mut blocked = Vec::new();
        for dep in &task.depends_on {
            if dep.kind != DependencyKind::Blocks {
                continue;
            }
            let status = self.index.read().by_id.get(&dep.task_id).map(|e| e.status);
            match status {
                // 被依赖任务已完成 / 取消 → 不再阻塞
                Some(TaskStatus::Done) | Some(TaskStatus::Cancelled) => {}
                // 未完成或不存在（悬挂引用）→ 阻塞
                _ => blocked.push(dep.task_id.clone()),
            }
        }
        Ok(blocked)
    }

    // ── Attachments ────────────────────────────────────────────────

    async fn list_attachments(&self, task_id: &TaskId) -> Result<Vec<AttachmentMeta>> {
        let task = self
            .get_task(task_id)
            .await?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        Ok(task.attachments)
    }

    async fn get_attachment(&self, att_id: &AttachmentId) -> Result<Option<AttachmentMeta>> {
        let task_id = match self.index.read().by_attachment.get(att_id) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        let task = self
            .get_task(&task_id)
            .await?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        Ok(task.attachments.into_iter().find(|m| &m.id == att_id))
    }

    async fn register_attachment(&self, task_id: &TaskId, meta: AttachmentMeta) -> Result<()> {
        let entry = self
            .index
            .read()
            .by_id
            .get(task_id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        if task.attachments.len() >= self.config.max_attachments_per_task {
            return Err(PmError::TooManyAttachments(task_id.to_string()));
        }
        if task.attachments.iter().any(|m| m.id == meta.id) {
            return Err(PmError::Internal(format!(
                "attachment {} already registered",
                meta.id
            )));
        }

        task.attachments.push(meta.clone());
        task.updated_at = Utc::now();
        atomic_write_json(&path, &task).await?;
        self.index
            .write()
            .register_attachment(meta.id, task_id.clone());
        Ok(())
    }

    async fn delete_attachment(&self, att_id: &AttachmentId) -> Result<()> {
        let task_id = self
            .index
            .read()
            .by_attachment
            .get(att_id)
            .cloned()
            .ok_or_else(|| PmError::AttachmentNotFound(att_id.to_string()))?;
        let entry = self
            .index
            .read()
            .by_id
            .get(&task_id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        let meta = task
            .attachments
            .iter()
            .find(|m| &m.id == att_id)
            .cloned()
            .ok_or_else(|| PmError::AttachmentNotFound(att_id.to_string()))?;

        task.attachments.retain(|m| &m.id != att_id);
        task.updated_at = Utc::now();
        atomic_write_json(&path, &task).await?;

        // 清理物理文件（attachments/{att_id}/ 目录）
        if let Some(parent) = Path::new(&meta.storage_path).parent() {
            let dir = entry.dir_path.join(parent);
            let _ = fs::remove_dir_all(&dir).await;
        }

        self.index.write().unregister_attachment(att_id);
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 内部辅助：索引条目刷新（状态 / 责任人变化后重建 by_status / by_assignee）
// ────────────────────────────────────────────────────────────────────────────

impl TreePmStore {
    fn refresh_entry(
        &self,
        entry: &TaskEntry,
        id: &TaskId,
        status: TaskStatus,
        assignee: Option<String>,
    ) {
        let mut index = self.index.write();
        index.remove(id);
        index.insert(
            id.clone(),
            TaskEntry {
                project_id: entry.project_id.clone(),
                status,
                assignee,
                depth: entry.depth,
                dir_path: entry.dir_path.clone(),
            },
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 单元测试
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CreateTask, Priority, TaskType};
    use tempfile::tempdir;

    fn test_config() -> PmConfig {
        let dir = tempdir().unwrap();
        let mut cfg = PmConfig::default();
        cfg.data_dir = dir.path().to_path_buf();
        cfg.index_rebuild_on_start = false;
        cfg
    }

    fn create_task_input(title: &str) -> CreateTask {
        CreateTask {
            title: title.to_string(),
            description: String::new(),
            task_type: TaskType::Task,
            priority: Priority::Normal,
            parent_task_id: None,
            depends_on: vec![],
            attachment_ids: vec![],
        }
    }

    #[tokio::test]
    async fn project_crud_roundtrip() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "Demo".into(),
                    description: "d".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        assert_eq!(p.title, "Demo");
        assert_eq!(store.get_project(&p.id).await.unwrap().unwrap().id, p.id);
        assert_eq!(store.list_projects().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_root_and_child_task() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();

        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        assert_eq!(store.index_entry(&root.id).unwrap().depth, 0);

        let child = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(root.id.clone()),
                    ..create_task_input("child")
                },
                "human",
            )
            .await
            .unwrap();
        assert_eq!(store.index_entry(&child.id).unwrap().depth, 1);
        assert_eq!(store.list_children(&root.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_task_removes_subtree() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        let child = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(root.id.clone()),
                    ..create_task_input("child")
                },
                "human",
            )
            .await
            .unwrap();

        store.delete_task(&root.id, true).await.unwrap();
        assert!(store.get_task(&root.id).await.unwrap().is_none());
        assert!(store.get_task(&child.id).await.unwrap().is_none());
        assert_eq!(store.indexed_task_count(), 0);
    }

    #[tokio::test]
    async fn reparent_detects_cycle() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        let a = store
            .create_task(&p.id, create_task_input("a"), "human")
            .await
            .unwrap();
        let b = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(a.id.clone()),
                    ..create_task_input("b")
                },
                "human",
            )
            .await
            .unwrap();

        // 把 a 移到 b 之下 → 环
        let err = store
            .reparent_task(&a.id, ReparentTask { new_parent: Some(b.id.clone()) })
            .await
            .unwrap_err();
        assert!(matches!(err, PmError::CycleDetected { .. }));
    }

    #[tokio::test]
    async fn lifecycle_claim_submit_review() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        let t = store
            .create_task(&p.id, create_task_input("t"), "agent-x")
            .await
            .unwrap();
        assert_eq!(t.review_status, ReviewStatus::Pending);

        let claimed = store.claim_task(&t.id, "agent-x").await.unwrap();
        assert_eq!(claimed.status, TaskStatus::InProgress);

        let submitted = store
            .submit_task(&t.id, "done", vec![], "agent-x")
            .await
            .unwrap();
        assert_eq!(submitted.status, TaskStatus::Submitted);

        let reviewed = store.review_task(&t.id, true, "human").await.unwrap();
        assert_eq!(reviewed.status, TaskStatus::Done);
        assert_eq!(reviewed.review_status, ReviewStatus::Approved);
    }

    #[tokio::test]
    async fn rebuild_index_recovers_all_tasks() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        let a = store
            .create_task(&p.id, create_task_input("a"), "human")
            .await
            .unwrap();
        let b = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(a.id.clone()),
                    ..create_task_input("b")
                },
                "human",
            )
            .await
            .unwrap();

        // 模拟崩溃：重建索引
        store.rebuild_index().await.unwrap();
        assert_eq!(store.indexed_task_count(), 2);
        assert_eq!(store.get_task(&b.id).await.unwrap().unwrap().id, b.id);
        assert_eq!(store.index_entry(&b.id).unwrap().depth, 1);
    }

    // ── 负路径覆盖 (ADR-061 P1 补测) ─────────────────────────────
    //
    // 这组测试专门验证设计文档 §21 中的**契约不变式**在反例下也成立。
    // 与正路径(成功流)分开,失败时能直接定位是哪条契约被破坏。

    /// 负路径 1:依赖图可计算阻塞关系。
    ///
    /// - T1 依赖 T0(kind=Blocks)
    /// - T0 仍为 Pending(未完成)→ T1 **被阻塞**
    /// - T0 review(Done)→ T1 **不再被阻塞**
    ///
    /// 注:`submit_task` 不检查依赖(P1 软契约,行为由 find_tasks only_blocked 体现),
    /// 所以这里用 `compute_blocked_by` 验证阻塞检测正确。
    #[tokio::test]
    async fn dependency_blocks_until_done() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let t0 = store
            .create_task(&p.id, create_task_input("blocker"), "human")
            .await
            .unwrap();
        let t1 = store
            .create_task(
                &p.id,
                CreateTask {
                    depends_on: vec![crate::types::Dependency {
                        task_id: t0.id.clone(),
                        kind: DependencyKind::Blocks,
                    }],
                    ..create_task_input("blocked")
                },
                "human",
            )
            .await
            .unwrap();

        // T0 Pending → T1 被 T0 阻塞
        let blocked = store.compute_blocked_by(&t1.id).await.unwrap();
        assert_eq!(blocked, vec![t0.id.clone()], "T1 must be blocked by T0 while T0 is open");

        // 完成 T0 → T1 不再被阻塞
        let claimed = store.claim_task(&t0.id, "agent-x").await.unwrap();
        assert_eq!(claimed.status, TaskStatus::InProgress);
        let submitted = store
            .submit_task(&t0.id, "done", vec![], "agent-x")
            .await
            .unwrap();
        assert_eq!(submitted.status, TaskStatus::Submitted);
        let reviewed = store.review_task(&t0.id, true, "human").await.unwrap();
        assert_eq!(reviewed.status, TaskStatus::Done);

        let blocked_after = store.compute_blocked_by(&t1.id).await.unwrap();
        assert!(blocked_after.is_empty(), "T1 must NOT be blocked after T0 is Done");
    }

    /// 负路径 2:深度超限被拒绝(`max_task_depth` 配置生效)。
    ///
    /// 实现契约:`if new_depth > max_task_depth` 严格大于,所以
    /// - max=1 允许 depth 0(根)和 depth 1(子):1 > 1 = false
    /// - max=1 拒绝 depth 2(孙):2 > 1 = true
    /// 这里设 max=1,先确认 child(depth=1)合法,再确认 grandchild(depth=2)被拒。
    #[tokio::test]
    async fn create_task_rejects_over_max_depth() {
        let mut cfg = test_config();
        cfg.max_task_depth = 1;
        let store = TreePmStore::new(cfg).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();

        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        assert_eq!(store.index_entry(&root.id).unwrap().depth, 0);

        // child(depth=1) 在 max=1 下合法(1 > 1 = false)
        let child = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(root.id.clone()),
                    ..create_task_input("child")
                },
                "human",
            )
            .await
            .expect("child (depth=1) should be allowed when max=1");
        assert_eq!(store.index_entry(&child.id).unwrap().depth, 1);

        // grandchild(depth=2 > max=1) 必须被拒绝
        let err = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(child.id.clone()),
                    ..create_task_input("grandchild")
                },
                "human",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, PmError::MaxDepthExceeded { depth: 2, max: 1 }),
            "expected MaxDepthExceeded {{ depth: 2, max: 1 }}, got {err:?}"
        );
    }

    /// 负路径 3:UpdateTask.assignee = Some(None) 清空责任人,Some(Some(s)) 设置。
    ///
    /// 类型签名 `assignee: Option<Option<String>>` 表达三态:
    /// - `None` → 不修改
    /// - `Some(None)` → 清空为 null
    /// - `Some(Some(s))` → 设为 s
    ///
    /// 注:`claim_task` 在 P1 阶段**不**自动写 assignee 字段(只设 claimed_at),
    /// 所以这里用 update_task 直接设,覆盖三态契约即可。
    #[tokio::test]
    async fn update_task_can_clear_assignee() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let t = store
            .create_task(&p.id, create_task_input("t"), "agent-x")
            .await
            .unwrap();
        assert!(t.assignee.is_none(), "fresh task has no assignee");

        // Some(Some(s)) → 设置
        let set = store
            .update_task(
                &t.id,
                UpdateTask {
                    assignee: Some(Some("agent-x".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(set.assignee.as_deref(), Some("agent-x"));
        assert_eq!(store.index_entry(&t.id).unwrap().assignee.as_deref(), Some("agent-x"));

        // Some(None) → 清空
        let cleared = store
            .update_task(
                &t.id,
                UpdateTask {
                    assignee: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(cleared.assignee.is_none(), "assignee must be cleared to None");
        assert_eq!(store.index_entry(&t.id).unwrap().assignee, None);

        // None → 不修改(已为 None,验证 round-trip)
        let untouched = store
            .update_task(
                &t.id,
                UpdateTask {
                    title: Some("renamed".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(untouched.assignee.is_none(), "None assignee must NOT be touched when UpdateTask.assignee=None");
        assert_eq!(untouched.title, "renamed");
    }

    /// 负路径 4:delete_task 的 `cascade` 参数在 P1 阶段被忽略(总是级联删除子树)。
    ///
    /// **承认 P1 行为**:接口保留 `cascade` 参数(为 P2+ SQL back-end 预留语义),
    /// 当前实现总是 rm -rf 整个子树。这是一个 P1 阶段已知限制。
    ///
    /// 测试目的:**锁定当前行为**,未来切 SQL 时这个测试会失败,作为迁移提醒。
    #[tokio::test]
    async fn delete_task_ignores_cascade_flag_in_p1() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        let child = store
            .create_task(
                &p.id,
                CreateTask { parent_task_id: Some(root.id.clone()), ..create_task_input("child") },
                "human",
            )
            .await
            .unwrap();

        // P1 阶段 cascade=false 仍级联删除子树
        store.delete_task(&root.id, false).await.unwrap();

        assert!(store.get_task(&root.id).await.unwrap().is_none());
        assert!(
            store.get_task(&child.id).await.unwrap().is_none(),
            "child must be removed even with cascade=false in P1 (acknowledged limitation)"
        );
        assert_eq!(store.indexed_task_count(), 0);
    }

    /// 负路径 5:reparent_task(new_parent=None) 把子任务提升为项目根任务。
    #[tokio::test]
    async fn reparent_to_root_promotes_task() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        let child = store
            .create_task(
                &p.id,
                CreateTask { parent_task_id: Some(root.id.clone()), ..create_task_input("child") },
                "human",
            )
            .await
            .unwrap();
        assert_eq!(store.index_entry(&child.id).unwrap().depth, 1);

        // 提升为根
        store
            .reparent_task(
                &child.id,
                ReparentTask { new_parent: None },
            )
            .await
            .unwrap();

        // depth=0,不再属于任何 root 的子
        assert_eq!(store.index_entry(&child.id).unwrap().depth, 0);
        assert!(store.list_children(&root.id).await.unwrap().is_empty());
        // 任务本身仍存在
        assert!(store.get_task(&child.id).await.unwrap().is_some());
    }

    /// 负路径 6:find_tasks 多字段过滤(status + assignee + project_id 同时生效)。
    ///
    /// 构造 4 个任务,在不同 project / 不同 status / 不同 assignee 下,
    /// 验证所有过滤条件 AND 生效。
    #[tokio::test]
    async fn find_tasks_combined_filter() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p1 = store
            .create_project(
                CreateProject { title: "P1".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let p2 = store
            .create_project(
                CreateProject { title: "P2".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();

        let t_p1_inprog_x = store
            .create_task(&p1.id, create_task_input("p1-inprog-x"), "agent-x")
            .await
            .unwrap();
        // claim_task 在 P1 不写 assignee,显式 update_task 设之(覆盖三态契约)
        store.claim_task(&t_p1_inprog_x.id, "agent-x").await.unwrap();
        store
            .update_task(
                &t_p1_inprog_x.id,
                UpdateTask {
                    assignee: Some(Some("agent-x".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let t_p1_pending_y = store
            .create_task(&p1.id, create_task_input("p1-pending-y"), "agent-y")
            .await
            .unwrap();
        store
            .update_task(
                &t_p1_pending_y.id,
                UpdateTask {
                    assignee: Some(Some("agent-y".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let t_p2_inprog_x = store
            .create_task(&p2.id, create_task_input("p2-inprog-x"), "agent-x")
            .await
            .unwrap();
        store.claim_task(&t_p2_inprog_x.id, "agent-x").await.unwrap();
        store
            .update_task(
                &t_p2_inprog_x.id,
                UpdateTask {
                    assignee: Some(Some("agent-x".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // 同时过滤 P1 + InProgress + agent-x → 仅 t_p1_inprog_x
        let filter = TaskFilter {
            project_id: Some(p1.id.clone()),
            status: Some(TaskStatus::InProgress),
            assignee: Some("agent-x".to_string()),
            ..Default::default()
        };
        let results = store.find_tasks(&filter).await.unwrap();
        assert_eq!(results.len(), 1, "only P1+InProgress+agent-x matches");
        assert_eq!(results[0].id, t_p1_inprog_x.id);

        // 仅 project_id=P1 → 2 个
        let p1_only = store
            .find_tasks(&TaskFilter { project_id: Some(p1.id.clone()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(p1_only.len(), 2);

        // 无过滤 → 全部 3 个
        let all = store.find_tasks(&TaskFilter::default()).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    /// 负路径 7:delete_attachment 反向清理(task.json + 索引 + 物理文件)。
    ///
    /// 验证三个清理动作都发生:
    /// 1. `task.attachments` 数组移除该 attachment
    /// 2. `index.by_attachment` 反注册
    /// 3. `{task_dir}/attachments/{att_id}/` 物理目录被删除
    #[tokio::test]
    async fn delete_attachment_cleans_index_and_disk() {
        use crate::types::{AttachmentId, AttachmentKind, AttachmentMeta};
        use std::path::Path;
        use tempfile::tempdir;

        // 用具名临时目录(便于断言物理路径)
        let dir = tempdir().unwrap();
        let mut cfg = PmConfig::default();
        cfg.data_dir = dir.path().to_path_buf();
        cfg.index_rebuild_on_start = false;
        let store = TreePmStore::new(cfg).await.unwrap();

        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let t = store
            .create_task(&p.id, create_task_input("t"), "human")
            .await
            .unwrap();

        // 构造附件元数据(storage_path 相对 task_dir)
        let att_id = AttachmentId::parse("att-12345678").unwrap();
        let meta = AttachmentMeta {
            id: att_id.clone(),
            filename: "test.png".into(),
            kind: AttachmentKind::Image,
            content_type: "image/png".into(),
            size: 1024,
            sha256: "deadbeef".into(),
            storage_path: format!("attachments/{att_id}/original.png"),
            thumb_path: None,
            width: None,
            height: None,
            uploaded_by: "human".into(),
            uploaded_at: chrono::Utc::now(),
        };

        store.register_attachment(&t.id, meta.clone()).await.unwrap();

        // 准备物理文件:写一个 dummy 文件到 attachments/{att_id}/original.png
        let entry = store.index_entry(&t.id).unwrap();
        let att_dir = entry.dir_path.join("attachments").join(att_id.as_str());
        std::fs::create_dir_all(&att_dir).unwrap();
        let file_path = att_dir.join("original.png");
        std::fs::write(&file_path, b"fake-png-bytes").unwrap();
        assert!(file_path.exists(), "precondition: attachment file must exist on disk");

        // 反向索引已注册
        assert!(store.get_attachment(&att_id).await.unwrap().is_some());

        // 执行删除
        store.delete_attachment(&att_id).await.unwrap();

        // 1) task.json 不再含此 attachment
        let task_after = store.get_task(&t.id).await.unwrap().unwrap();
        assert!(
            task_after.attachments.iter().all(|a| a.id != att_id),
            "task.attachments must no longer contain {att_id}"
        );

        // 2) 反向索引已清
        assert!(
            store.get_attachment(&att_id).await.unwrap().is_none(),
            "by_attachment index must no longer resolve {att_id}"
        );

        // 3) 物理目录被删除
        assert!(
            !att_dir.exists(),
            "physical attachment directory must be removed, but still exists at {}",
            att_dir.display()
        );
        // 验证父目录仍存在(只删除 attachments/{att_id}/ 子目录,不动 attachments/)
        let parent_attachments = entry.dir_path.join("attachments");
        assert!(parent_attachments.exists(), "attachments/ parent must remain");
        // Path::new(&meta.storage_path).parent() = "attachments/{att_id}",删除范围正确
        let _ = Path::new(&meta.storage_path); // 仅为路径 API 编译检查
    }
}
