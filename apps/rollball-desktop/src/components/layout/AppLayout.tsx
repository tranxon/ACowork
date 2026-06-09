import { useState, useCallback, useEffect, useRef } from "react";
import type { NavView } from "../../lib/types";
import { NavBar } from "./NavBar";
import { TitleBar } from "./TitleBar";
import { AgentList } from "../agent-list/AgentList";
import { ChatPanel } from "../chat/ChatPanel";
import { ResultsPanel } from "../results/ResultsPanel";
import { FileEditorPanel } from "../editor/FileEditorPanel";
import { GatewayBanner } from "./GatewayBanner";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useAgentStore } from "../../stores/agentStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
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

const MIN_RIGHT_WIDTH = 260;
const MAX_RIGHT_WIDTH = 600;
const DEFAULT_RIGHT_WIDTH = 340;
const RIGHT_WIDTH_KEY = "rollball-right-width";

const MIN_FILE_WIDTH = 200;
const MAX_FILE_WIDTH = 900;
const DEFAULT_FILE_WIDTH = 450;
const FILE_WIDTH_KEY = "rollball-file-width";

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

  // File panel resize handlers — dynamic max width to keep ChatPanel visible
  const maxFileWidthRef = useRef(MAX_FILE_WIDTH);

  const handleMouseMoveFile = useCallback((e: MouseEvent) => {
    if (!isResizingFile.current) return;
    const delta = e.clientX - startXFile.current;
    const newWidth = Math.min(Math.max(startWidthFile.current - delta, MIN_FILE_WIDTH), maxFileWidthRef.current);
    currentWidthRefFile.current = newWidth;
    setFileWidth(newWidth);
  }, []);

  const handleMouseUpFile = useCallback(() => {
    if (!isResizingFile.current) return;
    isResizingFile.current = false;
    document.removeEventListener("mousemove", handleMouseMoveFile);
    document.removeEventListener("mouseup", handleMouseUpFile);
    localStorage.setItem(FILE_WIDTH_KEY, String(currentWidthRefFile.current));
  }, [handleMouseMoveFile]);

  const handleMouseDownFile = useCallback((e: React.MouseEvent) => {
    isResizingFile.current = true;
    startXFile.current = e.clientX;
    startWidthFile.current = fileWidth;
    currentWidthRefFile.current = fileWidth;
    // Calculate dynamic max to ensure ChatPanel retains at least 200px
    const minChatWidth = 200;
    const navWidth = 48;
    const actualRightWidth = resultsCollapsed ? 0 : rightWidth;
    const dynamicMax = Math.max(window.innerWidth - sidebarWidth - actualRightWidth - navWidth - minChatWidth, MIN_FILE_WIDTH);
    maxFileWidthRef.current = Math.min(MAX_FILE_WIDTH, dynamicMax);
    document.addEventListener("mousemove", handleMouseMoveFile);
    document.addEventListener("mouseup", handleMouseUpFile);
  }, [handleMouseMoveFile, handleMouseUpFile, fileWidth, sidebarWidth, rightWidth, resultsCollapsed]);

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
              <div className="absolute inset-y-0 -left-2 w-[7px] group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60 transition-colors" />
            </div>

            {/* Chat panel — elastic */}
            <ChatPanel />

            {/* File editor panel — shown when files are open */}
            {hasOpenFiles && (
              <>
                {/* Resize handle between chat and file editor */}
                <div
                  className="group relative w-px shrink-0 cursor-col-resize select-none"
                  onMouseDown={handleMouseDownFile}
                  role="separator"
                  aria-label="Resize file editor"
                >
                  <div className="absolute inset-y-0 left-0 w-px bg-zinc-200 dark:bg-zinc-800" />
                  <div className="absolute inset-y-0 -left-2 w-[7px] group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60 transition-colors" />
                </div>
                <FileEditorPanel width={fileWidth} />
              </>
            )}

            {/* Results panel / Debug panel — unified tabs, collapsible, resizable */}
            {!resultsCollapsed && (
              <ResultsPanel width={rightWidth} onCollapse={toggleResults} isDebugMode={isDebugMode} onResizeStart={handleMouseDownRight} />
            )}
          </div>
        )}

        {currentView === "settings" && <SettingsPage initialTab={settingsInitialTab} />}

        {currentView === "harness" && <HarnessPage />}

        {(currentView === "projects" || currentView === "docs") && (
          <div className="flex flex-1 items-center justify-center bg-zinc-100 dark:bg-zinc-900">
            <div className="rounded-lg border border-zinc-200 bg-white p-8 dark:border-zinc-700 dark:bg-zinc-800">
              <p className="text-sm text-zinc-400 dark:text-zinc-500">TODO</p>
            </div>
          </div>
        )}
      </div>

    </div>
  );
}
