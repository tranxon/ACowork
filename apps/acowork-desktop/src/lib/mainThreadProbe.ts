// Temporary diagnostic probes for the "agent start → session content takes
// tens of seconds" report (2026-09-14). Logs to console only (devtools),
// never to disk. ponytail: diagnostic-only — delete after root cause found.

/**
 * 1s heartbeat; warns when the main thread stalls (gap between two ticks
 * exceeds 2s). Normal operation prints nothing. A stall surfaces as one
 * "blocked ~Nms" line — N is the exact stall duration.
 */
export function installMainThreadProbe(tag = "mt-probe"): void {
  let last = performance.now();
  setInterval(() => {
    const now = performance.now();
    const gap = now - last;
    last = now;
    if (gap > 2000) {
      console.warn(
        `[${tag}] main-thread blocked ~${Math.round(gap)}ms ` +
          `(tick ${new Date().toISOString()})`,
      );
    }
  }, 1000);
}

/** Report long tasks (>100ms) — pinpoints which boot phase stalls. */
export function installLongTaskObserver(): void {
  if (typeof PerformanceObserver === "undefined") return;
  try {
    const po = new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        if (entry.duration > 100) {
          console.warn(
            `[longtask] ${Math.round(entry.duration)}ms ` +
              `start=${Math.round(entry.startTime)}ms`,
          );
        }
      }
    });
    po.observe({ entryTypes: ["longtask"] });
  } catch {
    // PerformanceObserver unsupported — probe is best-effort.
  }
}
