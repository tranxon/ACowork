import { useState, useEffect, useRef, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { cn } from "../../lib/utils";
import type {
  PreparePublishResponse,
  BuildPublishResponse,
  ExportPackageResponse,
  AgentDetail,
} from "../../lib/types";
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { resolveAgentAvatarUrl } from "../../lib/avatar";
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
  ImagePlus,
  X as XIcon,
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

/**
 * Source of the avatar that will be baked into the published package.
 * Persisted to manifest.toml at build time.
 *
 *  - `builtin`     — use one of the bundled icon-XX.jpg icons. Stored as
 *                    `builtin_avatar = "icon-XX"`.
 *  - `packaged`    — ship a local image file at `manifest.avatar`. Stored as
 *                    `avatar = "<relative path>"`.
 *  - `none`        — neither. Clients that install the package will assign
 *                    a random builtin icon at first install.
 */
type AvatarSelection =
  | { kind: "builtin"; iconId: string }
  | { kind: "packaged"; relativePath: string }
  | { kind: "none" };

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

  // Agent detail (loaded on open to seed the avatar selection)
  const [, setAgentDetail] = useState<AgentDetail | null>(null);

  // Avatar selection — initialised from agentDetail once it loads, then
  // mutated by the user. `dirty` is true when the current selection differs
  // from what's persisted in manifest.toml.
  const [avatar, setAvatar] = useState<AvatarSelection>({ kind: "none" });
  const [avatarDirty, setAvatarDirty] = useState(false);
  const initialAvatarRef = useRef<AvatarSelection | null>(null);

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
      setAgentDetail(null);
      setAvatar({ kind: "none" });
      setAvatarDirty(false);
      initialAvatarRef.current = null;
    }
  }, [open]);

  // Load agent detail on open to seed the avatar picker
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    invoke<AgentDetail>("get_agent_detail", { agentId })
      .then((detail) => {
        if (cancelled) return;
        setAgentDetail(detail);
        const seed = deriveAvatarFromManifest(detail.avatar, detail.builtin_avatar);
        setAvatar(seed);
        initialAvatarRef.current = seed;
        setAvatarDirty(false);
      })
      .catch((err) => {
        if (cancelled) return;
        // Non-fatal: the wizard can still proceed without an initial selection.
        console.warn("Failed to load agent detail for publish wizard:", err);
      });
    return () => {
      cancelled = true;
    };
  }, [open, agentId]);

  // Persist the avatar selection to manifest.toml. Returns true on success.
  const persistAvatar = useCallback(async (): Promise<boolean> => {
    if (!avatarDirty) return true;
    try {
      const avatarField =
        avatar.kind === "packaged" ? avatar.relativePath : avatar.kind === "none" ? "" : null;
      const builtinField = avatar.kind === "builtin" ? avatar.iconId : avatar.kind === "none" ? "" : null;
      // The backend distinguishes "omit" (don't touch) from "set to empty" (clear).
      // We always send both fields, with empty string = clear. null = don't touch.
      await invoke("update_agent_manifest_avatar", {
        agentId,
        avatar: avatarField,
        builtinAvatar: builtinField,
      });
      setAvatarDirty(false);
      return true;
    } catch (err) {
      setError(`Failed to save avatar selection: ${err instanceof Error ? err.message : String(err)}`);
      return false;
    }
  }, [agentId, avatar, avatarDirty]);

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
      // Persist avatar first so the package reflects the user's choice.
      if (avatarDirty) {
        const ok = await persistAvatar();
        if (!ok) {
          setBusy(false);
          return;
        }
      }
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
          <h2 className="text-sm font-semibold text-zinc-800 dark:text-zinc-100">
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
              <h3 className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
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
              <h3 className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
                Clean Results
              </h3>
              <p className="text-xs text-zinc-500 dark:text-zinc-400">
                {cleanResult.cleaned
                  ? "Cleaned: removed dev flag, cleared recordings, reset config."
                  : "Clean completed (no changes needed)."}
              </p>
            </div>
          )}

          {/* Build step: avatar sub-form + (later) build result */}
          {step === "build" && (
            <div className="space-y-4">
              <AvatarPickerSubForm
                agentId={agentId}
                value={avatar}
                onChange={(next) => {
                  setAvatar(next);
                  setAvatarDirty(
                    !initialAvatarRef.current ||
                      JSON.stringify(next) !== JSON.stringify(initialAvatarRef.current),
                  );
                }}
                disabled={busy}
              />
              {buildResult && (
                <div className="space-y-2">
                  <h3 className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
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
            </div>
          )}

          {/* Sign result */}
          {signResult && (
            <div className="space-y-2">
              <h3 className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
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
              <h3 className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
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
              <h3 className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
                Distribute
              </h3>
              <p className="text-xs text-zinc-500 dark:text-zinc-400">
                The package is ready. You can export it or install it locally.
              </p>
              <button
                onClick={runExport}
                disabled={busy}
                className="flex items-center gap-2 rounded-md border border-zinc-200 px-3 py-1.5 text-xs font-medium text-zinc-600 transition-colors hover:bg-zinc-50 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-300 dark:hover:bg-zinc-700"
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
            className="rounded-md px-3 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 disabled:opacity-50 dark:text-zinc-400 dark:hover:bg-zinc-700"
          >
            {step === "distribute" ? "Close" : "Cancel"}
          </button>

          {currentAction && (
            <button
              onClick={currentAction.action}
              disabled={busy}
              className="flex items-center gap-2 rounded-md btn-solid px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
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

/**
 * Translate a manifest's `avatar` / `builtin_avatar` fields into the
 * wizard's internal `AvatarSelection` representation. Prefers the
 * packaged image (since `manifest.avatar` wins at install time), falling
 * back to a builtin hint, falling back to "none".
 */
function deriveAvatarFromManifest(
  avatar: string | undefined | null,
  builtinAvatar: string | undefined | null,
): AvatarSelection {
  if (avatar) return { kind: "packaged", relativePath: avatar };
  if (builtinAvatar) return { kind: "builtin", iconId: builtinAvatar };
  return { kind: "none" };
}

// ── Avatar sub-form ────────────────────────────────────────────────────

function AvatarPickerSubForm({
  agentId,
  value,
  onChange,
  disabled,
}: {
  agentId: string;
  value: AvatarSelection;
  onChange: (next: AvatarSelection) => void;
  disabled: boolean;
}) {
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const [pickerError, setPickerError] = useState<string | null>(null);

  const handlePickBuiltin = (iconId: string) => {
    setPickerError(null);
    onChange({ kind: "builtin", iconId });
  };

  const handlePickNone = () => {
    setPickerError(null);
    onChange({ kind: "none" });
  };

  const handlePickFile = async () => {
    setPickerError(null);
    try {
      const selected = await openDialog({
        multiple: false,
        directory: false,
        filters: [
          {
            name: "Image",
            extensions: ["png", "jpg", "jpeg", "gif", "webp", "svg"],
          },
        ],
      });
      if (!selected || typeof selected !== "string") return;
      await uploadImageFile(agentId, selected);
      // Derive a relative path inside the install dir. We adopt a
      // deterministic location — `assets/avatar.<ext>` — so the manifest
      // reference is stable across rebuilds.
      const ext = selected.split(".").pop()?.toLowerCase() ?? "png";
      const relative = `assets/avatar.${ext}`;
      onChange({ kind: "packaged", relativePath: relative });
    } catch (err) {
      setPickerError(err instanceof Error ? err.message : String(err));
    }
  };

  const handleClearPackaged = () => {
    setPickerError(null);
    onChange({ kind: "none" });
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2">
        <h3 className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
          Default Avatar
        </h3>
        <span className="text-[10px] text-zinc-400">
          baked into manifest.toml at build time
        </span>
      </div>

      {/* Current preview */}
      <AvatarPreview selection={value} agentId={agentId} />

      {/* Mode tabs */}
      <div className="grid grid-cols-3 gap-1 rounded-md bg-zinc-100 p-1 text-xs dark:bg-zinc-900/50">
        <ModeTab
          active={value.kind === "builtin"}
          onClick={() => {
            if (value.kind === "builtin") return;
            onChange({ kind: "builtin", iconId: BUILTIN_ICON_IDS[0] ?? "icon-01" });
          }}
          disabled={disabled}
        >
          Builtin icon
        </ModeTab>
        <ModeTab
          active={value.kind === "packaged"}
          onClick={handlePickFile}
          disabled={disabled}
        >
          Local image
        </ModeTab>
        <ModeTab
          active={value.kind === "none"}
          onClick={handlePickNone}
          disabled={disabled}
        >
          No avatar
        </ModeTab>
      </div>

      {/* Builtin grid */}
      {value.kind === "builtin" && (
        <div className="rounded-md border border-zinc-200 p-2 dark:border-zinc-700">
          <div className="grid grid-cols-7 gap-1.5">
            {BUILTIN_ICON_IDS.map((iconId) => {
              const active = value.iconId === iconId;
              return (
                <button
                  key={iconId}
                  type="button"
                  onClick={() => handlePickBuiltin(iconId)}
                  disabled={disabled}
                  className={cn(
                    "flex aspect-square items-center justify-center rounded-md p-0.5 transition-colors",
                    active
                      ? "bg-zinc-200 dark:bg-zinc-600"
                      : "hover:bg-zinc-100 dark:hover:bg-zinc-700",
                  )}
                  title={iconId}
                >
                  <img
                    src={BUILTIN_ICONS[iconId] ?? ""}
                    alt={iconId}
                    draggable={false}
                    className="h-full w-full rounded-full object-cover"
                  />
                </button>
              );
            })}
          </div>
        </div>
      )}

      {/* Packaged info */}
      {value.kind === "packaged" && (
        <div className="flex items-center justify-between rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700">
          <div className="min-w-0">
            <div className="font-mono text-zinc-700 dark:text-zinc-200">
              {value.relativePath}
            </div>
            <div className="text-[10px] text-zinc-400">
              ship as part of the .agent package
            </div>
          </div>
          <div className="flex items-center gap-1">
            <button
              type="button"
              onClick={handlePickFile}
              disabled={disabled}
              className="rounded-md px-2 py-1 text-[10px] text-zinc-500 hover:bg-zinc-100 hover:text-zinc-800 disabled:opacity-50 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
            >
              Replace
            </button>
            <button
              type="button"
              onClick={handleClearPackaged}
              disabled={disabled}
              className="rounded-md px-2 py-1 text-[10px] text-zinc-500 hover:bg-zinc-100 hover:text-red-600 disabled:opacity-50 dark:hover:bg-zinc-700"
              aria-label="Remove packaged avatar"
            >
              <XIcon className="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
      )}

      {/* None — random fallback note */}
      {value.kind === "none" && (
        <p className="rounded-md border border-dashed border-zinc-200 px-3 py-2 text-[11px] text-zinc-500 dark:border-zinc-700 dark:text-zinc-400">
          The client will assign a random builtin icon on first install.
        </p>
      )}

      {pickerError && (
        <p className="text-xs text-red-600 dark:text-red-400">{pickerError}</p>
      )}

      {/* Hidden file input — currently unused (we use Tauri dialog instead)
          but kept here for potential future "drop a file here" affordance. */}
      <input
        ref={fileInputRef}
        type="file"
        accept="image/*"
        className="hidden"
      />
    </div>
  );
}

function ModeTab({
  active,
  onClick,
  disabled,
  children,
}: {
  active: boolean;
  onClick: () => void;
  disabled?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={cn(
        "rounded-md px-2 py-1 font-medium transition-colors disabled:opacity-50",
        active
          ? "bg-white text-zinc-800 shadow-sm dark:bg-zinc-700 dark:text-zinc-100"
          : "text-zinc-500 hover:text-zinc-800 dark:text-zinc-400 dark:hover:text-zinc-200",
      )}
    >
      {children}
    </button>
  );
}

function AvatarPreview({ selection, agentId }: { selection: AvatarSelection; agentId: string }) {
  if (selection.kind === "builtin") {
    const src = BUILTIN_ICONS[selection.iconId] ?? BUILTIN_ICONS["icon-01"];
    return (
      <div className="flex items-center gap-3 rounded-md border border-zinc-200 px-3 py-2 dark:border-zinc-700">
        <img
          src={src}
          alt={selection.iconId}
          draggable={false}
          className="h-16 w-16 rounded-full object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60"
        />
        <div>
          <div className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
            Builtin icon: <span className="font-mono">{selection.iconId}</span>
          </div>
          <div className="text-[10px] text-zinc-400">
            stored as <span className="font-mono">builtin_avatar</span> in manifest.toml
          </div>
        </div>
      </div>
    );
  }
  if (selection.kind === "packaged") {
    // Preview the packaged image by hitting the gateway avatar endpoint.
    // The endpoint serves the file only if the manifest has avatar set; the
    // preview works after the user has uploaded the file (regardless of
    // whether manifest.toml has been updated yet).
    const url = resolveAgentAvatarUrl(agentId);
    return (
      <div className="flex items-center gap-3 rounded-md border border-zinc-200 px-3 py-2 dark:border-zinc-700">
        {url ? (
          <img
            src={url}
            alt={selection.relativePath}
            draggable={false}
            className="h-16 w-16 rounded-full object-cover ring-1 ring-zinc-300/60 dark:ring-zinc-600/60"
            onError={(e) => {
              // Fall back to a placeholder if the file isn't readable yet.
              (e.currentTarget as HTMLImageElement).style.display = "none";
            }}
          />
        ) : (
          <div className="h-16 w-16" />
        )}
        <div>
          <div className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
            Local image: <span className="font-mono">{selection.relativePath}</span>
          </div>
          <div className="text-[10px] text-zinc-400">
            stored as <span className="font-mono">avatar</span> in manifest.toml
          </div>
        </div>
      </div>
    );
  }
  return (
    <div className="flex items-center gap-3 rounded-md border border-dashed border-zinc-200 px-3 py-2 dark:border-zinc-700">
      <div className="flex h-16 w-16 items-center justify-center rounded-full bg-zinc-100 text-zinc-400 dark:bg-zinc-800">
        <ImagePlus className="h-6 w-6" />
      </div>
      <div>
        <div className="text-xs font-medium text-zinc-700 dark:text-zinc-200">
          No avatar selected
        </div>
        <div className="text-[10px] text-zinc-400">
          clients will fall back to a random builtin icon at install time
        </div>
      </div>
    </div>
  );
}

// ── Helpers ────────────────────────────────────────────────────────────

/**
 * Upload a user-selected image file to the gateway's
 * `POST /api/agents/{id}/manifest/file` endpoint. The gateway restricts
 * the destination path to image extensions and canonicalises the path
 * to prevent escape from the install dir.
 */
async function uploadImageFile(agentId: string, filePath: string): Promise<void> {
  // Derive the relative path. We adopt `assets/avatar.<ext>` so the
  // manifest reference is stable. The server will create the `assets/`
  // directory if it doesn't exist.
  const ext = filePath.split(".").pop()?.toLowerCase() ?? "png";
  const relative = `assets/avatar.${ext}`;
  await invoke("upload_agent_file", {
    agentId,
    relativePath: relative,
    filePath,
  });
}
