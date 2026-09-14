// P1-C / P2-B: Zustand store for the full-stack service diagnostic panel.
//
// Owns the latest `DiagnoseReport` plus a transient loading flag.
// `diagnose()` runs the full probe pass (P2: one Gateway snapshot fetch
// when available — see `servicesApi.ts`) and updates the store on
// completion. `probe()` is a per-row retry entry point used by the
// "重试" button on `ServicesPanel`.
//
// Why a separate store (vs reusing `gatewayStore`):
// - The diagnostic report is read by a single consumer
//   (`ServicesPanel` inside `SettingsPage.GatewayTab`); keeping it
//   isolated avoids polluting `gatewayStore` selectors that the
//   top-level layout subscribes to on every keystroke.
// - `gatewayStore` mixes *process* state (spawn / stop the local
//   Gateway child) with *network* state. Mixing the diagnostic
//   snapshot in there would create a third concern without a clean
//   ownership boundary.

import { create } from "zustand";

import { getGatewayUrl } from "../lib/config";
import { probeAllServices, probeService } from "../lib/servicesApi";
import type {
  DiagnoseReport,
  ServiceHealth,
  ServiceType,
} from "../lib/types";

interface ServicesState {
  /** Latest full diagnostic snapshot. `null` until the first
   *  `diagnose()` resolves. Persisted across Settings tab switches. */
  report: DiagnoseReport | null;
  /** `true` while a `diagnose()` pass is in flight (full-fleet probe). */
  loading: boolean;
  /** When the last `diagnose()` started (ms epoch; for "X seconds ago"
   *  UI in the panel). */
  lastProbeAt: number | null;
  /** Service types currently being re-probed by `probe()`. Used by the
   *  per-row spinner on `ServicesPanel`. */
  probing: Partial<Record<ServiceType, boolean>>;
  /** Last error message from a `diagnose()` call; `null` when healthy. */
  lastError: string | null;
  /** Monotonic generation for in-flight passes. `reset()` bumps it so a
   *  pass that started under the previous Gateway URL can never write
   *  its stale result into the fresh store (L-2 of the review). */
  epoch: number;

  /** Run the full 7-service probe pass in parallel. Always resolves
   *  (probe functions never throw) — but on a hard Gateway failure
   *  every row will be `online: false`. */
  diagnose: () => Promise<void>;
  /** Re-probe a single service and merge the result into the existing
   *  report. Called by the per-row "重试" button. */
  probe: (service_type: ServiceType) => Promise<void>;
  /** Reset the store to its initial state (used when the Gateway URL
   *  changes — the old report is stale). */
  reset: () => void;
}

const INITIAL: Pick<
  ServicesState,
  "report" | "loading" | "lastProbeAt" | "probing" | "lastError"
> = {
  report: null,
  loading: false,
  lastProbeAt: null,
  probing: {},
  lastError: null,
};

export const useServicesStore = create<ServicesState>((set, get) => ({
  ...INITIAL,
  epoch: 0,

  diagnose: async () => {
    // Guard against double-clicks: while a pass is already in flight,
    // a second `diagnose()` call is a no-op. Avoids racing two
    // `Promise.all` batches that would each leave a partial write.
    if (get().loading) return;
    const epoch = get().epoch;
    const startedAt = Date.now();
    set({ loading: true, lastError: null });
    const gatewayUrl = getGatewayUrl();
    try {
      const { services, source } = await probeAllServices(
        gatewayUrl,
        startedAt,
      );
      // A `reset()` (Gateway URL change) bumps the epoch mid-pass —
      // the stale result must not land in the fresh store.
      if (get().epoch !== epoch) return;
      const finishedAt = Date.now();
      set({
        report: {
          services,
          source,
          started_at: startedAt,
          finished_at: finishedAt,
          gateway_reachable: services.gateway.online,
        },
        loading: false,
        lastProbeAt: finishedAt,
      });
    } catch (e) {
      // probeAllServices promises never reject — this catch is a
      // belt-and-braces guard for future refactors.
      if (get().epoch !== epoch) return;
      set({
        loading: false,
        lastError: e instanceof Error ? e.message : String(e),
        lastProbeAt: Date.now(),
      });
    }
  },

  probe: async (service_type) => {
    if (get().probing[service_type]) return;
    const epoch = get().epoch;
    set((s) => ({ probing: { ...s.probing, [service_type]: true } }));
    try {
      const gatewayUrl = getGatewayUrl();
      const row: ServiceHealth = await probeService(
        service_type,
        gatewayUrl,
        Date.now(),
      );
      if (get().epoch !== epoch) return;
      set((s) => {
        const prev = s.report;
        if (!prev) {
          // No full report yet — keep `report` null, just clear the
          // probing flag. The user will press the global "运行诊断"
          // button for a full snapshot.
          return { probing: { ...s.probing, [service_type]: false } };
        }
        return {
          report: {
            ...prev,
            services: { ...prev.services, [service_type]: row },
          },
          probing: { ...s.probing, [service_type]: false },
        };
      });
    } catch (e) {
      if (get().epoch !== epoch) return;
      set((s) => ({
        probing: { ...s.probing, [service_type]: false },
        lastError: e instanceof Error ? e.message : String(e),
      }));
    }
  },

  reset: () => set((s) => ({ ...INITIAL, epoch: s.epoch + 1 })),
}));
