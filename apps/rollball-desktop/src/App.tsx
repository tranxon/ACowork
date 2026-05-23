import { useState } from "react";
import { AppLayout } from "./components/layout/AppLayout";
import { OnboardingFlow } from "./components/onboarding/OnboardingFlow";
import { ToastProvider } from "./components/common/ToastProvider";
import { ErrorBoundary } from "./components/common/ErrorBoundary";

function App() {
  const [onboardingDone, setOnboardingDone] = useState(() => {
    return localStorage.getItem("rollball_onboarding") === "completed";
  });

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
