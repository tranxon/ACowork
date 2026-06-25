import { useState, useEffect } from "react";
import { AppLayout } from "./components/layout/AppLayout";
import { SplashScreen } from "./components/layout/SplashScreen";
import { OnboardingFlow } from "./components/onboarding/OnboardingFlow";
import { ToastProvider } from "./components/common/ToastProvider";
import { ErrorBoundary } from "./components/common/ErrorBoundary";
import { useSystemResume } from "./hooks/useSystemResume";

function App() {
  // Detect system resume from sleep and reload webview to recover GPU compositor.
  // Must be mounted before any other UI so it survives the full app lifecycle.
  useSystemResume();

  // On sleep-recovery reload, skip splash screen — gateway is already running
  // (Rust backend survives reload) and Zustand persisted stores restore from
  // localStorage, so we can jump straight to AppLayout.
  const isRecoveryReload = sessionStorage.getItem("acowork_recovery_reload") === "1";

  const [onboardingDone, setOnboardingDone] = useState(() => {
    return localStorage.getItem("acowork_onboarding") === "completed";
  });

  const [gatewayReady, setGatewayReady] = useState(isRecoveryReload);

  // Clear the recovery flag after mount so it doesn't affect future loads.
  useEffect(() => {
    if (isRecoveryReload) {
      sessionStorage.removeItem("acowork_recovery_reload");
    }
  }, [isRecoveryReload]);

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
