import { useState, useRef, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { cn } from "../../lib/utils";
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

const MODE_DESCRIPTIONS: Record<CloneMode, { label: string; desc: string }> = {
  skeleton: {
    label: "Skeleton (骨架)",
    desc: "仅复制 manifest + prompts + config + tools + resources。开发模式下可从骨架开始定制。",
  },
  full: {
    label: "Full (完整)",
    desc: "复制全部内容，包括 skills + data + conversations + memory。适合调试和深度定制场景。",
  },
};

export function CloneDialog({
  open,
  agentId,
  agentName,
  onCloned,
  onClose,
}: CloneDialogProps) {
  const [newAgentId, setNewAgentId] = useState("");
  const [mode, setMode] = useState<CloneMode>("skeleton");
  const [cloning, setCloning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

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
      setError("Please enter a new agent ID");
      return;
    }
    if (trimmed === agentId) {
      setError("New agent ID must be different from source");
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

  const modeInfo = MODE_DESCRIPTIONS[mode];

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div className="absolute inset-0 bg-black/40" onClick={onClose} />

      {/* Dialog */}
      <div className="relative z-10 w-full max-w-md rounded-lg border border-zinc-200 bg-white shadow-xl dark:border-zinc-700 dark:bg-zinc-800">
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-zinc-200 px-5 py-3.5 dark:border-zinc-700">
          <Copy className="h-5 w-5 text-zinc-500 dark:text-zinc-400" />
          <h2 className="text-base font-semibold text-zinc-800 dark:text-zinc-100">
            Clone Agent
          </h2>
        </div>

        {/* Body */}
        <div className="space-y-4 px-5 py-4">
          {/* Source info */}
          <div className="flex items-center gap-2 rounded-md bg-zinc-50 px-3 py-2 text-sm dark:bg-zinc-700/50">
            <Info className="h-4 w-4 text-zinc-400" />
            <span className="text-zinc-500 dark:text-zinc-400">
              Cloning from:{" "}
            </span>
            <span className="font-medium text-zinc-700 dark:text-zinc-200">
              {agentName}
            </span>
            <span className="text-xs text-zinc-400">({agentId})</span>
          </div>

          {/* New agent ID */}
          <div>
            <label className="mb-1.5 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
              New Agent ID
            </label>
            <input
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
              placeholder="com.example.myagent"
              className="w-full rounded-md border border-zinc-200 bg-white px-3 py-2 text-sm text-zinc-800 placeholder-zinc-400 outline-none transition-colors focus:border-zinc-400 focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-700 dark:text-zinc-200 dark:placeholder-zinc-500"
            />
          </div>

          {/* Clone mode */}
          <div>
            <label className="mb-1.5 block text-xs font-medium text-zinc-500 dark:text-zinc-400">
              Clone Mode
            </label>
            <div className="flex gap-2">
              {(["skeleton", "full"] as CloneMode[]).map((m) => (
                <button
                  key={m}
                  onClick={() => setMode(m)}
                  className={cn(
                    "flex-1 rounded-md border px-3 py-2 text-sm font-medium transition-colors",
                    mode === m
                      ? "border-zinc-200 bg-zinc-200 text-zinc-800 dark:border-zinc-300 dark:bg-zinc-300 dark:text-zinc-900"
                      : "border-zinc-200 text-zinc-600 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700",
                  )}
                >
                  {MODE_DESCRIPTIONS[m].label}
                </button>
              ))}
            </div>
            <p className="mt-1.5 text-xs text-zinc-400 dark:text-zinc-500">
              {modeInfo.desc}
            </p>
          </div>

          {/* Error */}
          {error && (
            <div className="rounded-md bg-red-50 px-3 py-2 text-xs text-red-600 dark:bg-red-900/20 dark:text-red-400">
              {error}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex justify-end gap-2 border-t border-zinc-200 px-5 py-3 dark:border-zinc-700">
          <button
            onClick={onClose}
            disabled={cloning}
            className="rounded-md border border-zinc-200 px-4 py-1.5 text-sm font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
          >
            Cancel
          </button>
          <button
            onClick={handleClone}
            disabled={cloning || !newAgentId.trim()}
            className="flex items-center gap-2 rounded-md btn-solid px-4 py-1.5 text-sm font-medium disabled:cursor-not-allowed disabled:opacity-50"
          >
            {cloning ? (
              <>
                <div className="h-3.5 w-3.5 animate-spin rounded-full border-2 border-white/30 border-t-white" />
                Cloning...
              </>
            ) : (
              <>
                <Copy className="h-3.5 w-3.5" />
                Clone
              </>
            )}
          </button>
        </div>
      </div>
    </div>
  );
}
