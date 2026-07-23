import { useState, useCallback, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
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
import { MqttDebugControls } from "../debug/MqttDebugControls";
import { useChatStore } from "../../stores/chatStore";
import { useLayoutStore, type PanelTab } from "../../stores/layoutStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useTranslation } from "../../i18n/useTranslation";
import { Bot, MessagesSquare, Cpu } from "lucide-react";
import { log } from "../../lib/logger";

/** Settings tab type — keep in sync with SettingsPage */
type SettingsTab = "gateway" | "appearance" | "general" | "profile";
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
  const activeTab = useLayoutStore((s) => s.activePanelTab);
  const setActiveTab = useLayoutStore((s) => s.setActivePanelTab);
  const resultsCollapsed = useLayoutStore((s) => s.resultsCollapsed);
  const setResultsCollapsed = useLayoutStore((s) => s.setResultsCollapsed);
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

  // [DEBUG] Mount diagnostic — print state immediately after webview reload
  const mountLoggedRef = useRef(false);
  useEffect(() => {
    if (mountLoggedRef.current) return;
    mountLoggedRef.current = true;
    const a = useAgentStore.getState();
    const c = useChatStore.getState();
    const sid = a.selectedAgentId ?? "";
    log.debug("[AppLayout] MOUNT", {
      recoveryReloadFlag: sessionStorage.getItem("acowork_recovery_reload"),
      selectedAgentId: a.selectedAgentId,
      activeSessionId: c.agentStates[sid]?.activeSessionId ?? null,
      openSessionIds: c.agentStates[sid]?.openSessionIds ?? [],
      knownAgents: Object.keys(a.agents),
      sessionsForSelected: a.selectedAgentId ? (a.agents[a.selectedAgentId]?.sessions ?? []).map((s) => s.session_id) : null,
    });
  }, []);

  // Ensure agents are fetched on mount (AgentList already does this via its
  // own useEffect, but AppLayout may mount before AgentList, so we also
  // trigger it here as a guard.  fetchAgents internally does auto-select +
  // loadLatestSession, so no extra recovery logic is needed.)
  const fetchedRef = useRef(false);
  useEffect(() => {
    if (fetchedRef.current) return;
    fetchedRef.current = true;
    const store = useAgentStore.getState();
    if (Object.keys(store.agents).length === 0) {
      void store.fetchAgents().then(() => {
        log.debug("[AppLayout] initial fetchAgents complete", {
          selectedAgentId: store.selectedAgentId,
          knownAgents: Object.keys(store.agents),
        });
      });
    }
  }, []);

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
  // ADR-036: MQTT connection liveness is pushed from Rust via the
  // `mqtt-status` Tauri event.  The Rust eventloop owns reconnection —
  // we just reflect state into the UI.
  const mqttConnected = useChatStore((s) => s.mqttConnected);
  const lastMqttError = useChatStore((s) => s.lastMqttError);
  const { t } = useTranslation();

  // ── Glass tint color ──────────────────────────────────────────────
  // Read both `theme` and `osTheme` from the store. The store keeps
  // `osTheme` in sync with macOS appearance via a matchMedia listener
  // (see settingsStore.ts), so re-renders here happen automatically when
  // the user switches dark/light while the app is running.
  //
  // The blur itself is rendered by the OS-native effect layer
  // (NSVisualEffectView on macOS, DWM Acrylic on Windows — configured via
  // `set_effects` in src-tauri/src/lib.rs).  This layer only paints the
  // tint; when opacity=0, glassBg becomes rgba(...,0) and the native
  // vibrancy shows through.  Do NOT add `backdrop-filter: blur(...)` here:
  // it operates within the CSS stacking context and would blur whatever
  // sits behind this div (i.e. the html placeholder color), not the
  // native layer below the WKWebView.
  //
  // NOTE: macOS NSVisualEffectView with UnderWindowBackground material
  // is always light (Apple's design), which washes out dark mode when
  // the CSS tint-layer opacity is low.  We therefore swap the native
  // effect material to match the theme — see the `set_window_effect`
  // effect below.  Light mode keeps UnderWindowBackground (light tint);
  // dark mode switches to Effect::Dark (dark tint).
  //
  // Even with Effect::Dark the native NSVisualEffectView is still a
  // translucent material that blends with desktop wallpaper.  When the
  // opacity slider is dragged to 0 the CSS tint layer disappears,
  // exposing whatever sits behind the window.  On a light desktop
  // wallpaper this makes the window appear whitish in dark mode.
  //
  // The set_window_effect command applies a native color tint as the
  // first line of defense.  As a second layer we also enforce minimum
  // CSS opacity floors for both light and dark modes on macOS so there
  // is always enough tint to keep the visual appearance consistent
  // regardless of desktop content.
  //
  // Windows DWM Acrylic does NOT have this issue — it
  // faithfully blurs the desktop content in both light and dark modes.
  // Applying the floor on Windows would make the glass effect
  // unnecessarily opaque, so we skip it on non-macOS platforms.
  const isMacOS =
    typeof navigator !== "undefined" && /Mac/i.test(navigator.userAgent);
  const LIGHT_OPACITY_FLOOR = 0.18;
  const DARK_OPACITY_FLOOR = 0.10;
  const { opacity, theme, osTheme } = useSettingsStore();
  const isDark = theme === "dark" || (theme === "system" && osTheme === "dark");
  const lightOpacity = isMacOS
    ? Math.max(opacity, LIGHT_OPACITY_FLOOR)
    : opacity;
  const darkOpacity = isMacOS
    ? Math.max(opacity, DARK_OPACITY_FLOOR)
    : opacity;
  const glassBg = isDark
    ? `rgba(41,42,44,${darkOpacity})`
    : `rgba(226,227,233,${lightOpacity})`;

  // ── Sync macOS native visual-effect material with theme ────────────
  // The Rust setup() no longer applies an initial effect (to avoid
  // a race where its delayed retry loop clobbers this theme-aware
  // call).  This effect is the sole owner of the native material.
  // We retry up to 3 times with exponential backoff in case the
  // NSView hierarchy isn't ready yet when React first mounts.
  // Non-macOS: no-op.
  //
  // lastAppliedIsDarkRef guards against double-invocation: React StrictMode
  // (dev only) and React 18 concurrent rendering can both run this effect
  // twice for the same `isDark`.  The native `set_window_effect` is idempotent
  // so this is not a correctness issue, but a duplicate call shows up in the
  // console as `[vibrancy] set_window_effect(...) succeeded` twice and
  // obscures the real boot-time signal.  We only skip when the previous run
  // for this exact `isDark` value actually succeeded; failures do NOT update
  // the ref so the retry path still works on the next attempt.
  const lastAppliedIsDarkRef = useRef<boolean | null>(null);
  useEffect(() => {
    if (typeof navigator === "undefined" || !/Mac/i.test(navigator.userAgent)) return;
    if (lastAppliedIsDarkRef.current === isDark) return;

    let cancelled = false;
    const tryApply = async (attempt: number) => {
      try {
        await invoke("set_window_effect", { isDark });
        if (cancelled) return;
        log.debug(`[vibrancy] set_window_effect(isDark=${isDark}) succeeded (attempt ${attempt})`);
        lastAppliedIsDarkRef.current = isDark;
      } catch (e: unknown) {
        log.warn(`[vibrancy] set_window_effect attempt ${attempt} failed:`, e);
        if (!cancelled && attempt < 3) {
          const delay = 200 * Math.pow(2, attempt - 1); // 200, 400, 800 ms
          setTimeout(() => tryApply(attempt + 1), delay);
        }
      }
    };
    tryApply(1);

    return () => { cancelled = true; };
  }, [isDark]);

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

  // ── Reveal workspace panel on locate-in-tree requests ────────────
  // The FileEditorPanel's "locate" button publishes a request via
  // workspaceStore.requestLocate; here we ensure the right-side results
  // panel is expanded and the workspace tab is active so the user can
  // actually see the revealed file.
  const locateRequest = useWorkspaceStore((s) => s.locateRequest);
  const consumedLocateSeqRef = useRef<number>(-1);
  useEffect(() => {
    if (!locateRequest) return;
    if (locateRequest.seq <= consumedLocateSeqRef.current) return;
    consumedLocateSeqRef.current = locateRequest.seq;
    setResultsCollapsed(false);
    setActiveTab("workspace");
  }, [locateRequest, setResultsCollapsed, setActiveTab]);

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

  // ADR-036: surface MQTT connection liveness in the status bar.
  //
  // The Rust eventloop is the source-of-truth (it observes CONNACK /
  // DISCONNECT on `rumqttc`'s eventloop and emits `mqtt-status` events
  // to the frontend).  We do NOT trigger any reconnect from here —
  // reconnect is owned by `rumqttc`'s built-in retry; we only reflect
  // the state so the user sees when input has gone disabled.
  //
  // Status priority: a Gateway-level warning/error takes precedence over
  // a transient MQTT disconnect, so we only update if Gateway is healthy.
  //
  // **Distinguishing "never connected" from "lost connection"**: the
  // store starts with `lastMqttError === null`, so we use it as the
  // "have we ever observed a disconnect?" flag.  Without this guard the
  // banner flashes for ~1s on every cold start (between Rust spawn and
  // the first mqtt-status event reaching the frontend).
  useEffect(() => {
    if (gatewayStatus !== "connected") return;
    if (!mqttConnected && lastMqttError !== null) {
      const reason = lastMqttError ? `: ${lastMqttError}` : "";
      setStatus(t("statusBar.mqttDisconnected", { reason }), "warning");
    } else if (mqttConnected) {
      // MQTT just reconnected — clear any leftover MQTT status message
      // (but only if the status bar is currently showing an MQTT message;
      // a generic warning should not be cleared).
      const currentMsg = useStatusBarStore.getState().message;
      if (currentMsg && currentMsg.startsWith(t("statusBar.mqttDisconnected", { reason: "" }).slice(0, 12))) {
        clearStatus();
      }
    }
  }, [mqttConnected, lastMqttError, gatewayStatus, setStatus, clearStatus, t]);

  // Detect wake from sleep via visibility change and reconnect
  useEffect(() => {
    const handleVisibility = () => {
      if (document.visibilityState !== "visible") return;
      const a = useAgentStore.getState();
      const c = useChatStore.getState();
      const sid = a.selectedAgentId ?? "";
      log.debug("[AppLayout] Page visible after sleep/lock", {
        selectedAgentId: a.selectedAgentId,
        activeSessionId: c.agentStates[sid]?.activeSessionId ?? null,
        openSessionIds: c.agentStates[sid]?.openSessionIds ?? [],
        knownAgents: Object.keys(a.agents),
        sessionsForSelected: a.selectedAgentId ? (a.agents[a.selectedAgentId]?.sessions ?? []).map((s) => s.session_id) : null,
      });
      checkHealth();
      // ADR-036: reconnection is owned by the Rust `rumqttc` eventloop
      // (it polls and retries internally).  On visibility-change we only
      // poke the Gateway health check; if MQTT is down the next
      // `mqtt-status` event from Rust will flip `mqttConnected` and the
      // status bar effect above will display the warning.  We do NOT
      // touch the MQTT client here.
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
    <div className="flex h-full w-full flex-col" style={{ backgroundColor: glassBg } as React.CSSProperties}>
      {/* Custom title bar — on macOS, sits under the native traffic lights
          (titleBarStyle:"Overlay"). On Windows/Linux, decorations are
          disabled in Rust setup() so this is the only title bar. */}
      <TitleBar />

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
              aria-label={t("appLayout.ariaLabelResizeSidebar")}
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
                  aria-label={t("appLayout.ariaLabelResizeFileEditor")}
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

        {/* Right rail — 40px column. Renders the agent-config nav buttons in
            the chat view; in other views an empty placeholder of the same
            width and top/bottom padding is kept so the window chrome stays
            symmetric and switching tabs only changes the central content.
            Glass background bleeds through both branches (no explicit bg). */}
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
          <div className="flex flex-1 overflow-hidden rounded-xl bg-[#FAFAFA] dark:bg-zinc-900">
            <SettingsPage initialTab={settingsInitialTab} />
          </div>
        )}

        {currentView === "harness" && (
          <div className="flex flex-1 overflow-hidden rounded-xl bg-[#FAFAFA] dark:bg-zinc-900">
            <HarnessPage />
          </div>
        )}

        {(currentView === "projects" || currentView === "docs") && (
          <div className="flex flex-1 items-center justify-center overflow-hidden rounded-xl bg-[#FAFAFA] dark:bg-zinc-900">
            <div className="rounded-md border border-zinc-200 bg-white p-8 dark:border-zinc-700 dark:bg-zinc-800">
              <p className="text-sm text-zinc-400 dark:text-zinc-500">TODO</p>
            </div>
          </div>
        )}

        {/* Right rail placeholder for non-chat views — keeps the 40px right
            column reserved so window chrome stays symmetric when switching
            between nav targets. Positioned last so it always sits at the
            right edge, regardless of which central panel is active. */}
        {currentView !== "chat" && (
          <aside className="w-10 shrink-0 py-2 dark:border-zinc-800" aria-hidden="true" />
        )}
      </div>

      {/* Bottom status bar */}
      {/* Per-key:value pill style: opaque backdrop so the text stays readable when window opacity < 1 */}
      <div className="flex h-6 shrink-0 items-center gap-2 pl-14 pr-3 text-[11px] select-none dark:text-zinc-300">
        {statusVisible && (
          <span className={cn(
            "rounded-md px-2 py-px truncate",
            "bg-zinc-100/80 dark:bg-zinc-800/75",
            "border border-zinc-200/50 dark:border-zinc-700/60",
            statusType === "error" && "text-red-600 dark:text-red-400 bg-red-100 dark:bg-red-950/70 border-red-300/70 dark:border-red-800/70",
            statusType === "warning" && "text-amber-700 dark:text-amber-300 bg-amber-100 dark:bg-amber-950/70 border-amber-300/70 dark:border-amber-800/70",
            statusType === "info" && "text-zinc-700 dark:text-zinc-300",
          )}>
            {statusMsg}
          </span>
        )}
        {(resultsCollapsed || activeTab !== "status") && selectedAgent?.running && agentDisplayName && (
          <span className="flex items-center gap-2 truncate">
            <span className="flex items-center gap-1 pl-1 pr-4 py-px rounded-md bg-zinc-100/80 dark:bg-zinc-800/75 border border-zinc-200/50 dark:border-zinc-700/60">
              <Bot className="h-3 w-3 text-zinc-600 dark:text-zinc-400" aria-hidden="true" />
              <span className="text-zinc-600 dark:text-zinc-400">{t("statusBar.agent")}: </span>
              <span className="font-medium text-zinc-600 dark:text-zinc-400">{agentDisplayName}</span>
            </span>
            <span className="flex items-center gap-1 px-2 py-px rounded-md bg-zinc-100/80 dark:bg-zinc-800/75 border border-zinc-200/50 dark:border-zinc-700/60">
              <MessagesSquare className="h-3 w-3 text-zinc-600 dark:text-zinc-400" aria-hidden="true" />
              <span className="text-zinc-600 dark:text-zinc-400">{t("statusBar.sessions")}: </span>
              <span className="tabular-nums font-medium text-zinc-600 dark:text-zinc-400">{openSessionCount}</span>
            </span>
            {contextUsage && (
              <span className="flex items-center gap-1 px-2 py-px rounded-md bg-zinc-100/80 dark:bg-zinc-800/75 border border-zinc-200/50 dark:border-zinc-700/60">
                <Cpu className="h-3 w-3 text-zinc-600 dark:text-zinc-400" aria-hidden="true" />
                <span className="text-zinc-600 dark:text-zinc-400">{t("statusBar.context")}: </span>
                <span
                  className="tabular-nums font-medium text-zinc-600 dark:text-zinc-400"
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
                <span className="tabular-nums font-medium text-zinc-600 dark:text-zinc-400">
                  {formatTokenCount(contextUsage.total_tokens)}/{formatTokenCount(contextUsage.context_window)}
                </span>
              </span>
            )}
          </span>
        )}
        <MqttDebugControls />
      </div>

    </div>
  );
}

function formatTokenCount(n: number | null | undefined): string {
  if (typeof n !== "number" || !Number.isFinite(n)) return "—";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}
