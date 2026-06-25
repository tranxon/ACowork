import { useState, useCallback, useEffect, useRef } from "react";
import type { NavView } from "../../lib/types";
import { NavBar } from "./NavBar";
import { TitleBar } from "./TitleBar";
import { AgentList } from "../agent-list/AgentList";
import { ChatPanel } from "../chat/ChatPanel";
import { ResultsPanel } from "../results/ResultsPanel";
import { RightNavBar } from "./RightNavBar";
import { FileEditorPanel } from "../editor/FileEditorPanel";
import { GatewayBanner } from "./GatewayBanner";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useAgentStore } from "../../stores/agentStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useStatusBarStore } from "../../stores/statusBarStore";
import { cn } from "../../lib/utils";
import { SettingsPage } from "../settings/SettingsPage";
import { HarnessPage } from "../harness/HarnessPage";
import { useChatStore } from "../../stores/chatStore";
import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import { Bot, MessagesSquare, Cpu } from "lucide-react";

/** Settings tab type — keep in sync with SettingsPage */
type SettingsTab = "gateway" | "appearance" | "general" | "profile";
type PanelTab = "debug" | "status" | "setup" | "tools" | "memory" | "workspace";

const MIN_SIDEBAR_WIDTH = 100;
const AVATAR_SIDEBAR_WIDTH = 64;
const MAX_SIDEBAR_WIDTH = 400;
const DEFAULT_SIDEBAR_WIDTH = 240;
const SIDEBAR_WIDTH_KEY = "acowork-sidebar-width";

const MIN_RIGHT_WIDTH = 200;
const MAX_RIGHT_WIDTH = 600;
const DEFAULT_RIGHT_WIDTH = 340;
const RIGHT_WIDTH_KEY = "acowork-right-width";

const MIN_FILE_WIDTH = 200;
const MAX_FILE_WIDTH = 900;
const DEFAULT_FILE_WIDTH = 450;
const FILE_WIDTH_KEY = "acowork-file-width";
const MIN_CHAT_WIDTH = 288;

export function AppLayout() {
  const [currentView, setCurrentView] = useState<NavView>("chat");
  const [settingsInitialTab, setSettingsInitialTab] = useState<SettingsTab>("gateway");
  const [resultsCollapsed, setResultsCollapsed] = useState(false);
  const [activeTab, setActiveTab] = useState<PanelTab>("workspace");
  const [sidebarWidth, setSidebarWidth] = useState(() => {
    const stored = localStorage.getItem(SIDEBAR_WIDTH_KEY);
    if (stored) {
      const val = parseInt(stored, 10);
      if (val <= AVATAR_SIDEBAR_WIDTH) return AVATAR_SIDEBAR_WIDTH;
      return Math.min(val, MAX_SIDEBAR_WIDTH);
    }
    return DEFAULT_SIDEBAR_WIDTH;
  });
  const [rightWidth, setRightWidth] = useState(() => {
    const stored = localStorage.getItem(RIGHT_WIDTH_KEY);
    return stored ? Math.min(Math.max(parseInt(stored, 10), MIN_RIGHT_WIDTH), MAX_RIGHT_WIDTH) : DEFAULT_RIGHT_WIDTH;
  });
  const [fileWidth, setFileWidth] = useState(() => {
    const stored = localStorage.getItem(FILE_WIDTH_KEY);
    return stored ? Math.min(Math.max(parseInt(stored, 10), MIN_FILE_WIDTH), MAX_FILE_WIDTH) : DEFAULT_FILE_WIDTH;
  });
  const hasOpenFiles = useFileEditorStore((s) => s.openFiles.length > 0);
  const fileWidthInitialized = useRef(false);

  // Refs to track latest panel widths for proportional window-resize scaling
  const fileWidthValueRef = useRef(fileWidth);
  fileWidthValueRef.current = fileWidth;
  const sidebarWidthRef = useRef(sidebarWidth);
  sidebarWidthRef.current = sidebarWidth;
  const rightWidthRef = useRef(rightWidth);
  rightWidthRef.current = rightWidth;
  const resultsCollapsedRef = useRef(resultsCollapsed);
  resultsCollapsedRef.current = resultsCollapsed;

  // Auto-size file panel to half available area on first open
  useEffect(() => {
    if (hasOpenFiles && !fileWidthInitialized.current) {
      fileWidthInitialized.current = true;
      const navWidth = 48;
      const actualRightWidth = resultsCollapsed ? 0 : rightWidth;
      const available = window.innerWidth - sidebarWidth - actualRightWidth - navWidth;
      const halfWidth = Math.min(Math.max(Math.round(available / 2), MIN_FILE_WIDTH), MAX_FILE_WIDTH);
      // Always recalculate on first open to respect current window size,
      // preventing the stored width from obscuring the session panel
      setFileWidth(halfWidth);
      localStorage.setItem(FILE_WIDTH_KEY, String(halfWidth));
    }
    if (!hasOpenFiles) {
      fileWidthInitialized.current = false;
    }
  }, [hasOpenFiles, sidebarWidth, rightWidth, resultsCollapsed]);

  const gatewayStatus = useGatewayStore((s) => s.status);
  const checkHealth = useGatewayStore((s) => s.checkHealth);
  const setStatus = useStatusBarStore((s) => s.setStatus);
  const statusMsg = useStatusBarStore((s) => s.message);
  const statusType = useStatusBarStore((s) => s.type);
  const statusVisible = useStatusBarStore((s) => s.visible);
  const clearStatus = useStatusBarStore((s) => s.clearStatus);
  // Determine if selected agent is in debug mode
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const agents = useAgentStore((s) => s.agents);
  const selectedAgent = selectedAgentId ? (agents[selectedAgentId]?.meta ?? null) : null;
  const isDebugMode = selectedAgent?.dev_mode && selectedAgent?.running;
  const agentDisplayName = selectedAgent
    ? (agents[selectedAgent.agent_id]?.profile?.displayName ??
      selectedAgent.display_name ??
      selectedAgent.name)
    : null;
  // Agent session count + context usage for the bottom status bar
  const openSessionCount = useChatStore((s) => {
    if (!selectedAgentId) return 0;
    return s.agentStates[selectedAgentId]?.openSessionIds?.length ?? 0;
  });
  const contextUsage = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.contextUsage ?? null;
  });
  const { t } = useTranslation();

  // ── Glass background color ───────────────────────────────────────
  const { opacity, theme } = useSettingsStore();
  const isDark = theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  const glassBg = isDark ? `rgba(41,42,44,${opacity})` : `rgba(226,227,233,${opacity})`;

  // ── Switch to debug tab when entering debug mode ─────────────────
  const prevIsDebugMode = useRef(isDebugMode);
  useEffect(() => {
    if (isDebugMode && !prevIsDebugMode.current) {
      setActiveTab("debug");
    }
    prevIsDebugMode.current = isDebugMode;
  }, [isDebugMode]);

  // ── Track last non-debug tab so we can restore it when leaving debug ─
  const lastNonDebugTab = useRef<PanelTab>(activeTab);
  useEffect(() => {
    if (activeTab !== "debug") {
      lastNonDebugTab.current = activeTab;
    }
  }, [activeTab]);

  // ── When agent switches, restore last non-debug tab if new agent has no debug ─
  const prevSelectedAgentId = useRef(selectedAgentId);
  useEffect(() => {
    if (prevSelectedAgentId.current !== selectedAgentId) {
      prevSelectedAgentId.current = selectedAgentId;
      if (activeTab === "debug" && !isDebugMode) {
        setActiveTab(lastNonDebugTab.current);
      }
    }
  }, [selectedAgentId, isDebugMode, activeTab]);

  // ── Switch to status tab when agent stops ────────────────────────
  const prevRunning = useRef(selectedAgent?.running);
  useEffect(() => {
    const isRunning = selectedAgent?.running ?? false;
    const wasRunning = prevRunning.current;
    if (!isRunning && wasRunning !== false && (activeTab === "memory" || activeTab === "setup")) {
      setActiveTab("status");
    }
    prevRunning.current = isRunning;
  }, [selectedAgent?.running, activeTab]);

  const isResizing = useRef(false);
  const startX = useRef(0);
  const startWidth = useRef(DEFAULT_SIDEBAR_WIDTH);
  const currentWidthRef = useRef(DEFAULT_SIDEBAR_WIDTH);
  const isResizingRight = useRef(false);
  const startXRight = useRef(0);
  const startWidthRight = useRef(DEFAULT_RIGHT_WIDTH);
  const currentWidthRefRight = useRef(DEFAULT_RIGHT_WIDTH);
  const isResizingFile = useRef(false);
  const startXFile = useRef(0);
  const startWidthFile = useRef(DEFAULT_FILE_WIDTH);
  const currentWidthRefFile = useRef(DEFAULT_FILE_WIDTH);

  // Periodically check Gateway health to detect disconnections.
  // Gateway is spawned by Rust at exe startup — no need to start it here.
  useEffect(() => {
    checkHealth();
    const interval = setInterval(() => {
      if (useGatewayStore.getState().status !== "connected") {
        checkHealth();
      }
    }, 5000);
    return () => clearInterval(interval);
  }, [checkHealth]);

  // Update status bar on gateway status changes
  useEffect(() => {
    if (gatewayStatus === "connected") {
      clearStatus();
    } else if (gatewayStatus === "error") {
      setStatus("Gateway connection failed", "error");
    } else {
      setStatus("Connecting to Gateway...", "warning");
    }
  }, [gatewayStatus, setStatus, clearStatus]);

  // Detect wake from sleep via visibility change and reconnect
  useEffect(() => {
    const handleVisibility = () => {
      if (document.visibilityState !== "visible") return;
      console.log("[AppLayout] Page visible after sleep/lock, checking connections");
      checkHealth();
      // Reconnect all agent WebSocket connections
      const store = useChatStore.getState();
      const gwUrl = getGatewayUrl();
      for (const agentId of Object.keys(store.wsMap)) {
        const ws = store.wsMap[agentId];
        if (!ws || ws.readyState === WebSocket.CLOSED || ws.readyState === WebSocket.CLOSING) {
          store.connectStream(agentId, gwUrl);
        }
      }
    };
    document.addEventListener("visibilitychange", handleVisibility);
    return () => document.removeEventListener("visibilitychange", handleVisibility);
  }, [checkHealth]);

  // Scale file panel proportionally when window size changes significantly (maximize/restore).
  // Sidebar and right panel keep their absolute widths; only session & file panels scale.
  // Small manual edge-drags (<5%) are ignored to avoid jitter.
  const NAV_WIDTH = 48;
  const prevAvailableWidthRef = useRef(window.innerWidth - sidebarWidth - (resultsCollapsed ? 0 : rightWidth) - NAV_WIDTH);
  useEffect(() => {
    const handleWindowResize = () => {
      // Don't scale during manual panel resize
      if (isResizingFile.current) return;

      const newWindowWidth = window.innerWidth;
      const constantWidths = sidebarWidthRef.current + (resultsCollapsedRef.current ? 0 : rightWidthRef.current) + NAV_WIDTH;
      const newAvailable = newWindowWidth - constantWidths;
      const prevAvailable = prevAvailableWidthRef.current;

      // Guard against zero or negative available space
      if (prevAvailable <= 0 || newAvailable <= 0) return;

      const ratio = newAvailable / prevAvailable;

      // Only scale when available space changes significantly (>5%)
      if (Math.abs(ratio - 1) < 0.05) return;

      prevAvailableWidthRef.current = newAvailable;

      // Scale the file panel by the available-space ratio; ChatPanel (flex-1) gets the rest.
      // This preserves the same proportion of file vs session within the available space.
      const hasFiles = useFileEditorStore.getState().openFiles.length > 0;
      if (hasFiles) {
        const newFile = Math.min(Math.max(Math.round(fileWidthValueRef.current * ratio), MIN_FILE_WIDTH), MAX_FILE_WIDTH);
        setFileWidth(newFile);
        localStorage.setItem(FILE_WIDTH_KEY, String(newFile));
      }
    };

    window.addEventListener("resize", handleWindowResize);
    return () => window.removeEventListener("resize", handleWindowResize);
  }, []);

  const toggleResults = useCallback(() => {
    setResultsCollapsed((prev) => !prev);
  }, []);

  // Navigate to settings with profile tab when avatar is clicked
  const handleAvatarClick = useCallback(() => {
    setSettingsInitialTab("profile");
    setCurrentView("settings");
  }, []);

  // Navigate via nav bar — reset settings tab to default
  const handleViewChange = useCallback((view: NavView) => {
    if (view === "settings") {
      setSettingsInitialTab("profile");
    }
    setCurrentView(view);
  }, []);

  // Sidebar resize handlers
  const handleMouseMove = useCallback((e: MouseEvent) => {
    e.preventDefault();
    if (!isResizing.current) return;
    const delta = e.clientX - startX.current;
    const rawWidth = startWidth.current + delta;

    if (currentWidthRef.current === AVATAR_SIDEBAR_WIDTH) {
      // In collapsed state — only expand when dragged back past MIN_SIDEBAR_WIDTH
      if (rawWidth >= MIN_SIDEBAR_WIDTH) {
        const newWidth = Math.min(rawWidth, MAX_SIDEBAR_WIDTH);
        currentWidthRef.current = newWidth;
        setSidebarWidth(newWidth);
      }
      return;
    }

    if (rawWidth < MIN_SIDEBAR_WIDTH) {
      // Crossed below minimum — collapse to avatar width
      currentWidthRef.current = AVATAR_SIDEBAR_WIDTH;
      setSidebarWidth(AVATAR_SIDEBAR_WIDTH);
    } else if (rawWidth > MAX_SIDEBAR_WIDTH) {
      currentWidthRef.current = MAX_SIDEBAR_WIDTH;
      setSidebarWidth(MAX_SIDEBAR_WIDTH);
    } else {
      currentWidthRef.current = rawWidth;
      setSidebarWidth(rawWidth);
    }
  }, []);

  const handleMouseUp = useCallback(() => {
    if (!isResizing.current) return;
    isResizing.current = false;
    document.body.style.userSelect = '';
    document.removeEventListener("mousemove", handleMouseMove);
    document.removeEventListener("mouseup", handleMouseUp);
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(currentWidthRef.current));
  }, [handleMouseMove]);

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    document.body.style.userSelect = 'none';
    isResizing.current = true;
    startX.current = e.clientX;
    startWidth.current = sidebarWidth;
    currentWidthRef.current = sidebarWidth;
    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);
  }, [handleMouseMove, handleMouseUp, sidebarWidth]);

  // Right panel resize handlers
  const handleMouseMoveRight = useCallback((e: MouseEvent) => {
    e.preventDefault();
    if (!isResizingRight.current) return;
    const delta = e.clientX - startXRight.current;
    const newWidth = Math.min(Math.max(startWidthRight.current - delta, MIN_RIGHT_WIDTH), MAX_RIGHT_WIDTH);
    currentWidthRefRight.current = newWidth;
    setRightWidth(newWidth);
  }, []);

  const handleMouseUpRight = useCallback(() => {
    if (!isResizingRight.current) return;
    isResizingRight.current = false;
    document.body.style.userSelect = '';
    document.removeEventListener("mousemove", handleMouseMoveRight);
    document.removeEventListener("mouseup", handleMouseUpRight);
    localStorage.setItem(RIGHT_WIDTH_KEY, String(currentWidthRefRight.current));
  }, [handleMouseMoveRight]);

  const handleMouseDownRight = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    document.body.style.userSelect = 'none';
    isResizingRight.current = true;
    startXRight.current = e.clientX;
    startWidthRight.current = rightWidth;
    currentWidthRefRight.current = rightWidth;
    document.addEventListener("mousemove", handleMouseMoveRight);
    document.addEventListener("mouseup", handleMouseUpRight);
  }, [handleMouseMoveRight, handleMouseUpRight, rightWidth]);

  // File panel resize handlers — dynamic max width to keep ChatPanel visible
  const maxFileWidthRef = useRef(MAX_FILE_WIDTH);

  const handleMouseMoveFile = useCallback((e: MouseEvent) => {
    e.preventDefault();
    if (!isResizingFile.current) return;
    const delta = e.clientX - startXFile.current;
    const newWidth = Math.min(Math.max(startWidthFile.current - delta, MIN_FILE_WIDTH), maxFileWidthRef.current);
    currentWidthRefFile.current = newWidth;
    setFileWidth(newWidth);
  }, []);

  const handleMouseUpFile = useCallback(() => {
    if (!isResizingFile.current) return;
    isResizingFile.current = false;
    document.body.style.userSelect = '';
    document.removeEventListener("mousemove", handleMouseMoveFile);
    document.removeEventListener("mouseup", handleMouseUpFile);
    localStorage.setItem(FILE_WIDTH_KEY, String(currentWidthRefFile.current));
  }, [handleMouseMoveFile]);

  const handleMouseDownFile = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    document.body.style.userSelect = 'none';
    isResizingFile.current = true;
    startXFile.current = e.clientX;
    startWidthFile.current = fileWidth;
    currentWidthRefFile.current = fileWidth;
    // Calculate dynamic max to ensure ChatPanel retains enough width for the collapsed toolbar
    const navWidth = 48;
    const actualRightWidth = resultsCollapsed ? 0 : rightWidth;
    const dynamicMax = Math.max(window.innerWidth - sidebarWidth - actualRightWidth - navWidth - MIN_CHAT_WIDTH, MIN_FILE_WIDTH);
    maxFileWidthRef.current = Math.min(MAX_FILE_WIDTH, dynamicMax);
    document.addEventListener("mousemove", handleMouseMoveFile);
    document.addEventListener("mouseup", handleMouseUpFile);
  }, [handleMouseMoveFile, handleMouseUpFile, fileWidth, sidebarWidth, rightWidth, resultsCollapsed]);

  return (
    <div className="flex h-full w-full flex-col backdrop-blur-sm" style={{ backgroundColor: glassBg } as React.CSSProperties}>
      {/* Custom title bar — macOS only. On Windows/Linux, decorations:true
          already provides a native title bar, and titleBarStyle:"Overlay"
          is macOS-only. Rendering this on Windows would cause double
          title bars and duplicate window control buttons. */}
      {navigator.platform.includes("Mac") && <TitleBar />}

      {/* Gateway disconnected banner */}
      {gatewayStatus !== "connected" && <GatewayBanner />}

      {/* Main content area */}
      <div className="flex flex-1 overflow-hidden">
        {/* Navigation bar — 48px */}
        <NavBar currentView={currentView} onViewChange={handleViewChange} onAvatarClick={handleAvatarClick} />

        {/* Content area based on current view */}
        {currentView === "chat" && (
          <div className="flex flex-1 overflow-hidden">
            {/* Agent list — resizable */}
            <AgentList width={sidebarWidth} />

            {/* Resize handle */}
            <div
              className="group relative w-1 shrink-0 cursor-col-resize select-none"
              onMouseDown={handleMouseDown}
              role="separator"
              aria-label="Resize sidebar"
            >
              {/* Visible divider line — removed, use glass bg as separator */}
              <div className="absolute inset-y-0 left-0 w-1 group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60 transition-colors rounded-full" />
            </div>

            {/* Chat panel — elastic */}
            <ChatPanel />

            {/* File editor panel — shown when files are open */}
            {hasOpenFiles && (
              <>
                {/* Resize handle between chat and file editor */}
                <div
                  className="group relative w-1 shrink-0 cursor-col-resize select-none"
                  onMouseDown={handleMouseDownFile}
                  role="separator"
                  aria-label="Resize file editor"
                >
                  <div className="absolute inset-y-0 left-0 w-1 group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60 transition-colors rounded-full" />
                </div>
                <FileEditorPanel width={fileWidth} />
              </>
            )}

            {/* Results panel — unified tabs, collapsible, resizable */}
            {!resultsCollapsed && (
              <ResultsPanel width={rightWidth} onCollapse={toggleResults} isDebugMode={isDebugMode} onResizeStart={handleMouseDownRight} activeTab={activeTab} onTabChange={setActiveTab} />
            )}
          </div>
        )}

        {/* Right Nav Bar — always rendered, clicking expands panel */}
        {currentView === "chat" && (
          <RightNavBar
            activeTab={activeTab}
            onTabChange={(tab) => {
              if (!resultsCollapsed && tab === activeTab) {
                setResultsCollapsed(true);
              } else {
                setResultsCollapsed(false);
                setActiveTab(tab);
              }
            }}
            isDebugMode={!!isDebugMode}
            agentRunning={selectedAgent?.running ?? false}
            collapsed={resultsCollapsed}
          />
        )}

        {currentView === "settings" && (
          <div className="flex flex-1 overflow-hidden rounded-lg bg-[#FAFAFA] dark:bg-zinc-900">
            <SettingsPage initialTab={settingsInitialTab} />
          </div>
        )}

        {currentView === "harness" && (
          <div className="flex flex-1 overflow-hidden rounded-lg bg-[#FAFAFA] dark:bg-zinc-900">
            <HarnessPage />
          </div>
        )}

        {(currentView === "projects" || currentView === "docs") && (
          <div className="flex flex-1 items-center justify-center overflow-hidden rounded-lg bg-[#FAFAFA] dark:bg-zinc-900">
            <div className="rounded-md border border-zinc-200 bg-white p-8 dark:border-zinc-700 dark:bg-zinc-800">
              <p className="text-sm text-zinc-400 dark:text-zinc-500">TODO</p>
            </div>
          </div>
        )}
      </div>

      {/* Bottom status bar */}
      <div className="flex h-5 shrink-0 items-center gap-3 pl-12 pr-3 text-[11px] select-none dark:text-zinc-400">
        {statusVisible && (
          <span className={cn(
            "truncate",
            statusType === "error" && "text-red-500",
            statusType === "warning" && "text-amber-600",
            statusType === "info" && "text-zinc-500",
          )}>
            {statusMsg}
          </span>
        )}
        {(resultsCollapsed || activeTab !== "status") && selectedAgent?.running && agentDisplayName && (
          <span className="flex items-center gap-[22px] truncate">
            <span className="flex items-center gap-1 pl-1">
              <Bot className="h-3 w-3 text-zinc-500 dark:text-zinc-500" aria-hidden="true" />
              <span className="text-zinc-500 dark:text-zinc-500">{t("statusBar.agent")}: </span>
              <span className="font-medium text-zinc-500 dark:text-zinc-400">{agentDisplayName}</span>
            </span>
            <span className="flex items-center gap-1">
              <MessagesSquare className="h-3 w-3 text-zinc-500 dark:text-zinc-500" aria-hidden="true" />
              <span className="text-zinc-500 dark:text-zinc-500">{t("statusBar.sessions")}: </span>
              <span className="tabular-nums text-zinc-500 dark:text-zinc-400">{openSessionCount}</span>
            </span>
            {contextUsage && (
              <span className="flex items-center gap-1">
                <Cpu className="h-3 w-3 text-zinc-500 dark:text-zinc-500" aria-hidden="true" />
                <span className="text-zinc-500 dark:text-zinc-500">{t("statusBar.context")}: </span>
                <span
                  className="tabular-nums text-zinc-500 dark:text-zinc-400"
                  style={{
                    color:
                      contextUsage.usage_percent >= 90
                        ? "var(--color-accent)"
                        : undefined,
                  }}
                >
                  {contextUsage.usage_percent}%
                </span>
                <span className="text-zinc-400 dark:text-zinc-500"> | </span>
                <span className="tabular-nums text-zinc-500 dark:text-zinc-400">
                  {formatTokenCount(contextUsage.total_tokens)}/{formatTokenCount(contextUsage.context_window)}
                </span>
              </span>
            )}
          </span>
        )}
      </div>

    </div>
  );
}

function formatTokenCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}
