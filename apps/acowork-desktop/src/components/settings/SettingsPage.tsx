import { useState, useEffect, useCallback } from "react";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useChatStore } from "../../stores/chatStore";
import { useServicesStore } from "../../stores/servicesStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { AgentListResponse, GatewayConfig, GatewayMode, NodeInfo } from "../../lib/types";
import { fetchNodes } from "../../lib/gateway-api";
import { cn } from "../../lib/utils";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { ExpandableRow, ListBox, ListRow } from "../common/list";
import { RadioGroup } from "../common/RadioGroup";
import { DEFAULT_GATEWAY_URL, getGatewayUrl, DEFAULT_THEME, DEFAULT_FONT_SIZE, DEFAULT_CONTENT_WIDTH, DEFAULT_OPACITY, DEFAULT_ACCENT_COLOR } from "../../lib/config";
import { ACCENT_PRESETS } from "../../lib/accentPresets";
import { Bug, Monitor } from "lucide-react";
import { inputReadonly, inputBase } from "../../lib/ui-styles";
import { StyledInput } from "../common/StyledInput";
import { Dropdown } from "../common/Dropdown";
import { ProfileTab } from "./ProfileTab";
import { ServicesPanel } from "./ServicesPanel";
import { TabButton } from "../common/tab";
import { Tooltip } from "../common/Tooltip";
import { log } from "../../lib/logger";

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
      className="flex flex-1 flex-col bg-nav-surface"
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
  const { status, health, localState, localOwnership, checkHealth, checkLocalStatus, startLocalGateway, stopLocalGateway } = useGatewayStore();
  const gatewayUrl = useSettingsStore((s) => s.gatewayUrl);
  const setGatewayUrl = useSettingsStore((s) => s.setGatewayUrl);
  const gatewayMode = useSettingsStore((s) => s.gatewayMode);
  const setGatewayMode = useSettingsStore((s) => s.setGatewayMode);
  const [testing, setTesting] = useState(false);
  const [agents, setAgents] = useState<AgentListResponse[]>([]);
  const [agentsLoading, setAgentsLoading] = useState(false);
  const [nodes, setNodes] = useState<NodeInfo[]>([]);
  const [nodesLoading, setNodesLoading] = useState(false);
  const [urlDraft, setUrlDraft] = useState(gatewayUrl);
  const [starting, setStarting] = useState(false);
  const [stopping, setStopping] = useState(false);
  // Tools-tab style level-1 collapsible cards (default open)
  const [gatewayModeOpen, setGatewayModeOpen] = useState(true);
  const [localGatewayOpen, setLocalGatewayOpen] = useState(true);
  const [gatewayConnOpen, setGatewayConnOpen] = useState(true);
  const [nodesOpen, setNodesOpen] = useState(true);
  // P1: collapse state for the new diagnostic cards.
  const [servicesOpen, setServicesOpen] = useState(true);
  const [eventsOpen, setEventsOpen] = useState(true);
  // Per-node level-1 collapse state inside the Nodes section.
  const [openNodeIds, setOpenNodeIds] = useState<Record<string, boolean>>({});

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
    // P1-5 (a): "测试连接" 按钮语义升级为"运行诊断"——
    // 同时刷新 health (banner 用) 和跑一遍 servicesStore 的全量探针。
    // 两者并行，避免 health 探活串行阻塞诊断。
    setTesting(true);
    await Promise.all([checkHealth(), useServicesStore.getState().diagnose()]);
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
        setAgents(data.filter(a => a.alive));
      }
    } catch {
      // Gateway not reachable
    } finally {
      setAgentsLoading(false);
    }
  }, []);

  const fetchAll = useCallback(async () => {
    await Promise.all([
      fetchAgents(),
      (async () => {
        setNodesLoading(true);
        try {
          setNodes(await fetchNodes());
        } catch {
          // Gateway unreachable — empty topology, matches the prior
          // NodesTab behaviour.
          setNodes([]);
        } finally {
          setNodesLoading(false);
        }
      })(),
    ]);
  }, [fetchAgents]);

  useEffect(() => {
    if (status === "connected") {
      void fetchAll();
    }
  }, [status, fetchAll]);

  const toggleNode = useCallback((nodeId: string) => {
    setOpenNodeIds((prev) => ({ ...prev, [nodeId]: !prev[nodeId] }));
  }, []);

  const localIsRunning = gatewayMode === "local" && (status === "connected" || localState === "running");
  const localIsStarting = gatewayMode === "local" && (localState === "starting" || starting) && status !== "connected";
  // Whether Tauri itself owns the local Gateway process. Only in that case can
  // the user stop/restart it via the UI. If a Gateway is reachable but was
  // started outside of Tauri, we still show "Running" but no stop/restart buttons.
  const localIsTauriManaged = localState === "running";
  // A Gateway that answers at the configured URL but was NOT spawned by this
  // Desktop session (manual start, another machine, or kept running after a
  // previous quit). Desktop must never force-stop it — and the exit dialog
  // only appears for owned processes (tray quit handler).
  const localIsForeign = localOwnership === "foreign" && localIsRunning;

  return (
    <div className="max-w-lg space-y-4">
      {/* Mode selection */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={gatewayModeOpen}
          onToggle={() => setGatewayModeOpen((v) => !v)}
          title={t("settings.gatewayMode")}
          ariaLabel={t("settings.gatewayMode")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
          <RadioGroup
            name="gatewayMode"
            value={gatewayMode}
            options={[
              { label: t("settings.local"), value: "local" as GatewayMode },
              { label: t("settings.remote"), value: "remote" as GatewayMode },
            ]}
            onChange={handleModeChange}
          />
        </ExpandableRow>
      </ListBox>

      {/* Local mode: status + controls */}
      {gatewayMode === "local" && (
        <ListBox dividers={false}>
          <ExpandableRow
            open={localGatewayOpen}
            onToggle={() => setLocalGatewayOpen((v) => !v)}
            title={t("settings.localGateway")}
            ariaLabel={t("settings.localGateway")}
            bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
          >

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
            {!localIsTauriManaged && !localIsForeign && !localIsStarting && (
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
          {localIsForeign && (
            <p className="mt-2 text-[10px] text-zinc-500 dark:text-zinc-400">
              {t("settings.gatewayRunningExternal")}
            </p>
          )}
          </ExpandableRow>
        </ListBox>
      )}

      {/* Remote mode: URL + test */}
      {gatewayMode === "remote" && (
        <ListBox dividers={false}>
          <ExpandableRow
            open={gatewayConnOpen}
            onToggle={() => setGatewayConnOpen((v) => !v)}
            title={t("settings.gatewayConnection")}
            ariaLabel={t("settings.gatewayConnection")}
            bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
          >

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
          </ExpandableRow>
        </ListBox>
      )}

      {/* Nodes + their agents (shared between modes) */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={nodesOpen}
          onToggle={() => setNodesOpen((v) => !v)}
          title={t("settings.nodesTitle", { count: nodes.length })}
          ariaLabel={t("settings.nodesTitle", { count: nodes.length })}
          trailing={
            <span onClick={(e) => e.stopPropagation()}>
              <button
                onClick={() => void fetchAll()}
                disabled={nodesLoading || agentsLoading}
                className="rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
              >
                {t("settings.nodesRefresh")}
              </button>
            </span>
          }
          bodyClassName="overflow-hidden rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
        >
          {status !== "connected" ? (
            <div className="px-3 py-3 text-xs text-zinc-400">{t("settings.connectToSeeAgents")}</div>
          ) : nodesLoading && nodes.length === 0 ? (
            <div className="px-3 py-3 text-xs text-zinc-400">{t("settings.loading")}</div>
          ) : nodes.length === 0 ? (
            <div className="px-3 py-3 text-xs text-zinc-400">{t("settings.nodesEmpty")}</div>
          ) : (
            <NodesTree
              nodes={nodes}
              agents={agents}
              openNodeIds={openNodeIds}
              onToggleNode={toggleNode}
            />
          )}
        </ExpandableRow>
      </ListBox>

      {/* P1-5 (b): Full-stack service diagnostics. Self-contained
          card; `ServicesPanel` subscribes to servicesStore directly so
          this parent stays free of probe state. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={servicesOpen}
          onToggle={() => setServicesOpen((v) => !v)}
          title={t("settings.servicesTitle")}
          ariaLabel={t("settings.servicesTitle")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
          <ServicesPanel />
        </ExpandableRow>
      </ListBox>

      {/* P1-5 (d): Recent MQTT transition history (consumes
          `chatStore.transitionLog` from P0-4). Helps the user answer
          "why did the chat just drop?" without opening DevTools. */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={eventsOpen}
          onToggle={() => setEventsOpen((v) => !v)}
          title={t("settings.services.eventsTitle")}
          ariaLabel={t("settings.services.eventsTitle")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset dark:border-zinc-700"
        >
          <RecentEventsLog />
        </ExpandableRow>
      </ListBox>
    </div>
  );
}

/** Recent transition log (P1-5 d). Reads `chatStore.transitionLog`
 *  (max 20 entries, ring-buffered in P0-4) and renders them newest
 *  first. */
function RecentEventsLog() {
  const { t } = useTranslation();
  // I-1: named `entries` — a local `log` shadowed the module logger.
  const entries = useChatStore((s) => s.transitionLog);
  if (entries.length === 0) {
    return (
      <div className="px-3 py-3 text-xs text-zinc-500 dark:text-zinc-400">
        {t("settings.services.eventsEmpty")}
      </div>
    );
  }
  return (
    <ul className="max-h-48 overflow-y-auto divide-y divide-zinc-200 dark:divide-zinc-700">
      {entries
        .slice()
        .reverse()
        .map((entry, idx) => {
          const ts = new Date(entry.timestamp).toLocaleTimeString();
          const isErr = entry.to !== "connected";
          return (
            <li
              key={`${entry.timestamp}-${idx}`}
              className="flex items-center gap-2 px-3 py-1.5 text-[11px] font-mono"
            >
              <span className="text-zinc-400 dark:text-zinc-500">{ts}</span>
              <span className="text-zinc-600 dark:text-zinc-300">
                {entry.from} → {entry.to}
              </span>
              {entry.reason && (
                <span
                  className={cn(
                    "flex-1 truncate",
                    isErr
                      ? "text-red-600 dark:text-red-300"
                      : "text-zinc-500 dark:text-zinc-400",
                  )}
                  title={entry.reason}
                >
                  {entry.reason}
                </span>
              )}
              {!entry.reason && <span className="flex-1" />}
            </li>
          );
        })}
    </ul>
  );
}

/** Synthetic node_id bucket for agents whose node doesn't appear in
 *  `/api/nodes` (gateway-local agents before the node registry sees the
 *  local node, or stale rows after a node leaves). Keeps them visible
 *  instead of dropping them silently. */
const UNASSIGNED_NODE_ID = "__unassigned__";

/** Two-level collapsible list: each node row expands to reveal its agents. */
function NodesTree({
  nodes,
  agents,
  openNodeIds,
  onToggleNode,
}: {
  nodes: NodeInfo[];
  agents: AgentListResponse[];
  openNodeIds: Record<string, boolean>;
  onToggleNode: (nodeId: string) => void;
}) {
  const { t } = useTranslation();
  // Bucket agents by node_id; agents without a matching node fall into the
  // UNASSIGNED bucket so they never disappear from the view.
  const buckets = new Map<string, AgentListResponse[]>();
  for (const node of nodes) buckets.set(node.node_id, []);
  const orphan: AgentListResponse[] = [];
  for (const agent of agents) {
    const bucket = buckets.get(agent.node_id);
    if (bucket) bucket.push(agent);
    else orphan.push(agent);
  }
  const showUnassigned = orphan.length > 0;
  const renderedNodes: Array<{ id: string; node?: NodeInfo }> = [
    ...nodes.map((n) => ({ id: n.node_id, node: n })),
    ...(showUnassigned ? [{ id: UNASSIGNED_NODE_ID }] : []),
  ];

  return (
    <ListBox variant="plain">
      {renderedNodes.map(({ id, node }) => {
        const nodeAgents = id === UNASSIGNED_NODE_ID ? orphan : (buckets.get(id) ?? []);
        return (
          <ExpandableRow
            key={id}
            open={!!openNodeIds[id]}
            onToggle={() => onToggleNode(id)}
            surface="inset"
            title={node?.node_name ?? node?.hostname ?? node?.node_id ?? t("settings.nodesUnassigned")}
            meta={
              <span className="inline-flex items-center gap-1 text-[10px]">
                <span
                  className={cn(
                    "h-1.5 w-1.5 rounded-full",
                    node ? (node.online ? "bg-emerald-500" : "bg-zinc-400 dark:bg-zinc-500") : "bg-zinc-400 dark:bg-zinc-500",
                  )}
                />
                <span className={node?.online ? "text-emerald-600 dark:text-emerald-400" : "text-zinc-500"}>
                  {node ? (node.online ? t("settings.nodesOnline") : t("settings.nodesOffline")) : t("settings.nodesOffline")}
                </span>
              </span>
            }
            description={buildNodeDescription(node, t)}
            trailing={
              <span className="text-xs text-zinc-500">
                {nodeAgents.length}
                {node?.max_agents !== undefined && <span className="text-zinc-400">/{node.max_agents}</span>}
              </span>
            }
            bodyClassName="bg-zinc-50 dark:bg-zinc-900/60"
          >
            {nodeAgents.length === 0 ? (
              <div className="px-3 py-3 text-xs text-zinc-400">{t("settings.noAgentsRunning")}</div>
            ) : (
              <ListBox variant="plain">
                {nodeAgents.map((agent) => (
                  <RuntimeRow key={agent.instance_id} agent={agent} padding="nested" />
                ))}
              </ListBox>
            )}
          </ExpandableRow>
        );
      })}
    </ListBox>
  );
}

/** Build the small description line for a node row: hostname · os/arch. */
function buildNodeDescription(node: NodeInfo | undefined, t: (key: string) => string) {
  if (!node) return t("settings.nodesUnassignedDesc");
  const parts: string[] = [];
  if (node.hostname) parts.push(node.hostname);
  if (node.os && node.arch) parts.push(`${node.os}/${node.arch}`);
  else if (node.os) parts.push(node.os);
  else if (node.arch) parts.push(node.arch);
  if (node.node_version) parts.push(`v${node.node_version}`);
  return parts.join(" · ") || "—";
}

/** Single runtime row component — fetches model info independently */
function RuntimeRow({ agent, padding }: { agent: AgentListResponse; padding?: "default" | "nested" }) {
  const { t } = useTranslation();
  const [modelInfo, setModelInfo] = useState<{ provider: string; model: string } | null>(null);

  useEffect(() => {
    let cancelled = false;
    // ADR-073: `/api/agents/{id}` addresses the INSTANCE — using the
    // package `agent_id` would misroute in multi-instance deployments.
    fetch(`${getGatewayUrl()}/api/agents/${agent.instance_id}/model`)
      .then(r => r.ok ? r.json() : null)
      .then(data => {
        if (!cancelled && data) {
          setModelInfo({ provider: data.provider, model: data.model });
        }
      })
      .catch(() => { });
    return () => { cancelled = true; };
  }, [agent.instance_id]);

  return (
    <ListRow
      surface="inset"
      padding={padding ?? "default"}
      leading={<Monitor className="h-3.5 w-3.5 shrink-0 text-zinc-400" />}
      trailing={
        <div className="flex items-center gap-2 shrink-0">
          {modelInfo ? (
            <span className="text-xs text-zinc-500">{modelInfo.provider}/{modelInfo.model}</span>
          ) : (
            <span className="text-xs text-zinc-400">—</span>
          )}
        </div>
      }
    >
      <div className="flex items-center gap-2 min-w-0">
        <span className="truncate text-xs font-medium">{agent.name}</span>
        {/* ADR-048 follow-up: badge reflects current DevMode capability
            (debug_state), not startup intent (dev_mode) — an agent can be
            flipped into DevMode at runtime without restart. */}
        {agent.debug_state === "enabled" && (
          <span className="inline-flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
            <Bug className="h-3 w-3" />
            {t("settings.debug")}
          </span>
        )}
      </div>
    </ListRow>
  );
}

/** Appearance settings */
function AppearanceTab() {
  const { t } = useTranslation();
  const { theme, setTheme, fontSize, setFontSize, contentWidth, setContentWidth, opacity, setOpacity, accentColor, setAccentColor } = useSettingsStore();
  const [showResetConfirm, setShowResetConfirm] = useState(false);
  // Tools-tab style level-1 collapsible cards (default open)
  const [themeOpen, setThemeOpen] = useState(true);
  const [accentOpen, setAccentOpen] = useState(true);
  const [contentWidthOpen, setContentWidthOpen] = useState(true);
  const [fontSizeOpen, setFontSizeOpen] = useState(true);
  const [opacityOpen, setOpacityOpen] = useState(true);
  const [resetOpen, setResetOpen] = useState(true);

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
      <ListBox dividers={false}>
        <ExpandableRow
          open={themeOpen}
          onToggle={() => setThemeOpen((v) => !v)}
          title={t("settings.theme")}
          ariaLabel={t("settings.theme")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
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
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={accentOpen}
          onToggle={() => setAccentOpen((v) => !v)}
          title={t("settings.accentColor")}
          ariaLabel={t("settings.accentColor")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
          <p className="mb-3 text-xs text-zinc-500">{t("settings.accentColor")}</p>
          <div className="flex flex-wrap gap-[14px]">
            {ACCENT_PRESETS.map((c) => (
              <Tooltip content={c.label} variant="plain" key={c.id}>
                <button
                  onClick={() => setAccentColor(c.hex)}
                  aria-label={c.label}
                  data-accent-id={c.id}
                  className={cn(
                    "flex h-9 w-9 items-center justify-center rounded-full transition-transform",
                    accentColor.toLowerCase() === c.hex.toLowerCase()
                      ? "scale-110 ring-2 ring-offset-2 ring-offset-white dark:ring-offset-zinc-900"
                      : "hover:scale-105",
                  )}
                  style={{
                    backgroundColor: c.hex,
                    "--tw-ring-color": c.hex,
                  } as React.CSSProperties}
                />
              </Tooltip>
            ))}
          </div>
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={contentWidthOpen}
          onToggle={() => setContentWidthOpen((v) => !v)}
          title={t("settings.contentWidth")}
          ariaLabel={t("settings.contentWidth")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
          <p className="mb-2 text-xs text-zinc-500">{t("settings.contentWidthHint")}</p>
          <RadioGroup
            name="contentWidth"
            value={contentWidth}
            options={contentWidths}
            onChange={setContentWidth}
            noWrap
          />
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={fontSizeOpen}
          onToggle={() => setFontSizeOpen((v) => !v)}
          title={t("settings.fontSize")}
          ariaLabel={t("settings.fontSize")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
          <RadioGroup
            name="fontSize"
            value={fontSize}
            options={fontSizes}
            onChange={setFontSize}
          />
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={opacityOpen}
          onToggle={() => setOpacityOpen((v) => !v)}
          title={t("settings.opacity")}
          ariaLabel={t("settings.opacity")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
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
        </ExpandableRow>
      </ListBox>

      {/* Reset appearance to defaults */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={resetOpen}
          onToggle={() => setResetOpen((v) => !v)}
          title={t("settings.resetAppearance")}
          ariaLabel={t("settings.resetAppearance")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
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
        </ExpandableRow>
      </ListBox>
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
  // Tools-tab style level-1 collapsible cards (default open)
  const [logSetupOpen, setLogSetupOpen] = useState(true);
  const [dataDirectoryOpen, setDataDirectoryOpen] = useState(true);
  const [aboutOpen, setAboutOpen] = useState(true);
  const [resetOnboardingOpen, setResetOnboardingOpen] = useState(true);
  const { logLevel, setLogLevel, logFileSizeMb, setLogFileSizeMb, logFileCount, setLogFileCount, frontendLogLevel, setFrontendLogLevel } = useSettingsStore();

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
      <ListBox dividers={false}>
        <ExpandableRow
          open={logSetupOpen}
          onToggle={() => setLogSetupOpen((v) => !v)}
          title={t("settings.logSetup")}
          ariaLabel={t("settings.logSetup")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >

        {/* Log level */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-zinc-500 dark:text-zinc-400">
            {t("settings.logLevel")}
          </label>
          <div>
            <Dropdown
              className="w-[5.5rem]"
              value={currentLogLevel}
              onChange={async (val) => {
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
              options={[
                { value: "trace", label: "trace" },
                { value: "debug", label: "debug" },
                { value: "info", label: "info" },
                { value: "warn", label: "warn" },
                { value: "error", label: "error" },
              ]}
            />
          </div>
        </div>

        {/* Frontend log level (DevTools console) */}
        <div className="mb-3">
          <label className="block mb-1.5 text-xs text-zinc-500 dark:text-zinc-400">
            {t("settings.frontendLogLevel")}
          </label>
          <div>
            <Dropdown
              className="w-[5.5rem]"
              value={frontendLogLevel}
              onChange={(v) => setFrontendLogLevel(v as "trace" | "debug" | "info" | "warn" | "error" | "off")}
              options={[
                { value: "trace", label: "trace" },
                { value: "debug", label: "debug" },
                { value: "info", label: "info" },
                { value: "warn", label: "warn" },
                { value: "error", label: "error" },
                { value: "off", label: "off" },
              ]}
            />
          </div>
          <p className="mt-1 text-[10px] text-zinc-400">
            {t("settings.frontendLogLevelHint")}
          </p>
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
          <div className="fixed inset-0 z-50 flex items-cell justify-center bg-modal-overlay">
            <div className="w-[380px] rounded-md bg-modal-surface p-6 shadow-xl">
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
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={dataDirectoryOpen}
          onToggle={() => setDataDirectoryOpen((v) => !v)}
          title={t("settings.dataDirectory")}
          ariaLabel={t("settings.dataDirectory")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
          <input
            type="text"
            value={config?.data_dir ?? "\u2014"}
            readOnly
            className={`w-full ${inputReadonly}`}
          />
        </ExpandableRow>
      </ListBox>

      <ListBox dividers={false}>
        <ExpandableRow
          open={aboutOpen}
          onToggle={() => setAboutOpen((v) => !v)}
          title={t("settings.about")}
          ariaLabel={t("settings.about")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
          <div className="text-xs text-zinc-500 dark:text-zinc-400">
            <p>ACowork Desktop v0.1.0</p>
            <p className="mt-1">Built with Tauri v2 + React 19</p>
          </div>
        </ExpandableRow>
      </ListBox>

      {/* Reset Onboarding */}
      <ListBox dividers={false}>
        <ExpandableRow
          open={resetOnboardingOpen}
          onToggle={() => setResetOnboardingOpen((v) => !v)}
          title={t("settings.resetOnboarding")}
          ariaLabel={t("settings.resetOnboarding")}
          bodyClassName="rounded-b-md border-t border-zinc-300 bg-panel-inset p-3 dark:border-zinc-700"
        >
          <p className="text-xs text-zinc-500 dark:text-zinc-400">
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
                log.error("Failed to reset onboarding:", e);
              }
              window.location.reload();
            }}
            onCancel={() => setShowResetOnboardingConfirm(false)}
          />
        </ExpandableRow>
      </ListBox>
    </div>
  );
}
