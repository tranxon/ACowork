/**
 * PM 领域类型 — 对齐 acowork-pm 服务端契约（core/acowork-pm/src/types.rs）。
 *
 * 约定：字段名与 JSON 直接对应（snake_case），不做 camelCase 转换，
 * 与 Desktop 现有 store（agentStore 等）保持一致，避免映射层漂移。
 */

// ── 枚举 ──────────────────────────────────────────────────────────────

/** 项目状态 */
export type ProjectStatus = "active" | "archived" | "completed";

/** 任务状态（看板列）— 对齐服务端 TaskStatus */
export type TaskStatus =
  | "pending" // 待处理（看板第 1 列）
  | "in_progress" // 进行中（Agent claim 进入）
  | "submitted" // 已提交待审核（Agent submit 进入）
  | "done" // 已完成（review 通过）
  | "rejected" // 已拒绝（review 未通过）
  | "cancelled"; // 已取消

/** 审核状态 */
export type ReviewStatus =
  | "not_required"
  | "pending"
  | "approved"
  | "rejected";

/** 任务类型 */
export type TaskType =
  | "task"
  | "bug"
  | "feature"
  | "chore"
  | "checkpoint"
  | "milestone";

/** 优先级 */
export type Priority = "low" | "normal" | "high" | "urgent";

/** 附件种类 */
export type AttachmentKind = "image" | "file";

/** 依赖种类 */
export type DependencyKind = "blocks" | "relates" | "duplicates";

// ── 实体 ──────────────────────────────────────────────────────────────

/** 项目元数据 */
export interface PmProject {
  id: string;
  title: string;
  description: string;
  status: ProjectStatus;
  created_by: string;
  created_at: string;
  updated_at: string;
  metadata: Record<string, unknown>;
}

/** 任务实体（不含派生字段） */
export interface PmTask {
  id: string;
  project_id: string;
  title: string;
  description: string;
  type: TaskType;
  status: TaskStatus;
  review_status: ReviewStatus;
  priority: Priority;
  assignee: string | null;
  due_at: string | null;
  depends_on: PmDependency[];
  attachments: PmAttachmentMeta[];
  result: PmTaskResult | null;
  created_by: string;
  created_at: string;
  updated_at: string;
  claimed_at: string | null;
  submitted_at: string | null;
}

/** 任务完整响应（含派生字段，服务端计算，不写回存储） */
export interface PmTaskResponse extends PmTask {
  is_blocked: boolean;
  blocked_by: string[];
  depth: number;
  parent_id: string | null;
}

/** 依赖声明 */
export interface PmDependency {
  task_id: string;
  kind: DependencyKind;
}

/** Agent 提交结果 */
export interface PmTaskResult {
  text: string;
  attachment_ids: string[];
  submitted_by: string;
  submitted_at: string;
}

/** 附件元数据（二进制文件在服务端 attachments/{id}/ 目录） */
export interface PmAttachmentMeta {
  id: string;
  filename: string;
  kind: AttachmentKind;
  content_type: string;
  size: number;
  sha256: string;
  storage_path: string;
  thumb_path: string | null;
  width: number | null;
  height: number | null;
  uploaded_by: string;
  uploaded_at: string;
}

// ── 请求体 ────────────────────────────────────────────────────────────

export interface CreateProjectInput {
  title: string;
  description?: string;
  metadata?: Record<string, unknown>;
}

export interface UpdateProjectInput {
  title?: string;
  description?: string;
  status?: ProjectStatus;
  metadata?: Record<string, unknown>;
}

export interface CreateTaskInput {
  title: string;
  description?: string;
  type?: TaskType;
  priority?: Priority;
  parent_task_id?: string | null;
  depends_on?: PmDependency[];
  attachment_ids?: string[];
}

export interface UpdateTaskInput {
  title?: string;
  description?: string;
  type?: TaskType;
  status?: TaskStatus;
  priority?: Priority;
  assignee?: string | null;
  due_at?: string | null;
  depends_on?: PmDependency[];
}

export interface ReparentInput {
  new_parent: string | null;
}

export interface ReviewInput {
  approved: boolean;
  /** 拒绝理由（可选，对齐服务端 ReviewTaskRequest._comment） */
  _comment?: string;
}

export interface SubmitInput {
  text: string;
  attachment_ids?: string[];
}

// ── 看板分组辅助 ───────────────────────────────────────────────────────

/** 看板列定义（对齐服务端 board_column + 审核语义） */
export const BOARD_COLUMNS: {
  status: TaskStatus;
  key: string;
  i18nKey: string;
}[] = [
  { status: "pending", key: "pending", i18nKey: "pm.board.pending" },
  { status: "in_progress", key: "in_progress", i18nKey: "pm.board.inProgress" },
  { status: "submitted", key: "submitted", i18nKey: "pm.board.submitted" },
  { status: "done", key: "done", i18nKey: "pm.board.done" },
];
