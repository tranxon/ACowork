import { useEffect, useState } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useAgentProfileStore } from "../../stores/agentProfileStore";
import { UserAvatar, BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { getGatewayUrl } from "../../lib/config";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { useMcpStore } from "../../stores/mcpStore";

// ── Types ───────────────────────────────────────────────────────────────

interface AvailableTool {
  name: string;
  description: string;
  required_permissions: string[];
  always_on?: boolean;
}

interface ToolsResponse {
  agent_id: string;
  tools: AvailableTool[];
  active_tools: string[];
}

// ── Component ───────────────────────────────────────────────────────────

export function AgentSetupTab() {
  const { agents, selectedAgentId } = useAgentStore();
  const { getProfile, setProfile, resetProfile } = useAgentProfileStore();

  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);
  const profile = selectedAgentId ? getProfile(selectedAgentId) : null;

  // Fetch agent runtime config from Gateway API on mount
  const [_configLoading, setConfigLoading] = useState(false);
  const [configSaving, setConfigSaving] = useState(false);
  const [iconOpen, setIconOpen] = useState(false);
  const [showResetConfirm, setShowResetConfirm] = useState(false);

  // Tools configuration
  const [availableTools, setAvailableTools] = useState<AvailableTool[]>([]);
  const [activeTools, setActiveTools] = useState<string[]>([]);
  const [toolsLoading, setToolsLoading] = useState(false);

  // MCP server activation
  const { catalog, activeServers, loadCatalog, loadActiveServers, toggleServer, activationLoading } = useMcpStore();

  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;
    setConfigLoading(true);
    setToolsLoading(true);
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
          globalMaxTokens: data.global_max_output_tokens,
          availableModels: data.available_models,
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
    // Fetch tools
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/tools`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data: ToolsResponse | null) => {
        if (cancelled || !data) return;
        setAvailableTools(data.tools);
        setActiveTools(data.active_tools);
      })
      .catch((err) => {
          if (!cancelled) {
            // 503 = agent not ready yet, silently retry on next mount
            console.debug("[AgentSetup] Agent not ready:", err);
          }
        })
      .finally(() => {
        if (!cancelled) setToolsLoading(false);
      });
    // Load MCP catalog and per-agent activation
    loadCatalog();
    loadActiveServers(selectedAgentId);
    return () => {
      cancelled = true;
    };
  }, [selectedAgentId]);

  // Listen for global resource refresh events (triggered when provider
  // or MCP catalog changes) and re-fetch the active agent config.
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
              globalMaxTokens: data.global_max_output_tokens,
              availableModels: data.available_models,
              activeModel: data.model,
              activeProvider: data.provider,
            });
          })
          .catch(() => {});
      }
    };
    window.addEventListener('rollball:refresh-agent-config', handler);
    return () => window.removeEventListener('rollball:refresh-agent-config', handler);
  }, [selectedAgentId]);

  // Apply config to Gateway
  const handleApply = async () => {
    if (!selectedAgentId || !profile) return;
    setConfigSaving(true);
    try {
      const body: Record<string, unknown> = {};
      if (profile.maxTokens && profile.maxTokens > 0) body.max_output_tokens = profile.maxTokens;
      if (profile.maxIterations && profile.maxIterations > 0) body.max_iterations = profile.maxIterations;
      if (profile.temperature !== undefined) body.temperature = profile.temperature;
      if (profile.shellApprovalThreshold) body.shell_approval_threshold = profile.shellApprovalThreshold;
      // Always send active_tools (even empty array = disable all tools)
      if (activeTools.length >= 0) body.active_tools = activeTools;

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
        <span className="text-xs text-zinc-400 dark:text-zinc-500">No agent selected</span>
      </div>
    );
  }

  const agentName = profile.displayName ?? selectedAgent.name ?? selectedAgentId;

  return (
    <div className="flex-1 overflow-y-auto p-3">
      {/* Avatar preview — click to open icon picker */}
      <div className="mb-4 flex items-center gap-3">
        <div className="relative">
          <button
            onClick={() => setIconOpen(!iconOpen)}
            className="rounded-lg border border-transparent p-0.5 transition-colors hover:border-zinc-300 dark:hover:border-zinc-600"
            title="Choose icon"
          >
            <UserAvatar
              displayName={agentName}
              avatarType="icon"
              avatarIcon={profile.avatarIconId ?? undefined}
              size={64}
            />
          </button>
          {iconOpen && (
            <div className="absolute left-0 z-50 mt-1 w-max rounded-lg border border-zinc-200 bg-white p-1.5 shadow-lg dark:border-zinc-700 dark:bg-zinc-800">
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
        <div>
          <p className="text-sm font-medium text-zinc-800 dark:text-zinc-200">
            {agentName}
          </p>
          <p className="text-[10px] text-zinc-400 dark:text-zinc-500">
            {selectedAgentId}
          </p>
        </div>
      </div>

      {/* Agent Name */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          Agent Name
        </label>
        <input
          type="text"
          value={profile.displayName ?? selectedAgent.name ?? ""}
          onChange={(e) =>
            setProfile(selectedAgentId, { displayName: e.target.value || undefined })
          }
          placeholder={selectedAgent.name ?? "Agent name"}
          className="w-full rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200 dark:placeholder:text-zinc-500"
        />
      </div>

      {/* Max Output Tokens */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          Max Output Tokens
        </label>
        <input
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
          placeholder={`${profile.globalMaxTokens ?? 32768} (gateway limit)`}
          className="w-full rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          Leave empty to use runtime default
        </p>
      </div>

      {/* Active Model / Provider (informational) */}
      {(profile.activeModel || profile.activeProvider) ? (
        <div className="mb-3 space-y-1">
          <span className="text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
            Active Model
          </span>
          <div className="rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 dark:border-zinc-700 dark:bg-zinc-800">
            <span className="text-xs text-zinc-700 dark:text-zinc-300">
              {profile.activeProvider ?? ""}{profile.activeProvider ? " / " : ""}{profile.activeModel ?? ""}
            </span>
            {profile.availableModels && profile.availableModels.length > 0 && (
              <span className="ml-2 text-[9px] text-zinc-400 dark:text-zinc-500">
                ({profile.availableModels.length} models available)
              </span>
            )}
          </div>
        </div>
      ) : null}

      {/* Max Iterations */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          Max Iterations (per run)
        </label>
        <input
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
          placeholder="50 (default)"
          className="w-full rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          Leave empty to use runtime default
        </p>
      </div>

      {/* Temperature */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          Temperature: {profile.temperature ?? 0.7}
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
          <span>0 (deterministic)</span>
          <span>2 (creative)</span>
        </div>
      </div>

      {/* Shell Command Approval Threshold */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          Shell Command Approval
        </label>
        <select
          value={profile.shellApprovalThreshold ?? "medium"}
          onChange={(e) => {
            const v = e.target.value;
            setProfile(selectedAgentId, {
              shellApprovalThreshold: v === "medium" ? undefined : v,
            });
          }}
          className="w-full appearance-none rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
          style={{
            backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
            backgroundPosition: 'right 0.5rem center',
            backgroundRepeat: 'no-repeat',
            backgroundSize: '1.5em 1.5em',
          }}
        >
          <option value="medium">Medium (default) — e.g. curl, wget, python, node</option>
          <option value="low">Low — e.g. ls, cat, echo, pwd, grep (most cautious)</option>
          <option value="high">High — e.g. sudo, eval, source</option>
          <option value="never">Never — auto-approve all shell commands</option>
        </select>
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          Controls which shell commands require user confirmation. Default: Medium.
        </p>
      </div>

      {/* Tools Configuration */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          Active Tools
        </label>
        {toolsLoading ? (
          <span className="text-[10px] text-zinc-400 dark:text-zinc-500">Loading...</span>
        ) : availableTools.length === 0 ? (
          <span className="text-[10px] text-zinc-400 dark:text-zinc-500">No tools available</span>
        ) : (
          <div className="max-h-48 overflow-y-auto space-y-1 rounded-lg border border-zinc-200 bg-white p-1.5 dark:border-zinc-700 dark:bg-zinc-800">
            {availableTools.map((tool) => {
              const checked = activeTools.includes(tool.name);
              const toggle = () => {
                setActiveTools((prev) =>
                  checked ? prev.filter((n) => n !== tool.name) : [...prev, tool.name],
                );
              };
              const isAlwaysOn = tool.always_on === true;
              return (
                <label
                  key={tool.name}
                  className={"flex items-center gap-2 py-1 px-1.5 rounded " + (isAlwaysOn ? "opacity-60" : "hover:bg-zinc-50 dark:hover:bg-zinc-800/50 cursor-pointer")}
                >
                  <input
                    type="checkbox"
                    checked={isAlwaysOn || checked}
                    onChange={isAlwaysOn ? undefined : toggle}
                    disabled={isAlwaysOn}
                    className="h-3.5 w-3.5 shrink-0 rounded accent-[var(--color-accent)]"
                  />
                  <div className="flex-1 min-w-0">
                    <span className="text-[11px] font-medium text-zinc-700 dark:text-zinc-300">
                      {tool.name}
                    </span>
                    <span className="block text-[9px] text-zinc-400 dark:text-zinc-500 leading-tight">
                      {tool.description}
                    </span>
                  </div>
                </label>
              );
            })}
          </div>
        )}
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          Uncheck all to disable all tools; empty = use manifest defaults
        </p>
      </div>

      {/* MCP Server Activation */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          MCP Servers
        </label>
        {catalog.length === 0 ? (
          <div className="rounded-lg border border-zinc-200 bg-white p-2 dark:border-zinc-700 dark:bg-zinc-800">
            <span className="text-[10px] text-zinc-400 dark:text-zinc-500">
              No MCP servers in catalog. Add servers in Harness &gt; MCP tab.
            </span>
          </div>
        ) : (
          <div className="max-h-48 overflow-y-auto space-y-1 rounded-lg border border-zinc-200 bg-white p-1.5 dark:border-zinc-700 dark:bg-zinc-800">
            {catalog.map((server) => {
              const isChecked = activeServers.includes(server.name);
              return (
                <label
                  key={server.name}
                  className="flex items-center gap-2 py-1 px-1.5 rounded hover:bg-zinc-50 dark:hover:bg-zinc-800/50 cursor-pointer"
                >
                  <input
                    type="checkbox"
                    checked={isChecked}
                    onChange={() => toggleServer(server.name)}
                    disabled={activationLoading}
                    className="h-3.5 w-3.5 shrink-0 rounded accent-[var(--color-accent)]"
                  />
                  <div className="flex-1 min-w-0">
                    <div className="flex items-center gap-1.5">
                      <span className="text-[11px] font-medium text-zinc-700 dark:text-zinc-300">
                        {server.name}
                      </span>
                      <span className="rounded bg-zinc-100 px-1 py-0.5 text-[9px] text-zinc-400 dark:bg-zinc-700">
                        {server.transport}
                      </span>
                    </div>
                    <span className="block text-[9px] text-zinc-400 dark:text-zinc-500 leading-tight">
                      {server.command || server.url || ""}
                    </span>
                  </div>
                </label>
              );
            })}
          </div>
        )}
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          Toggle MCP servers for this agent. Add more in Harness &gt; MCP tab.
        </p>
      </div>

      {/* Action buttons */}
      <div className="mt-4 border-t border-zinc-200 pt-3 dark:border-zinc-700 flex gap-3">
        <button
          onClick={handleApply}
          disabled={configSaving}
          className="flex-1 rounded-lg btn-solid px-3 py-1.5 text-xs font-medium disabled:opacity-50"
        >
          {configSaving ? "Applying..." : "Apply to Runtime"}
        </button>
        <button
          onClick={() => setShowResetConfirm(true)}
          className="flex-1 rounded-lg btn-solid px-3 py-1.5 text-xs font-medium"
        >
          Reset to defaults
        </button>
      </div>

      <ConfirmDialog
        open={showResetConfirm}
        title="Reset Agent Setup"
        message="确定要重置 Agent 设置为默认值吗？包括名称、描述、提示词角色、头像图标、Shell 审批阈值和工具配置。"
        confirmLabel="Reset"
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
