import { useEffect, useState, useRef, useMemo, useCallback, Fragment } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useToast } from "../common/ToastProvider";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { AgentDetailDialog } from "./AgentDetailDialog";
import { CloneDialog } from "./CloneDialog";
import { PublishWizard } from "./PublishWizard";
import { CreateWizard } from "./CreateWizard";
import { AgentAvatar } from "../common/AgentAvatar";
import { Tooltip } from "../common/Tooltip";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { Play, Square, Trash2, Info, Copy, Plus, Search, Package, Sparkles, Bug, ChevronRight } from "lucide-react";
import { StyledInput } from "../common/StyledInput";
import { open } from "@tauri-apps/plugin-dialog";
import { isProcessing, instanceIdOf, type AgentInfo, type CloneResponse, type NodeInfo } from "../../lib/types";
import { startAgentAndSyncUI } from "../../lib/agent-start";
import { fetchNodes } from "../../lib/gateway-api";
import { partitionAgentsByNode, nodeDisplayName } from "./partitionAgentsByNode";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../common/ContextMenu";

interface AgentListProps {
  width?: number;
}

export function AgentList({ width }: AgentListProps) {
  const { t } = useTranslation();
  const isCollapsed = width !== undefined && width <= 80;
  const { selectedAgentId, loading, fetchAgents, selectAgent, stopAgent, uninstallAgent, fetchLatestSession } =
    useAgentStore();
  const agentsMap = useAgentStore((s) => s.agents);

  // ADR-014 derived view for the sidebar status dot — an agent shows the
  // dot when *any* of its cached sessions is not in `idle` (i.e. streaming,
  // waiting_approval, or paused). This is the IM-style "needs attention"
  // semantic: dot disappears when everything is idle, regardless of whether
  // the agent process itself is running.
  //
  // Source: `chatStore.agentStates[aid].sessionStates[sid].sessionStatus`,
  // kept up-to-date by `fetchSessions`'s ADR-014 Pull repair and by MQTT
  // `session_status_changed` events. Agents that have never been opened
  // have an empty sessionStates map — they show no dot until the user
  // opens them, which is the correct IM semantic (red dot = something
  // the user can act on).
  const sessionStatesByAgent = useChatStore((s) => s.agentStates);
  const activeAgentIds = useMemo(() => {
    const ids = new Set<string>();
    for (const [agentId, agentState] of Object.entries(sessionStatesByAgent)) {
      const sessionStates = agentState.sessionStates ?? {};
      for (const sess of Object.values(sessionStates)) {
        if (isProcessing(sess.sessionStatus)) {
          ids.add(agentId);
          break;
        }
      }
    }
    return ids;
  }, [sessionStatesByAgent]);
  const agentsList = useMemo(() => Object.values(agentsMap).map((s) => s.meta), [agentsMap]);

  // ADR-073: group instances by package. A package installed more than
  // once (same or different nodes) shows an instance/node badge on each
  // row so the user can tell the instances apart; singletons render the
  // pre-multi-instance layout unchanged.
  const packageCounts = useMemo(() => {
    const counts = new Map<string, number>();
    for (const a of agentsList) counts.set(a.agent_id, (counts.get(a.agent_id) ?? 0) + 1);
    return counts;
  }, [agentsList]);

  // ADR-073 §4: view mode is decided automatically by `gatewayMode`. In
  // remote mode (multi-node Gateway) the sidebar groups agents by node and
  // shows a collapsible 1/3-height header per node; in local mode the
  // pre-existing flat list renders unchanged.
  const gatewayMode = useSettingsStore((s) => s.gatewayMode);
  const isRemoteMode = gatewayMode === "remote";

  // Node list — only used when `isRemoteMode`. Refreshed alongside the
  // agent list so group headers and counts stay in sync with the network
  // view, and to gracefully degrade (empty `nodes`) when the Gateway
  // briefly can't answer.
  const [nodes, setNodes] = useState<NodeInfo[]>([]);
  const refreshNodes = useCallback(async () => {
    if (!isRemoteMode) return;
    try {
      setNodes(await fetchNodes());
    } catch {
      // Gateway unreachable — keep the previous snapshot; the next refresh
      // tick (or the agent fetch's own error) will surface the problem.
    }
  }, [isRemoteMode]);

  // ADR-073 §4: collapsible per-node groups. Default = all expanded (empty
  // Set = nothing collapsed). State is component-local — switching modes
  // or remounting resets it, which is acceptable for a sidebar view.
  const [collapsedNodes, setCollapsedNodes] = useState<Set<string>>(new Set());
  const toggleNode = useCallback((nodeId: string) => {
    setCollapsedNodes((prev) => {
      const next = new Set(prev);
      if (next.has(nodeId)) next.delete(nodeId);
      else next.add(nodeId);
      return next;
    });
  }, []);

  const { addToast } = useToast();
  const agentMenu = useContextMenu<{ agentId: string }>();
  const [installing, setInstalling] = useState(false);
  const addMenuRef = useRef<HTMLDivElement>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [addMenuOpen, setAddMenuOpen] = useState(false);
  // ADR-055 §6.13.3: when installing with >1 online node, the add-menu
  // switches to a node picker (`NodeInfo[]`); `null` = default menu.
  const [installNodes, setInstallNodes] = useState<NodeInfo[] | null>(null);

  // Track agents currently waiting for the Runtime to become ready.
  // Reading this Set is the dedup gate for `handleStart` — see guard there.
  const [startingAgentIds, setStartingAgentIds] = useState<Set<string>>(new Set());

  // Confirm dialog state
  const [confirmDialog, setConfirmDialog] = useState<{
    open: boolean;
    title: string;
    message: string;
    confirmLabel: string;
    destructive: boolean;
    onConfirm: () => void;
  }>({
    open: false,
    title: "",
    message: "",
      confirmLabel: t("common.confirm"),
    destructive: false,
    onConfirm: () => { },
  });

  // Agent detail dialog state
  const [detailAgentId, setDetailAgentId] = useState<string | null>(null);

  // Clone dialog state
  const [cloneSource, setCloneSource] = useState<{ agentId: string; agentName: string } | null>(null);

  // Publish wizard state
  const [publishTarget, setPublishTarget] = useState<{ agentId: string; agentName: string } | null>(null);

  // Create wizard state
  const [showCreateWizard, setShowCreateWizard] = useState(false);

  useEffect(() => {
    fetchAgents();
    void refreshNodes();
    const interval = setInterval(() => {
      fetchAgents();
      void refreshNodes();
    }, 30_000);
    return () => clearInterval(interval);
  }, [fetchAgents, refreshNodes]);

  // Ensure every ready agent's latest session title is loaded so the
  // sidebar shows it without requiring the user to click the agent.
  //
  // Two scenarios produce a stuck "skeleton" placeholder otherwise:
  //   1. System Agent (and other auto-started agents) whose lifecycle
  //      never goes through `startAgentAndSyncUI` → `initSessionForAgent`,
  //      so `sessionTitle` is never populated.
  //   2. Agents whose startup scan outlasts the 10-retry budget inside
  //      `initSessionForAgent`. The skeleton would otherwise persist until
  //      the user happens to click the agent.
  //
  // We key the effect on the *set* of agents that still need a fetch
  // (running && sessionTitle === undefined), so unrelated store
  // churn (sessions list updates, profile edits, MQTT online flips) does
  // not re-fire the requests.
  // Bug B v3 fix: only gate on `running` (the user-driven start
  // transition), not on `ready`. `ready` is pushed via MQTT retained
  // and arrives asynchronously to Runtime HTTP readiness. Previously,
  // if the sidebar list re-rendered with `ready=false`, we never
  // fetched the title even after the Runtime came up — the gate had
  // latched false. The fetcher `fetchLatestSession` now owns the 503
  // retry loop via `with503Retry`, so a transient 503 during the boot
  // window recovers transparently.
  const agentsNeedingTitle = useMemo(() => {
    const ids: string[] = [];
    for (const [id, storage] of Object.entries(agentsMap)) {
      if (
        storage.meta.running &&
        storage.sessionTitle === undefined
      ) {
        ids.push(id);
      }
    }
    return ids;
  }, [agentsMap]);

  useEffect(() => {
    if (agentsNeedingTitle.length === 0) return;
    for (const id of agentsNeedingTitle) {
      void fetchLatestSession(id);
    }
  }, [agentsNeedingTitle, fetchLatestSession]);

  // Close the "+ add agent" popover on outside click. The agent right-click
  // menu handles its own close inside `useContextMenu`.
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (addMenuRef.current && !addMenuRef.current.contains(e.target as Node)) {
        setAddMenuOpen(false);
        setInstallNodes(null);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, []);

  /** Pick a .agent file and install it (optionally to a specific node). */
  const doInstall = async (nodeId?: string) => {
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: t("agentList.filterAgentPackage"), extensions: ["agent"] }],
      });
      if (selected) {
        setInstalling(true);
        await useAgentStore.getState().installAgent(selected, nodeId);
        addToast({ type: "success", message: t("agentList.agentInstalled") });
        // Auto-select the newly installed agent
        await fetchAgents();
        const agentsNow = useAgentStore.getState().agents;
        const ids = Object.keys(agentsNow);
        if (ids.length > 0) {
          selectAgent(ids[ids.length - 1]);
        }
      }
    } catch (e) {
      addToast({ type: "error", message: t("agentList.errorFailedToInstallAgent", { error: String(e) }) });
    } finally {
      setInstalling(false);
    }
  };

  /**
   * "Install Agent" menu action (ADR-055 §6.13.3): resolve the online
   * nodes first. With >1 online node, switch the menu to a node picker;
   * otherwise install straight to the sole/default node.
   */
  const handleInstall = async () => {
    let onlineNodes: NodeInfo[] = [];
    try {
      onlineNodes = (await fetchNodes()).filter((n) => n.online);
    } catch {
      // Gateway unreachable — fall through to the default node; the
      // install itself will surface the connection error.
    }
    if (onlineNodes.length > 1) {
      setInstallNodes(onlineNodes);
      return;
    }
    setAddMenuOpen(false);
    await doInstall(onlineNodes.length === 1 ? onlineNodes[0].node_id : undefined);
  };

  const handleStart = async (agentId: string) => {
    // Dedup gate: prevent rapid double-fires (e.g. user double-clicks the list
    // item, or double-clicks and then triggers Start from the context menu).
    if (startingAgentIds.has(agentId)) return;
    setStartingAgentIds((prev) => new Set(prev).add(agentId));
    try {
      await startAgentAndSyncUI(agentId);
      addToast({ type: "success", message: t("agentList.agentStarted") });
    } catch (e: any) {
      addToast({ type: "error", message: e?.message ?? String(e) });
    } finally {
      setStartingAgentIds((prev) => {
        const next = new Set(prev);
        next.delete(agentId);
        return next;
      });
    }
  };

  const handleDebugStart = async (agentId: string) => {
    if (startingAgentIds.has(agentId)) return;
    setStartingAgentIds((prev) => new Set(prev).add(agentId));
    try {
      // ADR-033: MQTT replaces WebSocket; no need to clean up wsMap.
      await startAgentAndSyncUI(agentId, true);
      addToast({ type: "success", message: t("agentList.agentStartedDebug") });
    } catch (e: any) {
      addToast({ type: "error", message: e?.message ?? String(e) });
    } finally {
      setStartingAgentIds((prev) => {
        const next = new Set(prev);
        next.delete(agentId);
        return next;
      });
    }
  };

  const handleStop = async (agentId: string) => {
    const agent = agentsMap[agentId]?.meta;
    setConfirmDialog({
      open: true,
      title: t("agentList.titleStopAgent"),
      message: t("agentList.stopConfirm", { agent: agent?.name ?? agentId }),
      confirmLabel: t("agentList.confirmStop"),
      destructive: true,
      onConfirm: async () => {
        setConfirmDialog((prev) => ({ ...prev, open: false }));
        try {
          await stopAgent(agentId);
          addToast({ type: "success", message: t("agentList.agentStopped") });
        } catch (e) {
          addToast({ type: "error", message: t("agentList.errorFailedToStopAgent", { error: String(e) }) });
        }
      },
    });
  };

  const handleUninstall = (agentId: string) => {
    // Block uninstalling System Agent
    if (agentId === "com.acowork.system") {
      addToast({ type: "warning", message: t("agentList.systemAgentCannotUninstall") });
      return;
    }
    const agent = agentsMap[agentId]?.meta;
    setConfirmDialog({
      open: true,
      title: t("agentList.titleUninstallAgent"),
      message: t("agentList.uninstallConfirm", { agent: agent?.name ?? agentId }),
      confirmLabel: t("agentList.confirmUninstall"),
      destructive: true,
      onConfirm: async () => {
        setConfirmDialog((prev) => ({ ...prev, open: false }));
        try {
          await uninstallAgent(agentId);
          addToast({ type: "success", message: t("agentList.agentUninstalled") });
        } catch (e) {
          addToast({ type: "error", message: t("agentList.errorFailedToUninstallAgent", { error: String(e) }) });
        }
      },
    });
  };

  // Open the unified context menu. `useContextMenu.openAt` handles
  // preventDefault / stopPropagation / payload capture / selection snapshot
  // — see src/components/common/ContextMenu/useContextMenu.ts.
  const handleContextMenu = useCallback(
    (e: React.MouseEvent, agentId: string) => {
      agentMenu.openAt(e, { agentId });
    },
    [agentMenu],
  );

  const contextAgent = agentMenu.payload?.agentId
    ? agentsMap[agentMenu.payload.agentId]?.meta
    : undefined;

  // Memoised menu items. Built only when the resolved `contextAgent`
  // changes (so the Start / Stop / Uninstall variants flip correctly when
  // the user right-clicks a different agent) or when translations change.
  const agentMenuItems = useMemo<ContextMenuItem<{ agentId: string }>[]>(() => {
    const aid = agentMenu.payload?.agentId;
    if (!aid) return [];
    const items: ContextMenuItem<{ agentId: string }>[] = [];

    if (contextAgent && !contextAgent.running) {
      items.push({
        key: "start",
        icon: <Play size={14} />,
        label: t("agentList.contextStart"),
        onClick: ({ payload }) => payload && handleStart(payload.agentId),
      });
      items.push({
        key: "start-debug",
        icon: <Bug size={14} />,
        label: t("agentList.contextStartInDebug"),
        variant: "warning",
        onClick: ({ payload }) => payload && handleDebugStart(payload.agentId),
      });
    }
    if (contextAgent && contextAgent.running) {
      items.push({
        key: "stop",
        icon: <Square size={14} />,
        label: t("agentList.contextStop"),
        onClick: ({ payload }) => payload && handleStop(payload.agentId),
      });
    }
    items.push({
      key: "details",
      icon: <Info size={14} />,
      label: t("agentList.contextDetails"),
      onClick: ({ payload }) => payload && setDetailAgentId(payload.agentId),
    });
    items.push({
      key: "clone",
      icon: <Copy size={14} />,
      label: t("agentList.contextClone"),
      onClick: () => {
        if (!contextAgent) return;
        setCloneSource({
          // ADR-073: clone source is instance-scoped — the Gateway route
          // resolves through the installed table, so use the row's instance
          // key (aid) and never the package `agent_id` (ambiguous in
          // multi-instance deployments).
          agentId: aid,
          agentName: contextAgent.display_name ?? contextAgent.name,
        });
      },
    });
    items.push({
      key: "publish",
      icon: <Package size={14} />,
      label: t("agentList.contextPublish"),
      onClick: () => {
        if (!contextAgent) return;
        setPublishTarget({
          // ADR-073: publish prepare/execute and avatar upload are
          // instance-scoped routes — use the row's instance key (aid).
          agentId: aid,
          agentName: contextAgent.display_name ?? contextAgent.name,
        });
      },
    });
    if (contextAgent && contextAgent.agent_id !== "com.acowork.system") {
      items.push({
        key: "uninstall",
        icon: <Trash2 size={14} />,
        label: t("agentList.contextUninstall"),
        variant: "danger",
        dividerBefore: true,
        onClick: ({ payload }) => payload && handleUninstall(payload.agentId),
      });
    }
    return items;
    // contextAgent is the only signal that changes which items appear;
    // handlers are stable references from React state machinery below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentMenu.payload?.agentId, contextAgent, t]);
  const filteredAgents = agentsList.filter((a) =>
    a.name.toLowerCase().includes(searchQuery.toLowerCase()),
  );

  // ADR-073 §4: in remote mode we partition agents by `node_id` so each
  // group renders below a collapsible 1/3-height header. Groups are
  // emitted in `nodes` order (Gateway's natural ordering); any agents
  // whose `node_id` is missing or unknown to the Gateway fall into a
  // trailing "unknown" bucket so they never silently disappear.
  const nodeGroups = useMemo(() => {
    if (!isRemoteMode) return null;
    return partitionAgentsByNode(filteredAgents, nodes);
  }, [isRemoteMode, filteredAgents, nodes]);

  // Shared row renderer for both local (flat) and remote (grouped) modes.
  // `total` lets the row compute whether it is the last in its visual
  // scope (so the divider is drawn correctly inside remote groups too).
  const renderAgentItem = (agent: AgentInfo, index: number, total: number) => {
    // ADR-073: the sidebar row is an INSTANCE. `id` is the
    // canonical addressing key (instance_id, with legacy
    // agent_id fallback); `agent.agent_id` is display-only.
    const id = instanceIdOf(agent);
    const sessionTitle = agentsMap[id]?.sessionTitle;
    const multiInstance = (packageCounts.get(agent.agent_id) ?? 1) > 1;

    return (
      <div
        key={id}
        className={cn(
          "relative flex cursor-pointer items-center rounded-md px-3 py-2.5 transition-colors duration-150",
          isCollapsed ? "gap-0" : "gap-3",
          selectedAgentId === id
            ? "bg-[var(--color-accent)]/90 text-white"
            : "hover:bg-nav-item-hover",
          index < total - 1 && (isCollapsed ? "after:absolute after:bottom-0 after:left-1 after:right-1 after:border-b after:border-nav-divider/40 dark:after:border-zinc-600/40" : "after:absolute after:bottom-0 after:left-1.5 after:right-1.5 after:border-b after:border-nav-divider/40 dark:after:border-zinc-600/40")
        )}
        onClick={() => selectAgent(id)}
        onDoubleClick={() => {
          // Convenience: double-click a stopped agent to start it.
          // Running/starting agents ignore this — use context menu for Stop.
          if (!agent.running && !startingAgentIds.has(id)) {
            void handleStart(id);
          }
        }}
        title={agent.running ? undefined : t("agentList.doubleClickToStart")}
        onContextMenu={(e) => handleContextMenu(e, id)}
        role="listitem"
      >
        {/* Avatar */}
        <Tooltip
          content={isCollapsed ? (agentsMap[id]?.profile?.displayName ?? agent.display_name ?? agent.name) : ""}
          variant="plain"
          position="right"
          delayMs={0}
        >
          <div className="relative inline-flex">
            <AgentAvatar
              agentId={id}
              displayName={agent.display_name ?? agent.name}
              avatarUrl={agent.avatar}
              version={agent.version}
              builtinAvatarId={agent.builtin_avatar}
              size={40}
              className={isCollapsed ? "mx-auto" : ""}
            />
            {/* IM-style "needs attention" indicator dot — solid accent color,
                * borderless. Shown when the agent is running AND has at
                * least one session in a non-idle status (streaming /
                * waiting_approval / paused), per ADR-014. Disappears
                * once every session returns to idle. Offline agents
                * (online === false) AND auto-slept agents
                * (sleeping === true) show no dot — the latter is
                * about to flip to offline via the Runtime's Will
                * "offline" message; we suppress the dot to avoid
                * one final "active" flash on its way out. */}
            {agent.running &&
              activeAgentIds.has(id) &&
              agentsMap[id]?.online !== false &&
              agentsMap[id]?.sleeping !== true && (
                <span
                  className={cn(
                    "absolute -bottom-0.5 -right-0.5 h-2.5 w-2.5 rounded-full bg-[var(--color-accent)]"
                  )}
                />
              )}
          </div>
        </Tooltip>

        {/* Content area — width-collapsed when sidebar is collapsed to preserve item height */}
        <div className={cn("min-w-0 overflow-hidden", isCollapsed ? "w-0" : "flex-1")}>
            {/* Top row: name */}
            <div className="flex items-center justify-between gap-2">
              <div className="min-w-0 flex items-center gap-1.5">
                <span className={cn("truncate font-medium", selectedAgentId === id ? "text-white" : agent.running ? "text-zinc-900 dark:text-zinc-100" : "text-zinc-400 dark:text-zinc-500")} style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>{agentsMap[id]?.profile?.displayName ?? agent.display_name ?? agent.name}</span>
                {/* ADR-073: multi-instance badge — distinguishes this
                    row's instance/location from sibling instances. Only
                    shown in local mode; in remote mode the node
                    grouping header already disambiguates by node. */}
                {multiInstance && !isRemoteMode && (
                  <span
                    className={cn(
                      "shrink-0 rounded px-1 py-px text-[10px] leading-none font-medium",
                      selectedAgentId === id
                        ? "bg-white/20 text-white"
                        : "bg-nav-item-hover text-zinc-500 dark:text-zinc-400",
                    )}
                    title={`${t("agentList.node")}: ${agent.node_id ?? "?"}`}
                  >
                    {(agent.node_id ?? "?").slice(0, 10)}
                  </span>
                )}
              </div>
            </div>
            {/* Bottom row: current session title.
                * min-height + animate-pulse skeleton locks the row height so the agent
                * name above does not jump when the async session title loads. */}
            <div
              className="mt-0.5 flex items-center"
              style={{
                minHeight: "calc(var(--ui-font-size, 0.875rem) * 0.85 * 1.5)",
                fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)",
              }}
            >
              {agent.running ? (
                sessionTitle === undefined ? (
                  <span
                    aria-hidden
                    className={cn(
                      "block h-2.5 w-2/3 animate-pulse rounded",
                      selectedAgentId === id
                        ? "bg-modal-surface/40"
                        : "bg-zinc-300/60 dark:bg-zinc-600/60",
                    )}
                  />
                ) : (
                  <span
                    className={cn(
                      "block truncate",
                      selectedAgentId === id
                        ? "text-white/70"
                        : "text-zinc-500 dark:text-zinc-400",
                    )}
                  >
                    {sessionTitle === null ? (
                      <span aria-label="agent sleeping" className="inline-flex items-baseline">
                        <span className="zzz-n">z</span>
                        <span className="zzz-n">z</span>
                        <span className="zzz-n">z</span>
                        <span className="zzz-n">z</span>
                        <span className="zzz-n">z</span>
                      </span>
                    ) : (sessionTitle || t("sessionTabBar.untitled"))}
                  </span>
                )
              ) : (
                // Stopped agent — render the sleep animation directly
                // rather than the loading skeleton. A stopped agent will
                // never have its sessionTitle populated by the backend
                // (Runtime HTTP server is not listening), so the
                // `undefined → skeleton` branch would otherwise stay
                // stuck forever, misleading the user into thinking a
                // session is still being fetched.
                <span
                  aria-label="agent sleeping"
                  className={cn(
                    "block truncate",
                    selectedAgentId === id
                      ? "text-white/70"
                      : "text-zinc-500 dark:text-zinc-400",
                  )}
                >
                  <span className="inline-flex items-baseline">
                    <span className="zzz-n">z</span>
                    <span className="zzz-n">z</span>
                    <span className="zzz-n">z</span>
                    <span className="zzz-n">z</span>
                    <span className="zzz-n">z</span>
                  </span>
                </span>
              )}
            </div>
          </div>
      </div>
    );
  };

  return (
    <div
      className="flex flex-col shrink-0 bg-nav-surface rounded-xl border-r border-agentlist-border"
      style={{ width: width ?? 240 }}
    >
      {/* Header — search input */}
      <div className={cn(isCollapsed ? "px-1.5 py-2" : "px-3 py-2")}>
        <div className="relative min-w-0 flex-1">
          <Search
            className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-zinc-500 dark:text-zinc-400"
          />
          <StyledInput
            type="text"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            placeholder={isCollapsed ? "" : t("agentList.searchPlaceholder")}
            aria-label={t("agentList.searchPlaceholder")}
            className={cn(
              "rounded-md bg-nav-control pl-7 py-1.5",
              isCollapsed ? "min-w-0 pr-0" : "pr-2",
            )}
          />
        </div>
      </div>

      {/* Agent list */}
      <div className="flex-1 overflow-y-auto overflow-x-hidden" role="list" aria-label={t("agentList.ariaLabelAgentList")}>

        {loading && agentsList.length === 0 && (
          <div className="flex items-center justify-center py-8">
            <div className="h-5 w-5 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
          </div>
        )}

        {isRemoteMode && nodeGroups
          ? nodeGroups.map((group) => {
              const collapsed = collapsedNodes.has(group.nodeId);
              return (
                <Fragment key={group.nodeId}>
                  <NodeGroupHeader
                    nodeName={nodeDisplayName(group)}
                    online={group.node?.online ?? false}
                    collapsed={collapsed}
                    onToggle={() => toggleNode(group.nodeId)}
                    agentCount={group.agents.length}
                  />
                  {!collapsed &&
                    group.agents.map((agent, index) => renderAgentItem(agent, index, group.agents.length))}
                </Fragment>
              );
            })
          : filteredAgents.map((agent, index) => renderAgentItem(agent, index, filteredAgents.length))}

        {filteredAgents.length === 0 && !loading && (
          <div className="px-3 py-8 text-center text-xs text-zinc-400 dark:text-zinc-500">
            {agentsList.length === 0 ? t("agentList.noAgentsInstalled") : t("agentList.noMatchingAgents")}
          </div>
        )}
      </div>

      <div ref={addMenuRef} className="relative p-1.5">
        <button
          onClick={() => {
            setAddMenuOpen(!addMenuOpen);
            setInstallNodes(null);
          }}
          className="flex w-full items-center justify-center rounded-md px-0 py-[var(--ui-btn-py)] text-xs font-medium text-zinc-600 transition-colors hover:bg-nav-control focus-visible:bg-nav-control dark:text-zinc-300"
          aria-label={t("agentList.ariaLabelAddAgent")}
        >
          <Plus className="h-3.5 w-3.5" />
        </button>
        {addMenuOpen && (
          <div className="absolute bottom-full left-1 z-50 mb-1 w-max rounded-md border border-zinc-200 bg-modal-surface py-1 shadow-lg dark:border-zinc-700">
            {installNodes !== null ? (
              <>
                <div className="px-3 py-1.5 text-[10px] font-medium uppercase tracking-wide text-zinc-400">
                  {t("agentList.selectNode")}
                </div>
                {installNodes.map((node) => (
                  <button
                    key={node.node_id}
                    onClick={() => {
                      setAddMenuOpen(false);
                      setInstallNodes(null);
                      void doInstall(node.node_id);
                    }}
                    className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
                  >
                    <span className="h-2 w-2 rounded-full bg-[var(--color-accent)]" />
                    {node.node_id}
                    <span className="ml-auto text-zinc-400">
                      {node.os ?? ""} {node.arch ?? ""}
                    </span>
                  </button>
                ))}
                <button
                  onClick={() => setInstallNodes(null)}
                  className="flex w-full items-center gap-2 border-t border-zinc-100 px-3 py-1.5 text-xs text-zinc-400 transition-colors hover:bg-zinc-50 dark:border-zinc-700/50 dark:text-zinc-500 dark:hover:bg-zinc-700/50"
                >
                  {t("agentList.back")}
                </button>
              </>
            ) : (
              <>
                <button
                  onClick={() => {
                    setAddMenuOpen(false);
                    setShowCreateWizard(true);
                  }}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
                >
                  <Sparkles className="h-3.5 w-3.5" />
                  {t("agentList.createAgent")}
                </button>
                <button
                  onClick={() => {
                    void handleInstall();
                  }}
                  disabled={installing}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-600 transition-colors hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50"
                >
                  <Plus className="h-3.5 w-3.5" />
                  {t("agentList.installAgent")}
                </button>
              </>
            )}
          </div>
        )}
      </div>

      {/* Unified context menu — items only depend on the right-clicked agent. */}
      <ContextMenu<{ agentId: string }>
        isOpen={agentMenu.isOpen}
        menuProps={agentMenu.menuProps}
        items={agentMenuItems}
        payload={agentMenu.payload}
        selectionAtOpen={agentMenu.selectionAtOpen}
        onClose={agentMenu.close}
      />

      {/* Confirm dialog */}
      <ConfirmDialog
        open={confirmDialog.open}
        title={confirmDialog.title}
        message={confirmDialog.message}
        confirmLabel={confirmDialog.confirmLabel}
        destructive={confirmDialog.destructive}
        onConfirm={confirmDialog.onConfirm}
        onCancel={() => setConfirmDialog((prev) => ({ ...prev, open: false }))}
      />

      {/* Agent detail dialog */}
      <AgentDetailDialog
        open={!!detailAgentId}
        agentId={detailAgentId}
        onClose={() => setDetailAgentId(null)}
      />

      {/* Clone dialog */}
      <CloneDialog
        open={!!cloneSource}
        agentId={cloneSource?.agentId ?? ""}
        agentName={cloneSource?.agentName ?? ""}
        onCloned={(result: CloneResponse) => {
          setCloneSource(null);
          addToast({ type: "success", message: t("agentList.agentCloned", { agentId: result.agent_id }) });
          void fetchAgents().then(() => {
            // ADR-073: select by INSTANCE identity — the clone response
            // carries the new package id, so match the freshly installed
            // row through the manifest agent_id and select its instance key.
            const entry = Object.entries(useAgentStore.getState().agents).find(
              ([, s]) => s.meta.agent_id === result.agent_id,
            );
            if (entry) selectAgent(entry[0]);
          });
        }}
        onClose={() => setCloneSource(null)}
      />

      {/* Publish wizard */}
      <PublishWizard
        open={!!publishTarget}
        agentId={publishTarget?.agentId ?? ""}
        agentName={publishTarget?.agentName ?? ""}
        onClose={() => setPublishTarget(null)}
      />

      {/* Create wizard */}
      <CreateWizard
        open={showCreateWizard}
        onCreated={(agentId) => {
          setShowCreateWizard(false);
          addToast({ type: "success", message: t("agentList.agentCreated", { agentId }) });
          void fetchAgents().then(() => {
            selectAgent(agentId);
          });
        }}
        onClose={() => setShowCreateWizard(false)}
      />
    </div>
  );
}

/**
 * ADR-073 §4: collapsible group header shown above each node bucket in
 * remote-mode view. ~1/3 of an agent row's height (h-5 = 20px vs row's
 * ~56px), no background, no border — only the same divider the agent
 * rows use, plus a chevron + the node display name. Click anywhere on
 * the row to toggle; default state is collapsed=false (expanded).
 */
interface NodeGroupHeaderProps {
  nodeName: string;
  online: boolean;
  collapsed: boolean;
  onToggle: () => void;
  agentCount: number;
}

function NodeGroupHeader({
  nodeName,
  online,
  collapsed,
  onToggle,
  agentCount,
}: NodeGroupHeaderProps) {
  return (
    <button
      type="button"
      onClick={onToggle}
      aria-expanded={!collapsed}
      aria-label={`Toggle node group: ${nodeName}`}
      data-testid="node-group-header"
      className={cn(
        // h-5 (20px) ≈ 1/3 of the agent row's ~56px height.
        "flex h-5 w-full items-center gap-1.5 px-3 text-left",
        "text-[10px] font-medium uppercase tracking-wide",
        "text-zinc-400 dark:text-zinc-500",
        "hover:text-zinc-600 dark:hover:text-zinc-300",
        "transition-colors duration-150",
        // Mirror the agent row's bottom divider so visual rhythm is
        // preserved even when the header sits above an empty group.
        "border-b border-nav-divider/40 dark:border-zinc-600/40",
      )}
    >
      <ChevronRight
        className={cn(
          "h-3 w-3 shrink-0 transition-transform duration-150",
          !collapsed && "rotate-90",
        )}
      />
      <span
        className={cn(
          "h-1.5 w-1.5 shrink-0 rounded-full",
          online ? "bg-emerald-500/70" : "bg-zinc-400/40 dark:bg-zinc-500/40",
        )}
        aria-hidden
      />
      <span className="truncate">{nodeName}</span>
      <span className="ml-auto text-[10px] font-normal opacity-60">
        {agentCount}
      </span>
    </button>
  );
}
