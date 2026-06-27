import { useState, useEffect, useCallback } from "react";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { AgentListResponse, GatewayConfig, GatewayMode } from "../../lib/types";
import { cn } from "../../lib/utils";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { RadioGroup } from "../common/RadioGroup";
import { DEFAULT_GATEWAY_URL, getGatewayUrl, DEFAULT_THEME, DEFAULT_FONT_SIZE, DEFAULT_CONTENT_WIDTH, DEFAULT_OPACITY, DEFAULT_ACCENT_COLOR } from "../../lib/config";
import { Bug, Monitor } from "lucide-react";
import { inputReadonly, inputBase } from "../../lib/ui-styles";
import { StyledInput } from "../common/StyledInput";
import { ProfileTab } from "./ProfileTab";
import { TabButton } from "../common/tab";
import { Tooltip } from "../common/Tooltip";

type SettingsTab = "gateway" | "appearance" | "general" | "profile";

export function SettingsPage({ initialTab = "profile" }: { initialTab?: SettingsTab }) {
  const { t } = useTranslation();
  const [activeTab, setActiveTab] = useState<SettingsTab>(initialTab);

  const tabs: { id: SettingsTab; label: string }[] = [
    { id: "profile", label: t("settings.tabProfile") },
    { id: "general", label: t("settings.tabGeneral") },
    { id: "appearance", label: t("settings.tabAppearance") },
    { id: "gateway", label: t("settings.tabGateway") },
  ];

  return (
    <div
      className="flex flex-1 flex-col bg-zinc-50 dark:bg-zinc-900"
    >
      {/* Tabs */}
      <div className="flex gap-1 border-b border-zinc-200 px-6 pt-2 dark:border-zinc-800">
        {tabs.map((tab) => (
          <TabButton
            key={tab.id}
            onClick={() => setActiveTab(tab.id)}
            active={activeTab === tab.id}
          >
            {tab.label}
          </TabButton>
        ))}
      </div>

      {/* Tab content — CSS visibility preserves component state across tab switches */}
      <div className="flex-1 overflow-y-auto p-6">
        <div style={{ display: activeTab === "gateway" ? "block" : "none" }}><GatewayTab /></div>
        <div style={{ display: activeTab === "appearance" ? "block" : "none" }}><AppearanceTab /></div>
        <div style={{ display: activeTab === "general" ? "block" : "none" }}><GeneralTab /></div>
        <div style={{ display: activeTab === "profile" ? "block" : "none" }}><ProfileTab /></div>
      </div>
    </div>
  );
}

/** Gateway connection settings */
function GatewayTab() {
  const { t } = useTranslation();
  const { status, health, localState, checkHealth, checkLocalStatus, startLocalGateway, stopLocalGateway } = useGatewayStore();
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);
  const setGatewayUrl = useSettingsStore((s) => s.setGatewayUrl);
  const gatewayMode = useSettingsStore((s) => s.gatewayMode);
  const setGatewayMode = useSettingsStore((s) => s.setGatewayMode);
  const [testing, setTesting] = useState(false);
  const [agents, setAgents] = useState<AgentListResponse[]>([]);
  const [agentsLoading, setAgentsLoading] = useState(false);
  const [urlDraft, setUrlDraft] = useState(gatewayUrl);
  const [starting, setStarting] = useState(false);
  const [stopping, setStopping] = useState(false);

  // Sync draft when gatewayUrl changes externally
  useEffect(() => { setUrlDraft(gatewayUrl); }, [gatewayUrl]);

  // On mount, sync both the HTTP health status and the local process
  // handle from the Rust side. This covers the case where the Gateway
  // was spawned by the SplashScreen boot path (which bypasses the store
  // action) — without this, `localState` would stay "idle" and the UI
  // would show a spurious "Start Gateway" button next to "Running".
  useEffect(() => {
    checkHealth();
    checkLocalStatus();
  }, [checkHealth, checkLocalStatus]);

  const handleModeChange = useCallback((mode: GatewayMode) => {
    setGatewayMode(mode);
  }, [setGatewayMode]);

  const handleUrlSave = useCallback(() => {
    const trimmed = urlDraft.trim();
    if (trimmed && trimmed !== gatewayUrl) {
      setGatewayUrl(trimmed);
    } else if (!trimmed) {
      setUrlDraft(gatewayUrl);
    }
  }, [urlDraft, gatewayUrl, setGatewayUrl]);

  const handleUrlKeyDown = useCallback((e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      handleUrlSave();
    }
  }, [handleUrlSave]);

  const handleTest = useCallback(async () => {
    setTesting(true);
    await checkHealth();
    setTesting(false);
  }, [checkHealth]);

  const handleStartLocal = useCallback(async () => {
    setStarting(true);
    try {
      await startLocalGateway();
    } catch {
      // Error handled by store
    } finally {
      setStarting(false);
    }
  }, [startLocalGateway]);

  const handleStopLocal = useCallback(async () => {
    setStopping(true);
    try {
      await stopLocalGateway();
    } catch {
      // Error handled by store
    } finally {
      setStopping(false);
    }
  }, [stopLocalGateway]);

  const handleRestartLocal = useCallback(async () => {
    setStarting(true);
    try {
      await stopLocalGateway();
      await startLocalGateway();
    } catch {
      // Error handled by store
    } finally {
      setStarting(false);
    }
  }, [startLocalGateway, stopLocalGateway]);

  const fetchAgents = useCallback(async () => {
    setAgentsLoading(true);
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents`);
      if (resp.ok) {
        const data: AgentListResponse[] = await resp.json();
        setAgents(data.filter(a => a.running || a.connected));
      }
    } catch {
      // Gateway not reachable
    } finally {
      setAgentsLoading(false);
    }
  }, []);

  useEffect(() => {
    if (status === "connected") {
      fetchAgents();
    }
  }, [status, fetchAgents]);

  const localIsRunning = gatewayMode === "local" && (status === "connected" || localState === "running");
  const localIsStarting = gatewayMode === "local" && (localState === "starting" || starting) && status !== "connected";
  // Whether Tauri itself owns the local Gateway process. Only in that case can
  // the user stop/restart it via the UI. If a Gateway is reachable but was
  // started outside of Tauri, we still show "Running" but no stop/restart buttons.
  const localIsTauriManaged = localState === "running";

  return (
    <div className="max-w-lg space-y-4">
      {/* Mode selection */}
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.gatewayMode")}</h2>
        <RadioGroup
          name="gatewayMode"
          value={gatewayMode}
          options={[
            { label: t("settings.local"), value: "local" as GatewayMode },
            { label: t("settings.remote"), value: "remote" as GatewayMode },
          ]}
          onChange={handleModeChange}
        />
      </div>

      {/* Local mode: status + controls */}
      {gatewayMode === "local" && (
        <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
          <h2 className="mb-3 text-xs font-medium">{t("settings.localGateway")}</h2>

          <div className="flex items-center gap-2 text-xs">
            <span className="text-zinc-500">{t("settings.status")}</span>
            <span
              className={cn(
                "h-2 w-2 rounded-full",
                localIsRunning ? "bg-[var(--color-accent)]" : localIsStarting ? "bg-amber-500" : "bg-zinc-400",
              )}
            />
            <span className={cn(
              localIsRunning ? "text-[var(--color-accent)]" :
                localIsStarting ? "text-amber-600 dark:text-amber-400" :
                  "text-zinc-500"
            )}>
              {localIsRunning ? t("settings.running") : localIsStarting ? t("settings.starting") : t("settings.stopped")}
            </span>
          </div>

          {health && localIsRunning && (
            <div className="mt-2 flex items-center gap-2 text-xs">
              <span className="text-zinc-500">{t("settings.version")}</span>
              <span>{health.version}</span>
            </div>
          )}

          <div className="mt-3 flex gap-2">
            {!localIsTauriManaged && !localIsStarting && (
              <button
                onClick={handleStartLocal}
                disabled={starting}
                className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
              >
                {starting ? t("settings.starting") : t("settings.startGateway")}
              </button>
            )}
            {localIsTauriManaged && (
              <>
                <button
                  onClick={handleRestartLocal}
                  disabled={starting}
                  className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
                >
                  {starting ? t("settings.restarting") : t("settings.restart")}
                </button>
                <button
                  onClick={handleStopLocal}
                  disabled={stopping}
                  className="rounded-md border border-zinc-300 px-3 py-[var(--ui-btn-py)] text-xs font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700 disabled:opacity-50"
                >
                  {stopping ? t("settings.stopping") : t("settings.stop")}
                </button>
              </>
            )}
          </div>
          {localIsRunning && !localIsTauriManaged && (
            <p className="mt-2 text-[10px] text-zinc-500 dark:text-zinc-400">
              {t("settings.gatewayRunningExternal")}
            </p>
          )}
        </div>
      )}

      {/* Remote mode: URL + test */}
      {gatewayMode === "remote" && (
        <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
          <h2 className="mb-3 text-xs font-medium">{t("settings.gatewayConnection")}</h2>

          <div className="space-y-3">
            <div>
              <label className="mb-1 block text-xs text-zinc-500">{t("settings.gatewayUrl")}</label>
              <div className="flex gap-2">
                <input
                  type="text"
                  value={urlDraft}
                  onChange={(e) => setUrlDraft(e.target.value)}
                  onBlur={handleUrlSave}
                  onKeyDown={handleUrlKeyDown}
                  placeholder={DEFAULT_GATEWAY_URL}
                  className={`flex-1 ${inputBase}`}
                />
                {urlDraft !== gatewayUrl && (
                  <button
                    onClick={handleUrlSave}
                    className="rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium text-white hover:opacity-90" style={{ backgroundColor: "var(--color-accent)" }}
                  >
                    {t("settings.apply")}
                  </button>
                )}
              </div>
            </div>

            <div className="flex items-center gap-2 text-xs">
              <span className="text-zinc-500">{t("settings.status")}</span>
              <span
                className={cn(
                  "h-2 w-2 rounded-full",
                  status === "connected" ? "bg-[var(--color-accent)]" : status === "error" ? "bg-red-500" : "bg-zinc-400",
                )}
              />
              <span className={cn(
                status === "connected" ? "text-[var(--color-accent)]" :
                  status === "error" ? "text-red-600 dark:text-red-400" :
                    "text-zinc-500"
              )}>
                {status === "connected" ? t("settings.connected") : status === "error" ? t("settings.error") : t("settings.disconnected")}
              </span>
            </div>

            {health && (
              <div className="flex items-center gap-2 text-xs">
                <span className="text-zinc-500">{t("settings.version")}</span>
                <span>{health.version}</span>
              </div>
            )}

            <button
              onClick={handleTest}
              disabled={testing || !urlDraft.trim()}
              className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
            >
              {testing ? t("settings.testing") : t("settings.testConnection")}
            </button>
          </div>
        </div>
      )}

      {/* Connected Agents (shared between modes) */}
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.connectedAgents")}</h2>

        {status !== "connected" ? (
          <p className="text-xs text-zinc-400">{t("settings.connectToSeeAgents")}</p>
        ) : agentsLoading ? (
          <p className="text-xs text-zinc-400">{t("settings.loading")}</p>
        ) : agents.length === 0 ? (
          <p className="text-xs text-zinc-400">{t("settings.noAgentsRunning")}</p>
        ) : (
          <div className="space-y-1">
            {agents.map((agent) => (
              <RuntimeRow key={agent.agent_id} agent={agent} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

/** Single runtime row component — fetches model info independently */
function RuntimeRow({ agent }: { agent: AgentListResponse }) {
  const { t } = useTranslation();
  const [modelInfo, setModelInfo] = useState<{ provider: string; model: string } | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetch(`${getGatewayUrl()}/api/agents/${agent.agent_id}/model`)
      .then(r => r.ok ? r.json() : null)
      .then(data => {
        if (!cancelled && data) {
          setModelInfo({ provider: data.provider, model: data.model });
        }
      })
      .catch(() => { });
    return () => { cancelled = true; };
  }, [agent.agent_id]);

  return (
    <div className="flex items-center justify-between rounded-md border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
      <div className="flex items-center gap-2 min-w-0">
        <Monitor className="h-3.5 w-3.5 shrink-0 text-zinc-400" />
        <span className="text-xs font-medium truncate">{agent.name}</span>
        {agent.dev_mode && (
          <span className="inline-flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
            <Bug className="h-3 w-3" />
            {t("settings.debug")}
          </span>
        )}
      </div>
      <div className="flex items-center gap-2 shrink-0">
        {modelInfo ? (
          <span className="text-xs text-zinc-500">{modelInfo.provider}/{modelInfo.model}</span>
        ) : (
          <span className="text-xs text-zinc-400">—</span>
        )}
      </div>
    </div>
  );
}

/** Appearance settings */
function AppearanceTab() {
  const { t } = useTranslation();
  const { theme, setTheme, fontSize, setFontSize, contentWidth, setContentWidth, opacity, setOpacity, accentColor, setAccentColor } = useSettingsStore();
  const [showResetConfirm, setShowResetConfirm] = useState(false);

  // Content width options: 40-100%, step 10
  const contentWidths = [
    { label: "40%", value: 40 },
    { label: "50%", value: 50 },
    { label: "60%", value: 60 },
    { label: "70%", value: 70 },
    { label: "80%", value: 80 },
    { label: "90%", value: 90 },
    { label: "100%", value: 100 },
  ];

  // Font size options: M = previous default (text-sm = 0.875rem)
  const fontSizes = [
    { label: "S", value: 0.75 },
    { label: "M", value: 0.875 },
    { label: "L", value: 1.0 },
    { label: "XL", value: 1.125 },
    { label: "XXL", value: 1.25 },
  ];

  return (
    <div className="w-fit space-y-4">
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.theme")}</h2>
        <RadioGroup
          name="theme"
          value={theme}
          options={[
            { label: t("settings.light"), value: "light" as const },
            { label: t("settings.dark"), value: "dark" as const },
            { label: t("settings.system"), value: "system" as const },
          ]}
          onChange={setTheme}
        />
      </div>

      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.accentColor")}</h2>
        <p className="mb-3 text-xs text-zinc-500">{t("settings.accentColor")}</p>
        <div className="flex flex-wrap gap-[14px]">
          {[
            // Cool tones
            { label: "Blue", value: "#3b82f6" },
            { label: "Indigo", value: "#6366f1" },
            { label: "Violet", value: "#8b5cf6" },
            { label: "Cyan", value: "#06b6d4" },
            { label: "Teal", value: "#14b8a6" },
            // Warm tones
            { label: "Green", value: "#00C375" },
            { label: "Rose", value: "#f43f5e" },
            { label: "Orange", value: "#f97316" },
            { label: "Amber", value: "#f59e0b" },
            { label: "Pink", value: "#ec4899" },
          ].map((c) => (
            <Tooltip content={c.label} variant="plain" key={c.value}>
              <button
                onClick={() => setAccentColor(c.value)}
                className={cn(
                  "flex h-9 w-9 items-center justify-center rounded-full transition-transform",
                  accentColor === c.value
                    ? "scale-110 ring-2 ring-offset-2 ring-offset-white dark:ring-offset-zinc-900"
                    : "hover:scale-105",
                )}
                style={{
                  backgroundColor: c.value,
                  "--tw-ring-color": c.value,
                } as React.CSSProperties}
              />
            </Tooltip>
          ))}
        </div>
      </div>

      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.contentWidth")}</h2>
        <p className="mb-2 text-xs text-zinc-500">{t("settings.contentWidthHint")}</p>
        <RadioGroup
          name="contentWidth"
          value={contentWidth}
          options={contentWidths}
          onChange={setContentWidth}
          noWrap
        />
      </div>

      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.fontSize")}</h2>
        <RadioGroup
          name="fontSize"
          value={fontSize}
          options={fontSizes}
          onChange={setFontSize}
        />
      </div>

      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.opacity")}</h2>
        <p className="mb-2 text-xs text-zinc-500">{t("settings.opacityHint")}</p>
        <div className="flex items-center gap-3">
          <input
            type="range"
            min="0"
            max="1.0"
            step="0.01"
            value={opacity}
            onChange={(e) => setOpacity(parseFloat(e.target.value))}
            className="flex-1"
            style={{ "--progress": `${opacity * 100}%` } as React.CSSProperties}
          />
          <span className="w-10 text-right text-xs text-zinc-600 dark:text-zinc-400">
            {Math.round(opacity * 100)}%
          </span>
        </div>
      </div>

      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <button
          onClick={() => setShowResetConfirm(true)}
          className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs"
        >
          {t("settings.resetToDefaults")}
        </button>

        <ConfirmDialog
          open={showResetConfirm}
          title={t("settings.resetAppearance")}
          message={t("settings.resetAppearanceConfirm")}
          confirmLabel={t("settings.reset")}
          destructive
          onConfirm={() => {
            setTheme(DEFAULT_THEME); setFontSize(DEFAULT_FONT_SIZE); setContentWidth(DEFAULT_CONTENT_WIDTH); setOpacity(DEFAULT_OPACITY); setAccentColor(DEFAULT_ACCENT_COLOR);
            setShowResetConfirm(false);
          }}
          onCancel={() => setShowResetConfirm(false)}
        />
      </div>
    </div>
  );
}

/** General settings */
function GeneralTab() {
  const { t } = useTranslation();
  const [config, setConfig] = useState<GatewayConfig | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [showDeleteConfirm, setShowDeleteConfirm] = useState(false);
  const [showResetOnboardingConfirm, setShowResetOnboardingConfirm] = useState(false);
  const { logLevel, setLogLevel, logFileSizeMb, setLogFileSizeMb, logFileCount, setLogFileCount } = useSettingsStore();

  useEffect(() => {
    fetch(`${getGatewayUrl()}/api/config`)
      .then((r) => { if (!r.ok) throw new Error(`HTTP ${r.status}`); return r.json(); })
      .then((cfg: GatewayConfig) => {
        setConfig(cfg);
        // Gateway value takes precedence over localStorage
        setLogLevel(cfg.log_level);
        if (cfg.log_file_size_mb !== undefined) {
          setLogFileSizeMb(cfg.log_file_size_mb);
        }
        if (cfg.log_file_count !== undefined) {
          setLogFileCount(cfg.log_file_count);
        }
      })
      .catch(() => { });
  }, [setLogLevel, setLogFileSizeMb, setLogFileCount]);

  const currentLogLevel = config?.log_level || logLevel || "info";
  const currentLogFileSize = config?.log_file_size_mb ?? logFileSizeMb;
  const currentLogFileCount = config?.log_file_count ?? logFileCount;

  const handleDeleteLogs = async () => {
    setShowDeleteConfirm(false);
    setDeleting(true);
    try {
      await fetch(`${getGatewayUrl()}/api/logs`, { method: "DELETE" });
    } catch { /* ignore */ }
    finally { setDeleting(false); }
  };

  return (
    <div className="max-w-lg space-y-4">
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.logSetup")}</h2>

        {/* Log level */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-zinc-500 dark:text-zinc-400">
            {t("settings.logLevel")}
          </label>
          <div>
            <select
              value={currentLogLevel}
              onChange={async (e) => {
                const val = e.target.value;
                try {
                  await fetch(`${getGatewayUrl()}/api/config`, {
                    method: "PUT",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ log_level: val }),
                  });
                  setConfig((prev) => (prev ? { ...prev, log_level: val } : prev));
                  setLogLevel(val);
                } catch { /* ignore */ }
              }}
              className="w-[5.5rem] appearance-none rounded border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 outline-none transition-colors focus:border-[var(--color-accent)] dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
              style={{
                backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
                backgroundPosition: 'right 0.5rem center',
                backgroundRepeat: 'no-repeat',
                backgroundSize: '1.5em 1.5em',
              }}
            >
              <option value="trace">trace</option>
              <option value="debug">debug</option>
              <option value="info">info</option>
              <option value="warn">warn</option>
              <option value="error">error</option>
            </select>
          </div>
        </div>

        {/* Log file size */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-zinc-500 dark:text-zinc-400">
            {t("settings.logFileSize")}
          </label>
          <div className="flex items-center gap-2">
            <StyledInput
              type="number"
              min={0}
              max={1024}
              value={currentLogFileSize}
              onChange={async (e) => {
                const val = Math.max(0, parseInt(e.target.value, 10) || 0);
                setLogFileSizeMb(val);
                try {
                  await fetch(`${getGatewayUrl()}/api/config`, {
                    method: "PUT",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ log_file_size_mb: val }),
                  });
                  setConfig((prev) => (prev ? { ...prev, log_file_size_mb: val } : prev));
                } catch { /* ignore */ }
              }}
              className="w-16"
            />
            <span className="text-xs text-zinc-400">
              {currentLogFileSize === 0 ? t("settings.noSplit") : t("settings.autoSplit", { size: currentLogFileSize })}
            </span>
          </div>
          <p className="mt-1 text-[10px] text-zinc-400">
            {t("settings.logFileSizeHint")}
          </p>
        </div>

        {/* Max log file count */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-zinc-500 dark:text-zinc-400">
            {t("settings.maxLogFiles")}
          </label>
          <div className="flex items-center gap-2">
            <StyledInput
              type="number"
              min={0}
              max={999}
              value={currentLogFileCount}
              onChange={async (e) => {
                const val = Math.max(0, parseInt(e.target.value, 10) || 0);
                setLogFileCount(val);
                try {
                  await fetch(`${getGatewayUrl()}/api/config`, {
                    method: "PUT",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ log_file_count: val }),
                  });
                  setConfig((prev) => (prev ? { ...prev, log_file_count: val } : prev));
                } catch { /* ignore */ }
              }}
              className="w-16"
            />
            <span className="text-xs text-zinc-400">
              {currentLogFileCount === 0 ? t("settings.unlimited") : t("settings.keepFiles", { count: currentLogFileCount })}
            </span>
          </div>
          <p className="mt-1 text-[10px] text-zinc-400">
            {t("settings.maxLogFilesHint")}
          </p>
        </div>

        {/* Delete all logs */}
        <button
          onClick={() => setShowDeleteConfirm(true)}
          disabled={deleting}
          className="rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
        >
          {deleting ? t("settings.deleting") : t("settings.deleteAllLogs")}
        </button>

        {/* Delete confirmation dialog */}
        {showDeleteConfirm && (
          <div className="fixed inset-0 z-50 flex items-cell justify-center bg-black/50">
            <div className="w-[380px] rounded-md bg-white p-6 shadow-xl dark:bg-zinc-800">
              <h3 className="mb-2 text-sm font-semibold">{t("settings.deleteLogsConfirmTitle")}</h3>
              <p className="mb-4 text-xs text-zinc-500 dark:text-zinc-400">
                {t("settings.deleteLogsConfirmMsg")}
              </p>
              <div className="flex justify-end gap-2">
                <button
                  onClick={() => setShowDeleteConfirm(false)}
                  className="rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
                >
                  {t("common.cancel")}
                </button>
                <button
                  onClick={handleDeleteLogs}
                  className="btn-accent rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium"
                >
                  {t("settings.confirmDelete")}
                </button>
              </div>
            </div>
          </div>
        )}
      </div>

      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.dataDirectory")}</h2>
        <input
          type="text"
          value={config?.data_dir ?? "\u2014"}
          readOnly
          className={`w-full ${inputReadonly}`}
        />
      </div>

      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.about")}</h2>
        <div className="text-xs text-zinc-500 dark:text-zinc-400">
          <p>ACowork Desktop v0.1.0</p>
          <p className="mt-1">Built with Tauri v2 + React 19</p>
        </div>
      </div>

      {/* Reset Onboarding */}
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="text-xs font-medium">{t("settings.resetOnboarding")}</h2>
        <p className="mt-1 text-xs text-zinc-500 dark:text-zinc-400">
          {t("settings.resetOnboardingDesc")}
        </p>
        <button
          onClick={() => setShowResetOnboardingConfirm(true)}
          className="mt-3 rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
        >
          {t("settings.resetOnboardingBtn")}
        </button>

        <ConfirmDialog
          open={showResetOnboardingConfirm}
          title={t("settings.resetOnboarding")}
          message={t("settings.resetOnboardingConfirm")}
          confirmLabel={t("settings.reset")}
          destructive
          onConfirm={async () => {
            setShowResetOnboardingConfirm(false);
            try {
              const { resetOnboarding } = await import("../../lib/gateway-api");
              await resetOnboarding();
            } catch (e) {
              console.error("Failed to reset onboarding:", e);
            }
            window.location.reload();
          }}
          onCancel={() => setShowResetOnboardingConfirm(false)}
        />
      </div>
    </div>
  );
}
