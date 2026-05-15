import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { cn } from "../../lib/utils";
import type {
  PreparePublishResponse,
  BuildPublishResponse,
  ExportPackageResponse,
} from "../../lib/types";
import {
  CheckCircle,
  XCircle,
  AlertTriangle,
  Package,
  Brush,
  FileDown,
  Key,
  Check,
  Loader2,
  ExternalLink,
} from "lucide-react";

interface PublishWizardProps {
  open: boolean;
  agentId: string;
  agentName: string;
  onClose: () => void;
}

type WizardStep = "check" | "clean" | "build" | "sign" | "distribute";

const STEPS: { key: WizardStep; label: string; icon: React.ElementType }[] = [
  { key: "check", label: "Check", icon: CheckCircle },
  { key: "clean", label: "Clean", icon: Brush },
  { key: "build", label: "Package", icon: Package },
  { key: "sign", label: "Sign", icon: Key },
  { key: "distribute", label: "Distribute", icon: FileDown },
];

export function PublishWizard({
  open,
  agentId,
  agentName,
  onClose,
}: PublishWizardProps) {
  const [step, setStep] = useState<WizardStep>("check");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Prepare results
  const [checkResult, setCheckResult] = useState<PreparePublishResponse | null>(null);
  const [cleanResult, setCleanResult] = useState<PreparePublishResponse | null>(null);

  // Build results
  const [buildResult, setBuildResult] = useState<BuildPublishResponse | null>(null);
  const [signResult, setSignResult] = useState<BuildPublishResponse | null>(null);

  // Export result
  const [exportResult, setExportResult] = useState<ExportPackageResponse | null>(null);

  // Reset on open
  useEffect(() => {
    if (open) {
      setStep("check");
      setError(null);
      setCheckResult(null);
      setCleanResult(null);
      setBuildResult(null);
      setSignResult(null);
      setExportResult(null);
    }
  }, [open]);

  // Close on Escape
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, busy, onClose]);

  const runCheck = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await invoke<PreparePublishResponse>("prepare_publish", {
        agentId,
        clean: false,
      });
      setCheckResult(result);
      setStep("clean");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const runClean = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await invoke<PreparePublishResponse>("prepare_publish", {
        agentId,
        clean: true,
      });
      setCleanResult(result);
      setStep("build");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const runBuild = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await invoke<BuildPublishResponse>("build_publish", {
        agentId,
        sign: false,
      });
      setBuildResult(result);
      setStep("sign");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const runSign = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await invoke<BuildPublishResponse>("build_publish", {
        agentId,
        sign: true,
      });
      setSignResult(result);
      setStep("distribute");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const runExport = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await invoke<ExportPackageResponse>("export_package", {
        agentId,
      });
      setExportResult(result);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const stepActions: Record<WizardStep, { label: string; action: () => void } | null> = {
    check: { label: "Run Check", action: runCheck },
    clean: { label: "Run Clean", action: runClean },
    build: { label: "Build Package", action: runBuild },
    sign: { label: "Sign Package", action: runSign },
    distribute: null, // manual actions
  };

  const stepIndex = STEPS.findIndex((s) => s.key === step);
  const currentAction = stepActions[step];

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div
        className="absolute inset-0 bg-black/40"
        onClick={busy ? undefined : onClose}
      />

      {/* Dialog */}
      <div className="relative z-10 flex w-full max-w-2xl flex-col rounded-lg border border-zinc-200 bg-white shadow-xl dark:border-zinc-700 dark:bg-zinc-800">
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-zinc-200 px-5 py-3.5 dark:border-zinc-700">
          <Package className="h-5 w-5 text-zinc-500 dark:text-zinc-400" />
          <h2 className="text-base font-semibold text-zinc-800 dark:text-zinc-100">
            Publish: {agentName}
          </h2>
        </div>

        {/* Step indicators */}
        <div className="flex items-center gap-0 border-b border-zinc-200 px-5 py-3 dark:border-zinc-700">
          {STEPS.map((s, i) => {
            const Icon = s.icon;
            const active = s.key === step;
            const passed = i < stepIndex;
            return (
              <div key={s.key} className="flex items-center">
                <div
                  className={cn(
                    "flex items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium transition-colors",
                    active && "bg-zinc-200 text-zinc-800 dark:bg-zinc-300 dark:text-zinc-900",
                    passed && "bg-green-100 text-green-700 dark:bg-green-900/30 dark:text-green-400",
                    !active && !passed && "text-zinc-400 dark:text-zinc-500",
                  )}
                >
                  {passed ? (
                    <Check className="h-3 w-3" />
                  ) : (
                    <Icon className="h-3 w-3" />
                  )}
                  {s.label}
                </div>
                {i < STEPS.length - 1 && (
                  <div
                    className={cn(
                      "mx-1 h-px w-4",
                      i < stepIndex
                        ? "bg-green-300 dark:bg-green-600"
                        : "bg-zinc-200 dark:bg-zinc-600",
                    )}
                  />
                )}
              </div>
            );
          })}
        </div>

        {/* Step content */}
        <div className="flex-1 space-y-4 overflow-y-auto px-5 py-4">
          {/* Check results */}
          {checkResult && (
            <div className="space-y-2">
              <h3 className="text-sm font-medium text-zinc-700 dark:text-zinc-200">
                Check Results
              </h3>
              {checkResult.checks.map((item, i) => (
                <div
                  key={i}
                  className={cn(
                    "flex items-start gap-2 rounded-md px-3 py-2 text-xs",
                    item.status === "ok" && "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400",
                    item.status === "warn" && "bg-yellow-50 text-yellow-700 dark:bg-yellow-900/20 dark:text-yellow-400",
                    item.status === "error" && "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400",
                  )}
                >
                  {item.status === "ok" ? (
                    <CheckCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  ) : item.status === "warn" ? (
                    <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  ) : (
                    <XCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  )}
                  <div>
                    <span className="font-medium">{item.field}</span>
                    {item.message && (
                      <span className="ml-1 text-zinc-500 dark:text-zinc-400">
                        — {item.message}
                      </span>
                    )}
                  </div>
                </div>
              ))}
              {checkResult.errors.length > 0 && (
                <div className="rounded-md bg-red-50 px-3 py-2 dark:bg-red-900/20">
                  {checkResult.errors.map((e, i) => (
                    <p key={i} className="text-xs text-red-600 dark:text-red-400">
                      {e}
                    </p>
                  ))}
                </div>
              )}
              {checkResult.warnings.length > 0 && (
                <div className="rounded-md bg-yellow-50 px-3 py-2 dark:bg-yellow-900/20">
                  {checkResult.warnings.map((w, i) => (
                    <p key={i} className="text-xs text-yellow-700 dark:text-yellow-400">
                      {w}
                    </p>
                  ))}
                </div>
              )}
            </div>
          )}

          {/* Clean results */}
          {cleanResult && (
            <div className="space-y-2">
              <h3 className="text-sm font-medium text-zinc-700 dark:text-zinc-200">
                Clean Results
              </h3>
              <p className="text-xs text-zinc-500 dark:text-zinc-400">
                {cleanResult.cleaned
                  ? "Cleaned: removed dev flag, cleared recordings, reset config."
                  : "Clean completed (no changes needed)."}
              </p>
            </div>
          )}

          {/* Build result */}
          {buildResult && (
            <div className="space-y-2">
              <h3 className="text-sm font-medium text-zinc-700 dark:text-zinc-200">
                Build Result
              </h3>
              <div className="rounded-md bg-green-50 px-3 py-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                <p>
                  Package built:{" "}
                  <span className="font-mono">{buildResult.output_path}</span>
                </p>
                <p>
                  Size:{" "}
                  {(buildResult.file_size / 1024).toFixed(1)} KB
                </p>
              </div>
            </div>
          )}

          {/* Sign result */}
          {signResult && (
            <div className="space-y-2">
              <h3 className="text-sm font-medium text-zinc-700 dark:text-zinc-200">
                Sign Result
              </h3>
              <div
                className={cn(
                  "rounded-md px-3 py-2 text-xs",
                  signResult.signed
                    ? "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400"
                    : "bg-yellow-50 text-yellow-700 dark:bg-yellow-900/20 dark:text-yellow-400",
                )}
              >
                <p>
                  Status: {signResult.signed ? "Signed ✓" : "Unsigned"}
                </p>
                <p>
                  <span className="font-mono">{signResult.output_path}</span>
                </p>
              </div>
            </div>
          )}

          {/* Export result */}
          {exportResult && (
            <div className="space-y-2">
              <h3 className="text-sm font-medium text-zinc-700 dark:text-zinc-200">
                Export Result
              </h3>
              <div className="rounded-md bg-green-50 px-3 py-2 text-xs text-green-700 dark:bg-green-900/20 dark:text-green-400">
                <p>
                  Status: {exportResult.status}
                </p>
                <p className="font-mono">{exportResult.output_path}</p>
              </div>
            </div>
          )}

          {/* Distribute step - manual actions */}
          {step === "distribute" && (
            <div className="space-y-3">
              <h3 className="text-sm font-medium text-zinc-700 dark:text-zinc-200">
                Distribute
              </h3>
              <p className="text-xs text-zinc-500 dark:text-zinc-400">
                The package is ready. You can export it or install it locally.
              </p>
              <button
                onClick={runExport}
                disabled={busy}
                className="flex items-center gap-2 rounded-md border border-zinc-200 px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
              >
                {busy ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  <ExternalLink className="h-4 w-4" />
                )}
                Export Package
              </button>
            </div>
          )}

          {/* Error */}
          {error && (
            <div className="rounded-md bg-red-50 px-3 py-2 text-xs text-red-600 dark:bg-red-900/20 dark:text-red-400">
              {error}
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex justify-between border-t border-zinc-200 px-5 py-3 dark:border-zinc-700">
          <button
            onClick={onClose}
            disabled={busy}
            className="rounded-md border border-zinc-200 px-4 py-1.5 text-sm font-medium text-zinc-700 hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
          >
            {step === "distribute" ? "Close" : "Cancel"}
          </button>

          {currentAction && (
            <button
              onClick={currentAction.action}
              disabled={busy}
              className="flex items-center gap-2 rounded-md btn-solid px-4 py-1.5 text-sm font-medium disabled:cursor-not-allowed disabled:opacity-50"
            >
              {busy ? (
                <>
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  Working...
                </>
              ) : (
                currentAction.label
              )}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
