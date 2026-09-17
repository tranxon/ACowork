/**
 * TaskEditDialog — 创建 / 编辑任务对话框（T2-7）。
 *
 * 对齐 UX 设计 §3.5：
 * - 模态对话框，宽 640px
 * - 字段顺序：标题 → 类型/优先级 → 描述 → 指派/截止 → 父任务/依赖
 * - 仅"标题"必填，其余字段均可后补
 * - 父任务下拉：同项目内可选，含"无"（顶层任务）
 * - 指派下拉：仅列出该项目成员（联动指派），含"未指派"；无成员时提示先添加
 * - 依赖：简单多选（同项目任务）
 *
 * 服务端契约（T2-0 记录的偏差）：
 * - CreateTask 不含 assignee/due_at → 创建成功后用 updateTask 补充
 * - 附件上传服务端后补（T2-10），本期表单不含附件区
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { createTask, updateTask } from "../../lib/pm-api";
import { useAgentStore } from "../../stores/agentStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { useTranslation } from "../../i18n/useTranslation";
import { Dropdown } from "../../components/common/Dropdown";
import { StyledInput, StyledTextarea } from "../../components/common/StyledInput";
import { showToast } from "../../components/common/ToastProvider";
import type { PmTaskResponse, Priority, TaskType } from "../../lib/pm-types";

export interface TaskEditDialogProps {
  mode: "create" | "edit";
  projectId: string;
  /** 编辑模式：初始任务 */
  initial?: PmTaskResponse | null;
  /** 创建模式：父任务 id（如从 Drawer 添加子任务） */
  defaultParentId?: string | null;
  onClose: () => void;
  onSaved?: () => void;
}

const TASK_TYPES: TaskType[] = ["task", "feature", "bug", "chore", "checkpoint", "milestone"];
const PRIORITIES: Priority[] = ["low", "normal", "high", "urgent"];

const TYPE_I18N: Record<TaskType, string> = {
  task: "pm.taskType.task",
  feature: "pm.taskType.feature",
  bug: "pm.taskType.bug",
  chore: "pm.taskType.chore",
  checkpoint: "pm.taskType.checkpoint",
  milestone: "pm.taskType.milestone",
};

const PRIORITY_I18N: Record<Priority, string> = {
  low: "pm.priority.low",
  normal: "pm.priority.normal",
  high: "pm.priority.high",
  urgent: "pm.priority.urgent",
};

export function TaskEditDialog({
  mode,
  projectId,
  initial,
  defaultParentId = null,
  onClose,
  onSaved,
}: TaskEditDialogProps) {
  const { t } = useTranslation();
  const agents = useAgentStore((s) => s.agents);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const boardTasks = usePmBoardStore((s) => s.tasks);
  const reload = usePmBoardStore((s) => s.reload);

  // 项目成员（instance_id 集合）— 联动指派：assignee 必须是项目成员。
  // selector 返回数组元素引用（find），非新建对象，符合 store 契约。
  const project = usePmProjectStore((s) => s.projects.find((p) => p.id === projectId));
  const memberIds = useMemo(
    () => new Set((project?.members ?? []).map((m) => m.instance_id)),
    [project?.members],
  );

  const [title, setTitle] = useState(initial?.title ?? "");
  const [type, setType] = useState<TaskType>(initial?.type ?? "task");
  const [priority, setPriority] = useState<Priority>(initial?.priority ?? "normal");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [assignee, setAssignee] = useState<string>(initial?.assignee ?? "");
  const [dueAt, setDueAt] = useState<string>(initial?.due_at?.slice(0, 10) ?? "");
  const [parentId, setParentId] = useState<string>(initial?.parent_id ?? defaultParentId ?? "");
  const [dependsOn, setDependsOn] = useState<string[]>(
    initial?.depends_on?.map((d) => d.task_id) ?? [],
  );
  const [saving, setSaving] = useState(false);

  // 打开时确保 agent 列表已加载（用于指派下拉）
  useEffect(() => {
    if (Object.keys(agents).length === 0) void fetchAgents();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ESC 关闭（保存中忽略）
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !saving) onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose, saving]);

  // 父任务候选：同项目内、排除自身（编辑时）与其后代
  const parentCandidates = useMemo(() => {
    if (mode === "edit" && initial) {
      return boardTasks.filter(
        (task) => task.id !== initial.id && task.parent_id !== initial.id,
      );
    }
    return boardTasks;
  }, [boardTasks, mode, initial]);

  // ADR-073: dropdown value 是 agent_instance_id（UUID）— 与 task.assignee 语义一致。
  // label 用 display_name 解析（meta.display_name ?? meta.name ?? agent_id）。
  // 联动指派：仅列出项目成员（memberIds），服务端强校验 assignee ∈ 成员 ∪ "human"。
  const agentOptions = useMemo(
    () => buildAgentOptions(Object.values(agents), memberIds),
    [agents, memberIds],
  );

  const parentOptions = useMemo(
    () => [
      { value: "", label: t("pm.task.noParent") },
      ...parentCandidates.map((task) => ({
        value: task.id,
        label: task.title,
      })),
    ],
    [parentCandidates, t],
  );

  const depOptions = useMemo(
    () =>
      boardTasks
        .filter((task) => task.id !== initial?.id)
        .map((task) => ({
          value: task.id,
          label: task.title,
        })),
    [boardTasks, initial],
  );

  const handleSave = useCallback(async () => {
    const trimmed = title.trim();
    if (!trimmed) {
      showToast({ type: "warning", message: t("pm.taskTitleRequired") });
      return;
    }
    setSaving(true);
    try {
      if (mode === "create") {
        const created = await createTask(projectId, {
          title: trimmed,
          description: description.trim() || undefined,
          type,
          priority,
          parent_task_id: parentId || null,
          depends_on: dependsOn.map((id) => ({ task_id: id, kind: "relates" })),
        });
        // CreateTask 不含 assignee/due_at → PATCH 补充
        if (assignee || dueAt) {
          await updateTask(created.id, {
            assignee: assignee || null,
            due_at: dueAt ? new Date(dueAt).toISOString() : null,
          });
        }
      } else if (initial) {
        await updateTask(initial.id, {
          title: trimmed,
          description: description.trim() || undefined,
          type,
          priority,
          assignee: assignee || null,
          due_at: dueAt ? new Date(dueAt).toISOString() : null,
          depends_on: dependsOn.map((id) => ({ task_id: id, kind: "relates" })),
        });
        // 父任务变化 → reparent
        if (parentId !== initial.parent_id) {
          const { reparentTask } = await import("../../lib/pm-api");
          await reparentTask(initial.id, parentId || null);
        }
      }
      await reload();
      showToast({ type: "success", message: t("pm.task.taskSaved") });
      onSaved?.();
      onClose();
    } catch (e) {
      showToast({ type: "error", message: `${t("pm.task.taskSaveFailed")}: ${e instanceof Error ? e.message : e}` });
    } finally {
      setSaving(false);
    }
  }, [title, description, type, priority, assignee, dueAt, parentId, dependsOn, mode, initial, projectId, reload, onClose, onSaved, t]);

  const toggleDep = (id: string) => {
    setDependsOn((prev) =>
      prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id],
    );
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />
      <div
        className="relative z-10 flex max-h-[90vh] w-full max-w-[640px] flex-col overflow-hidden rounded-md border border-zinc-200 bg-modal-surface shadow-xl dark:border-zinc-700"
        role="dialog"
        aria-modal="true"
        aria-labelledby="pm-edit-task-title"
      >
        {/* 头部 */}
        <header className="flex shrink-0 items-center justify-between border-b border-zinc-200 px-5 py-3 dark:border-zinc-700">
          <h3 id="pm-edit-task-title" className="text-sm font-semibold">
            {mode === "create" ? t("pm.newTask") : t("pm.task.edit")}
          </h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-1.5 py-0.5 text-sm text-text-tertiary hover:bg-zinc-200 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
            aria-label={t("common.close") ?? "Close"}
          >
            ×
          </button>
        </header>

        {/* 表单区 */}
        <div className="min-h-0 flex-1 space-y-3 overflow-y-auto px-5 py-4">
          <Field label={t("pm.task.title")} required>
            <StyledInput
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              placeholder={t("pm.taskTitlePlaceholder")}
              autoFocus
            />
          </Field>

          <div className="grid grid-cols-2 gap-3">
            <Field label={t("pm.task.type")}>
              <Dropdown
                value={type}
                onChange={(v) => setType(v as TaskType)}
                options={TASK_TYPES.map((ttype) => ({
                  value: ttype,
                  label: t(TYPE_I18N[ttype]),
                }))}
              />
            </Field>
            <Field label={t("pm.task.priority")}>
              <Dropdown
                value={priority}
                onChange={(v) => setPriority(v as Priority)}
                options={PRIORITIES.map((p) => ({
                  value: p,
                  label: t(PRIORITY_I18N[p]),
                }))}
              />
            </Field>
          </div>

          <Field label={t("pm.task.description")}>
            <StyledTextarea
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              rows={3}
              placeholder={t("pm.task.descriptionPlaceholder")}
            />
          </Field>

          <div className="grid grid-cols-2 gap-3">
            <Field label={t("pm.task.assignee")}>
              {memberIds.size === 0 ? (
                <p className="rounded-md border border-dashed border-zinc-300 px-2 py-1.5 text-[11px] text-text-tertiary dark:border-zinc-600">
                  {t("pm.assigneeNoMembersHint")}
                </p>
              ) : (
                <Dropdown
                  value={assignee}
                  onChange={setAssignee}
                  options={agentOptions}
                  placeholder={{ value: "", label: t("pm.task.unassigned") }}
                />
              )}
            </Field>
            <Field label={t("pm.task.dueDate")}>
              <StyledInput
                type="date"
                value={dueAt}
                onChange={(e) => setDueAt(e.target.value)}
              />
            </Field>
          </div>

          <Field label={t("pm.task.parentTask")}>
            <Dropdown
              value={parentId}
              onChange={setParentId}
              options={parentOptions}
            />
          </Field>

          <Field label={t("pm.task.dependencies")}>
            {depOptions.length === 0 ? (
              <p className="text-xs text-text-tertiary">{t("pm.task.noCandidates")}</p>
            ) : (
              <div className="max-h-28 space-y-1 overflow-y-auto rounded-md border border-zinc-200 p-2 dark:border-zinc-700">
                {depOptions.map((opt) => (
                  <label
                    key={opt.value}
                    className="flex cursor-pointer items-center gap-2 text-xs text-text-secondary "
                  >
                    <input
                      type="checkbox"
                      checked={dependsOn.includes(opt.value)}
                      onChange={() => toggleDep(opt.value)}
                      className="accent-[var(--color-accent)]"
                    />
                    <span className="min-w-0 flex-1 truncate">{opt.label}</span>
                  </label>
                ))}
              </div>
            )}
          </Field>
        </div>

        {/* 底部操作 */}
        <footer className="flex shrink-0 justify-end gap-2 border-t border-zinc-200 px-5 py-3 dark:border-zinc-700">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50  dark:hover:bg-zinc-700"
            disabled={saving}
          >
            {t("common.cancel")}
          </button>
          <button
            type="button"
            onClick={handleSave}
            className="rounded-md bg-zinc-800 px-4 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-700 dark:hover:bg-zinc-600"
            disabled={saving}
          >
            {saving ? t("common.saving") : t("pm.task.save")}
          </button>
        </footer>
      </div>
    </div>
  );
}

function Field({
  label,
  required,
  children,
}: {
  label: string;
  required?: boolean;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="mb-1 block text-[11px] font-medium text-text-tertiary ">
        {label}
        {required && <span className="ml-0.5 text-red-500" aria-hidden>*</span>}
      </span>
      {children}
    </label>
  );
}

// ── Pure helpers (exported for testing) ──────────────────────────────
// ADR-073: assignee dropdown uses instance_id as value (matches
// task.assignee semantics); label is human-readable display name.
// Extracted as a pure function so it's directly testable without
// needing to mount the React component.
export interface AgentMeta {
  instance_id: string;
  agent_id: string;
  display_name?: string;
  name?: string;
}

export function buildAgentOptions(
  agentList: ReadonlyArray<{ meta: AgentMeta }>,
  onlyIds?: ReadonlySet<string> | null,
): Array<{ value: string; label: string }> {
  return agentList
    .filter((a) => !onlyIds || onlyIds.has(a.meta.instance_id))
    .map((a) => ({
      value: a.meta.instance_id,
      label: a.meta.display_name || a.meta.name || a.meta.agent_id,
    }));
}
