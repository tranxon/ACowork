import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useAgentStore } from "../../stores/agentStore";
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";
import { getGatewayUrl } from "../../lib/config";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import {
  clearAgentAvatarCache,
  fetchAvatarAssets,
  fetchAvatarConfig,
  updateAvatarConfig,
  deleteAvatarFile,
  resolveAgentAvatarFileUrl,
} from "../../lib/avatar";
import type { AvatarAssetEntry, AvatarConfigResponse } from "../../lib/types";

// ── Helpers ───────────────────────────────────────────────────────────

/** Compute the next available avatar-XX filename (zero-padded to 2 digits). */
function nextAvatarName(assets: AvatarAssetEntry[], ext: string): string {
  const used = new Set<number>();
  for (const a of assets) {
    const fn = a.relative_path.split("/").pop() ?? "";
    const m = fn.match(/^avatar-(\d+)\./i);
    if (m) used.add(parseInt(m[1], 10));
  }
  let n = 1;
  while (used.has(n)) n++;
  return `avatar-${String(n).padStart(2, "0")}.${ext}`;
}

const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp", "svg"];

// ── Component ───────────────────────────────────────────────────────────

export function AgentSetupTab() {
  const { t } = useTranslation();
  const { agents, selectedAgentId, fetchAgents } = useAgentStore();
  const { getProfile, setProfile, resetProfile } = useAgentStore();

  const storage = selectedAgentId ? agents[selectedAgentId] : null;
  const selectedAgent = storage?.meta ?? null;
  const profile = selectedAgentId ? getProfile(selectedAgentId) : null;

  // Fetch agent runtime config from Gateway API on mount
  const [_configLoading, setConfigLoading] = useState(false);
  const [configSaving, setConfigSaving] = useState(false);
  const [showResetConfirm, setShowResetConfirm] = useState(false);

  // Avatar picker state (ADR-017)
  const [avatarTab, setAvatarTab] = useState<"custom" | "builtin">("custom");
  const [avatarPopupOpen, setAvatarPopupOpen] = useState(false);
  const [avatarAssets, setAvatarAssets] = useState<AvatarAssetEntry[]>([]);
  const [avatarConfig, setAvatarConfig] = useState<AvatarConfigResponse | null>(null);
  const [avatarBusy, setAvatarBusy] = useState(false);

  // Load avatar config + assets on mount and agent switch
  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;
    setAvatarAssets([]);
    setAvatarConfig(null);

    fetchAvatarConfig(selectedAgentId)
      .then((cfg) => { if (!cancelled) setAvatarConfig(cfg); })
      .catch((err) => { if (!cancelled) console.debug("[AgentSetup] Avatar config fetch failed:", err); });

    fetchAvatarAssets(selectedAgentId)
      .then((resp) => { if (!cancelled) setAvatarAssets(resp.assets); })
      .catch((err) => { if (!cancelled) console.debug("[AgentSetup] Avatar assets fetch failed:", err); });

    return () => { cancelled = true; };
  }, [selectedAgentId]);

  // Fetch agent runtime config on mount
  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;
    setConfigLoading(true);
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/config`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (cancelled || !data) return;
        setProfile(selectedAgentId, {
          maxTokens: data.max_output_tokens,
          maxIterations: data.max_iterations,
          shellApprovalThreshold: data.shell_approval_threshold,
          approvalTimeoutSecs: data.approval_timeout_secs ?? 300,
          globalMaxTokens: data.global_max_output_tokens,
          activeModel: data.model,
          activeProvider: data.provider,
        });
      })
      .catch((err) => {
        if (!cancelled) console.debug("[AgentSetup] Agent not ready:", err);
      })
      .finally(() => { if (!cancelled) setConfigLoading(false); });
    return () => { cancelled = true; };
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
      if (profile.shellApprovalThreshold) body.shell_approval_threshold = profile.shellApprovalThreshold;
      if (profile.approvalTimeoutSecs !== undefined && profile.approvalTimeoutSecs > 0) body.approval_timeout_secs = profile.approvalTimeoutSecs;
      const res = await fetch(
        `${getGatewayUrl()}/api/agents/${selectedAgentId}/config`,
        { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) },
      );
      if (!res.ok) console.warn("[AgentSetup] Config update failed:", res.status);
    } catch {
      // silently ignore network errors
    } finally {
      setConfigSaving(false);
    }
  };

  // ── Avatar selection handlers ──────────────────────────────────────

  const handleSelectCustom = async (relativePath: string) => {
    if (!selectedAgentId) return;
    setAvatarBusy(true);
    try {
      const cfg = await updateAvatarConfig(selectedAgentId, { avatar: relativePath, builtin_avatar: "" });
      setAvatarConfig(cfg);
      clearAgentAvatarCache(selectedAgentId);
      await fetchAgents();
    } catch (err) {
      console.warn("[AgentSetup] Select custom avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  const handleSelectBuiltin = async (iconId: string) => {
    if (!selectedAgentId) return;
    setAvatarBusy(true);
    try {
      const cfg = await updateAvatarConfig(selectedAgentId, { avatar: "", builtin_avatar: iconId });
      setAvatarConfig(cfg);
      clearAgentAvatarCache(selectedAgentId);
      await fetchAgents();
    } catch (err) {
      console.warn("[AgentSetup] Select builtin avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  // ── Avatar upload (does NOT auto-select) ──────────────────────────

  const handleUploadClick = async () => {
    if (!selectedAgentId) return;
    const selected = await openDialog({
      multiple: false,
      filters: [{ name: "Images", extensions: IMAGE_EXTENSIONS }],
    });
    if (!selected || typeof selected !== "string") return;

    const ext = selected.split(".").pop()?.toLowerCase() ?? "png";
    if (!IMAGE_EXTENSIONS.includes(ext)) return;

    const relative = `assets/${nextAvatarName(avatarAssets, ext)}`;
    setAvatarBusy(true);
    try {
      await invoke("upload_agent_file", {
        agentId: selectedAgentId,
        relativePath: relative,
        filePath: selected,
      });
      // Refresh assets list — user manually selects afterwards
      const resp = await fetchAvatarAssets(selectedAgentId);
      setAvatarAssets(resp.assets);
    } catch (err) {
      console.warn("[AgentSetup] Avatar upload failed:", err);
    } finally {
      setAvatarBusy(false);
    }
  };

  // ── Avatar delete ──────────────────────────────────────────────────

  const handleDeleteAvatar = async (relativePath: string) => {
    if (!selectedAgentId) return;
    setAvatarBusy(true);
    try {
      await deleteAvatarFile(selectedAgentId, relativePath);
      // Refresh both — backend clears avatar field if deleted file was current
      const [assetsResp, cfg] = await Promise.all([
        fetchAvatarAssets(selectedAgentId),
        fetchAvatarConfig(selectedAgentId),
      ]);
      setAvatarAssets(assetsResp.assets);
      setAvatarConfig(cfg);
      clearAgentAvatarCache(selectedAgentId);
      await fetchAgents();
    } catch (err) {
      console.warn("[AgentSetup] Delete avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
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
      {/* Avatar preview — click to open picker popup */}
      <div className="mb-3 flex items-center gap-3">
        <div className="relative">
          <button
            onClick={() => setAvatarPopupOpen((v) => !v)}
            className="relative block rounded-full ring-1 ring-zinc-300/60 transition hover:ring-zinc-400 dark:ring-zinc-600/60 dark:hover:ring-zinc-400"
          >
            <AgentAvatar
              agentId={selectedAgentId}
              avatarUrl={avatarConfig?.avatar ?? null}
              builtinAvatarId={avatarConfig?.builtin_avatar ?? null}
              version={selectedAgent.version}
              size={64}
            />
            {/* Pencil badge */}
            <span className="absolute -bottom-0.5 -right-0.5 flex h-5 w-5 items-center justify-center rounded-full bg-zinc-800 text-white shadow-sm dark:bg-zinc-600">
              <svg viewBox="0 0 16 16" className="h-3 w-3 fill-current" xmlns="http://www.w3.org/2000/svg">
                <path d="M11.013 1.427a1.75 1.75 0 0 1 2.474 0l1.086 1.086a1.75 1.75 0 0 1 0 2.474l-8.61 8.61c-.21.21-.47.364-.756.445l-3.251.93a.75.75 0 0 1-.927-.928l.929-3.25c.081-.286.235-.547.445-.758l8.61-8.61Zm.176 4.823L11.5 7l-3-3-.31.31a.75.75 0 0 0-.177.764l.93 3.251a.75.75 0 0 1-.927.928l-3.251-.93Z" />
              </svg>
            </span>
          </button>

          {/* Avatar picker popup */}
          {avatarPopupOpen && (
            <>
              {/* Click-outside overlay */}
              <div
                className="fixed inset-0 z-40"
                onClick={() => setAvatarPopupOpen(false)}
              />
              <div className="absolute left-0 top-full z-50 mt-2 w-72 rounded-lg border border-zinc-200 bg-white p-3 shadow-lg dark:border-zinc-700 dark:bg-zinc-800">
                {/* Tabs */}
                <div className="mb-3 flex gap-1 border-b border-zinc-200 dark:border-zinc-700">
                  <button
                    onClick={() => setAvatarTab("custom")}
                    className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "custom"
                      ? "border-b-2 border-zinc-800 text-zinc-800 dark:border-zinc-200 dark:text-zinc-200"
                      : "text-zinc-400 hover:text-zinc-600 dark:text-zinc-500"
                      }`}
                  >
                    Custom
                  </button>
                  <button
                    onClick={() => setAvatarTab("builtin")}
                    className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "builtin"
                      ? "border-b-2 border-zinc-800 text-zinc-800 dark:border-zinc-200 dark:text-zinc-200"
                      : "text-zinc-400 hover:text-zinc-600 dark:text-zinc-500"
                      }`}
                  >
                    Builtin
                  </button>
                </div>

                {/* Custom tab */}
                {avatarTab === "custom" && (
                  <div className="grid grid-cols-4 gap-2">
                    <button
                      onClick={handleUploadClick}
                      disabled={avatarBusy}
                      className="flex aspect-square items-center justify-center rounded-md border border-dashed border-zinc-300 text-zinc-400 transition-colors hover:border-zinc-400 hover:text-zinc-600 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-500 dark:hover:border-zinc-400"
                    >
                      <span className="text-lg">+</span>
                    </button>
                    {avatarAssets.map((asset) => {
                      const isSelected = avatarConfig?.avatar === asset.relative_path;
                      return (
                        <div
                          key={asset.relative_path}
                          className={`group relative aspect-square overflow-hidden rounded-md border-2 transition-colors ${isSelected
                            ? "border-zinc-800 dark:border-zinc-200"
                            : "border-transparent hover:border-zinc-300 dark:hover:border-zinc-600"
                            }`}
                        >
                          <img
                            src={resolveAgentAvatarFileUrl(selectedAgentId, asset.relative_path)}
                            alt={asset.relative_path}
                            draggable={false}
                            className="h-full w-full cursor-pointer object-cover"
                            onClick={() => handleSelectCustom(asset.relative_path)}
                          />
                          <button
                            onClick={(e) => {
                              e.stopPropagation();
                              handleDeleteAvatar(asset.relative_path);
                            }}
                            disabled={avatarBusy}
                            className="absolute right-0.5 top-0.5 flex h-4 w-4 items-center justify-center rounded bg-red-500/80 text-[8px] text-white opacity-0 transition-opacity group-hover:opacity-100"
                          >
                            ×
                          </button>
                        </div>
                      );
                    })}
                  </div>
                )}

                {/* Builtin tab */}
                {avatarTab === "builtin" && (
                  <div className="grid grid-cols-4 gap-2">
                    {BUILTIN_ICON_IDS.map((iconId) => {
                      const isSelected = avatarConfig?.builtin_avatar === iconId;
                      return (
                        <button
                          key={iconId}
                          onClick={() => handleSelectBuiltin(iconId)}
                          disabled={avatarBusy}
                          className={`flex items-center justify-center rounded-md p-1 transition-colors ${isSelected
                            ? "bg-zinc-200 dark:bg-zinc-600"
                            : "hover:bg-zinc-100 dark:hover:bg-zinc-700"
                            }`}
                        >
                          <img
                            src={BUILTIN_ICONS[iconId] ?? ""}
                            alt={iconId}
                            draggable={false}
                            className="h-12 w-12 rounded-full object-cover"
                          />
                        </button>
                      );
                    })}
                  </div>
                )}
              </div>
            </>
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
