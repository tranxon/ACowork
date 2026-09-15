/**
 * MemberManagerDialog — 项目成员管理对话框。
 *
 * 需求：每个项目可添加 Agent 实例为项目成员。联动指派的前提：
 * 任务 assignee 必须是项目成员（或 "human"），因此成员管理入口
 * 放在 ProjectHeader（成员头像组点击打开）。
 *
 * 数据契约：
 * - 成员以 `instance_id`（UUID，ADR-073）标识，与 task.assignee 同一身份体系。
 * - 头像/名称不存快照，实时 join agentStore（agents: instance_id → AgentStorage）。
 * - 已是成员但 agentStore 缺失（Agent 已卸载）→ 占位行，仅允许移除。
 * - 添加失败按 PmApiError.code 映射本地化文案（409 member_already_exists /
 *   member_has_open_tasks / 404 member_not_found）。
 */

import { useEffect, useMemo, useState } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { AgentAvatar } from "../../components/common/AgentAvatar";
import { showToast } from "../../components/common/ToastProvider";
import { useTranslation } from "../../i18n/useTranslation";
import { PmApiError } from "../../lib/pm-api";
import type { PmProject } from "../../lib/pm-types";

interface MemberManagerDialogProps {
  project: PmProject;
  onClose: () => void;
}

export function MemberManagerDialog({ project, onClose }: MemberManagerDialogProps) {
  const { t } = useTranslation();
  const agents = useAgentStore((s) => s.agents);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const addMember = usePmProjectStore((s) => s.addMember);
  const removeMember = usePmProjectStore((s) => s.removeMember);
  const [busyId, setBusyId] = useState<string | null>(null);

  const memberIds = useMemo(
    () => new Set(project.members.map((m) => m.instance_id)),
    [project.members],
  );

  // 打开时确保 agent 列表已加载（成员候选来自 agentStore）
  useEffect(() => {
    if (Object.keys(agents).length === 0) void fetchAgents();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ESC 关闭
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const memberError = (e: unknown): string => {
    if (e instanceof PmApiError) {
      switch (e.code) {
        case "member_already_exists":
          return t("pm.memberAlreadyExists");
        case "member_has_open_tasks":
          return t("pm.memberHasOpenTasks");
        case "member_not_found":
          return t("pm.memberNotFound");
      }
    }
    return e instanceof Error ? e.message : String(e);
  };

  const handleAdd = async (instanceId: string) => {
    setBusyId(instanceId);
    try {
      await addMember(project.id, instanceId);
      showToast({ type: "success", message: t("pm.memberAdded") });
    } catch (e) {
      showToast({ type: "error", message: memberError(e) });
    } finally {
      setBusyId(null);
    }
  };

  const handleRemove = async (instanceId: string) => {
    setBusyId(instanceId);
    try {
      await removeMember(project.id, instanceId);
      showToast({ type: "success", message: t("pm.memberRemoved") });
    } catch (e) {
      showToast({ type: "error", message: memberError(e) });
    } finally {
      setBusyId(null);
    }
  };

  // 可添加候选：agentStore 中存在、且还不是成员（按显示名排序）
  const candidates = useMemo(
    () =>
      Object.values(agents)
        .filter((a) => !memberIds.has(a.meta.instance_id))
        .sort((a, b) =>
          (a.meta.display_name || a.meta.name || "").localeCompare(
            b.meta.display_name || b.meta.name || "",
          ),
        ),
    [agents, memberIds],
  );

  // 已是成员：agentStore 存在 → 解析显示；不存在（Agent 已卸载）→ 占位
  const currentMembers = useMemo(
    () =>
      project.members.map((m) => ({
        instance_id: m.instance_id,
        meta: agents[m.instance_id]?.meta ?? null,
      })),
    [project.members, agents],
  );

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />
      <div
        className="relative z-10 flex max-h-[80vh] w-full max-w-md flex-col overflow-hidden rounded-md border border-zinc-200 bg-modal-surface shadow-xl dark:border-zinc-700"
        role="dialog"
        aria-modal="true"
        aria-label={t("pm.memberManagerTitle")}
      >
        <header className="flex items-center justify-between border-b border-zinc-200 px-5 py-3 dark:border-zinc-700">
          <h3 className="text-sm font-semibold text-zinc-800 dark:text-zinc-100">
            {t("pm.memberManagerTitle")}
          </h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded px-1.5 text-sm text-zinc-500 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
            aria-label={t("common.close")}
          >
            ×
          </button>
        </header>

        <p className="border-b border-zinc-200 px-5 py-2 text-[11px] text-zinc-500 dark:border-zinc-700 dark:text-zinc-400">
          {t("pm.memberManagerHint")}
        </p>

        <div className="flex-1 overflow-y-auto px-5 py-3">
          {/* 当前成员 */}
          <div className="mb-1 text-[11px] font-medium text-zinc-500 dark:text-zinc-400">
            {t("pm.members")}（{currentMembers.length}）
          </div>
          {currentMembers.length === 0 ? (
            <p className="mb-4 text-xs italic text-zinc-400">{t("pm.noMembers")}</p>
          ) : (
            <ul className="mb-4 space-y-1">
              {currentMembers.map((m) => (
                <li
                  key={m.instance_id}
                  className="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-50 dark:hover:bg-zinc-800/60"
                >
                  {m.meta ? (
                    <AgentAvatar
                      agentId={m.meta.instance_id}
                      displayName={m.meta.display_name ?? m.meta.name}
                      avatarUrl={m.meta.avatar ?? null}
                      builtinAvatarId={m.meta.builtin_avatar ?? null}
                      size={24}
                    />
                  ) : (
                    <div className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-zinc-200 text-[10px] text-zinc-500 dark:bg-zinc-700">
                      ?
                    </div>
                  )}
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-xs text-zinc-800 dark:text-zinc-100">
                      {m.meta?.display_name ?? m.meta?.name ?? t("pm.memberNotFound")}
                    </div>
                    <div className="truncate text-[10px] text-zinc-400">{m.instance_id}</div>
                  </div>
                  <button
                    type="button"
                    disabled={busyId === m.instance_id}
                    onClick={() => handleRemove(m.instance_id)}
                    className="shrink-0 rounded-md px-2 py-1 text-[11px] text-red-600 hover:bg-red-50 disabled:opacity-40 dark:text-red-400 dark:hover:bg-red-950/40"
                  >
                    {t("pm.removeMember")}
                  </button>
                </li>
              ))}
            </ul>
          )}

          {/* 可添加候选 */}
          <div className="mb-1 text-[11px] font-medium text-zinc-500 dark:text-zinc-400">
            {t("pm.manageMembers")}
          </div>
          {candidates.length === 0 ? (
            <p className="text-xs italic text-zinc-400">{t("pm.noMembers")}</p>
          ) : (
            <ul className="space-y-1">
              {candidates.map((a) => (
                <li
                  key={a.meta.instance_id}
                  className="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-50 dark:hover:bg-zinc-800/60"
                >
                  <AgentAvatar
                    agentId={a.meta.instance_id}
                    displayName={a.meta.display_name ?? a.meta.name}
                    avatarUrl={a.meta.avatar ?? null}
                    builtinAvatarId={a.meta.builtin_avatar ?? null}
                    size={24}
                  />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-xs text-zinc-800 dark:text-zinc-100">
                      {a.meta.display_name ?? a.meta.name ?? a.meta.agent_id}
                    </div>
                    <div className="truncate text-[10px] text-zinc-400">{a.meta.instance_id}</div>
                  </div>
                  <button
                    type="button"
                    disabled={busyId === a.meta.instance_id}
                    onClick={() => handleAdd(a.meta.instance_id)}
                    className="shrink-0 rounded-md px-2 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-100 disabled:opacity-40 dark:text-zinc-300 dark:hover:bg-zinc-700"
                  >
                    {t("pm.addMember")}
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>
    </div>
  );
}
