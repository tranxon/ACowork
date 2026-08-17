import { useEffect, useState } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useMcpStore } from "../../stores/mcpStore";
import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { Switch } from "../common/Switch";
import type { SearchProviderListItem, AgentSearchProvider } from "../../lib/types";

const EMPTY_ARRAY: string[] = [];

interface BuiltinToolEntry {
  name: string;
  enabled: boolean;
  /**
   * ADR-029 + ADR-052: When `true`, the runtime force-enables this tool
   * regardless of the persisted `enabled` flag (the backend routes every
   * write path through `BuiltinToolEntry::with_resolved_enabled`, which
   * consults `PLATFORM_PROTECTED_TOOLS` — see `core/acowork-runtime/src/
   * tools/registry.rs`). The desktop must therefore render this row as
   * non-interactive: a disabled Switch wrapped in a Tooltip pointing to
   * the global Tool Compression toggle, and `toggleBuiltinTool` must
   * short-circuit so the user cannot accidentally fire a PUT that
   * pollutes `agent_tools.json` with a value the server will discard.
   *
   * Absent / `false` → standard interactive Switch.
   */
  platform_protected?: boolean;
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
  const mcpError = useMcpStore((s) => s.error);
  const loadCatalog = useMcpStore((s) => s.loadCatalog);
  const toggleServer = useMcpStore((s) => s.toggleServer);

  // Search provider configuration
  const [searchProviders, setSearchProviders] = useState<SearchProviderListItem[]>([]);
  const [activeSearch, setActiveSearch] = useState<AgentSearchProvider[]>([]);
  const [searchSaving, setSearchSaving] = useState(false);

  // ADR-029: Builtin tools configuration
  const [builtinToolsAll, setBuiltinToolsAll] = useState<BuiltinToolEntry[]>([]);
  const [builtinSaving, setBuiltinSaving] = useState(false);

  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;

    // MCP catalog (gateway global, independent of agent state)
    loadCatalog();

    // Tools, MCP servers, search providers — fetch once from merged /tools endpoint.
    // ADR-034 Phase 5: Replaces 3 separate calls (config, mcp-servers, search-providers).
    (async () => {
      try {
        const resp = await fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/tools`);
        if (resp.ok && !cancelled) {
          const data = await resp.json();
          // tools — builtin tools list
          if (data.tools && Array.isArray(data.tools)) {
            setBuiltinToolsAll(data.tools as BuiltinToolEntry[]);
          }
          // mcp_servers — active MCP server names
          if (data.mcp_servers && Array.isArray(data.mcp_servers)) {
            useMcpStore.setState((s) => ({
              activeServers: { ...s.activeServers, [selectedAgentId!]: data.mcp_servers },
            }));
          }
          // search — search providers (both available list and active config).
          // ADR-034 §7.6.5: the merged /tools endpoint returns a single
          // `providers` array — ordered active providers for this agent.
          // Treating `providers` as "available candidates" and
          // `active_providers` as "currently active" used to leave
          // `activeSearch` empty on first render (the server only ships
          // one key, not two), so the fresh-mount case showed no checked
          // checkboxes even though search was already configured.
          if (Array.isArray(data.search?.providers)) {
            setSearchProviders(data.search.providers as SearchProviderListItem[]);
            setActiveSearch(data.search.providers as AgentSearchProvider[]);
          }
        }
      } catch { /* agent not ready — user can re-open tab later */ }
    })();

    return () => { cancelled = true; };
  }, [selectedAgentId]);

  // Refresh on global events (e.g. after MCP toggle)
  useEffect(() => {
    if (!selectedAgentId) return;
    const handler = async (e: Event) => {
      const ce = e as CustomEvent<{ agentId: string }>;
      if (ce.detail?.agentId !== selectedAgentId) return;
      try {
        const resp = await fetch(
          `${getGatewayUrl()}/api/agents/${selectedAgentId}/tools`,
        );
        if (!resp.ok) return;
        const data = await resp.json();
        if (data.tools && Array.isArray(data.tools)) {
          setBuiltinToolsAll(data.tools as BuiltinToolEntry[]);
        }
        if (data.mcp_servers && Array.isArray(data.mcp_servers)) {
          useMcpStore.setState((s) => ({
            activeServers: { ...s.activeServers, [selectedAgentId!]: data.mcp_servers },
          }));
        }
        if (data.search) {
          // ADR-034 §7.6.5: server returns a single `providers` array which
          // is the merged "active selection" view. Mirror the initial-load
          // shape so the catalog list AND the active checkboxes stay in
          // sync when an MCP/Search toggle elsewhere triggers a refresh.
          if (Array.isArray(data.search.providers)) {
            setSearchProviders(data.search.providers as SearchProviderListItem[]);
            setActiveSearch(data.search.providers as AgentSearchProvider[]);
          }
        }
      } catch { /* ignore */ }
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

  // ── ADR-029: Builtin tools helpers ─────────────────────────────────

  /** Toggle a builtin tool enabled/disabled and PUT to Gateway.
   *
   * Platform-protected tools (see `BuiltinToolEntry.platform_protected`)
   * short-circuit here: the user cannot toggle them in the UI, but if
   * they somehow fire this function (keyboard, dev-tools paste, stale
   * code path) we MUST NOT issue the PUT — the server would silently
   * accept it and write `enabled=false` into `agent_tools.json`, which
   * would then be silently overwritten by `with_resolved_enabled` on
   * every cold start. Net effect: wasted disk write, no behavioural
   * change, confusing diff in the persisted file.
   */
  const toggleBuiltinTool = async (toolName: string) => {
    if (!selectedAgentId) return;
    const target = builtinToolsAll.find((e) => e.name === toolName);
    if (target?.platform_protected) return; // ADR-029 + ADR-052
    const next = builtinToolsAll.map((entry) =>
      entry.name === toolName ? { ...entry, enabled: !entry.enabled } : entry
    );
    setBuiltinToolsAll(next);
    setBuiltinSaving(true);
    try {
      const enabledNames = next.filter((e) => e.enabled).map((e) => e.name);
      // ADR-040 follow-up: `builtin_tools` moved off `/config` (where
      // it conflated model knobs with tool state) onto its own route.
      // The Gateway has no special-case logic here — it transparently
      // reverse-proxies the path to the Runtime, so the desktop just
      // hits the matched endpoint.
      await fetch(
        `${getGatewayUrl()}/api/agents/${selectedAgentId}/builtin-tools`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ builtin_tools: enabledNames }),
        },
      );
    } catch {
      // silently ignore network errors; optimistic update is OK
    } finally {
      setBuiltinSaving(false);
    }
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
      {/* ADR-029: Builtin Tools */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.builtinTools")}
        </label>
        {builtinToolsAll.length === 0 ? (
          <div className="rounded-md border border-zinc-200 bg-modal-surface p-2 dark:border-zinc-700">
            <span className="text-[10px] text-zinc-400 dark:text-zinc-500">
              {t("agentSetup.noBuiltinTools")}
            </span>
          </div>
        ) : (
          <div className="max-h-48 overflow-y-auto rounded-md border border-zinc-200 bg-modal-surface dark:border-zinc-700">
            <div className="divide-y divide-zinc-200 dark:divide-zinc-700">
              {builtinToolsAll.map((entry) => {
                // ADR-029 + ADR-052: platform-protected tools are force-
                // enabled on the server regardless of the user's preference.
                // Render them as a non-interactive Switch (greyed out) with
                // a tooltip that points at the Tool Compression global
                // toggle — the only knob that controls them.
                const isProtected = entry.platform_protected === true;
                const switchEl = (
                  <Switch
                    checked={entry.enabled}
                    onChange={() => toggleBuiltinTool(entry.name)}
                    disabled={isProtected || builtinSaving || !selectedAgentId}
                    size="sm"
                    aria-label={entry.name}
                  />
                );
                return (
                  <div
                    key={entry.name}
                    className={`flex items-center gap-2 px-3 py-2 transition-colors ${
                      isProtected
                        ? "opacity-70"
                        : "hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
                    }`}
                  >
                    <span className="flex-1 min-w-0 text-[11px] font-medium text-zinc-700 dark:text-zinc-300">
                      {entry.name}
                    </span>
                    {isProtected && (
                      <span className="rounded bg-zinc-100 px-1.5 py-0.5 text-[9px] text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400">
                        {t("agentSetup.builtinToolPlatformProtectedHint")}
                      </span>
                    )}
                    {isProtected ? (
                      <Tooltip
                        content={t("agentSetup.builtinToolPlatformProtectedTooltip")}
                        variant="plain"
                      >
                        {switchEl}
                      </Tooltip>
                    ) : (
                      switchEl
                    )}
                  </div>
                );
              })}
            </div>
          </div>
        )}
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.builtinToolsDesc")}
        </p>
      </div>

      {/* Web Search Providers */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.webSearchProviders")}
        </label>
        {searchProviders.length === 0 ? (
          <div className="rounded-md border border-zinc-200 bg-modal-surface p-2 dark:border-zinc-700">
            <span className="text-[10px] text-zinc-400 dark:text-zinc-500">
              {t("agentSetup.noSearchKeys")}
            </span>
          </div>
        ) : (
          <div className="max-h-48 overflow-y-auto rounded-md border border-zinc-200 bg-modal-surface dark:border-zinc-700">
            <div className="divide-y divide-zinc-200 dark:divide-zinc-700">
              {searchProviders.map((sp) => {
                const active = activeSearch.find((p) => p.provider === sp.id);
                const isChecked = !!active;
                const priority = active?.priority;
                const hasKey = !!sp.id; // Providers listed here already have vault keys
                const activeIdx = activeSearch.findIndex((p) => p.provider === sp.id);
                return (
                  <Tooltip key={sp.id} content={hasKey ? "" : t("agentSetup.noApiKey")} variant="plain">
                    <div
                      className={`flex items-center gap-2 px-3 py-2 transition-colors ${hasKey
                        ? "hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
                        : "opacity-50"
                        }`}
                    >
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
                      <Switch
                        checked={isChecked}
                        onChange={() => toggleSearchProvider(sp.id)}
                        disabled={searchSaving || !hasKey}
                        size="sm"
                        aria-label={sp.name || sp.id}
                      />
                    </div>
                  </Tooltip>
                );
              })}
            </div>
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
          <div className="rounded-md border border-zinc-200 bg-modal-surface p-2 dark:border-zinc-700">
            <span className="text-[10px] text-zinc-400 dark:text-zinc-500">
              {t("agentSetup.noMcpInCatalog")}
            </span>
          </div>
        ) : (
          <div className="max-h-48 overflow-y-auto rounded-md border border-zinc-200 bg-modal-surface dark:border-zinc-700">
            <div className="divide-y divide-zinc-200 dark:divide-zinc-700">
              {catalog.map((server) => {
                const isChecked = activeServers.includes(server.name);
                return (
                  <div
                    key={server.name}
                    className="flex items-center gap-2 px-3 py-2 transition-colors hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
                  >
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
                    <Switch
                      checked={isChecked}
                      onChange={() => selectedAgentId && toggleServer(selectedAgentId, server.name)}
                      disabled={activationLoading || !selectedAgentId}
                      size="sm"
                      aria-label={server.name}
                    />
                  </div>
                );
              })}
            </div>
          </div>
        )}
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.mcpToggleDesc")}
        </p>
        {/* Surface MCP PUT errors that the store would otherwise swallow.
            The setActiveServers action optimistically updates the UI and
            rolls back on failure; this banner makes the rollback visible
            to the user instead of leaving the checkbox mysteriously
            unchecked. */}
        {mcpError && (
          <div
            role="alert"
            className="mt-1 flex items-start gap-1.5 rounded-md border border-[var(--color-destructive)]/30 bg-[var(--color-destructive)]/10 px-2 py-1 text-[10px] text-[var(--color-destructive)] dark:border-[var(--color-destructive)]/40 dark:bg-[var(--color-destructive)]/15"
          >
            <span className="flex-1 break-words">
              {t("agentSetup.mcpToggleError", { error: mcpError })}
            </span>
            <button
              type="button"
              onClick={() => useMcpStore.setState({ error: null })}
              className="shrink-0 text-[10px] underline hover:no-underline"
              aria-label="Dismiss error"
            >
              ×
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
