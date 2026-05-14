import { useEffect, useState } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useAgentProfileStore } from "../../stores/agentProfileStore";
import { UserAvatar, BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { getGatewayUrl } from "../../lib/config";

// ── Component ───────────────────────────────────────────────────────────

export function AgentSetupTab() {
  const { agents, selectedAgentId } = useAgentStore();
  const { getProfile, setProfile, resetProfile } = useAgentProfileStore();

  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);
  const profile = selectedAgentId ? getProfile(selectedAgentId) : null;

  // Fetch agent runtime config from Gateway API on mount
  const [configLoading, setConfigLoading] = useState(false);
  const [configSaving, setConfigSaving] = useState(false);
  const [iconOpen, setIconOpen] = useState(false);

  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;
    setConfigLoading(true);
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/config`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (cancelled || !data) return;
        // Populate profile defaults from API response
        setProfile(selectedAgentId, {
          maxTokens: data.max_output_tokens,
          maxIterations: data.max_iterations,
          temperature: data.temperature,
        });
      })
      .catch(() => {})
      .finally(() => {
        if (!cancelled) setConfigLoading(false);
      });
    return () => {
      cancelled = true;
    };
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
              size={48}
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
                    className={`flex items-center justify-center rounded-md p-1 transition-colors ${
                      profile.avatarIconId === iconId
                        ? "bg-zinc-200 dark:bg-zinc-600"
                        : "hover:bg-zinc-100 dark:hover:bg-zinc-700"
                    }`}
                  >
                    <img
                      src={BUILTIN_ICONS[iconId] ?? ""}
                      alt={iconId}
                      draggable={false}
                      className="h-10 w-10 rounded-full object-cover"
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
          placeholder="32768 (default)"
          className="w-full rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          Leave empty to use runtime default
        </p>
      </div>

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
          className="w-full accent-zinc-600 dark:accent-zinc-400"
        />
        <div className="flex justify-between text-[9px] text-zinc-400 dark:text-zinc-500">
          <span>0 (deterministic)</span>
          <span>2 (creative)</span>
        </div>
      </div>

      {/* Action buttons */}
      <div className="mt-4 border-t border-zinc-200 pt-3 dark:border-zinc-700 space-y-2">
        <button
          onClick={handleApply}
          disabled={configSaving}
          className="w-full rounded-lg bg-zinc-800 px-3 py-1.5 text-xs text-white hover:bg-zinc-700 disabled:opacity-50 dark:bg-zinc-200 dark:text-zinc-800 dark:hover:bg-zinc-300"
        >
          {configSaving ? "Applying..." : "Apply to Runtime"}
        </button>
        <button
          onClick={() => resetProfile(selectedAgentId)}
          className="w-full rounded-lg border border-zinc-300 px-3 py-1.5 text-xs text-zinc-500 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-400 dark:hover:bg-zinc-800"
        >
          Reset to defaults
        </button>
      </div>
    </div>
  );
}
