// P1-D / P2-C: full-stack service diagnostic panel.
//
// Renders the 7 `ServiceType` rows grouped by `ServiceGroup` (critical /
// important / optional). Each row shows:
//   - status dot (green / amber / red)
//   - service name + version
//   - latency_ms (or "—")
//   - detail (free-text, e.g. "2/3 nodes online")
//   - retry button (per-row, calls `probe(service_type)`)
//   - last_error (only when offline; tooltip on the dot)
// The summary line carries a source badge (P2-C) telling the user
// whether the report came from the single Gateway snapshot fetch or
// from the legacy direct probes.
//
// The component is **stateless** w.r.t. the diagnostic data — it
// subscribes to `servicesStore` directly. The "运行诊断" button
// delegates to `diagnose()` on the store. This keeps the parent
// (`SettingsPage.GatewayTab`) free of any probe-specific state.

import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { Loader, RefreshCw, Server } from "lucide-react";

import { cn } from "../../lib/utils";
import { useServicesStore } from "../../stores/servicesStore";
import type { ServiceGroup, ServiceHealth, ServiceType } from "../../lib/types";

const ORDER: ServiceType[] = [
  "gateway",
  "mqtt",
  "node",
  "embed",
  "pm",
  "doc",
  "lsp-relay",
];

/** I18n key suffixes for the service names. Mirrors `ORDER` 1:1. */
const NAME_KEY: Record<ServiceType, string> = {
  gateway: "settings.services.gateway",
  mqtt: "settings.services.mqtt",
  node: "settings.services.node",
  embed: "settings.services.embed",
  pm: "settings.services.pm",
  doc: "settings.services.doc",
  "lsp-relay": "settings.services.lsp-relay",
};

/** Per-row dot color. Green = healthy, amber = degraded (we currently
 *  treat every "online" row as healthy; P2 may add a degraded tier),
 *  red = offline. */
function dotCls(row: ServiceHealth): string {
  if (!row.online) {
    return "bg-red-500";
  }
  if (row.latency_ms > 800) {
    // Slow but reachable — amber so the user can see something is off
    return "bg-amber-500";
  }
  return "bg-emerald-500";
}

/** Per-group label key. */
function groupLabelKey(g: ServiceGroup): string {
  switch (g) {
    case "critical":
      return "settings.services.groupCritical";
    case "important":
      return "settings.services.groupImportant";
    case "optional":
      return "settings.services.groupOptional";
  }
}

export function ServicesPanel() {
  const { t } = useTranslation();
  const report = useServicesStore((s) => s.report);
  const loading = useServicesStore((s) => s.loading);
  const lastProbeAt = useServicesStore((s) => s.lastProbeAt);
  const probing = useServicesStore((s) => s.probing);
  const diagnose = useServicesStore((s) => s.diagnose);
  const probe = useServicesStore((s) => s.probe);

  // Group the 7 rows into 3 buckets. `useMemo` so the per-row
  // `<ServiceRow>` set keeps a stable identity across unrelated
  // re-renders (e.g. when the user types in the URL input above).
  const grouped = useMemo(() => {
    const out: Record<ServiceGroup, ServiceType[]> = {
      critical: [],
      important: [],
      optional: [],
    };
    if (report) {
      for (const type of ORDER) {
        const row = report.services[type];
        out[row.group].push(type);
      }
    }
    return out;
  }, [report]);

  const healthyCount = useMemo(() => {
    if (!report) return 0;
    return ORDER.filter((t2) => report.services[t2]?.online).length;
  }, [report]);

  const totalLabel = t("settings.services.healthyCount", {
    online: healthyCount,
    total: ORDER.length,
  });

  return (
    <div data-testid="services-panel" className="space-y-3">
      {/* Summary + diagnose button.

          Layout contract: every text fragment (healthy count, "probed Ns
          ago", source badge, button label) keeps its own single line via
          `whitespace-nowrap`. The two siblings are allowed to wrap to
          a second row when the panel is narrow — but each fragment is
          never broken mid-phrase. Without this, English labels like
          "7/7 services healthy" / "via Gateway snapshot" / "Run diagnose"
          were splitting at arbitrary word boundaries, producing ugly
          double-line fragments ("healthy" on its own row, "diagnose" on
          its own row, etc.). `shrink-0` on the icon/badge/button prevents
          flex from squeezing them. */}
      <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-2">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-text-tertiary ">
          <Server className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
          <span className="whitespace-nowrap">{totalLabel}</span>
          {lastProbeAt !== null && (
            <span className="whitespace-nowrap text-text-tertiary ">
              · {t("settings.services.lastProbeAt", {
                seconds: Math.max(0, Math.floor((Date.now() - lastProbeAt) / 1000)),
              })}
            </span>
          )}
          {/* P2-C: which probe path produced this report — "via Gateway
              snapshot" (single fetch, local + remote) vs "direct probes"
              (legacy per-endpoint walk on a pre-P2 Gateway). */}
          {report && (
            <span
              className="shrink-0 whitespace-nowrap rounded-full border border-zinc-300 px-1.5 py-px text-[10px] leading-4 text-text-tertiary dark:border-zinc-600 "
              data-testid="services-source"
            >
              {t(
                report.source === "gateway-api"
                  ? "settings.services.viaGateway"
                  : "settings.services.viaDirect",
              )}
            </span>
          )}
        </div>
        <button
          type="button"
          onClick={() => void diagnose()}
          disabled={loading}
          className="inline-flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-md border border-zinc-300 px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-600  dark:hover:bg-zinc-700"
          aria-label={t("settings.services.diagnose")}
        >
          {loading ? (
            <Loader className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
          ) : (
            <RefreshCw className="h-3.5 w-3.5" aria-hidden="true" />
          )}
          {loading
            ? t("settings.services.diagnosing")
            : t("settings.services.diagnose")}
        </button>
      </div>

      {/* Gateway-down banner (only when the full report says so) */}
      {report && !report.gateway_reachable && (
        <div className="rounded-md border border-red-300/60 bg-red-50 px-3 py-2 text-xs text-red-900 dark:border-red-700/60 dark:bg-red-950/40 dark:text-red-100">
          {t("settings.services.gatewayDownHint")}
        </div>
      )}

      {/* 3 grouped sections */}
      {(["critical", "important", "optional"] as ServiceGroup[]).map(
        (group) => {
          const types = grouped[group];
          if (types.length === 0) return null;
          return (
            <section key={group} className="space-y-1">
              <h4 className="text-[10px] font-medium uppercase tracking-wider text-text-tertiary ">
                {t(groupLabelKey(group))}
              </h4>
              <ul className="divide-y divide-zinc-200 overflow-hidden rounded-md border border-zinc-200 dark:divide-zinc-700 dark:border-zinc-700">
                {types.map((type) => {
                  const row = report?.services[type];
                  return (
                    <ServiceRow
                      key={type}
                      type={type}
                      row={row}
                      isProbing={!!probing[type]}
                      onRetry={() => void probe(type)}
                    />
                  );
                })}
              </ul>
            </section>
          );
        },
      )}

      {/* Empty state — first render before any diagnose() call */}
      {!report && !loading && (
        <div className="rounded-md border border-dashed border-zinc-300 px-3 py-6 text-center text-xs text-text-tertiary dark:border-zinc-700 ">
          {t("settings.services.emptyHint")}
        </div>
      )}
    </div>
  );
}

function ServiceRow({
  type,
  row,
  isProbing,
  onRetry,
}: {
  type: ServiceType;
  row: ServiceHealth | undefined;
  isProbing: boolean;
  onRetry: () => void;
}) {
  const { t } = useTranslation();
  // Render a placeholder row when no snapshot exists yet — keeps the
  // layout stable when the user first opens Settings.
  const status = row?.online ?? false;
  const version = row?.version ?? "—";
  const latency = row?.latency_ms ?? 0;
  const detail = row?.detail;
  const lastError = row?.last_error;

  return (
    <li
      className="flex items-center gap-3 px-3 py-2 text-xs"
      data-testid={`services-row-${type}`}
      data-online={status ? "true" : "false"}
    >
      {/* Status dot with tooltip on hover for last_error */}
      <span
        className={cn("h-2 w-2 flex-shrink-0 rounded-full", dotCls(row ?? emptyRow(type)))}
        aria-hidden="true"
        title={lastError ?? undefined}
      />
      {/* Name */}
      <span className="min-w-[7rem] font-medium text-text ">
        {t(NAME_KEY[type])}
      </span>
      {/* Version */}
      <span className="font-mono text-[11px] text-text-tertiary ">
        v{version}
      </span>
      {/* Latency */}
      <span className="font-mono text-[11px] tabular-nums text-text-tertiary ">
        {latency > 0 ? `${latency}ms` : "—"}
      </span>
      {/* Detail */}
      {detail && (
        <span className="flex-1 truncate text-[11px] text-text-tertiary ">
          {detail}
        </span>
      )}
      {!detail && <span className="flex-1" />}
      {/* Retry button */}
      <button
        type="button"
        onClick={onRetry}
        disabled={isProbing}
        className="ml-auto inline-flex items-center gap-1 rounded border border-zinc-300 px-2 py-0.5 text-[11px] font-medium text-text-secondary hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-600  dark:hover:bg-zinc-700"
        aria-label={t("settings.services.retry", { name: t(NAME_KEY[type]) })}
      >
        {isProbing ? (
          <Loader className="h-3 w-3 animate-spin" aria-hidden="true" />
        ) : (
          <RefreshCw className="h-3 w-3" aria-hidden="true" />
        )}
        {t("settings.services.retryShort")}
      </button>
    </li>
  );
}

/** Sentinel used only for the dot color when no row exists yet. */
function emptyRow(type: ServiceType): ServiceHealth {
  return {
    service_type: type,
    group: "optional",
    online: false,
    version: "—",
    latency_ms: 0,
    detail: undefined,
    last_error: null,
    probed_at: 0,
  };
}
