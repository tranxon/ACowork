import { useState, useCallback, useEffect, useRef } from "react";
import type { NavView } from "../../lib/types";
import { NavBar } from "./NavBar";
import { TitleBar } from "./TitleBar";
import { AgentList } from "../agent-list/AgentList";
import { ChatPanel } from "../chat/ChatPanel";
import { ResultsPanel } from "../results/ResultsPanel";
import { GatewayBanner } from "./GatewayBanner";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useAgentStore } from "../../stores/agentStore";
import { SettingsPage } from "../settings/SettingsPage";
import { HarnessPage } from "../harness/HarnessPage";
import { useChatStore } from "../../stores/chatStore";
import { getGatewayUrl } from "../../lib/config";

/** Settings tab type — keep in sync with SettingsPage */
type SettingsTab = "gateway" | "appearance" | "general" | "profile";

const MIN_SIDEBAR_WIDTH = 160;
const MAX_SIDEBAR_WIDTH = 400;
const DEFAULT_SIDEBAR_WIDTH = 240;
const SIDEBAR_WIDTH_KEY = "rollball-sidebar-width";

const MIN_RIGHT_WIDTH = 340;
const MAX_RIGHT_WIDTH = 600;
const DEFAULT_RIGHT_WIDTH = 340;
const RIGHT_WIDTH_KEY = "rollball-right-width";

export function AppLayout() {
  const [currentView, setCurrentView] = useState<NavView>("chat");
  const [settingsInitialTab, setSettingsInitialTab] = useState<SettingsTab>("gateway");
  const [resultsCollapsed, setResultsCollapsed] = useState(false);
  const [sidebarWidth, setSidebarWidth] = useState(() => {
    const stored = localStorage.getItem(SIDEBAR_WIDTH_KEY);
    return stored ? Math.min(Math.max(parseInt(stored, 10), MIN_SIDEBAR_WIDTH), MAX_SIDEBAR_WIDTH) : DEFAULT_SIDEBAR_WIDTH;
  });
  const [rightWidth, setRightWidth] = useState(() => {
    const stored = localStorage.getItem(RIGHT_WIDTH_KEY);
    return stored ? Math.min(Math.max(parseInt(stored, 10), MIN_RIGHT_WIDTH), MAX_RIGHT_WIDTH) : DEFAULT_RIGHT_WIDTH;
  });
  const gatewayStatus = useGatewayStore((s) => s.status);
  const checkHealth = useGatewayStore((s) => s.checkHealth);
  // Determine if selected agent is in debug mode
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const agents = useAgentStore((s) => s.agents);
  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);
  const isDebugMode = selectedAgent?.dev_mode && selectedAgent?.running;
  const isResizing = useRef(false);
  const startX = useRef(0);
  const startWidth = useRef(DEFAULT_SIDEBAR_WIDTH);
  const currentWidthRef = useRef(DEFAULT_SIDEBAR_WIDTH);
  const isResizingRight = useRef(false);
  const startXRight = useRef(0);
  const startWidthRight = useRef(DEFAULT_RIGHT_WIDTH);
  const currentWidthRefRight = useRef(DEFAULT_RIGHT_WIDTH);

  // Check Gateway health on mount and periodically
  useEffect(() => {
    checkHealth();
    const interval = setInterval(() => {
      if (useGatewayStore.getState().status !== "connected") {
        checkHealth();
      }
    }, 5000);
    return () => clearInterval(interval);
  }, [checkHealth]);

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
    if (!isResizing.current) return;
    const delta = e.clientX - startX.current;
    const newWidth = Math.min(Math.max(startWidth.current + delta, MIN_SIDEBAR_WIDTH), MAX_SIDEBAR_WIDTH);
    currentWidthRef.current = newWidth;
    setSidebarWidth(newWidth);
  }, []);

  const handleMouseUp = useCallback(() => {
    if (!isResizing.current) return;
    isResizing.current = false;
    document.removeEventListener("mousemove", handleMouseMove);
    document.removeEventListener("mouseup", handleMouseUp);
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(currentWidthRef.current));
  }, [handleMouseMove]);

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    isResizing.current = true;
    startX.current = e.clientX;
    startWidth.current = sidebarWidth;
    currentWidthRef.current = sidebarWidth;
    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);
  }, [handleMouseMove, handleMouseUp, sidebarWidth]);

  // Right panel resize handlers
  const handleMouseMoveRight = useCallback((e: MouseEvent) => {
    if (!isResizingRight.current) return;
    const delta = e.clientX - startXRight.current;
    const newWidth = Math.min(Math.max(startWidthRight.current - delta, MIN_RIGHT_WIDTH), MAX_RIGHT_WIDTH);
    currentWidthRefRight.current = newWidth;
    setRightWidth(newWidth);
  }, []);

  const handleMouseUpRight = useCallback(() => {
    if (!isResizingRight.current) return;
    isResizingRight.current = false;
    document.removeEventListener("mousemove", handleMouseMoveRight);
    document.removeEventListener("mouseup", handleMouseUpRight);
    localStorage.setItem(RIGHT_WIDTH_KEY, String(currentWidthRefRight.current));
  }, [handleMouseMoveRight]);

  const handleMouseDownRight = useCallback((e: React.MouseEvent) => {
    isResizingRight.current = true;
    startXRight.current = e.clientX;
    startWidthRight.current = rightWidth;
    currentWidthRefRight.current = rightWidth;
    document.addEventListener("mousemove", handleMouseMoveRight);
    document.addEventListener("mouseup", handleMouseUpRight);
  }, [handleMouseMoveRight, handleMouseUpRight, rightWidth]);

  return (
    <div className="flex h-full w-full flex-col">
      {/* Custom title bar */}
      <TitleBar panelExpanded={!resultsCollapsed} onTogglePanel={toggleResults} />

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
              className="group relative w-px shrink-0 cursor-col-resize select-none"
              onMouseDown={handleMouseDown}
              role="separator"
              aria-label="Resize sidebar"
            >
              {/* Visible divider line */}
              <div className="absolute inset-y-0 left-0 w-px bg-zinc-200 dark:bg-zinc-800" />
              {/* Hover/active area for resize */}
              <div className="absolute inset-y-0 -left-2 w-[7px] group-hover:bg-blue-400/30 group-active:bg-blue-400/60 transition-colors" />
            </div>

            {/* Chat panel — elastic */}
            <ChatPanel />

            {/* Results panel / Debug panel — unified tabs, collapsible, resizable */}
            {!resultsCollapsed && (
              <ResultsPanel width={rightWidth} onCollapse={toggleResults} isDebugMode={isDebugMode} onResizeStart={handleMouseDownRight} />
            )}
          </div>
        )}

        {currentView === "settings" && <SettingsPage initialTab={settingsInitialTab} />}

        {currentView === "harness" && <HarnessPage />}
      </div>

    </div>
  );
}
