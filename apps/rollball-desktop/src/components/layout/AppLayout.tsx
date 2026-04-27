import { useState, useCallback, useEffect } from "react";
import type { NavView } from "../../lib/types";
import { NavBar } from "./NavBar";
import { AgentList } from "../agent-list/AgentList";
import { ChatPanel } from "../chat/ChatPanel";
import { ResultsPanel } from "../results/ResultsPanel";
import { GatewayBanner } from "./GatewayBanner";
import { useGatewayStore } from "../../stores/gatewayStore";
import { ModelsPage } from "../models/ModelsPage";
import { SettingsPage } from "../settings/SettingsPage";

export function AppLayout() {
  const [currentView, setCurrentView] = useState<NavView>("chat");
  const [resultsCollapsed, setResultsCollapsed] = useState(false);
  const gatewayStatus = useGatewayStore((s) => s.status);
  const checkHealth = useGatewayStore((s) => s.checkHealth);

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

  return (
    <div className="flex h-full w-full flex-col">
      {/* Gateway disconnected banner */}
      {gatewayStatus !== "connected" && <GatewayBanner />}

      {/* Main content area */}
      <div className="flex flex-1 overflow-hidden">
        {/* Navigation bar — 48px */}
        <NavBar currentView={currentView} onViewChange={setCurrentView} />

        {/* Content area based on current view */}
        {currentView === "chat" && (
          <div className="flex flex-1 overflow-hidden">
            {/* Agent list — 240px */}
            <AgentList />

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
                ◀
              </button>
            )}
          </div>
        )}

        {currentView === "models" && <ModelsPage />}
        {currentView === "settings" && <SettingsPage />}

        {currentView === "skills" && (
          <div className="flex flex-1 items-center justify-center text-zinc-400 dark:text-zinc-500">
            <div className="text-center">
              <p className="text-lg">Skills</p>
              <p className="text-sm mt-1">Available in Developer Mode</p>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
