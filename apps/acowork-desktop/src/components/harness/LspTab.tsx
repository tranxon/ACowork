import { useState, useEffect, useCallback } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useAgentStore } from "../../stores/agentStore";
import { fetchLspServers, fetchLspStatus, fetchLspStatusForLanguage, fetchLspInstallScript, runLspInstall, getLspRelayUrl } from "../../lib/gateway-api";
import type { LspServersConfig, LspServerEntry, LspServerStatusEntry, LspHealthStatus } from "../../lib/types";
import { CheckCircle2, XCircle, Loader2, Eye, Terminal, Code2, RefreshCw } from "lucide-react";
import { ErrorBox } from "../common/ErrorBox";

/**
 * Module-level cache of LSP install-status results, keyed by relay URL.
 *
 * Purpose: avoid re-fetching (and triggering the relay's full PROBE storm)
 * when the user switches Chat → Harness → Chat → Harness in quick
 * succession. Each Harness re-entry remounts `LspTab` (AppLayout
 * unmounts `HarnessPage` on view switch — see AppLayout.tsx), so the
 * `useState` inside the component is reset. The module-level cache
 * survives unmount, so the second mount can `loadAll` and skip both
 * the "checking" flash AND the network call for any language whose
 * status was probed within `MODULE_HEALTH_CACHE_TTL_MS`.
 *
 * The relay itself also caches results (Phase 1) — the module-level
 * cache is purely a UX optimization to skip the "checking" spinner
 * flicker on re-mount, when the relay's cache is also likely fresh.
 *
 * Mirrors `MODULE_HEALTH_CACHE_TTL_MS` to the relay's
 * `DEFAULT_STATUS_TTL_SECS` (30 min) — if the two go out of sync
 * the worst case is a "checking" flash with a fast cache-hit
 * underneath (still no fork on the relay side).
 */
type ModuleHealthCache = {
  status: Record<string, LspHealthStatus>;
  timestamp: Record<string, number>;
};
const moduleHealthCache = new Map<string, ModuleHealthCache>();
const MODULE_HEALTH_CACHE_TTL_MS = 30 * 60 * 1000;

function getModuleHealthCache(relayUrl: string): ModuleHealthCache | null {
  return moduleHealthCache.get(relayUrl) ?? null;
}

function seedModuleHealthCache(
  relayUrl: string,
  entries: LspServerStatusEntry[],
): void {
  const existing = moduleHealthCache.get(relayUrl) ?? {
    status: {},
    timestamp: {},
  };
  const now = Date.now();
  for (const entry of entries) {
    existing.status[entry.language] = entry.installed
      ? "installed"
      : "not_installed";
    existing.timestamp[entry.language] = now;
  }
  moduleHealthCache.set(relayUrl, existing);
}

/** Language display names for UI */
const LANGUAGE_LABELS: Record<string, string> = {
  rust: "Rust",
  python: "Python",
  typescript: "TypeScript / JavaScript",
  go: "Go",
  c: "C / C++",
  json: "JSON",
  yaml: "YAML",
  html: "HTML",
  css: "CSS / SCSS / Less",
  markdown: "Markdown",
  java: "Java",
};

/** Language icon colors */
const LANGUAGE_COLORS: Record<string, string> = {
  rust: "#DEA584",
  python: "#3572A5",
  typescript: "#3178C6",
  go: "#00ADD8",
  c: "#555555",
  json: "#292929",
  yaml: "#CB171E",
  html: "#E34F26",
  css: "#563D7C",
  markdown: "#083FA1",
  java: "#B07219",
};

export function LspTab() {
  const { t } = useTranslation();
  const status = useGatewayStore((s) => s.status);
  // ADR-055 §6.7 (Phase 4): the relay is a node-local sidecar, so the
  // endpoint is resolved per agent — use the currently selected agent.
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const [config, setConfig] = useState<LspServersConfig | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [healthStatus, setHealthStatus] = useState<Record<string, LspHealthStatus>>({});
  const [healthErrors, setHealthErrors] = useState<Record<string, string | null>>({});
  const [checkingLangs, setCheckingLangs] = useState<Set<string>>(new Set());
  const [installingLangs, setInstallingLangs] = useState<Set<string>>(new Set());
  const [installResults, setInstallResults] = useState<Record<string, { success: boolean; stdout: string; stderr: string }>>({});
  const [scriptDialog, setScriptDialog] = useState<{ language: string; script: string; filename: string } | null>(null);
  const [scriptLoading, setScriptLoading] = useState(false);
  /** LSP Relay base URL (e.g. "http://127.0.0.1:19878"), null when not available */
  const [relayUrl, setRelayUrl] = useState<string | null>(null);

  // Discover LSP Relay endpoint when Gateway is connected
  useEffect(() => {
    if (status !== "connected" || !selectedAgentId) {
      setRelayUrl(null);
      return;
    }
    let cancelled = false;
    getLspRelayUrl(selectedAgentId)
      .then((url) => {
        if (!cancelled) setRelayUrl(url);
      })
      .catch(() => {
        if (!cancelled) setRelayUrl(null);
      });
    return () => { cancelled = true; };
  }, [status, selectedAgentId]);

  const loadAll = useCallback(async (options: { force?: boolean } = {}) => {
    if (!relayUrl) return;
    setRefreshing(true);
    setError(null);

    try {
      // Phase 1: fetch the configured server list. The endpoint reads
      // from a process-lifetime cache, so it returns in milliseconds and
      // does not probe PATH. Once `setConfig` fires, React renders the
      // full list immediately — each row's badge falls back to
      // "unknown" (neutral pending) via `healthStatus[lang] ?? "unknown"`
      // in JSX, so the user never sees an empty list area.
      const cfg = await fetchLspServers(relayUrl);
      setConfig(cfg);

      // Flip every row from the neutral "pending" badge to the amber
      // "checking" badge so the user can see that the slow phase has
      // started. React batches this setState with the `setConfig` call
      // above, so the list and the "checking" badges appear together
      // in a single paint.
      //
      // Skip the "checking" flip when `force = false` AND we already
      // have a non-pending cached status for every language — this is
      // the fast path on Tab re-mount: the user shouldn't see a
      // "checking" flash if the LSP Relay has fresh cached data.
      const cachedModule = getModuleHealthCache(relayUrl);
      const langs = Object.keys(cfg.servers);
      const allCachedFresh =
        !options.force &&
        cachedModule != null &&
        langs.every((lang) => {
          const ts = cachedModule.timestamp[lang];
          return typeof ts === "number" && Date.now() - ts < MODULE_HEALTH_CACHE_TTL_MS;
        });
      if (langs.length > 0 && !allCachedFresh) {
        setHealthStatus((prev) => {
          const next: Record<string, LspHealthStatus> = { ...prev };
          for (const lang of langs) {
            next[lang] = "checking";
          }
          return next;
        });
      }

      // Phase 2: probe PATH per language (cached on the relay).
      // The list is already on screen; only badges need updating when
      // the response arrives.
      try {
        const entries = await fetchLspStatus(relayUrl, { force: options.force });
        setHealthStatus((prev) => {
          const next = { ...prev };
          for (const entry of entries) {
            next[entry.language] = entry.installed ? "installed" : "not_installed";
          }
          return next;
        });
        // Mirror the result into the module-level cache so a remount
        // (e.g. switching Chat → Harness → back to LSP) can skip the
        // network call entirely.
        seedModuleHealthCache(relayUrl, entries);
      } catch (statusErr) {
        // Phase 1 succeeded but the status probe failed — the list is
        // fine, so we don't surface a page-level error. Flip every row
        // to "error" and stash the message so the row's error label
        // explains what happened. The user can still hit Refresh.
        const msg = statusErr instanceof Error ? statusErr.message : "Status check failed";
        setHealthStatus((prev) => {
          const next = { ...prev };
          for (const lang of langs) {
            next[lang] = "error";
          }
          return next;
        });
        setHealthErrors((prev) => {
          const next = { ...prev };
          for (const lang of langs) {
            next[lang] = msg;
          }
          return next;
        });
      }
    } catch (e) {
      // Phase 1 failed (very unlikely — handler is a pure memory read).
      // Surface the error in the page-level ErrorBox.
      setError(e instanceof Error ? e.message : "Failed to load LSP servers");
    } finally {
      setRefreshing(false);
    }
  }, [relayUrl]);

  useEffect(() => {
    if (status === "connected" && relayUrl) {
      // Two-phase load (see `loadAll`): the list arrives almost
      // immediately, then badges resolve incrementally as PATH probes
      // complete on the server.
      void loadAll();
    }
  }, [status, relayUrl, loadAll]);

  /** Check if an LSP server is available by querying the relay's PATH lookup */
  const handleCheck = useCallback(async (language: string) => {
    // relayUrl is guaranteed non-null: the Check button is only rendered
    // after the early-return above for `!relayUrl`. Use an early return
    // to satisfy TypeScript's flow analysis (matches the `!` pattern
    // used in `handleInstall`).
    if (!relayUrl) return;
    setCheckingLangs((prev) => new Set(prev).add(language));
    setHealthStatus((prev) => ({ ...prev, [language]: "checking" }));
    setHealthErrors((prev) => ({ ...prev, [language]: null }));

    try {
      // Single-language endpoint: probes (or cache-hits) only this row.
      // The old behavior fetched the full status array and re-probed
      // every configured language just to update one badge — wasteful,
      // and the entire grid would flicker through a "checking" state
      // 12 times longer than necessary. The relay canonicalizes the
      // language (e.g. "js" → "typescript") so the response key
      // matches the canonical row key in our healthStatus map.
      const entry = await fetchLspStatusForLanguage(relayUrl, language);
      setHealthStatus((prev) => ({
        ...prev,
        [entry.language]: entry.installed ? "installed" : "not_installed",
      }));
      // Mirror the single-language result into the module-level cache
      // so a future batch load can use it without re-fetching.
      seedModuleHealthCache(relayUrl, [entry]);
    } catch (e) {
      setHealthStatus((prev) => ({ ...prev, [language]: "error" }));
      setHealthErrors((prev) => ({
        ...prev,
        [language]: e instanceof Error ? e.message : "Status check failed",
      }));
    } finally {
      setCheckingLangs((prev) => {
        const next = new Set(prev);
        next.delete(language);
        return next;
      });
    }
  }, [relayUrl]);

  /** View install script for a language */
  const handleViewScript = useCallback(async (language: string) => {
    if (!relayUrl) return;
    setScriptLoading(true);
    try {
      const resp = await fetchLspInstallScript(language, relayUrl);
      setScriptDialog({
        language: resp.language,
        script: resp.script,
        filename: resp.filename,
      });
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load install script");
    } finally {
      setScriptLoading(false);
    }
  }, [relayUrl]);

  /** Run install script for a language */
  const handleInstall = useCallback(async (language: string) => {
    // No guard needed: the UI only renders Install buttons when relayUrl is
    // available (see the early return above). If this function is called
    // without a relayUrl, fail loudly so the bug is immediately visible.
    setInstallingLangs((prev) => new Set(prev).add(language));
    setError(null);
    try {
      const result = await runLspInstall(language, relayUrl!);
      setInstallResults((prev) => ({
        ...prev,
        [language]: {
          success: result.success,
          stdout: result.stdout,
          stderr: result.stderr,
        },
      }));
      if (result.success) {
        setHealthStatus((prev) => ({ ...prev, [language]: "installed" }));
      }
    } catch (e) {
      setInstallResults((prev) => ({
        ...prev,
        [language]: {
          success: false,
          stdout: "",
          stderr: e instanceof Error ? e.message : "Install failed",
        },
      }));
    } finally {
      setInstallingLangs((prev) => {
        const next = new Set(prev);
        next.delete(language);
        return next;
      });
    }
  }, [relayUrl]);

  if (status !== "connected") {
    return (
      <div className="max-w-lg">
        <p className="text-xs text-zinc-400">{t("harnessLsp.connectToGateway")}</p>
      </div>
    );
  }

  if (!relayUrl) {
    return (
      <div className="max-w-lg">
        <p className="text-xs text-zinc-400">
          LSP Relay not available. The relay process may not be running.
          Ensure the Gateway started the LSP Relay successfully.
        </p>
      </div>
    );
  }

  const servers = config?.servers ?? {};
  const serverEntries = Object.entries(servers);

  return (
    <div className="max-w-2xl space-y-4">
      {/* Header */}
      <div className="rounded-md border border-zinc-200 bg-modal-surface p-4 dark:border-zinc-700">
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-xs font-medium">{t("harnessLsp.lspServerManagement")}</h2>
          <button
            onClick={() => void loadAll({ force: true })}
            disabled={refreshing}
            className="inline-flex items-center gap-1 text-xs text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300"
          >
            {refreshing ? (
              <Loader2 className="h-3 w-3 animate-spin" />
            ) : (
              <RefreshCw className="h-3 w-3" />
            )}
            {refreshing ? t("harnessLsp.refreshing") : t("harnessLsp.refresh")}
          </button>
        </div>

        {/* Error message */}
        {error && (
          <div className="mb-3">
            <ErrorBox message={error} onClose={() => setError(null)} />
          </div>
        )}

        {/* Loading state */}
        {refreshing && serverEntries.length === 0 && (
          <p className="text-xs text-zinc-400">{t("harnessLsp.loadingServers")}</p>
        )}

        {/* Empty state */}
        {!refreshing && serverEntries.length === 0 && (
          <p className="text-xs text-zinc-400">{t("harnessLsp.noLspServers")}</p>
        )}

        {/* Server list */}
        {serverEntries.length > 0 && (
          <div className="space-y-2">
            {serverEntries.map(([language, entry]) => (
              <LspServerCard
                key={language}
                language={language}
                entry={entry}
                healthStatus={healthStatus[language] ?? "unknown"}
                healthError={healthErrors[language] ?? null}
                isChecking={checkingLangs.has(language)}
                isInstalling={installingLangs.has(language)}
                installResult={installResults[language] ?? null}
                onCheck={() => handleCheck(language)}
                onViewScript={() => handleViewScript(language)}
                onInstall={() => handleInstall(language)}
              />
            ))}
          </div>
        )}
      </div>

      {/* Install script dialog */}
      {scriptDialog && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
          <div className="w-[600px] max-h-[85vh] overflow-y-auto rounded-md bg-modal-surface p-6 shadow-xl">
            <div className="flex items-center justify-between mb-3">
              <h3 className="text-sm font-semibold">
                {t("harnessLsp.scriptContent")} — {LANGUAGE_LABELS[scriptDialog.language] ?? scriptDialog.language}
              </h3>
              <span className="rounded bg-zinc-100 px-2 py-0.5 text-[10px] font-mono text-zinc-500 dark:bg-zinc-700">
                {scriptDialog.filename}
              </span>
            </div>
            <pre className="max-h-96 overflow-auto rounded-md bg-zinc-50 p-4 text-[11px] leading-relaxed dark:bg-zinc-900/50">
              <code>{scriptDialog.script}</code>
            </pre>
            <div className="mt-4 flex justify-end">
              <button
                onClick={() => setScriptDialog(null)}
                className="inline-flex items-center gap-1 rounded btn-solid px-3 py-[var(--ui-btn-py)] text-xs font-medium"
              >
                {t("harnessLsp.close")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Script loading overlay */}
      {scriptLoading && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
          <div className="rounded-md bg-modal-surface p-6 shadow-xl">
            <Loader2 className="mx-auto h-6 w-6 animate-spin text-zinc-400" />
            <p className="mt-2 text-xs text-zinc-500">{t("harnessLsp.loading")}</p>
          </div>
        </div>
      )}
    </div>
  );
}

/** Individual LSP server card */
function LspServerCard({
  language,
  entry,
  healthStatus,
  healthError,
  isChecking,
  isInstalling,
  installResult,
  onCheck,
  onViewScript,
  onInstall,
}: {
  language: string;
  entry: LspServerEntry;
  healthStatus: LspHealthStatus;
  healthError: string | null;
  isChecking: boolean;
  isInstalling: boolean;
  installResult: { success: boolean; stdout: string; stderr: string } | null;
  onCheck: () => void;
  onViewScript: () => void;
  onInstall: () => void;
}) {
  const { t } = useTranslation();
  const [showOutput, setShowOutput] = useState(false);
  const langColor = LANGUAGE_COLORS[language] ?? "#888";
  const langLabel = LANGUAGE_LABELS[language] ?? language;

  return (
    <div className="rounded-md border border-zinc-100 bg-modal-surface p-3 dark:border-zinc-600/50">
      {/* Header row */}
      <div className="flex items-start justify-between gap-2">
        <div className="flex items-center gap-2 min-w-0">
          {/* Language icon */}
          <div
            className="flex h-7 w-7 shrink-0 items-center justify-center rounded text-[10px] font-bold text-white"
            style={{ backgroundColor: langColor }}
          >
            {language.slice(0, 2).toUpperCase()}
          </div>

          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <span className="text-xs font-semibold">{langLabel}</span>
              {/* Health indicator — order matters:
                  - "unknown": status hasn't been resolved yet (defensive
                    fallback when the backend returns fewer status entries
                    than server entries). Renders a neutral pending badge
                    so the row is never empty.
                  - "checking": a probe is in flight (either the auto probe
                    triggered by loadAll / Refresh, or a manual per-row
                    Check). Amber distinguishes user-initiated probes from
                    the neutral pending state.
                  - "installed" / "not_installed": terminal states from
                    the most recent successful probe.
                  - "error": error message is rendered below. */}
              {healthStatus === "unknown" && (
                <span
                  data-testid="lsp-pending-badge"
                  className="inline-flex items-center gap-1 rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] text-zinc-600 dark:bg-zinc-700 dark:text-zinc-400"
                >
                  <Loader2 className="h-2.5 w-2.5 animate-spin" />
                  {t("harnessLsp.pendingCheck")}
                </span>
              )}
              {healthStatus === "checking" && (
                <span
                  data-testid="lsp-checking-badge"
                  className="inline-flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] text-amber-700 dark:bg-amber-900/30 dark:text-amber-400"
                >
                  <Loader2 className="h-2.5 w-2.5 animate-spin" />
                  {t("harnessLsp.checking")}
                </span>
              )}
              {healthStatus === "installed" && (
                <span className="inline-flex items-center gap-1 rounded bg-green-100 px-1.5 py-0.5 text-[10px] text-green-700 dark:bg-green-900/30 dark:text-green-400">
                  <CheckCircle2 className="h-2.5 w-2.5" />
                  {t("harnessLsp.installed")}
                </span>
              )}
              {healthStatus === "not_installed" && (
                <span className="inline-flex items-center gap-1 rounded bg-red-100 px-1.5 py-0.5 text-[10px] text-red-700 dark:bg-red-900/30 dark:text-red-400">
                  <XCircle className="h-2.5 w-2.5" />
                  {t("harnessLsp.notInstalled")}
                </span>
              )}
            </div>
            <p className="mt-0.5 text-[10px] text-zinc-500 dark:text-zinc-400 line-clamp-1">
              {entry.description}
            </p>
          </div>
        </div>

        {/* Action buttons */}
        <div className="flex shrink-0 items-center gap-1.5">
          {/* Check button */}
          <button
            onClick={onCheck}
            disabled={isChecking}
            className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
          >
            {isChecking ? (
              <Loader2 className="h-3 w-3 animate-spin" />
            ) : (
              <Code2 className="h-3 w-3" />
            )}
            {isChecking ? t("harnessLsp.checking") : t("harnessLsp.checkStatus")}
          </button>

          {/* View Script button */}
          {entry.install_script && (
            <button
              onClick={onViewScript}
              className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium"
            >
              <Eye className="h-3 w-3" />
              {t("harnessLsp.viewScript")}
            </button>
          )}

          {/* Install button — hidden once we know the server is installed.
              Mirrors the MCP Tab pattern: instead of a no-op button we show
              a green "installed" indicator in the action area. The user can
              still re-run Check to confirm the server actually responds to
              LSP protocol messages. */}
          {entry.install_script && healthStatus !== "installed" && (
            <button
              onClick={onInstall}
              disabled={isInstalling}
              className="inline-flex items-center gap-1 rounded btn-solid px-2 py-1 text-[11px] font-medium disabled:opacity-50"
            >
              {isInstalling ? (
                <Loader2 className="h-3 w-3 animate-spin" />
              ) : (
                <Terminal className="h-3 w-3" />
              )}
              {isInstalling ? t("harnessLsp.installing") : t("harnessLsp.install")}
            </button>
          )}
          {entry.install_script && healthStatus === "installed" && (
            <span
              data-testid="lsp-installed-indicator"
              className="inline-flex items-center gap-1 rounded bg-green-100 px-2 py-1 text-[11px] font-medium text-green-700 dark:bg-green-900/30 dark:text-green-400"
            >
              <CheckCircle2 className="h-3 w-3" />
              {t("harnessLsp.installed")}
            </span>
          )}
        </div>
      </div>

      {/* Health error */}
      {healthStatus === "not_installed" && healthError && (
        <p className="mt-1.5 text-[10px] text-red-500 break-all">{healthError}</p>
      )}

      {/* Candidates list */}
      {entry.candidates.length > 0 && (
        <div className="mt-1.5 flex flex-wrap items-center gap-1">
          <span className="text-[10px] text-zinc-400">{t("harnessLsp.candidates")}:</span>
          {entry.candidates.map((cmd) => (
            <code
              key={cmd}
              className="rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] font-mono text-zinc-600 dark:bg-zinc-700 dark:text-zinc-400"
            >
              {cmd}
            </code>
          ))}
        </div>
      )}

      {/* Install hint */}
      {entry.install_hint && (
        <div className="mt-1.5 flex items-center gap-1">
          <span className="text-[10px] text-zinc-400">{t("harnessLsp.installHint")}:</span>
          <code className="rounded bg-zinc-100 px-1.5 py-0.5 text-[10px] font-mono text-amber-600 dark:bg-zinc-700 dark:text-amber-400">
            {entry.install_hint}
          </code>
        </div>
      )}

      {/* Install result output */}
      {installResult && (
        <div className="mt-2">
          <div className="flex items-center gap-2 mb-1">
            {installResult.success ? (
              <span className="inline-flex items-center gap-1 text-[10px] text-green-600 dark:text-green-400">
                <CheckCircle2 className="h-2.5 w-2.5" />
                {t("harnessLsp.installSuccess")}
              </span>
            ) : (
              <span className="inline-flex items-center gap-1 text-[10px] text-red-600 dark:text-red-400">
                <XCircle className="h-2.5 w-2.5" />
                {t("harnessLsp.installFailed")}
              </span>
            )}
            <button
              onClick={() => setShowOutput(!showOutput)}
              className="text-[10px] text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300"
            >
              {showOutput ? "Hide output" : "Show output"}
            </button>
          </div>
          {showOutput && (
            <pre className="max-h-40 overflow-auto rounded-md bg-zinc-50 p-2 text-[10px] leading-relaxed dark:bg-zinc-900/50">
              <code>{installResult.stdout || installResult.stderr || "(no output)"}</code>
            </pre>
          )}
        </div>
      )}
    </div>
  );
}
