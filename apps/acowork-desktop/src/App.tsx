import { useState, useEffect } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import { AppLayout } from "./components/layout/AppLayout";
import { SplashScreen } from "./components/layout/SplashScreen";
import { OnboardingFlow } from "./components/onboarding/OnboardingFlow";
import { ToastProvider } from "./components/common/ToastProvider";
import { ErrorBoundary } from "./components/common/ErrorBoundary";
import { initMqttListener } from "./stores/chatStore";
import { initWorkspaceFsListener } from "./lib/workspaceFsEvents";
import { log } from "./lib/logger";

function App() {
  // On sleep-recovery reload, skip splash screen — gateway is already running
  // (Rust backend survives reload) and Zustand persisted stores restore from
  // localStorage, so we can jump straight to AppLayout.
  const isRecoveryReload = sessionStorage.getItem("acowork_recovery_reload") === "1";
  log.debug("[App] boot branch selection", { isRecoveryReload });

  const [onboardingDone, setOnboardingDone] = useState(() => {
    return localStorage.getItem("acowork_onboarding") === "completed";
  });

  const [gatewayReady, setGatewayReady] = useState(isRecoveryReload);

  // Clear the recovery flag after mount so it doesn't affect future loads.
  // Also re-register the MQTT listener: recovery reload skips SplashScreen
  // (gateway is already running), but the webview reload destroyed all
  // Tauri event listeners. Without re-registering, chatStore.mqttConnected
  // stays false forever and the UI permanently shows "Connecting to agent".
  useEffect(() => {
    if (isRecoveryReload) {
      sessionStorage.removeItem("acowork_recovery_reload");
      initMqttListener().catch((e) =>
        log.warn("[App] initMqttListener failed on recovery reload:", e)
      );
      // ADR-058: workspace fs-changed listeners die with the webview
      // reload — re-register alongside the MQTT listener.
      initWorkspaceFsListener().catch((e) =>
        log.warn("[App] initWorkspaceFsListener failed on recovery reload:", e)
      );
      // Post-wake renderer recovery: report the first painted frame to
      // the Rust backend. requestAnimationFrame is driven by the GPU
      // compositor — it only fires once a frame was actually composited,
      // so this is the page's own "the UI is truly visible again" signal
      // that `recover_from_wake` verifies against (heartbeat-based
      // verification was a false positive: a thawed old page's catch-up
      // heartbeat landed right after the async reload call). If the
      // compositor is still coming back, the rAF callback is deferred
      // until it recovers, and the backend's verify window catches it.
      requestAnimationFrame(() => {
        invoke("desktop_recovery_visible").catch((e) =>
          log.warn("[App] desktop_recovery_visible invoke failed:", e)
        );
      });
    }
  }, [isRecoveryReload]);

  // Show the window after first render. The window starts hidden (visible:false
  // in tauri.conf.json) so the user never sees the empty/transparent window or
  // the decoration flicker that occurs before React mounts. By the time this
  // effect fires, SplashScreen / OnboardingFlow / AppLayout is already painted.
  useEffect(() => {
    const showWindow = async () => {
      try {
        const win = getCurrentWindow();
        await win.show();
        await win.setFocus();
      } catch (e) {
        log.error("Failed to show window:", e);
      }
    };
    showWindow();
  }, []);

  if (!gatewayReady && onboardingDone) {
    return (
      <div className="h-screen w-screen overflow-hidden">
        <SplashScreen onReady={() => setGatewayReady(true)} />
      </div>
    );
  }

  return (
    <ErrorBoundary>
      <ToastProvider>
        {!onboardingDone ? (
          <OnboardingFlow onComplete={() => setOnboardingDone(true)} />
        ) : (
          <AppLayout />
        )}
      </ToastProvider>
    </ErrorBoundary>
  );
}

export default App;
