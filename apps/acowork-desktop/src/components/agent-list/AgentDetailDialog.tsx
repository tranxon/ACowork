import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AgentDetail } from "../../lib/types";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";

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
  const { t } = useTranslation();
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
      <div className="relative z-10 w-full max-w-md rounded-md border border-zinc-200 bg-white p-6 shadow-xl dark:border-zinc-700 dark:bg-zinc-800">
        <div className="mb-4 flex items-center justify-between">
          <h3 className="text-sm font-semibold">Agent Details</h3>
          <button
            ref={closeRef}
            onClick={onClose}
            className="text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
            aria-label={t("agentDetailDialog.ariaLabelClose")}
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
          <div className="text-sm">
            <ErrorBox message={`Failed to load agent details: ${error}`} onClose={() => setError(null)} />
          </div>
        )}

        {detail && !loading && (
          <div className="space-y-3 text-xs">
            <DetailRow label={t("agentDetailDialog.labelName")} value={detail.name} />
            <DetailRow label={t("agentDetailDialog.labelAgentId")} value={detail.agent_id} mono />
            <DetailRow label={t("agentDetailDialog.labelVersion")} value={detail.version} />
            <DetailRow label={t("agentDetailDialog.labelAuthor")} value={detail.author || "—"} />
            <DetailRow label={t("agentDetailDialog.labelDescription")} value={detail.description || "—"} />
            <DetailRow
              label={t("agentDetailDialog.labelStatus")}
              value={
                <span className="flex items-center gap-1.5">
                  <span
                    className={cn(
                      "inline-block h-2 w-2 rounded-full",
                      detail.running ? "bg-[var(--color-accent)]" : "bg-zinc-300 dark:bg-zinc-600",
                    )}
                  />
                  {detail.running ? t("agentDetailDialog.statusRunning") : t("agentDetailDialog.statusStopped")}
                </span>
              }
            />
            {detail.pid !== null && <DetailRow label={t("agentDetailDialog.labelPid")} value={String(detail.pid)} mono />}
            {detail.started_at && <DetailRow label={t("agentDetailDialog.labelStartedAt")} value={detail.started_at} />}
            <DetailRow label={t("agentDetailDialog.labelInstallPath")} value={detail.install_path} mono />
            {modelInfo && (
              <DetailRow
                label={t("agentDetailDialog.labelCurrentModel")}
                value={
                  <span className="flex items-center gap-1.5">
                    <span className="font-mono text-xs" style={{ color: "var(--color-accent)" }}>{modelInfo.model}</span>
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
            className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
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
