import { useState, useEffect } from "react";
import { emit } from "@tauri-apps/api/event";
import { getCurrentWindow, Effect } from "@tauri-apps/api/window";
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

  const [onboardingDone, setOnboardingDone] = useState(() => {
    return localStorage.getItem("acowork_onboarding") === "completed";
  });

  const [gatewayReady, setGatewayReady] = useState(false);
  const [splashShown, setSplashShown] = useState(false);

  // Signal Rust to show the native window after the first React render.
  // The window starts hidden (visible: false in tauri.conf.json) to prevent
  // the white/transparent flash before the splash screen is ready.
  // Rust listens for "splash-ready" and calls window.show() from the native side.
  //
  // Window effects (acrylic/blur/mica) are NOT set in tauri.conf.json to speed
  // up WebView2 initialization. They are applied at runtime here after the
  // splash screen is rendered, so the effect is visible but startup is faster.
  useEffect(() => {
    if (!splashShown) {
      setSplashShown(true);
      requestAnimationFrame(() => {
        emit("splash-ready").catch((err) => {
          console.warn("Failed to emit splash-ready:", err);
        });
        // Apply window effects after splash is ready
        getCurrentWindow().setEffects({
          effects: [Effect.Acrylic, Effect.Blur, Effect.Mica],
          radius: 12,
        }).catch((err) => {
          console.warn("Failed to set window effects:", err);
        });
      });
    }
  }, [splashShown]);

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
