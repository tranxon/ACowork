import { useState, useCallback, useEffect, useRef } from "react";
import type { NavView } from "../../lib/types";
import { NavBar } from "./NavBar";
import { TitleBar } from "./TitleBar";
import { AgentList } from "../agent-list/AgentList";
import { ChatPanel } from "../chat/ChatPanel";
import { ResultsPanel } from "../results/ResultsPanel";
import { GatewayBanner } from "./GatewayBanner";
import { useGatewayStore } from "../../stores/gatewayStore";
import { SettingsPage } from "../settings/SettingsPage";
import { ToolApprovalModal } from "../tools/ToolApprovalModal";
import { PanelRightClose } from "lucide-react";

const MIN_SIDEBAR_WIDTH = 160;
const MAX_SIDEBAR_WIDTH = 400;
const DEFAULT_SIDEBAR_WIDTH = 240;
const SIDEBAR_WIDTH_KEY = "rollball-sidebar-width";

export function AppLayout() {
  const [currentView, setCurrentView] = useState<NavView>("chat");
  const [resultsCollapsed, setResultsCollapsed] = useState(false);
  const [sidebarWidth, setSidebarWidth] = useState(() => {
    const stored = localStorage.getItem(SIDEBAR_WIDTH_KEY);
    return stored ? Math.min(Math.max(parseInt(stored, 10), MIN_SIDEBAR_WIDTH), MAX_SIDEBAR_WIDTH) : DEFAULT_SIDEBAR_WIDTH;
  });
  const gatewayStatus = useGatewayStore((s) => s.status);
  const checkHealth = useGatewayStore((s) => s.checkHealth);
  const isResizing = useRef(false);
  const startX = useRef(0);
  const startWidth = useRef(DEFAULT_SIDEBAR_WIDTH);
  const currentWidthRef = useRef(DEFAULT_SIDEBAR_WIDTH);

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

  const toggleResults = useCallback(() => {
    setResultsCollapsed((prev) => !prev);
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

  return (
    <div className="flex h-full w-full flex-col bg-[#BEBFC5] dark:bg-[#292A2C]">
      {/* Custom title bar */}
      <TitleBar />

      {/* Gateway disconnected banner */}
      {gatewayStatus !== "connected" && <GatewayBanner />}

      {/* Main content area */}
      <div className="flex flex-1 overflow-hidden">
        {/* Navigation bar — 48px */}
        <NavBar currentView={currentView} onViewChange={setCurrentView} />

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

            {/* Results panel — 320px, collapsible */}
            {!resultsCollapsed && <ResultsPanel onCollapse={toggleResults} />}
            {resultsCollapsed && (
              <button
                onClick={toggleResults}
                className="flex w-8 items-center justify-center border-l border-zinc-200 bg-zinc-50 text-zinc-400 hover:text-zinc-600 dark:border-zinc-800 dark:bg-zinc-900 dark:text-zinc-500 dark:hover:text-zinc-300"
                aria-label="Expand results panel"
              >
                <PanelRightClose className="h-4 w-4" />
              </button>
            )}
          </div>
        )}

        {currentView === "settings" && <SettingsPage />}
      </div>

      {/* Global tool approval modal */}
      <ToolApprovalModal />
    </div>
  );
}
