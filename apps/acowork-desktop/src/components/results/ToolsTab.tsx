import { useEffect, useState } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useMcpStore } from "../../stores/mcpStore";
import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import type { SearchProviderListItem, AgentSearchProvider } from "../../lib/types";

const EMPTY_ARRAY: string[] = [];

interface SearchProvidersResponse {
  agent_id: string;
  providers: SearchProviderListItem[];
}

// ── Component ───────────────────────────────────────────────────────────

export function ToolsTab() {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  const selectedAgent = useAgentStore((s) => s.selectedAgentId ? s.agents[s.selectedAgentId]?.meta : undefined);

  // MCP server activation — per-agent selectors
  const catalog = useMcpStore((s) => s.catalog);
  const activeServers = useMcpStore((s) => selectedAgentId ? (s.activeServers[selectedAgentId] ?? EMPTY_ARRAY) : EMPTY_ARRAY);
  const activationLoading = useMcpStore((s) => selectedAgentId ? (s.activationLoading[selectedAgentId] ?? false) : false);
  const loadCatalog = useMcpStore((s) => s.loadCatalog);
  const toggleServer = useMcpStore((s) => s.toggleServer);

  // Search provider configuration
  const [searchProviders, setSearchProviders] = useState<SearchProviderListItem[]>([]);
  const [activeSearch, setActiveSearch] = useState<AgentSearchProvider[]>([]);
  const [searchSaving, setSearchSaving] = useState(false);

  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;

    // Load MCP catalog
    loadCatalog();

    // Fetch search providers catalog
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/search-providers`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data: SearchProvidersResponse | null) => {
        if (cancelled || !data) return;
        setSearchProviders(data.providers);
      })
      .catch(() => { });

    // Fetch config for MCP and search
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/config`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (cancelled || !data) return;
        useMcpStore.setState((s) => ({
          activeServers: { ...s.activeServers, [selectedAgentId!]: data.active_mcp_servers ?? [] },
        }));
        setActiveSearch(data.search_config?.providers ?? []);
      })
      .catch((err) => {
        console.debug("[ToolsTab] Agent not ready:", err);
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
            useMcpStore.setState((s) => ({
              activeServers: { ...s.activeServers, [selectedAgentId!]: data.active_mcp_servers ?? [] },
            }));
            setActiveSearch(data.search_config?.providers ?? []);
          })
          .catch(() => { });
      }
    };
    window.addEventListener('acowork:refresh-agent-config', handler);
    return () => window.removeEventListener('acowork:refresh-agent-config', handler);
  }, [selectedAgentId]);

  // ── Search config helpers ──────────────────────────────────────────

  /** Save search provider config via PUT /api/agents/{id}/search-config */
  const saveSearchConfig = async (providers: AgentSearchProvider[]) => {
    if (!selectedAgentId) return;
    setSearchSaving(true);
    try {
      await fetch(
        `${getGatewayUrl()}/api/agents/${selectedAgentId}/search-config`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ providers }),
        },
      );
    } catch {
      // silently ignore network errors
    } finally {
      setSearchSaving(false);
    }
  };

  /** Toggle a search provider ON/OFF for this agent */
  const toggleSearchProvider = (providerId: string) => {
    const current = activeSearch.find((p) => p.provider === providerId);
    let next: AgentSearchProvider[];
    if (current) {
      // Remove from active
      next = activeSearch.filter((p) => p.provider !== providerId);
      // Re-number priorities
      next = next.map((p, i) => ({ ...p, priority: i + 1 }));
    } else {
      // Add with next priority
      const maxPrio = activeSearch.reduce((max, p) => Math.max(max, p.priority), 0);
      next = [...activeSearch, { provider: providerId, priority: maxPrio + 1 }];
    }
    setActiveSearch(next);
    saveSearchConfig(next);
  };

  /** Move a provider up in priority (lower number = higher priority) */
  const moveSearchProviderUp = (providerId: string) => {
    const idx = activeSearch.findIndex((p) => p.provider === providerId);
    if (idx <= 0) return;
    const next = [...activeSearch];
    // Swap priorities
    const prevPriority = next[idx - 1].priority;
    next[idx - 1] = { ...next[idx - 1], priority: next[idx].priority };
    next[idx] = { ...next[idx], priority: prevPriority };
    // Sort by priority
    next.sort((a, b) => a.priority - b.priority);
    // Re-normalize
    const normalized = next.map((p, i) => ({ ...p, priority: i + 1 }));
    setActiveSearch(normalized);
    saveSearchConfig(normalized);
  };

  if (!selectedAgentId || !selectedAgent) {
    return (
      <div className="flex flex-1 items-center justify-center p-6">
        <span className="text-xs text-zinc-400 dark:text-zinc-500">{t("agentSetup.noAgentSelected")}</span>
      </div>
    );
  }

  return (
    <div className="flex-1 overflow-y-auto p-3">
      {/* Web Search Providers */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.webSearchProviders")}
        </label>
        {searchProviders.length === 0 ? (
          <div className="rounded-md border border-zinc-200 bg-white p-2 dark:border-zinc-700 dark:bg-zinc-800">
            <span className="text-[10px] text-zinc-400 dark:text-zinc-500">
              {t("agentSetup.noSearchKeys")}
            </span>
          </div>
        ) : (
          <div className="max-h-48 overflow-y-auto space-y-1 rounded-md border border-zinc-200 bg-white p-1.5 dark:border-zinc-700 dark:bg-zinc-800">
            {searchProviders.map((sp) => {
              const active = activeSearch.find((p) => p.provider === sp.id);
              const isChecked = !!active;
              const priority = active?.priority;
              const hasKey = !!sp.id; // Providers listed here already have vault keys
              const activeIdx = activeSearch.findIndex((p) => p.provider === sp.id);
              return (
                <Tooltip key={sp.id} content={hasKey ? "" : t("agentSetup.noApiKey")} variant="plain">
                  <div
                    className={`flex items-center gap-2 py-1 px-1.5 rounded ${hasKey
                      ? "hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
                      : "opacity-50"
                      }`}
                  >
                    <input
                      type="checkbox"
                      checked={isChecked}
                      onChange={() => toggleSearchProvider(sp.id)}
                      disabled={searchSaving || !hasKey}
                      className="h-3.5 w-3.5 shrink-0 rounded accent-[var(--color-accent)]"
                    />
                    <div className="flex-1 min-w-0">
                      <div className="flex items-center gap-1.5">
                        <span className={`text-[11px] font-medium ${hasKey
                          ? "text-zinc-700 dark:text-zinc-300"
                          : "text-zinc-400 dark:text-zinc-500"
                          }`}>
                          {sp.name || sp.id}
                        </span>
                        {isChecked && priority !== undefined && (
                          <span className="rounded bg-zinc-100 px-1 py-0.5 text-[9px] text-zinc-400 dark:bg-zinc-700">
                            {t("agentSetup.priority", { value: priority })}
                          </span>
                        )}
                        {!hasKey && (
                          <span className="rounded bg-amber-50 px-1 py-0.5 text-[9px] text-amber-600 dark:bg-amber-900/30 dark:text-amber-400">
                            {t("agentSetup.noKey")}
                          </span>
                        )}
                      </div>
                      <span className="block text-[9px] text-zinc-400 dark:text-zinc-500 leading-tight">
                        {sp.description || sp.base_url || ""}
                      </span>
                    </div>
                    {isChecked && activeIdx > 0 && (
                      <Tooltip content={t("agentSetup.moveUp")} variant="plain">
                        <button
                          onClick={() => moveSearchProviderUp(sp.id)}
                          disabled={searchSaving}
                          className="shrink-0 rounded p-0.5 text-zinc-400 hover:bg-zinc-100 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
                        >
                          <svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                            <path d="m18 15-6-6-6 6" />
                          </svg>
                        </button>
                      </Tooltip>
                    )}
                  </div>
                </Tooltip>
              );
            })}
          </div>
        )}
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.searchProvidersDesc")}
        </p>
      </div>

      {/* MCP Server Activation */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.mcpServers")}
        </label>
        {catalog.length === 0 ? (
          <div className="rounded-md border border-zinc-200 bg-white p-2 dark:border-zinc-700 dark:bg-zinc-800">
            <span className="text-[10px] text-zinc-400 dark:text-zinc-500">
              {t("agentSetup.noMcpInCatalog")}
            </span>
          </div>
        ) : (
          <div className="max-h-48 overflow-y-auto space-y-1 rounded-md border border-zinc-200 bg-white p-1.5 dark:border-zinc-700 dark:bg-zinc-800">
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
                    onChange={() => selectedAgentId && toggleServer(selectedAgentId, server.name)}
                    disabled={activationLoading || !selectedAgentId}
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
          {t("agentSetup.mcpToggleDesc")}
        </p>
      </div>
    </div>
  );
}
