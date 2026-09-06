import { useEffect, useState } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useMcpStore } from "../../stores/mcpStore";
import { getGatewayUrl } from "../../lib/config";
import { with503Retry } from "../../lib/httpRetry";
import { log } from "../../lib/logger";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { Switch } from "../common/Switch";
import { ListBox, ListRow, ExpandableRow, Badge, EmptyState } from "../common/list";
import type { SearchProviderListItem, AgentSearchProvider, McpServerView, AgentMcpToolItem } from "../../lib/types";

const EMPTY_ARRAY: string[] = [];

interface BuiltinToolEntry {
  name: string;
  enabled: boolean;
}

// ── Sub-component: collapsible MCP server card ────────────────────────

interface McpServerCardProps {
  server: McpServerView;
  isChecked: boolean;
  /** ADR-069: complete per-tool list advertised by `server`, each row
   *  carrying `name` + `enabled` + `description`. Sourced entirely
   *  from the backend `GET /agents/{id}/mcp-tools` response — the
   *  frontend never maintains its own tool list or defaults. */
  tools: AgentMcpToolItem[];
  onToggleTool: (toolName: string) => void;
  onToggleServer: () => void;
  switchDisabled: boolean;
  toolSwitchDisabled: boolean;
}

/**
 * Collapsible MCP server card (ADR-068 UX, ADR-069 data).
 *
 * Layout mirrors `PromptList.tsx`'s Debug-panel style:
 *   - Header row: ChevronDown/Right + server name + transport badge +
 *     activation Switch. Clicking anywhere on the header toggles
 *     collapse; the Switch stops propagation so toggling activation
 *     does not also collapse.
 *   - Body (when open): separated by `border-t`, one tool per row —
 *     name + description on the left, a unified Switch on the right
 *     (same visual contract as the Builtin Tools list). Deep left
 *     indent so the nested tools read as children of the server card.
 *
 * Default collapsed. A future enhancement could persist the open
 * state per-server in localStorage; for v1 we keep it in-memory only.
 */
function McpServerCard({
  server,
  isChecked,
  tools,
  onToggleTool,
  onToggleServer,
  switchDisabled,
  toolSwitchDisabled,
}: McpServerCardProps) {
  const [open, setOpen] = useState(false);
  // ADR-068/069 UX: the chevron is only meaningful when there is
  // actually something to expand — the server is active AND its tool
  // list is available. An inactive server (e.g. `playwright` before it
  // is enabled) or one with no reconciled tool data would otherwise
  // show a right-arrow that expands to an empty body, which reads as
  // broken. Hide the chevron in that case (ExpandableRow disabled);
  // enabling the server (PUT /mcp-servers → reconnect → reconcile)
  // materialises the tool list and the chevron appears.
  const hasExpandableBody = isChecked && tools.length > 0;
  const showBody = open && hasExpandableBody;
  // Note: no outer ListBox here — the caller places each server row
  // inside the MCP group card's plain body list (Debug-panel style),
  // which owns the hairline separation between rows.
  return (
    <ExpandableRow
      open={showBody}
      onToggle={() => setOpen((v) => !v)}
      disabled={!hasExpandableBody}
      title={`${server.name}${tools.length > 0 ? ` (${tools.length})` : ""}`}
      meta={<Badge>{server.transport}</Badge>}
      description={server.command || server.url || ""}
      trailing={
        // The Switch component owns its own click handler; we stop
        // propagation so clicking the activation toggle does not
        // also collapse/expand the row.
        <span onClick={(e) => e.stopPropagation()}>
          <Switch
            checked={isChecked}
            onChange={onToggleServer}
            disabled={switchDisabled}
            size="sm"
            aria-label={server.name}
          />
        </span>
      }
      surface="inset"
      bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset-2 dark:border-zinc-700"
      ariaLabel={`Toggle ${server.name} tools`}
    >
      {/* Body — one row per tool, name on the left, Switch on the
          right (same visual contract as the Builtin Tools list) with a
          deep left indent so the nested tools read clearly as
          "children of the server row". Right edge stays aligned with
          the header Switch column. */}
      <ListBox variant="plain">
        {tools.map((tool) => (
          <ListRow
            key={tool.name}
            padding="nested"
            surface="inset"
            trailing={
              <Switch
                checked={tool.enabled}
                onChange={() => onToggleTool(tool.name)}
                disabled={toolSwitchDisabled}
                size="sm"
                aria-label={tool.name}
              />
            }
          >
            <span className="block truncate font-mono text-[11px] font-medium text-zinc-700 dark:text-zinc-300">
              {tool.name}
            </span>
            {tool.description && (
              <span className="block truncate text-[9px] leading-tight text-zinc-400 dark:text-zinc-500">
                {tool.description}
              </span>
            )}
          </ListRow>
        ))}
      </ListBox>
    </ExpandableRow>
  );
}

// ── Component ───────────────────────────────────────────────────────────

export function ToolsTab() {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  const selectedAgent = useAgentStore((s) => s.selectedAgentId ? s.agents[s.selectedAgentId]?.meta : undefined);

  // MCP server activation — per-agent selectors.
  // The toggle list is rendered from the per-agent merged MCP list
  // (`mcp_servers_defs` = catalog ∪ local) fetched from the merged
  // /tools endpoint, so agent-installed (local) and system-injected
  // (pm) servers appear here too — not just gateway-global catalog
  // entries. Checked state comes from `activeServers` (active_names).
  const activeServers = useMcpStore((s) => selectedAgentId ? (s.activeServers[selectedAgentId] ?? EMPTY_ARRAY) : EMPTY_ARRAY);
  const activationLoading = useMcpStore((s) => selectedAgentId ? (s.activationLoading[selectedAgentId] ?? false) : false);
  const mcpError = useMcpStore((s) => s.error);
  const toggleServer = useMcpStore((s) => s.toggleServer);

  // Per-agent MCP server definitions (catalog ∪ local) from merged /tools.
  const [mcpServerDefs, setMcpServerDefs] = useState<McpServerView[]>([]);

  // Search provider configuration
  const [searchProviders, setSearchProviders] = useState<SearchProviderListItem[]>([]);
  const [activeSearch, setActiveSearch] = useState<AgentSearchProvider[]>([]);
  const [searchSaving, setSearchSaving] = useState(false);

  // ADR-029: Builtin tools configuration
  const [builtinToolsAll, setBuiltinToolsAll] = useState<BuiltinToolEntry[]>([]);
  const [builtinSaving, setBuiltinSaving] = useState(false);

  // ADR-069: per-MCP-server complete tool list. `mcpToolsConfig` maps
  // server name → full `AgentMcpToolItem[]` (name + enabled +
  // description) fetched from `GET /agents/{id}/mcp-tools`. The
  // backend reconciles this against the live `tools/list` at connect
  // time, so the frontend simply renders what the server sends — it
  // never maintains its own tool list or defaults.
  const [mcpToolsConfig, setMcpToolsConfig] = useState<Record<string, AgentMcpToolItem[]>>({});
  const [mcpToolsSaving, setMcpToolsSaving] = useState(false);

  // Group-level collapse state for the three tool cards (Builtin Tools /
  // Web Search / MCP). Each card mirrors the Debug-panel "Context
  // Snapshots" level-1 collapsible style; default open so the previous
  // always-visible behavior is preserved on first mount.
  const [builtinOpen, setBuiltinOpen] = useState(true);
  const [searchOpen, setSearchOpen] = useState(true);
  const [mcpOpen, setMcpOpen] = useState(true);

  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;

    // Tools, MCP servers, search providers — fetch once from merged /tools endpoint.
    // ADR-034 Phase 5: Replaces 3 separate calls (config, mcp-servers, search-providers).
    // Bug B v3 fix: the merged `/tools` endpoint proxies through the
    // Runtime and 503s during the boot window. `with503Retry` rides out
    // the transient 503 so the RightPanel/Tools tab does not have to
    // gate on `meta.ready` — same root-cause as `fetchWorkspaces` /
    // `fetchTree` / `fetchNodes` / `fetchLatestSession`.
    (async () => {
      try {
        const resp = await with503Retry(
          () => fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/tools`),
          { tag: `ToolsTab.fetchTools(${selectedAgentId})`, logger: log },
        );
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
          // mcp_servers_defs — full per-agent MCP list (catalog ∪ local)
          // with active flags; powers the per-agent toggle rows so pm /
          // agent-installed MCPs show up too.
          if (data.mcp_servers_defs && Array.isArray(data.mcp_servers_defs)) {
            setMcpServerDefs(data.mcp_servers_defs as McpServerView[]);
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

        // ADR-069: per-tool opt-in list — separate endpoint from the
        // merged /tools view (which has no `mcp_tools` field). The
        // backend is the single source of truth: it reconciles
        // agent_mcp_tools.json against the live MCP `tools/list` at
        // connect time, so this is the complete list the frontend
        // renders directly (name + enabled + description).
        try {
          const mcpToolsResp = await with503Retry(
            () => fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/mcp-tools`),
            { tag: `ToolsTab.fetchMcpTools(${selectedAgentId})`, logger: log },
          );
          if (mcpToolsResp.ok && !cancelled) {
            const mcpToolsData = await mcpToolsResp.json();
            if (mcpToolsData.servers && typeof mcpToolsData.servers === "object") {
              setMcpToolsConfig(mcpToolsData.servers as Record<string, AgentMcpToolItem[]>);
            }
          }
        } catch { /* agent not ready — user can re-open tab later */ }
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
        if (data.mcp_servers_defs && Array.isArray(data.mcp_servers_defs)) {
          setMcpServerDefs(data.mcp_servers_defs as McpServerView[]);
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
        // ADR-069: refresh the per-tool opt-in list from its dedicated
        // endpoint (the merged /tools view has no `mcp_tools` field).
        try {
          const mcpToolsResp = await fetch(
            `${getGatewayUrl()}/api/agents/${selectedAgentId}/mcp-tools`,
          );
          if (mcpToolsResp.ok) {
            const mcpToolsData = await mcpToolsResp.json();
            if (mcpToolsData.servers && typeof mcpToolsData.servers === "object") {
              setMcpToolsConfig(mcpToolsData.servers as Record<string, AgentMcpToolItem[]>);
            }
          }
        } catch { /* ignore */ }
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

  /** Toggle a builtin tool enabled/disabled and PUT to Gateway */
  const toggleBuiltinTool = async (toolName: string) => {
    if (!selectedAgentId) return;
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

  // ── ADR-069: MCP tool helpers ──────────────────────────────────────

  /**
   * PUT the current `mcpToolsConfig` to the Runtime. The whole map is
   * sent as the request body — the backend's `put_mcp_tools` persists
   * it verbatim to `agent_mcp_tools.json` (no server-side merge; the
   * desktop's list is authoritative). Wire shape is identical to the
   * GET response — three-way identity between the on-disk file, the
   * HTTP body, and the desktop render (ADR-069).
   */
  const saveMcpToolsConfig = async (
    next: Record<string, AgentMcpToolItem[]>,
  ) => {
    if (!selectedAgentId) return;
    setMcpToolsSaving(true);
    try {
      await fetch(
        `${getGatewayUrl()}/api/agents/${selectedAgentId}/mcp-tools`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ servers: next }),
        },
      );
    } catch {
      // silently ignore network errors; optimistic update is OK
    } finally {
      setMcpToolsSaving(false);
    }
  };

  /**
   * Toggle a single tool's `enabled` flag for a server. The backend
   * provides the complete per-server list (name + enabled +
   * description) via GET /mcp-tools, so the toggle is a pure local
   * flip: find the row, invert `enabled`, PUT the whole map. No
   * defaults, no hardcoded tool lists — the frontend just reflects
   * back what the backend gave it (ADR-069).
   */
  const toggleMcpTool = (serverName: string, toolName: string) => {
    const rows = mcpToolsConfig[serverName];
    if (!rows) return;
    const nextRows = rows.map((row) =>
      row.name === toolName ? { ...row, enabled: !row.enabled } : row
    );
    const next = { ...mcpToolsConfig, [serverName]: nextRows };
    setMcpToolsConfig(next);
    saveMcpToolsConfig(next);
  };

  if (!selectedAgentId || !selectedAgent) {
    return (
      <div className="flex flex-1 items-center justify-center bg-right-panel p-6">
        <span className="text-xs text-zinc-400 dark:text-zinc-500">{t("agentSetup.noAgentSelected")}</span>
      </div>
    );
  }

  return (
    <div className="flex-1 overflow-y-auto bg-right-panel p-3">
      {/* ── Builtin Tools card ─────────────────────────────────────
          Level-1 collapsible card in the same language as the
          Debug-panel "Context Snapshots" list: chevron on the left,
          title + count Badge on the right of it, click to expand the
          tool rows (ADR-029). Row contract: name + activation Switch. */}
      <div>
        <ListBox dividers={false}>
          <ExpandableRow
            open={builtinOpen}
            onToggle={() => setBuiltinOpen((v) => !v)}
            title={t("agentSetup.builtinTools", { count: builtinToolsAll.length })}
            ariaLabel={t("agentSetup.builtinTools", { count: builtinToolsAll.length })}
            bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
          >
            {builtinToolsAll.length === 0 ? (
              <EmptyState message={t("agentSetup.noBuiltinTools")} />
            ) : (
              <ListBox variant="plain" maxHeight={192}>
                {builtinToolsAll.map((entry) => (
                  <ListRow
                    key={entry.name}
                    surface="inset"
                    trailing={
                      <Switch
                        checked={entry.enabled}
                        onChange={() => toggleBuiltinTool(entry.name)}
                        disabled={builtinSaving || !selectedAgentId}
                        size="sm"
                        aria-label={entry.name}
                      />
                    }
                  >
                    <span className="truncate text-[11px] font-medium text-zinc-700 dark:text-zinc-300">
                      {entry.name}
                    </span>
                  </ListRow>
                ))}
              </ListBox>
            )}
          </ExpandableRow>
        </ListBox>
        <p className="mt-1 text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.builtinToolsDesc")}
        </p>
      </div>

      {/* Divider — full panel-width hairline separating the functional
          blocks (Builtin Tools / Web Search / MCP). Matches the
          workspace/memory panel divider style. */}
      <div className="-mx-3 my-2 border-t border-zinc-200 dark:border-zinc-800" />

      {/* Web Search Providers card — same Debug-panel collapsible
          style as the Builtin Tools card above. */}
      <div>
        <ListBox dividers={false}>
          <ExpandableRow
            open={searchOpen}
            onToggle={() => setSearchOpen((v) => !v)}
            title={t("agentSetup.webSearchProviders", { count: searchProviders.length })}
            ariaLabel={t("agentSetup.webSearchProviders", { count: searchProviders.length })}
            bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
          >
            {searchProviders.length === 0 ? (
              <EmptyState message={t("agentSetup.noSearchKeys")} />
            ) : (
              <ListBox variant="plain" maxHeight={192}>
                {searchProviders.map((sp) => {
                  const active = activeSearch.find((p) => p.provider === sp.id);
                  const isChecked = !!active;
                  const priority = active?.priority;
                  const hasKey = !!sp.id; // Providers listed here already have vault keys
                  const activeIdx = activeSearch.findIndex((p) => p.provider === sp.id);
                  return (
                    <Tooltip key={sp.id} content={hasKey ? "" : t("agentSetup.noApiKey")} variant="plain">
                      <ListRow
                        disabled={!hasKey}
                        surface="inset"
                        trailing={
                          <div className="flex items-center gap-1">
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
                        }
                      >
                        <div className="flex items-center gap-1.5">
                          <span className="truncate text-[11px] font-medium text-zinc-700 dark:text-zinc-300">
                            {sp.name || sp.id}
                          </span>
                          {isChecked && priority !== undefined && (
                            <Badge>{t("agentSetup.priority", { value: priority })}</Badge>
                          )}
                          {!hasKey && <Badge tone="warning">{t("agentSetup.noKey")}</Badge>}
                        </div>
                        <span className="block truncate text-[9px] leading-tight text-zinc-400 dark:text-zinc-500">
                          {sp.description || sp.base_url || ""}
                        </span>
                      </ListRow>
                    </Tooltip>
                  );
                })}
              </ListBox>
            )}
          </ExpandableRow>
        </ListBox>
        <p className="mt-1 text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.searchProvidersDesc")}
        </p>
      </div>

      {/* Divider — full panel-width hairline separating the functional
          blocks (Builtin Tools / Web Search / MCP). Matches the
          workspace/memory panel divider style. */}
      <div className="-mx-3 my-2 border-t border-zinc-200 dark:border-zinc-800" />

      {/* MCP Servers card — the group is a level-1 collapsible card
          like the Debug-panel snapshot list; each server renders as a
          collapsible row inside the card body, and each server row
          expands to its per-tool list (ADR-068 UX / ADR-069 data). */}
      <div>
        <ListBox dividers={false}>
          <ExpandableRow
            open={mcpOpen}
            onToggle={() => setMcpOpen((v) => !v)}
            title={t("agentSetup.mcpServers", { count: mcpServerDefs.length })}
            ariaLabel={t("agentSetup.mcpServers", { count: mcpServerDefs.length })}
            bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
          >
            {mcpServerDefs.length === 0 ? (
              <EmptyState message={t("agentSetup.noMcpInCatalog")} />
            ) : (
              <ListBox variant="plain">
                {mcpServerDefs.map((server) => {
                  const isChecked = activeServers.includes(server.name);
                  // ADR-069: the per-tool list comes entirely from
                  // `GET /agents/{id}/mcp-tools` — the backend provides
                  // the complete list (name + enabled + description)
                  // reconciled against the live MCP `tools/list`. The
                  // frontend renders it directly and never maintains a
                  // hardcoded tool list.
                  const tools: AgentMcpToolItem[] = mcpToolsConfig[server.name] ?? [];
                  return (
                    <McpServerCard
                      key={server.name}
                      server={server}
                      isChecked={isChecked}
                      tools={tools}
                      onToggleTool={(tool) => toggleMcpTool(server.name, tool)}
                      onToggleServer={() =>
                        selectedAgentId && toggleServer(selectedAgentId, server.name)
                      }
                      switchDisabled={activationLoading || !selectedAgentId}
                      toolSwitchDisabled={mcpToolsSaving || !selectedAgentId}
                    />
                  );
                })}
              </ListBox>
            )}
          </ExpandableRow>
        </ListBox>
        <p className="mt-1 text-[9px] text-zinc-400 dark:text-zinc-500">
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
