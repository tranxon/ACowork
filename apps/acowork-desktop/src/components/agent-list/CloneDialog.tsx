import { useState, useRef, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";
import { cn } from "../../lib/utils";
import { StyledInput } from "../common/StyledInput";
import type { CloneMode, CloneResponse } from "../../lib/types";
import { Copy, Info } from "lucide-react";

interface CloneDialogProps {
  open: boolean;
  /** Source agent ID to clone from */
  agentId: string;
  /** Source agent display name */
  agentName: string;
  /** Called when cloning succeeds */
  onCloned: (result: CloneResponse) => void;
  onClose: () => void;
}

export function CloneDialog({
  open,
  agentId,
  agentName,
  onCloned,
  onClose,
}: CloneDialogProps) {
  const { t } = useTranslation();
  const [newAgentId, setNewAgentId] = useState("");
  const [mode, setMode] = useState<CloneMode>("skeleton");
  const [cloning, setCloning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const modeDescriptions: Record<CloneMode, { label: string; desc: string }> = {
    skeleton: {
      label: t("cloneDialog.skeleton"),
      desc: t("cloneDialog.skeletonDesc"),
    },
    full: {
      label: t("cloneDialog.full"),
      desc: t("cloneDialog.fullDesc"),
    },
  };

  // Auto-generate suggestion based on source agent name
  useEffect(() => {
    if (open) {
      const timestamp = new Date().toISOString().slice(2, 10).replace(/-/g, "");
      const suffix = agentId.includes(".") ? "" : ".cloned";
      setNewAgentId(`${agentId}${suffix}-${timestamp}`);
      setError(null);
      setTimeout(() => inputRef.current?.focus(), 50);
    }
  }, [open, agentId]);

  // Close on Escape
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, onClose]);

  const handleClone = async () => {
    const trimmed = newAgentId.trim();
    if (!trimmed) {
      setError(t("cloneDialog.errorEmptyId"));
      return;
    }
    if (trimmed === agentId) {
      setError(t("cloneDialog.errorSameAsSource"));
      return;
    }
    setCloning(true);
    setError(null);
    try {
      const result = await invoke<CloneResponse>("clone_agent", {
        agentId,
        newAgentId: trimmed,
        mode,
      });
      onCloned(result);
    } catch (e) {
      setError(String(e));
    } finally {
      setCloning(false);
    }
  };

  if (!open) return null;

  const modeInfo = modeDescriptions[mode];

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div className="absolute inset-0 bg-black/40" onClick={onClose} />

      {/* Dialog */}
      <div className="relative z-10 w-full max-w-md rounded-md border border-zinc-200 bg-white shadow-xl dark:border-zinc-700 dark:bg-zinc-800">
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-zinc-200 px-5 py-3.5 dark:border-zinc-700">
          <Copy className="h-5 w-5 text-zinc-500 dark:text-zinc-400" />
          <h2 className="text-sm font-semibold text-zinc-800 dark:text-zinc-100">
            {t("cloneDialog.title")}
          </h2>
        </div>

        {/* Body */}
        <div className="space-y-4 px-5 py-4">
          {/* Source info */}
          <div className="flex items-center gap-2 rounded-md bg-zinc-50 px-3 py-2 text-xs dark:bg-zinc-700/50">
            <Info className="h-4 w-4 text-zinc-400" />
            <span className="text-zinc-500 dark:text-zinc-400">
              {t("cloneDialog.cloningFrom")}{" "}
            </span>
            <span className="font-medium text-zinc-700 dark:text-zinc-200">
              {agentName}
            </span>
            <span className="text-xs text-zinc-400">({agentId})</span>
          </div>

          {/* New agent ID */}
          <div>
            <label className="mb-1.5 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
              {t("cloneDialog.newAgentIdLabel")}
            </label>
            <StyledInput
              ref={inputRef}
              type="text"
              value={newAgentId}
              onChange={(e) => {
                setNewAgentId(e.target.value);
                setError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") void handleClone();
              }}
              placeholder={t("cloneDialog.newAgentIdPlaceholder")}
              className="bg-white dark:bg-zinc-700"
            />
          </div>

          {/* Clone mode */}
          <div>
            <label className="mb-1.5 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
              {t("cloneDialog.cloneModeLabel")}
            </label>
            <div className="flex gap-2">
              {(["skeleton", "full"] as CloneMode[]).map((m) => (
                <button
                  key={m}
                  onClick={() => setMode(m)}
                  className={cn(
                    "flex-1 rounded-md border px-3 py-1.5 text-xs font-medium transition-colors",
                    mode === m
                      ? "border-zinc-200 bg-zinc-200 text-zinc-800 dark:border-zinc-300 dark:bg-zinc-300 dark:text-zinc-900"
                      : "border-zinc-200 text-zinc-600 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700",
                  )}
                >
                  {modeDescriptions[m].label}
                </button>
              ))}
            </div>
            <p className="mt-1.5 text-xs text-zinc-400 dark:text-zinc-500">
              {modeInfo.desc}
            </p>
          </div>

          {/* Error */}
          {error && (
            <ErrorBox message={error} onClose={() => setError(null)} />
          )}
        </div>

        {/* Footer */}
        <div className="flex justify-end gap-2 border-t border-zinc-200 px-5 py-3 dark:border-zinc-700">
          <button
            onClick={onClose}
            disabled={cloning}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
          >
            {t("common.cancel")}
          </button>
          <button
            onClick={handleClone}
            disabled={cloning || !newAgentId.trim()}
            className="flex items-center gap-2 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
          >
            {cloning ? (
              <>
                <div className="h-3.5 w-3.5 animate-spin rounded-full border-2 border-white/30 border-t-white" />
                {t("cloneDialog.cloning")}
              </>
            ) : (
              <>
                <Copy className="h-3.5 w-3.5" />
                {t("cloneDialog.clone")}
              </>
            )}
          </button>
        </div>
      </div>
    </div>
  );
}
