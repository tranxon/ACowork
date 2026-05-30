import { useState, useEffect, useCallback } from "react";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import type { AgentListResponse, GatewayConfig } from "../../lib/types";
import { cn } from "../../lib/utils";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { RadioGroup } from "../common/RadioGroup";
import { DEFAULT_GATEWAY_URL, getGatewayUrl } from "../../lib/config";
import { Bug, Monitor } from "lucide-react";
import { inputReadonly, selectBase, inputBase } from "../../lib/ui-styles";
import { ProfileTab } from "./ProfileTab";

type SettingsTab = "gateway" | "appearance" | "general" | "profile";

export function SettingsPage({ initialTab = "profile" }: { initialTab?: SettingsTab }) {
  const [activeTab, setActiveTab] = useState<SettingsTab>(initialTab);

  const tabs: { id: SettingsTab; label: string }[] = [
    { id: "profile", label: "My Profile" },
    { id: "general", label: "General" },
    { id: "appearance", label: "Appearance" },
    { id: "gateway", label: "Gateway" },
  ];

  return (
    <div
      className="flex flex-1 flex-col bg-zinc-50 dark:bg-zinc-900"
    >
      {/* Tabs */}
      <div className="flex gap-1 border-b border-zinc-200 px-6 pt-2 dark:border-zinc-800">
        {tabs.map((tab) => (
          <button
            key={tab.id}
            onClick={() => setActiveTab(tab.id)}
            className={cn(
              "border-b-2 px-3 py-2 text-sm transition-colors",
              activeTab === tab.id
                ? "border-[var(--color-accent)] font-semibold text-zinc-700 dark:text-zinc-200"
                : "border-transparent font-normal text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300",
            )}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {/* Tab content */}
      <div className="flex-1 overflow-y-auto p-6">
        {activeTab === "gateway" && <GatewayTab />}
        {activeTab === "appearance" && <AppearanceTab />}
        {activeTab === "general" && <GeneralTab />}
        {activeTab === "profile" && <ProfileTab />}
      </div>
    </div>
  );
}

/** Gateway connection settings */
function GatewayTab() {
  const { status, health, checkHealth } = useGatewayStore();
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);
  const setGatewayUrl = useSettingsStore((s) => s.setGatewayUrl);
  const [testing, setTesting] = useState(false);
  const [agents, setAgents] = useState<AgentListResponse[]>([]);
  const [agentsLoading, setAgentsLoading] = useState(false);
  const [urlDraft, setUrlDraft] = useState(gatewayUrl);

  // Sync draft when gatewayUrl changes externally
  useEffect(() => { setUrlDraft(gatewayUrl); }, [gatewayUrl]);

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

  const fetchAgents = useCallback(async () => {
    setAgentsLoading(true);
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/agents`);
      if (resp.ok) {
        const data: AgentListResponse[] = await resp.json();
        // Show only running/connected agents
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

  return (
    <div className="max-w-lg space-y-4">
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Gateway Connection</h2>

        <div className="space-y-3">
          <div>
            <label className="mb-1 block text-xs text-zinc-500">Gateway URL</label>
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
                  Apply
                </button>
              )}
            </div>
            <p className="mt-1 text-xs text-zinc-400">
              Set the Gateway HTTP API address. Default: {DEFAULT_GATEWAY_URL}
            </p>
          </div>

          <div className="flex items-center gap-2 text-xs">
            <span className="text-zinc-500">Status</span>
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
              {status === "connected" ? "Connected" : status === "error" ? "Error" : "Disconnected"}
            </span>
          </div>

          {health && (
            <>
              <div className="flex items-center gap-2 text-xs">
                <span className="text-zinc-500">Version</span>
                <span>{health.version}</span>
              </div>
            </>
          )}

          <button
            onClick={handleTest}
            disabled={testing}
            className="rounded-md btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
          >
            {testing ? "Testing..." : "Test Connection"}
          </button>
        </div>
      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Connected Agents</h2>

        {status !== "connected" ? (
          <p className="text-xs text-zinc-400">Connect to Gateway to see running agents</p>
        ) : agentsLoading ? (
          <p className="text-xs text-zinc-400">Loading...</p>
        ) : agents.length === 0 ? (
          <p className="text-xs text-zinc-400">No agents running</p>
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
    <div className="flex items-center justify-between rounded-lg border border-zinc-200 px-3 py-1.5 dark:border-zinc-700">
      <div className="flex items-center gap-2 min-w-0">
        <Monitor className="h-3.5 w-3.5 shrink-0 text-zinc-400" />
        <span className="text-xs font-medium truncate">{agent.name}</span>
        {agent.dev_mode && (
          <span className="inline-flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
            <Bug className="h-3 w-3" />
            Debug
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
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Theme</h2>
        <RadioGroup
          name="theme"
          value={theme}
          options={[
            { label: "Light", value: "light" as const },
            { label: "Dark", value: "dark" as const },
            { label: "System", value: "system" as const },
          ]}
          onChange={setTheme}
        />
      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Accent Color</h2>
        <p className="mb-3 text-xs text-zinc-500">全局高亮色</p>
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
            <button
              key={c.value}
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
              title={c.label}
            />
          ))}
        </div>
      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Content Width</h2>
        <p className="mb-2 text-xs text-zinc-500">聊天消息、工具调用、Thinking 的最大显示宽度</p>
        <RadioGroup
          name="contentWidth"
          value={contentWidth}
          options={contentWidths}
          onChange={setContentWidth}
          noWrap
        />
      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Font Size</h2>
        <RadioGroup
          name="fontSize"
          value={fontSize}
          options={fontSizes}
          onChange={setFontSize}
        />
      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Opacity</h2>
        <p className="mb-2 text-xs text-zinc-500">窗口透明度（需配合窗口毛玻璃效果使用）</p>
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

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <button
          onClick={() => setShowResetConfirm(true)}
          className="rounded-lg btn-solid px-3 py-[var(--ui-btn-py)] text-xs"
        >
          Reset to defaults
        </button>

        <ConfirmDialog
          open={showResetConfirm}
          title="Reset Appearance"
          message="确定要重置所有外观设置为默认值吗？包括主题、字体大小、内容宽度、透明度和高亮色。"
          confirmLabel="Reset"
          destructive
          onConfirm={() => {
            setTheme("system"); setFontSize(0.875); setContentWidth(90); setOpacity(1.0); setAccentColor("#3b82f6");
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
  const [config, setConfig] = useState<GatewayConfig | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [showDeleteConfirm, setShowDeleteConfirm] = useState(false);
  const { logLevel, setLogLevel, logFileSizeMb, setLogFileSizeMb } = useSettingsStore();

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
      })
      .catch(() => { });
  }, [setLogLevel, setLogFileSizeMb]);

  const currentLogLevel = config?.log_level || logLevel || "info";
  const currentLogFileSize = config?.log_file_size_mb ?? logFileSizeMb;

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
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Log setup</h2>

        {/* Log level */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-zinc-500 dark:text-zinc-400">
            Log Level
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
              className="w-[5.5rem] appearance-none rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
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
            Log File Size (MB)
          </label>
          <div className="flex items-center gap-2">
            <input
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
              className="w-16 rounded-md border border-zinc-200 px-2 py-[var(--ui-input-py)] text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
            />
            <span className="text-xs text-zinc-400">
              {currentLogFileSize === 0 ? "No split" : `Auto-split at ${currentLogFileSize} MB`}
            </span>
          </div>
          <p className="mt-1 text-[10px] text-zinc-400">
            0 = disable split. Files named as YYYYMMDD_HHMMSS.log
          </p>
        </div>

        {/* Delete all logs */}
        <button
          onClick={() => setShowDeleteConfirm(true)}
          disabled={deleting}
          className="rounded-lg btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-50"
        >
          {deleting ? "Deleting..." : "Delete all logs"}
        </button>

        {/* Delete confirmation dialog */}
        {showDeleteConfirm && (
          <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
            <div className="w-[380px] rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-800">
              <h3 className="mb-2 text-sm font-semibold">Delete all logs</h3>
              <p className="mb-4 text-xs text-zinc-500 dark:text-zinc-400">
                This will delete all log files from Gateway and all Agent workspaces.
                Running agents will also rotate their log files.
                This action cannot be undone.
              </p>
              <div className="flex justify-end gap-2">
                <button
                  onClick={() => setShowDeleteConfirm(false)}
                  className="rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
                >
                  Cancel
                </button>
                <button
                  onClick={handleDeleteLogs}
                  className="btn-accent rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium"
                >
                  Confirm Delete
                </button>
              </div>
            </div>
          </div>
        )}
      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Data Directory</h2>
        <input
          type="text"
          value={config?.data_dir ?? "\u2014"}
          readOnly
          className={`w-full ${inputReadonly}`}
        />
      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">About</h2>
        <div className="text-xs text-zinc-500 dark:text-zinc-400">
          <p>Rollball Desktop v0.1.0</p>
          <p className="mt-1">Built with Tauri v2 + React 19</p>
        </div>
      </div>

      {/* Reset Onboarding */}
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="text-xs font-medium">Reset Onboarding</h2>
        <p className="mt-1 text-xs text-zinc-500 dark:text-zinc-400">
          Reset the onboarding wizard to reconfigure your Gateway connection and API keys.
          Gateway models cache will be reloaded from disk (no re-download needed).
        </p>
        <button
          onClick={async () => {
            try {
              const { resetOnboarding } = await import("../../lib/gateway-api");
              await resetOnboarding();
              alert("Onboarding has been reset. Please reload the page to start again.");
              window.location.reload();
            } catch (e) {
              alert(`Failed to reset: ${e instanceof Error ? e.message : String(e)}`);
            }
          }}
          className="mt-3 rounded-md btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
        >
          Reset Onboarding
        </button>
      </div>
    </div>
  );
}
