import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentDetail } from "../../lib/types";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";

interface AgentDetailDialogProps {
  open: boolean;
  agentId: string | null;
  onClose: () => void;
}

interface AgentModelInfo {
  provider: string;
  model: string;
  available_models: string[];
}

export function AgentDetailDialog({ open, agentId, onClose }: AgentDetailDialogProps) {
  const [detail, setDetail] = useState<AgentDetail | null>(null);
  const [modelInfo, setModelInfo] = useState<AgentModelInfo | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const closeRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open || !agentId) return;

    setLoading(true);
    setError(null);
    invoke<AgentDetail>("get_agent_detail", { agentId })
      .then((d) => setDetail(d))
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));

    // Fetch model info from Gateway API
    fetch(`${getGatewayUrl()}/api/agents/${agentId}/model`)
      .then((resp) => resp.ok ? resp.json() as Promise<AgentModelInfo> : null)
      .then((data) => setModelInfo(data))
      .catch(() => setModelInfo(null));
  }, [open, agentId]);

  // Focus close button on open; Escape to close
  useEffect(() => {
    if (!open) return;
    closeRef.current?.focus();
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div className="absolute inset-0 bg-black/40" onClick={onClose} />

      {/* Dialog */}
      <div className="relative z-10 w-full max-w-md rounded-lg border border-zinc-200 bg-white p-6 shadow-xl dark:border-zinc-700 dark:bg-zinc-800">
        <div className="mb-4 flex items-center justify-between">
          <h3 className="text-base font-semibold">Agent Details</h3>
          <button
            ref={closeRef}
            onClick={onClose}
            className="text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
            aria-label="Close"
          >
            ✕
          </button>
        </div>

        {loading && (
          <div className="flex items-center justify-center py-8">
            <div className="h-5 w-5 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
          </div>
        )}

        {error && (
          <div className="rounded-md bg-red-50 p-3 text-sm text-red-600 dark:bg-red-950 dark:text-red-400">
            Failed to load agent details: {error}
          </div>
        )}

        {detail && !loading && (
          <div className="space-y-3 text-sm">
            <DetailRow label="Name" value={detail.name} />
            <DetailRow label="Agent ID" value={detail.agent_id} mono />
            <DetailRow label="Version" value={detail.version} />
            <DetailRow label="Author" value={detail.author || "—"} />
            <DetailRow label="Description" value={detail.description || "—"} />
            <DetailRow
              label="Status"
              value={
                <span className="flex items-center gap-1.5">
                  <span
                    className={cn(
                      "inline-block h-2 w-2 rounded-full",
                      detail.running ? "bg-green-500" : "bg-zinc-300 dark:bg-zinc-600",
                    )}
                  />
                  {detail.running ? "Running" : "Stopped"}
                </span>
              }
            />
            {detail.pid !== null && <DetailRow label="PID" value={String(detail.pid)} mono />}
            {detail.started_at && <DetailRow label="Started At" value={detail.started_at} />}
            <DetailRow label="Install Path" value={detail.install_path} mono />
            {modelInfo && (
              <DetailRow
                label="Current Model"
                value={
                  <span className="flex items-center gap-1.5">
                    <span className="font-mono text-xs text-accent-green">{modelInfo.model}</span>
                    <span className="text-[10px] text-zinc-400">({modelInfo.provider})</span>
                  </span>
                }
              />
            )}
          </div>
        )}

        <div className="mt-6 flex justify-end">
          <button
            onClick={onClose}
            className="rounded-md border border-zinc-200 px-4 py-1.5 text-sm font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
          >
            Close
          </button>
        </div>
      </div>
    </div>
  );
}

function DetailRow({
  label,
  value,
  mono = false,
}: {
  label: string;
  value: React.ReactNode;
  mono?: boolean;
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-xs text-zinc-400 dark:text-zinc-500">{label}</span>
      {typeof value === "string" ? (
        <span className={cn("text-zinc-800 dark:text-zinc-200", mono && "font-mono text-xs")}>
          {value}
        </span>
      ) : (
        value
      )}
    </div>
  );
}
