/**
 * PM REST API 客户端 — 对齐 acowork-pm 服务端（core/acowork-pm/src/api）。
 *
 * 路径前缀：`{gw}/api/pm/...`（Gateway 反代挂载，见 acowork-gateway/src/http/pm_api.rs）。
 * 错误格式：`{"error": {"code": "...", "message": "..."}}`（见 acowork-pm/src/error.rs）。
 * 人类操作需带 `X-Actor` 头（服务端 created_by / 审核者来源）。
 */

import { getGatewayUrl } from "./config";
import type {
  CreateProjectInput,
  CreateTaskInput,
  PmAttachmentMeta,
  PmProject,
  PmTask,
  PmTaskResponse,
  ReparentInput,
  ReviewInput,
  UpdateProjectInput,
  UpdateTaskInput,
} from "./pm-types";

/** 当前人类操作者标识（服务端约定：human 或 agent:xxx） */
const HUMAN_ACTOR = "human";

export class PmApiError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "PmApiError";
    this.code = code;
    this.status = status;
  }
}

async function request<T>(
  path: string,
  init?: RequestInit & { actor?: string },
): Promise<T> {
  const { actor = HUMAN_ACTOR, ...rest } = init ?? {};
  const headers: Record<string, string> = {
    "X-Actor": actor,
    ...(rest.headers as Record<string, string> | undefined),
  };
  if (rest.body && !(rest.body instanceof FormData)) {
    headers["Content-Type"] = "application/json";
  }

  const res = await fetch(`${getGatewayUrl()}/api/pm${path}`, {
    ...rest,
    headers,
  });

  if (!res.ok) {
    let code = "http_error";
    let message = `HTTP ${res.status}`;
    try {
      const body = (await res.json()) as {
        error?: { code?: string; message?: string };
      };
      code = body.error?.code ?? code;
      message = body.error?.message ?? message;
    } catch {
      // 非 JSON 响应体，保留默认
    }
    throw new PmApiError(res.status, code, message);
  }

  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

// ── Projects ──────────────────────────────────────────────────────────

export function listProjects() {
  return request<PmProject[]>("/projects");
}

export function createProject(input: CreateProjectInput) {
  return request<PmProject>("/projects", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

export function getProject(pid: string) {
  return request<PmProject>(`/projects/${encodeURIComponent(pid)}`);
}

export function updateProject(pid: string, input: UpdateProjectInput) {
  return request<PmProject>(`/projects/${encodeURIComponent(pid)}`, {
    method: "PATCH",
    body: JSON.stringify(input),
  });
}

export function deleteProject(pid: string) {
  return request<void>(`/projects/${encodeURIComponent(pid)}`, {
    method: "DELETE",
  });
}

// ── Tasks ─────────────────────────────────────────────────────────────

/** 项目任务列表 — 返回 TaskResponse[]（含 parent_id/depth/is_blocked 派生字段） */
export function listProjectTasks(pid: string) {
  return request<PmTaskResponse[]>(`/projects/${encodeURIComponent(pid)}/tasks`);
}

export function createTask(pid: string, input: CreateTaskInput) {
  return request<PmTask>(`/projects/${encodeURIComponent(pid)}/tasks`, {
    method: "POST",
    body: JSON.stringify(input),
  });
}

export function getTask(tid: string) {
  return request<PmTaskResponse>(`/tasks/${encodeURIComponent(tid)}`);
}

export function updateTask(tid: string, input: UpdateTaskInput) {
  return request<PmTask>(`/tasks/${encodeURIComponent(tid)}`, {
    method: "PATCH",
    body: JSON.stringify(input),
  });
}

export function deleteTask(tid: string, opts?: { cascade?: boolean }) {
  const q = opts?.cascade ? "?cascade=true" : "";
  return request<void>(`/tasks/${encodeURIComponent(tid)}${q}`, {
    method: "DELETE",
  });
}

/** 移动任务到新父下（new_parent=null 提升为根任务） */
export function reparentTask(tid: string, newParent: string | null) {
  return request<void>(`/tasks/${encodeURIComponent(tid)}/parent`, {
    method: "PATCH",
    body: JSON.stringify({ new_parent: newParent } satisfies ReparentInput),
  });
}

/** 人类审核（submitted → done / rejected）；comment 为拒绝理由（可选） */
export function reviewTask(tid: string, approved: boolean, comment?: string) {
  const body: ReviewInput = { approved };
  if (comment) body._comment = comment;
  return request<PmTask>(`/tasks/${encodeURIComponent(tid)}/review`, {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** 追加备注（服务端 P3 预留，当前 404 则忽略） */
export function addNote(tid: string, text: string) {
  return request<void>(`/tasks/${encodeURIComponent(tid)}/notes`, {
    method: "POST",
    body: JSON.stringify({ text }),
  });
}

// ── Attachments ───────────────────────────────────────────────────────

export function listTaskAttachments(tid: string) {
  return request<PmAttachmentMeta[]>(`/tasks/${encodeURIComponent(tid)}/attachments`);
}

export function uploadAttachment(tid: string, file: File, actor?: string) {
  const form = new FormData();
  form.append("file", file);
  return request<PmAttachmentMeta>(
    `/tasks/${encodeURIComponent(tid)}/attachments`,
    { method: "POST", body: form, actor },
  );
}

export function deleteAttachment(aid: string) {
  return request<void>(`/attachments/${encodeURIComponent(aid)}`, {
    method: "DELETE",
  });
}

/** 附件下载/预览 URL（thumbnail 仅图片） */
export function attachmentUrl(aid: string, opts?: { download?: boolean; thumb?: boolean }) {
  const q = new URLSearchParams();
  if (opts?.download) q.set("download", "1");
  if (opts?.thumb) q.set("thumb", "1");
  const qs = q.toString();
  return `${getGatewayUrl()}/api/pm/attachments/${encodeURIComponent(aid)}${qs ? `?${qs}` : ""}`;
}
