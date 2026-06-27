import { useState, useEffect, useCallback } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useGatewayStore } from "../../stores/gatewayStore";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { fetchLspServers, fetchLspInstallScript, runLspInstall } from "../../lib/gateway-api";
import type { LspServersConfig, LspServerEntry, LspHealthStatus } from "../../lib/types";
import { CheckCircle2, XCircle, Loader2, Eye, Terminal, Code2, RefreshCw } from "lucide-react";
import { ErrorBox } from "../common/ErrorBox";

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
  const [config, setConfig] = useState<LspServersConfig | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [healthStatus, setHealthStatus] = useState<Record<string, LspHealthStatus>>({});
  const [healthErrors, setHealthErrors] = useState<Record<string, string | null>>({});
  const [checkingLangs, setCheckingLangs] = useState<Set<string>>(new Set());
  const [installingLangs, setInstallingLangs] = useState<Set<string>>(new Set());
  const [installResults, setInstallResults] = useState<Record<string, { success: boolean; stdout: string; stderr: string }>>({});
  const [scriptDialog, setScriptDialog] = useState<{ language: string; script: string; filename: string } | null>(null);
  const [scriptLoading, setScriptLoading] = useState(false);

  const loadServers = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const resp = await fetchLspServers();
      setConfig(resp);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load LSP servers");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (status === "connected") {
      loadServers();
    }
  }, [status, loadServers]);

  /** Check if an LSP server is available by attempting a WebSocket handshake */
  const handleCheck = useCallback(async (language: string) => {
    setCheckingLangs((prev) => new Set(prev).add(language));
    setHealthStatus((prev) => ({ ...prev, [language]: "checking" }));
    setHealthErrors((prev) => ({ ...prev, [language]: null }));

    const httpUrl = getGatewayUrl();
    const wsUrl = httpUrl.replace(/^http/, "ws");
    const url = `${wsUrl}/lsp/${encodeURIComponent(language)}`;

    try {
      const ws = new WebSocket(url);

      const result = await new Promise<{ installed: boolean; error?: string }>((resolve) => {
        const timeout = setTimeout(() => {
          ws.close();
          resolve({ installed: false, error: "Connection timed out" });
        }, 5000);

        ws.onopen = () => {
          clearTimeout(timeout);
          // Send a minimal JSON-RPC message to trigger a response
          ws.send(JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            method: "initialize",
            params: {
              processId: null,
              capabilities: {},
              rootUri: null,
            },
          }));
        };

        ws.onmessage = (event) => {
          clearTimeout(timeout);
          try {
            const data = JSON.parse(event.data);
            if (data.result?.capabilities || data.id === 1) {
              resolve({ installed: true });
            } else if (data.error) {
              resolve({ installed: true, error: data.error.message });
            } else {
              resolve({ installed: true });
            }
          } catch {
            resolve({ installed: true });
          }
          ws.close();
        };

        ws.onerror = () => {
          clearTimeout(timeout);
          resolve({ installed: false, error: "Connection failed" });
        };

        ws.onclose = (e) => {
          clearTimeout(timeout);
          if (e.code !== 1000 && e.code !== 1005) {
            resolve({ installed: false, error: `Connection closed (${e.code})` });
          }
        };
      });

      setHealthStatus((prev) => ({
        ...prev,
        [language]: result.installed ? "installed" : "not_installed",
      }));
      setHealthErrors((prev) => ({
        ...prev,
        [language]: result.error ?? null,
      }));
    } catch (e) {
      setHealthStatus((prev) => ({ ...prev, [language]: "error" }));
      setHealthErrors((prev) => ({
        ...prev,
        [language]: e instanceof Error ? e.message : "Unknown error",
      }));
    } finally {
      setCheckingLangs((prev) => {
        const next = new Set(prev);
        next.delete(language);
        return next;
      });
    }
  }, []);

  /** View install script for a language */
  const handleViewScript = useCallback(async (language: string) => {
    setScriptLoading(true);
    try {
      const resp = await fetchLspInstallScript(language);
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
  }, []);

  /** Run install script for a language */
  const handleInstall = useCallback(async (language: string) => {
    setInstallingLangs((prev) => new Set(prev).add(language));
    setError(null);
    try {
      const result = await runLspInstall(language);
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
  }, []);

  if (status !== "connected") {
    return (
      <div className="max-w-lg">
        <p className="text-xs text-zinc-400">{t("harnessLsp.connectToGateway")}</p>
      </div>
    );
  }

  const servers = config?.servers ?? {};
  const serverEntries = Object.entries(servers);

  return (
    <div className="max-w-2xl space-y-4">
      {/* Header */}
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-xs font-medium">{t("harnessLsp.lspServerManagement")}</h2>
          <button
            onClick={loadServers}
            disabled={loading}
            className="inline-flex items-center gap-1 text-xs text-zinc-500 hover:text-zinc-700 dark:text-zinc-400 dark:hover:text-zinc-300"
          >
            <RefreshCw className={cn("h-3 w-3", loading && "animate-spin")} />
            {loading ? t("harnessLsp.loading") : t("harnessLsp.refresh")}
          </button>
        </div>

        {/* Error message */}
        {error && (
          <div className="mb-3">
            <ErrorBox message={error} onClose={() => setError(null)} />
          </div>
        )}

        {/* Loading state */}
        {loading && serverEntries.length === 0 && (
          <p className="text-xs text-zinc-400">{t("harnessLsp.loadingServers")}</p>
        )}

        {/* Empty state */}
        {!loading && serverEntries.length === 0 && (
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
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
          <div className="w-[600px] max-h-[85vh] overflow-y-auto rounded-md bg-white p-6 shadow-xl dark:bg-zinc-800">
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
                className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-3 py-1.5 text-xs font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
              >
                {t("harnessLsp.close")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Script loading overlay */}
      {scriptLoading && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/30">
          <div className="rounded-md bg-white p-6 shadow-xl dark:bg-zinc-800">
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
    <div className="rounded-md border border-zinc-100 bg-white p-3 dark:border-zinc-600 dark:bg-zinc-800/50">
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
              {/* Health indicator */}
              {healthStatus === "checking" && (
                <span className="inline-flex items-center gap-1 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] text-amber-700 dark:bg-amber-900/30 dark:text-amber-400">
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
            className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-2 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
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
              className="inline-flex items-center gap-1 rounded-md border border-zinc-300 px-2 py-1 text-[11px] font-medium text-zinc-700 hover:bg-zinc-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
            >
              <Eye className="h-3 w-3" />
              {t("harnessLsp.viewScript")}
            </button>
          )}

          {/* Install button */}
          {entry.install_script && (
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
