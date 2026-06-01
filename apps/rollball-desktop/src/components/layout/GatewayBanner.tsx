import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";

export function GatewayBanner() {
  const checkHealth = useGatewayStore((s) => s.checkHealth);
  const localState = useGatewayStore((s) => s.localState);
  const startLocalGateway = useGatewayStore((s) => s.startLocalGateway);
  const gatewayMode = useSettingsStore((s) => s.gatewayMode);

  const isLocal = gatewayMode === "local";
  const isStarting = localState === "starting";

  return (
    <div className="flex items-center gap-3 border-b border-amber-200 bg-amber-50 px-4 py-2 text-sm text-amber-800 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
      {isLocal ? (
        <span>
          {isStarting ? "Starting local Gateway..." : "Local Gateway is not running."}
        </span>
      ) : (
        <span>Gateway not connected. Please check your connection settings.</span>
      )}
      {isLocal && !isStarting && (
        <button
          onClick={startLocalGateway}
          className="rounded px-2 py-0.5 text-xs font-medium hover:bg-amber-100 dark:hover:bg-amber-900"
        >
          Start Gateway
        </button>
      )}
      {isLocal && (
        <button
          onClick={checkHealth}
          className="rounded px-2 py-0.5 text-xs font-medium hover:bg-amber-100 dark:hover:bg-amber-900"
        >
          Retry
        </button>
      )}
      {!isLocal && (
        <button
          onClick={checkHealth}
          className="rounded px-2 py-0.5 text-xs font-medium hover:bg-amber-100 dark:hover:bg-amber-900"
        >
          Retry
        </button>
      )}
    </div>
  );
}
