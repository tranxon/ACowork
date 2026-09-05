/**
 * DocEditor — doc 视图右侧编辑器（设计 §7 / plan D2-3）。
 *
 * - 编辑/预览双模式：编辑 = 等宽 textarea；预览 = DocMarkdownView（同渲染栈）。
 * - 保存：PUT 携带 `base_version`（乐观并发）；409 `version_conflict` →
 *   amber banner「文档已被他人更新」+ 刷新按钮（不静默覆盖）。
 * - 来源标记：Agent add-to-doc 导入的文档展示 agent_id + workspace_path badge。
 * - 快捷键 Ctrl/Cmd+S 保存（textarea 聚焦时）。
 * - 切换文档且本地有未保存修改 → ConfirmDialog 确认丢弃。
 */

import { useState } from "react";
import { Check, Eye, FileText, Loader2, Pencil, RefreshCw, Save, Sparkles } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import { useDocHealthStore } from "../../stores/doc/healthStore";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { cn } from "../../lib/utils";
import { DocMarkdownView } from "./DocMarkdownView";

export function DocEditor() {
  const { t } = useTranslation();
  const healthy = useDocHealthStore((s) => s.healthy);
  const doc = useDocEditorStore((s) => s.doc);
  const content = useDocEditorStore((s) => s.content);
  const dirty = useDocEditorStore((s) => s.dirty);
  const saving = useDocEditorStore((s) => s.saving);
  const loading = useDocEditorStore((s) => s.loading);
  const mode = useDocEditorStore((s) => s.mode);
  const conflict = useDocEditorStore((s) => s.conflict);
  const saveError = useDocEditorStore((s) => s.saveError);
  const pendingOpenDocId = useDocEditorStore((s) => s.pendingOpenDocId);
  const setMode = useDocEditorStore((s) => s.setMode);
  const setContent = useDocEditorStore((s) => s.setContent);
  const save = useDocEditorStore((s) => s.save);
  const reload = useDocEditorStore((s) => s.reload);
  const confirmPendingOpen = useDocEditorStore((s) => s.confirmPendingOpen);
  const cancelPendingOpen = useDocEditorStore((s) => s.cancelPendingOpen);

  const [savedTick, setSavedTick] = useState(0);

  // 空状态
  if (!doc && !loading) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 text-zinc-400">
        <FileText className="h-8 w-8 opacity-40" aria-hidden />
        <p className="text-xs">{t("doc.editorEmpty")}</p>
      </div>
    );
  }

  if (loading && !doc) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-xs text-zinc-400">
        <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
      </div>
    );
  }

  if (!doc) return null;

  const handleSave = async () => {
    const ok = await save();
    if (ok) {
      setSavedTick((n) => n + 1);
      setTimeout(() => setSavedTick((n) => n + 1), 2500);
    }
  };

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col bg-editor-canvas">
      {/* ── 顶栏：标题 + 元信息 + 模式/保存 ─────────────────── */}
      <div className="flex shrink-0 items-center gap-2 border-b border-zinc-200 px-3 py-1.5 dark:border-zinc-800">
        <FileText className="h-4 w-4 shrink-0 text-zinc-400" aria-hidden />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate text-xs font-medium text-zinc-700 dark:text-zinc-100">
              {doc.meta.name}
            </span>
            {doc.meta.import && (
              <span
                className="inline-flex max-w-[45%] shrink items-center gap-1 truncate rounded-full bg-violet-50 px-1.5 py-0.5 text-[10px] text-violet-600 dark:bg-violet-900/30 dark:text-violet-300"
                title={`${doc.meta.import.agent_id} · ${doc.meta.import.workspace_path}`}
              >
                <Sparkles className="h-2.5 w-2.5 shrink-0" aria-hidden />
                <span className="truncate">{t("doc.importedBy", { agent: doc.meta.import.agent_id })}</span>
              </span>
            )}
          </div>
          <div className="flex items-center gap-2 text-[10px] text-zinc-400">
            <span className="truncate">{doc.path}</span>
            <span aria-label={t("doc.versionLabel")}>v{doc.meta.version}</span>
            {dirty && <span className="text-amber-500">{t("doc.unsaved")}</span>}
            {savedTick % 2 === 1 && !dirty && (
              <span className="inline-flex items-center gap-0.5 text-emerald-600">
                <Check className="h-2.5 w-2.5" aria-hidden />
                {t("doc.saved")}
              </span>
            )}
          </div>
        </div>

        {/* 模式切换（编辑/预览） */}
        <div
          className="flex shrink-0 items-center rounded-md border border-zinc-200 p-0.5 text-[11px] dark:border-zinc-700"
          role="tablist"
          aria-label={t("doc.modeLabel")}
        >
          <button
            type="button"
            role="tab"
            aria-selected={mode === "edit"}
            disabled={!healthy}
            onClick={() => setMode("edit")}
            className={cn(
              "flex items-center gap-1 rounded px-2 py-0.5 transition-colors",
              mode === "edit"
                ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)]"
                : "text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-200",
            )}
          >
            <Pencil className="h-3 w-3" aria-hidden />
            {t("doc.edit")}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "preview"}
            onClick={() => setMode("preview")}
            className={cn(
              "flex items-center gap-1 rounded px-2 py-0.5 transition-colors",
              mode === "preview"
                ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)]"
                : "text-zinc-500 hover:text-zinc-700 dark:hover:text-zinc-200",
            )}
          >
            <Eye className="h-3 w-3" aria-hidden />
            {t("doc.preview")}
          </button>
        </div>

        <button
          type="button"
          disabled={!dirty || saving || !healthy}
          onClick={() => void handleSave()}
          className={cn(
            "flex shrink-0 items-center gap-1 rounded-md px-2 py-1 text-[11px] font-medium transition-colors disabled:opacity-40",
            dirty
              ? "bg-[var(--color-accent)] text-white hover:opacity-90"
              : "border border-zinc-200 text-zinc-400 dark:border-zinc-700",
          )}
        >
          {saving ? <Loader2 className="h-3 w-3 animate-spin" aria-hidden /> : <Save className="h-3 w-3" aria-hidden />}
          {t("doc.save")}
        </button>
      </div>

      {/* ── 409 版本冲突 / 错误 banner ─────────────────────── */}
      {conflict && (
        <div className="flex shrink-0 items-center gap-2 border-b border-amber-200 bg-amber-50 px-3 py-1.5 text-[11px] text-amber-800 dark:border-amber-800/40 dark:bg-amber-900/25 dark:text-amber-200">
          <span className="flex-1">{t("doc.versionConflict")}</span>
          <button
            type="button"
            onClick={() => void reload()}
            className="inline-flex items-center gap-1 rounded bg-amber-600 px-2 py-0.5 font-medium text-white hover:bg-amber-700"
          >
            <RefreshCw className="h-3 w-3" aria-hidden />
            {t("doc.refreshNow")}
          </button>
        </div>
      )}
      {saveError && !conflict && (
        <div className="flex shrink-0 items-center gap-2 border-b border-red-200 bg-red-50 px-3 py-1.5 text-[11px] text-red-700 dark:border-red-800/40 dark:bg-red-900/25 dark:text-red-200">
          <span className="flex-1">{saveError}</span>
        </div>
      )}

      {/* ── 编辑 / 预览 ────────────────────────────────────── */}
      {mode === "preview" ? (
        <DocMarkdownView content={content} />
      ) : (
        <textarea
          value={content}
          onChange={(e) => setContent(e.target.value)}
          onKeyDown={(e) => {
            if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
              e.preventDefault();
              void handleSave();
            }
          }}
          disabled={!healthy}
          spellCheck={false}
          aria-label={t("doc.editorAria")}
          className="h-full min-h-0 w-full flex-1 resize-none bg-editor-canvas px-5 py-4 font-mono text-xs leading-relaxed text-zinc-800 outline-none placeholder:text-zinc-400 disabled:opacity-50 dark:text-zinc-200"
        />
      )}

      {/* ── 切文档确认（丢弃未保存修改；由 editorStore.pendingOpenDocId 驱动） */}
      <ConfirmDialog
        open={pendingOpenDocId !== null}
        title={t("doc.discardTitle")}
        message={t("doc.discardSwitchMsg")}
        confirmLabel={t("doc.discard")}
        destructive
        onCancel={() => cancelPendingOpen()}
        onConfirm={() => void confirmPendingOpen()}
      />
    </div>
  );
}
