import { useEffect, useState, useRef } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useAgentProfileStore } from "../../stores/agentProfileStore";
import { UserAvatar, BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { getGatewayUrl } from "../../lib/config";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import { Tooltip } from "../common/Tooltip";

// ── Component ───────────────────────────────────────────────────────────

export function AgentSetupTab() {
  const { t } = useTranslation();
  const { agents, selectedAgentId } = useAgentStore();
  const { getProfile, setProfile, resetProfile } = useAgentProfileStore();

  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);
  const profile = selectedAgentId ? getProfile(selectedAgentId) : null;

  // Fetch agent runtime config from Gateway API on mount
  const [_configLoading, setConfigLoading] = useState(false);
  const [configSaving, setConfigSaving] = useState(false);
  const [iconOpen, setIconOpen] = useState(false);
  const iconRef = useRef<HTMLDivElement>(null);
  const [showResetConfirm, setShowResetConfirm] = useState(false);

  // Close icon picker on outside click
  useEffect(() => {
    if (!iconOpen) return;
    const handler = (e: MouseEvent) => {
      if (iconRef.current && !iconRef.current.contains(e.target as Node)) {
        setIconOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [iconOpen]);

  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;
    setConfigLoading(true);
    // Fetch config
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/config`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (cancelled || !data) return;
        setProfile(selectedAgentId, {
          maxTokens: data.max_output_tokens,
          maxIterations: data.max_iterations,
          temperature: data.temperature,
          shellApprovalThreshold: data.shell_approval_threshold,
          approvalTimeoutSecs: data.approval_timeout_secs ?? 300,
          globalMaxTokens: data.global_max_output_tokens,
          activeModel: data.model,
          activeProvider: data.provider,
        });
      })
      .catch((err) => {
        if (!cancelled) {
          // 503 = agent not ready yet, silently retry on next mount
          console.debug("[AgentSetup] Agent not ready:", err);
        }
      })
      .finally(() => {
        if (!cancelled) setConfigLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedAgentId]);

  // Listen for global resource refresh events
  useEffect(() => {
    if (!selectedAgentId) return;
    const handler = (e: Event) => {
      const ce = e as CustomEvent<{ agentId: string }>;
      if (ce.detail?.agentId === selectedAgentId) {
        fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/config`)
          .then((res) => (res.ok ? res.json() : null))
          .then((data) => {
            if (!data) return;
            setProfile(selectedAgentId, {
              maxTokens: data.max_output_tokens,
              maxIterations: data.max_iterations,
              temperature: data.temperature,
              shellApprovalThreshold: data.shell_approval_threshold,
              approvalTimeoutSecs: data.approval_timeout_secs ?? 300,
              globalMaxTokens: data.global_max_output_tokens,
              activeModel: data.model,
              activeProvider: data.provider,
            });
          })
          .catch(() => { });
      }
    };
    window.addEventListener('acowork:refresh-agent-config', handler);
    return () => window.removeEventListener('acowork:refresh-agent-config', handler);
  }, [selectedAgentId]);

  // ── Apply config to Gateway ────────────────────────────────────────

  const handleApply = async () => {
    if (!selectedAgentId || !profile) return;
    setConfigSaving(true);
    try {
      const body: Record<string, unknown> = {};
      if (profile.maxTokens && profile.maxTokens > 0) body.max_output_tokens = profile.maxTokens;
      if (profile.maxIterations && profile.maxIterations > 0) body.max_iterations = profile.maxIterations;
      if (profile.temperature !== undefined) body.temperature = profile.temperature;
      if (profile.shellApprovalThreshold) body.shell_approval_threshold = profile.shellApprovalThreshold;
      if (profile.approvalTimeoutSecs !== undefined && profile.approvalTimeoutSecs > 0) body.approval_timeout_secs = profile.approvalTimeoutSecs;
      const res = await fetch(
        `${getGatewayUrl()}/api/agents/${selectedAgentId}/config`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        },
      );
      if (!res.ok) {
        console.warn("[AgentSetup] Config update failed:", res.status);
      }
    } catch {
      // silently ignore network errors
    } finally {
      setConfigSaving(false);
    }
  };

  if (!selectedAgentId || !selectedAgent || !profile) {
    return (
      <div className="flex flex-1 items-center justify-center p-6">
        <span className="text-xs text-zinc-400 dark:text-zinc-500">{t("agentSetup.noAgentSelected")}</span>
      </div>
    );
  }

  const agentName = profile.displayName ?? selectedAgent.name ?? selectedAgentId;

  return (
    <div className="flex-1 overflow-y-auto p-3">
      {/* Avatar preview — click to open icon picker */}
      <div className="mb-4 flex items-center gap-3">
        <div className="relative shrink-0" ref={iconRef}>
          <Tooltip content={t("agentSetup.chooseIcon")} variant="plain">
            <button
              onClick={() => setIconOpen(!iconOpen)}
              className="rounded-md border border-transparent p-0.5 transition-colors hover:border-zinc-300 dark:hover:border-zinc-600"
            >
              <UserAvatar
                displayName={agentName}
                avatarType="icon"
                avatarIcon={profile.avatarIconId ?? undefined}
                size={64}
              />
            </button>
          </Tooltip>
          {iconOpen && (
            <div className="absolute left-0 z-50 mt-1 w-max rounded-md border border-zinc-200 bg-white p-1.5 shadow-lg dark:border-zinc-700 dark:bg-zinc-800">
              <div className="grid grid-cols-4 gap-1">
                {BUILTIN_ICON_IDS.map((iconId) => (
                  <button
                    key={iconId}
                    onClick={() => {
                      setProfile(selectedAgentId, { avatarIconId: iconId });
                      setIconOpen(false);
                    }}
                    className={`flex items-center justify-center rounded-md p-1 transition-colors ${profile.avatarIconId === iconId
                      ? "bg-zinc-200 dark:bg-zinc-600"
                      : "hover:bg-zinc-100 dark:hover:bg-zinc-700"
                      }`}
                  >
                    <img
                      src={BUILTIN_ICONS[iconId] ?? ""}
                      alt={iconId}
                      draggable={false}
                      className="h-16 w-16 rounded-full object-cover"
                    />
                  </button>
                ))}
              </div>
            </div>
          )}
        </div>
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium text-zinc-800 dark:text-zinc-200">
            {agentName}
          </p>
          <p className="truncate text-[10px] text-zinc-400 dark:text-zinc-500">
            {selectedAgentId}
          </p>
        </div>
      </div>

      {/* Agent Name */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.agentName")}
        </label>
        <StyledInput
          type="text"
          value={profile.displayName ?? selectedAgent.name ?? ""}
          onChange={(e) =>
            setProfile(selectedAgentId, { displayName: e.target.value || undefined })
          }
          placeholder={selectedAgent.name ?? "Agent name"}
          className="rounded-md bg-white dark:bg-zinc-800"
        />
      </div>

      {/* Max Output Tokens */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.maxOutputTokens")}
        </label>
        <StyledInput
          type="number"
          min={0}
          max={131072}
          step={1024}
          value={profile.maxTokens && profile.maxTokens > 0 ? profile.maxTokens : ""}
          onChange={(e) => {
            const v = e.target.value;
            setProfile(selectedAgentId, {
              maxTokens: v === "" ? 0 : Math.max(0, parseInt(v, 10) || 0),
            });
          }}
          placeholder={`${profile.globalMaxTokens ?? 32768} ${t("agentSetup.defaultModelLimit")}`}
          className="rounded-md bg-white dark:bg-zinc-800"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.leaveEmptyDefault")}
        </p>
      </div>

      {/* Max Iterations */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.maxIterations")}
        </label>
        <StyledInput
          type="number"
          min={0}
          max={200}
          value={profile.maxIterations && profile.maxIterations > 0 ? profile.maxIterations : ""}
          onChange={(e) => {
            const v = e.target.value;
            setProfile(selectedAgentId, {
              maxIterations: v === "" ? 0 : Math.max(0, parseInt(v, 10) || 0),
            });
          }}
          placeholder={t("agentSetup.defaultIterations")}
          className="rounded-md bg-white dark:bg-zinc-800"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.leaveEmptyDefault")}
        </p>
      </div>

      {/* Temperature */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.temperature", { value: profile.temperature ?? 0.7 })}
        </label>
        <input
          type="range"
          min={0}
          max={2}
          step={0.1}
          value={profile.temperature ?? 0.7}
          onChange={(e) =>
            setProfile(selectedAgentId, {
              temperature: parseFloat(e.target.value),
            })
          }
          className="w-full"
          style={{ "--progress": `${((profile.temperature ?? 0.7) / 2) * 100}%` } as React.CSSProperties}
        />
        <div className="flex justify-between text-[9px] text-zinc-400 dark:text-zinc-500">
          <span>{t("agentSetup.deterministic")}</span>
          <span>{t("agentSetup.creative")}</span>
        </div>
      </div>

      {/* Shell Command Approval Threshold */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.shellCommandApproval")}
        </label>
        <select
          value={profile.shellApprovalThreshold ?? "medium"}
          onChange={(e) => {
            const v = e.target.value;
            setProfile(selectedAgentId, {
              shellApprovalThreshold: v,
            });
          }}
          className="w-full appearance-none rounded border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          style={{
            backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
            backgroundPosition: 'right 0.5rem center',
            backgroundRepeat: 'no-repeat',
            backgroundSize: '1.5em 1.5em',
          }}
        >
          <option value="medium">{t("agentSetup.approvalMedium")}</option>
          <option value="low">{t("agentSetup.approvalLow")}</option>
          <option value="high">{t("agentSetup.approvalHigh")}</option>
          <option value="never">{t("agentSetup.approvalNever")}</option>
        </select>
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.approvalDesc")}
        </p>
      </div>

      {/* Approval Timeout */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.approvalTimeout")}
        </label>
        <StyledInput
          type="number"
          min={0}
          max={3600}
          step={30}
          value={profile.approvalTimeoutSecs && profile.approvalTimeoutSecs > 0 ? profile.approvalTimeoutSecs : ""}
          onChange={(e) => {
            const v = e.target.value;
            setProfile(selectedAgentId, {
              approvalTimeoutSecs: v === "" ? undefined : Math.max(0, parseInt(v, 10) || 0),
            });
          }}
          placeholder="300 (5 min)"
          className="rounded-md bg-white dark:bg-zinc-800"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.approvalTimeoutDesc")}
        </p>
      </div>

      {/* Action buttons */}
      <div className="mt-4 border-t border-zinc-200 pt-3 dark:border-zinc-700 flex gap-3">
        <button
          onClick={handleApply}
          disabled={configSaving}
          className="flex-1 rounded btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
        >
          {configSaving ? t("agentSetup.applying") : t("agentSetup.applyToRuntime")}
        </button>
        <button
          onClick={() => setShowResetConfirm(true)}
          className="flex-1 rounded btn-solid px-3 py-1.5 text-xs font-medium"
        >
          {t("agentSetup.resetToDefaults")}
        </button>
      </div>

      <ConfirmDialog
        open={showResetConfirm}
        title={t("agentSetup.resetAgentSetup")}
        message={t("agentSetup.resetConfirm")}
        confirmLabel={t("agentSetup.reset")}
        destructive
        onConfirm={() => {
          resetProfile(selectedAgentId);
          setShowResetConfirm(false);
        }}
        onCancel={() => setShowResetConfirm(false)}
      />
    </div>
  );
}
