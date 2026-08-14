import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useAgentStore, type AgentProfileSettings } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";
import { getGatewayUrl } from "../../lib/config";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import { Switch } from "../common/Switch";
import {
  clearAgentAvatarCache,
  fetchAvatarAssets,
  fetchAvatarConfig,
  updateAvatarConfig,
  deleteAvatarFile,
  resolveAgentAvatarFileUrl,
} from "../../lib/avatar";
import type { AvatarAssetEntry, AvatarConfigResponse } from "../../lib/types";
import { log } from "../../lib/logger";

// ── Helpers ───────────────────────────────────────────────────────────

/** Compute the next available avatar-XX filename (zero-padded to 2 digits). */
function nextAvatarName(assets: AvatarAssetEntry[], ext: string): string {
  const used = new Set<number>();
  for (const a of assets) {
    const fn = a.relative_path.split("/").pop() ?? "";
    const m = fn.match(/^avatar-(\d+)\./i);
    if (m) used.add(parseInt(m[1], 10));
  }
  let n = 1;
  while (used.has(n)) n++;
  return `avatar-${String(n).padStart(2, "0")}.${ext}`;
}

const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp", "svg"];

// ── Write-through persistence config (ADR-052 follow-up) ──────────────
//
// Setup panel writes through to the runtime per-field on change. No more
// batched "Apply" step — see `saveField` below. A single 500ms debounce
// applies to every wired field:
//
//   - Visual feedback (`setProfile`) is synchronous — users see the
//     change instantly, regardless of debounce.
//   - 500ms is the lower bound of "noticeable delay" — short enough that
//     the saving indicator's transition (`Saving N change(s)...` →
//     `All changes saved`) feels responsive; long enough that rapid input
//     ("1" → "12" → "123", slider drag, double-click Switch) collapses
//     into a single PUT.
//   - Single value keeps the mental model simple — adding a new wired
//     field doesn't require picking a debounce bucket.
//
// The wire field names mirror `UpdateAgentConfigRequest` in
// `acowork-runtime/src/http/server.rs`; keep these in lockstep when
// adding a new field.

type WiredField =
  | "maxTokens"
  | "maxIterations"
  | "maxSessions"
  | "temperature"
  | "contextWindow"
  | "shellApprovalThreshold"
  | "approvalTimeoutSecs"
  | "toolCompressionEnabled";

const WIRE_FIELD: Record<WiredField, string> = {
  maxTokens: "max_output_tokens",
  maxIterations: "max_iterations",
  maxSessions: "max_sessions",
  temperature: "temperature",
  contextWindow: "context_window",
  shellApprovalThreshold: "shell_approval_threshold",
  approvalTimeoutSecs: "approval_timeout_secs",
  toolCompressionEnabled: "tool_compression_enabled",
};

const FIELD_DEBOUNCE_MS = 500;

const DEBOUNCE_BY_FIELD: Record<WiredField, number> = {
  toolCompressionEnabled: FIELD_DEBOUNCE_MS,
  shellApprovalThreshold: FIELD_DEBOUNCE_MS,
  temperature: FIELD_DEBOUNCE_MS,
  contextWindow: FIELD_DEBOUNCE_MS,
  approvalTimeoutSecs: FIELD_DEBOUNCE_MS,
  maxTokens: FIELD_DEBOUNCE_MS,
  maxIterations: FIELD_DEBOUNCE_MS,
  maxSessions: FIELD_DEBOUNCE_MS,
};

// ── Component ───────────────────────────────────────────────────────────

export function AgentSetupTab() {
  const { t } = useTranslation();
  const { agents, selectedAgentId, fetchAgents } = useAgentStore();
  const { getProfile, setProfile, resetProfile } = useAgentStore();

  const storage = selectedAgentId ? agents[selectedAgentId] : null;
  const selectedAgent = storage?.meta ?? null;
  const profile = selectedAgentId ? getProfile(selectedAgentId) : null;

  // Fetch agent runtime config from Gateway API on mount
  const [_configLoading, setConfigLoading] = useState(false);
  const [showResetConfirm, setShowResetConfirm] = useState(false);

  // ADR-052 follow-up: Setup panel is now write-through. Each input
  // change optimistically updates `profile` via `setProfile`, and a
  // debounced PUT sends the change to the runtime. Per-field debounce
  // windows are configured in `DEBOUNCE_BY_FIELD` (see file top).
  //   - `savingFields` lets the status line render an in-flight indicator.
  //   - `debounceTimersRef` holds pending timers keyed by field name, so
  //     rapid keystrokes ("1" → "12" → "123") collapse to one PUT.
  const [savingFields, setSavingFields] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const debounceTimersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(
    new Map(),
  );

  // Avatar picker state (ADR-017)
  const [avatarTab, setAvatarTab] = useState<"custom" | "builtin">("custom");
  const [avatarPopupOpen, setAvatarPopupOpen] = useState(false);
  const [avatarAssets, setAvatarAssets] = useState<AvatarAssetEntry[]>([]);
  const [avatarConfig, setAvatarConfig] = useState<AvatarConfigResponse | null>(null);
  const [avatarBusy, setAvatarBusy] = useState(false);

  // Load avatar config + assets on mount and agent switch
  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;
    setAvatarAssets([]);
    setAvatarConfig(null);

    fetchAvatarConfig(selectedAgentId)
      .then((cfg) => { if (!cancelled) setAvatarConfig(cfg); })
      .catch((err) => { if (!cancelled) log.debug("[AgentSetup] Avatar config fetch failed:", err); });

    fetchAvatarAssets(selectedAgentId)
      .then((resp) => { if (!cancelled) setAvatarAssets(resp.assets); })
      .catch((err) => { if (!cancelled) log.debug("[AgentSetup] Avatar assets fetch failed:", err); });

    return () => { cancelled = true; };
  }, [selectedAgentId]);

  // Fetch agent runtime config on mount
  useEffect(() => {
    if (!selectedAgentId) return;
    let cancelled = false;
    setConfigLoading(true);
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/config`)
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (cancelled || !data) return;
        // Runtime's `GET /agents/{id}/config` returns a nested envelope
        // `{ agent_id, matches, config: <AgentConfig>, manifest_path, work_dir }`
        // (see acowork-runtime/src/http/server.rs::get_agent_config). The
        // Setup panel cares about the **flattened** config fields, so we
        // unwrap `data.config` here. Tolerating a missing/empty `config`
        // (e.g. agent_id mismatch) keeps the rest of the panel usable —
        // it just renders the localStorage fallback values.
        const cfg = (data.config ?? {}) as {
          max_output_tokens?: number | null;
          max_iterations?: number | null;
          max_sessions?: number | null;
          temperature?: number | null;
          context_window?: number | null;
          shell_approval_threshold?: string | null;
          approval_timeout_secs?: number | null;
          tool_compression_enabled?: boolean | null;
        };
        // Race-safe merge (ADR-052 follow-up): only overwrite each
        // local profile field when the server returned a concrete
        // value. All `AgentConfig` fields use `skip_serializing_if =
        // "Option::is_none"` (acowork-runtime/src/agent_config.rs), so
        // the runtime response represents "no opinion" as a missing
        // or JSON-null field. The old `?? undefined` / `?? 300` shape
        // would silently clobber a value the user typed into a number
        // input or clicked on the Switch before this GET returned,
        // dropping their intent before the next Apply could persist
        // it (this is the wider pattern behind the original
        // tool_compression_enabled bug — see `saveField` below).
        const patch: Partial<typeof profile> = {
          // `global_max_output_tokens` lives on the Gateway
          // AgentConfigResponse (not on Runtime AgentConfig), so the
          // proxy response doesn't carry it. We leave the previously
          // cached value alone if it's already set; the panel reads
          // `globalMaxTokens` as a "fallback limit" hint.
          activeModel: data.model,
          activeProvider: data.provider,
        };
        if (typeof cfg.max_output_tokens === "number") {
          patch.maxTokens = cfg.max_output_tokens;
        }
        if (typeof cfg.max_iterations === "number") {
          patch.maxIterations = cfg.max_iterations;
        }
        if (typeof cfg.max_sessions === "number") {
          patch.maxSessions = cfg.max_sessions;
        }
        if (typeof cfg.temperature === "number") {
          patch.temperature = cfg.temperature;
        }
        if (typeof cfg.context_window === "number") {
          patch.contextWindow = cfg.context_window;
        }
        if (typeof cfg.shell_approval_threshold === "string") {
          patch.shellApprovalThreshold = cfg.shell_approval_threshold;
        }
        if (typeof cfg.approval_timeout_secs === "number") {
          patch.approvalTimeoutSecs = cfg.approval_timeout_secs;
        }
        if (typeof cfg.tool_compression_enabled === "boolean") {
          patch.toolCompressionEnabled = cfg.tool_compression_enabled;
        }
        setProfile(selectedAgentId, patch);
      })
      .catch((err) => {
        if (!cancelled) log.debug("[AgentSetup] Agent not ready:", err);
      })
      .finally(() => { if (!cancelled) setConfigLoading(false); });
    return () => { cancelled = true; };
  }, [selectedAgentId]);

  // Listen for global resource refresh events
  useEffect(() => {
    if (!selectedAgentId) return;
    const handler = (e: Event) => {
      const ce = e as CustomEvent<{ agentId: string }>;
      if (ce.detail?.agentId === selectedAgentId) {
        fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/config`)
          .then((res) => (res.ok ? res.json() : null))
          .then((data) => {
            if (!data) return;
            // Same nested-envelope unwrap as the mount effect above
            // (see comment there for the rationale). Keep the two paths
            // in lockstep so a refresh event doesn't silently lose the
            // flattening logic.
            const cfg = (data.config ?? {}) as {
              max_output_tokens?: number | null;
              max_iterations?: number | null;
              max_sessions?: number | null;
              temperature?: number | null;
              context_window?: number | null;
              shell_approval_threshold?: string | null;
              approval_timeout_secs?: number | null;
              tool_compression_enabled?: boolean | null;
            };
            // Same race-safe merge as the mount effect above. The
            // retained-MQTT snapshot fires this refresh *after* every
            // successful PUT, so the clobber-to-undefined path was the
            // exact mechanism that erased a user's Switch click before
            // the click even made it into the PUT body. The new
            // write-through `saveField` (below) makes this race
            // window much narrower — a user edit lands in the local
            // store and is sent to the server *before* the
            // refresh handler can observe the response — but we keep
            // the typeof-guard as belt-and-suspenders.
            const patch: Partial<typeof profile> = {
              activeModel: data.model,
              activeProvider: data.provider,
            };
            if (typeof cfg.max_output_tokens === "number") {
              patch.maxTokens = cfg.max_output_tokens;
            }
            if (typeof cfg.max_iterations === "number") {
              patch.maxIterations = cfg.max_iterations;
            }
            if (typeof cfg.max_sessions === "number") {
              patch.maxSessions = cfg.max_sessions;
            }
            if (typeof cfg.temperature === "number") {
              patch.temperature = cfg.temperature;
            }
            if (typeof cfg.context_window === "number") {
              patch.contextWindow = cfg.context_window;
            }
            if (typeof cfg.shell_approval_threshold === "string") {
              patch.shellApprovalThreshold = cfg.shell_approval_threshold;
            }
            if (typeof cfg.approval_timeout_secs === "number") {
              patch.approvalTimeoutSecs = cfg.approval_timeout_secs;
            }
            if (typeof cfg.tool_compression_enabled === "boolean") {
              patch.toolCompressionEnabled = cfg.tool_compression_enabled;
            }
            setProfile(selectedAgentId, patch);
          })
          .catch(() => { });
      }
    };
    window.addEventListener('acowork:refresh-agent-config', handler);
    return () => window.removeEventListener('acowork:refresh-agent-config', handler);
  }, [selectedAgentId]);

  // ── Write-through persistence (ADR-052 follow-up) ──────────────────
  //
  // Replaces the old batched `handleApply` / "Apply" button. Every field
  // mutation now flows through `saveField`:
  //
  //   1. Optimistic local update (`setProfile`) so the UI reflects the
  //      change instantly.
  //   2. Schedule a PUT to `/api/agents/{id}/config` carrying only the
  //      changed field. Debounced per field type (see
  //      `DEBOUNCE_BY_FIELD`), so "1" → "12" → "123" collapses to one PUT.
  //   3. The runtime's `UpdateAgentConfigRequest` is a partial-PUT DTO
  //      (each field `Option<serde_json::Value>`, missing = leave alone),
  //      so per-field PUTs compose cleanly and never clobber unrelated
  //      on-disk values.
  //
  // The race window that used to lose a Switch click (load effect's
  // `?? undefined` overwriting the user's intent before Apply could
  // persist it) is now closed two ways:
  //   - local updates land before the next debounced PUT fires, so the
  //     PUT body always carries the user's intent.
  //   - the load effect / refresh handler only overwrite fields the
  //     server actually returned (typeof-guard, see above).

  const putField = useCallback(
    async (field: WiredField, value: unknown) => {
      if (!selectedAgentId) return;
      const body: Record<string, unknown> = { [WIRE_FIELD[field]]: value };
      try {
        setSavingFields((prev) => {
          if (prev.has(field)) return prev;
          const next = new Set(prev);
          next.add(field);
          return next;
        });
        const res = await fetch(
          `${getGatewayUrl()}/api/agents/${selectedAgentId}/config`,
          {
            method: "PUT",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(body),
          },
        );
        if (!res.ok) {
          log.warn("[AgentSetup] Field save failed:", field, res.status);
        }
      } catch (err) {
        log.warn("[AgentSetup] Field save error:", field, err);
      } finally {
        setSavingFields((prev) => {
          if (!prev.has(field)) return prev;
          const next = new Set(prev);
          next.delete(field);
          return next;
        });
      }
    },
    [selectedAgentId],
  );

  const saveField = useCallback(
    (field: WiredField, value: unknown) => {
      if (!selectedAgentId) return;
      // Step 1: optimistic local update.
      setProfile(selectedAgentId, { [field]: value } as Partial<AgentProfileSettings>);

      // Step 2: schedule PUT, debounced per field type.
      const debounceMs = DEBOUNCE_BY_FIELD[field];
      const existing = debounceTimersRef.current.get(field);
      if (existing) clearTimeout(existing);

      if (debounceMs <= 0) {
        debounceTimersRef.current.delete(field);
        void putField(field, value);
        return;
      }
      const timer = setTimeout(() => {
        debounceTimersRef.current.delete(field);
        void putField(field, value);
      }, debounceMs);
      debounceTimersRef.current.set(field, timer);
    },
    [selectedAgentId, setProfile, putField],
  );

  // Flush any pending debounced PUTs when the agent changes or the
  // panel unmounts. Otherwise a fast user who switches tabs mid-typing
  // would silently lose the last `debounceMs`-worth of edits.
  useEffect(() => {
    return () => {
      const timers = debounceTimersRef.current;
      timers.forEach((timer) => clearTimeout(timer));
      timers.clear();
    };
  }, [selectedAgentId]);

  // Sync the new temperature into the chat store so the ResultsPanel
  // status tab shows the updated value right away, without waiting for
  // the next WebSocket session_state event (which may be delayed if the
  // agent is mid-streaming). Lifted out of the old `handleApply` and
  // driven directly off `profile.temperature` so write-through updates
  // trigger it too.
  useEffect(() => {
    if (!selectedAgentId) return;
    const newTemp = profile?.temperature ?? null;
    const chatStore = useChatStore.getState();
    const agentState = chatStore.agentStates[selectedAgentId];
    if (!agentState?.activeSessionId) return;
    const sessionState =
      agentState.sessionStates[agentState.activeSessionId] ?? {};
    if (sessionState.temperature === newTemp) return;
    useChatStore.setState({
      agentStates: {
        ...chatStore.agentStates,
        [selectedAgentId]: {
          ...agentState,
          sessionStates: {
            ...agentState.sessionStates,
            [agentState.activeSessionId]: {
              ...sessionState,
              temperature: newTemp,
            },
          },
        },
      },
    });
  }, [selectedAgentId, profile?.temperature]);

  // ── Avatar selection handlers ──────────────────────────────────────

  const handleSelectCustom = async (relativePath: string) => {
    if (!selectedAgentId) return;
    setAvatarBusy(true);
    try {
      const cfg = await updateAvatarConfig(selectedAgentId, { avatar: relativePath, builtin_avatar: "" });
      setAvatarConfig(cfg);
      clearAgentAvatarCache(selectedAgentId);
      await fetchAgents();
    } catch (err) {
      log.warn("[AgentSetup] Select custom avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  const handleSelectBuiltin = async (iconId: string) => {
    if (!selectedAgentId) return;
    setAvatarBusy(true);
    try {
      const cfg = await updateAvatarConfig(selectedAgentId, { avatar: "", builtin_avatar: iconId });
      setAvatarConfig(cfg);
      clearAgentAvatarCache(selectedAgentId);
      await fetchAgents();
    } catch (err) {
      log.warn("[AgentSetup] Select builtin avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  // ── Avatar upload (does NOT auto-select) ──────────────────────────

  const handleUploadClick = async () => {
    if (!selectedAgentId) return;
    const selected = await openDialog({
      multiple: false,
      filters: [{ name: "Images", extensions: IMAGE_EXTENSIONS }],
    });
    if (!selected || typeof selected !== "string") return;

    const ext = selected.split(".").pop()?.toLowerCase() ?? "png";
    if (!IMAGE_EXTENSIONS.includes(ext)) return;

    const relative = `assets/${nextAvatarName(avatarAssets, ext)}`;
    setAvatarBusy(true);
    try {
      await invoke("upload_agent_file", {
        agentId: selectedAgentId,
        relativePath: relative,
        filePath: selected,
      });
      // Refresh assets list — user manually selects afterwards
      const resp = await fetchAvatarAssets(selectedAgentId);
      setAvatarAssets(resp.assets);
    } catch (err) {
      log.warn("[AgentSetup] Avatar upload failed:", err);
    } finally {
      setAvatarBusy(false);
    }
  };

  // ── Avatar delete ──────────────────────────────────────────────────

  const handleDeleteAvatar = async (relativePath: string) => {
    if (!selectedAgentId) return;
    setAvatarBusy(true);
    try {
      await deleteAvatarFile(selectedAgentId, relativePath);
      // Refresh both — backend clears avatar field if deleted file was current
      const [assetsResp, cfg] = await Promise.all([
        fetchAvatarAssets(selectedAgentId),
        fetchAvatarConfig(selectedAgentId),
      ]);
      setAvatarAssets(assetsResp.assets);
      setAvatarConfig(cfg);
      clearAgentAvatarCache(selectedAgentId);
      await fetchAgents();
    } catch (err) {
      log.warn("[AgentSetup] Delete avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  if (!selectedAgentId || !selectedAgent || !profile) {
    return (
      <div className="flex flex-1 items-center justify-center p-6">
        <span className="text-xs text-zinc-400 dark:text-zinc-500">{t("agentSetup.noAgentSelected")}</span>
      </div>
    );
  }

  const agentName = profile.displayName ?? selectedAgent.name ?? selectedAgentId;

  return (
    <div className="flex-1 overflow-y-auto p-3">
      {/* Avatar preview — click to open picker popup */}
      <div className="mb-3 flex items-center gap-3">
        <div className="relative">
          <button
            onClick={() => setAvatarPopupOpen((v) => !v)}
            className="relative block rounded-full ring-1 ring-zinc-300/60 transition hover:ring-zinc-400 dark:ring-zinc-600/60 dark:hover:ring-zinc-400"
          >
            <AgentAvatar
              agentId={selectedAgentId}
              avatarUrl={avatarConfig?.avatar ?? null}
              builtinAvatarId={avatarConfig?.builtin_avatar ?? null}
              version={selectedAgent.version}
              size={64}
            />
            {/* Pencil badge */}
            <span className="absolute -bottom-0.5 -right-0.5 flex h-5 w-5 items-center justify-center rounded-full bg-zinc-800 text-white shadow-sm dark:bg-zinc-600">
              <svg viewBox="0 0 16 16" className="h-3 w-3 fill-current" xmlns="http://www.w3.org/2000/svg">
                <path d="M11.013 1.427a1.75 1.75 0 0 1 2.474 0l1.086 1.086a1.75 1.75 0 0 1 0 2.474l-8.61 8.61c-.21.21-.47.364-.756.445l-3.251.93a.75.75 0 0 1-.927-.928l.929-3.25c.081-.286.235-.547.445-.758l8.61-8.61Zm.176 4.823L11.5 7l-3-3-.31.31a.75.75 0 0 0-.177.764l.93 3.251a.75.75 0 0 1-.927.928l-3.251-.93Z" />
              </svg>
            </span>
          </button>

          {/* Avatar picker popup */}
          {avatarPopupOpen && (
            <>
              {/* Click-outside overlay */}
              <div
                className="fixed inset-0 z-40"
                onClick={() => setAvatarPopupOpen(false)}
              />
              <div className="absolute left-0 top-full z-50 mt-2 w-72 rounded-lg border border-zinc-200 bg-modal-surface p-3 shadow-lg dark:border-zinc-700">
                {/* Tabs */}
                <div className="mb-3 flex gap-1 border-b border-zinc-200 dark:border-zinc-700">
                  <button
                    onClick={() => setAvatarTab("custom")}
                    className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "custom"
                      ? "border-b-2 border-zinc-800 text-zinc-800 dark:border-zinc-200 dark:text-zinc-200"
                      : "text-zinc-400 hover:text-zinc-600 dark:text-zinc-500"
                      }`}
                  >
                    Custom
                  </button>
                  <button
                    onClick={() => setAvatarTab("builtin")}
                    className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "builtin"
                      ? "border-b-2 border-zinc-800 text-zinc-800 dark:border-zinc-200 dark:text-zinc-200"
                      : "text-zinc-400 hover:text-zinc-600 dark:text-zinc-500"
                      }`}
                  >
                    Builtin
                  </button>
                </div>

                {/* Custom tab */}
                {avatarTab === "custom" && (
                  <div className="grid grid-cols-4 gap-2">
                    <button
                      onClick={handleUploadClick}
                      disabled={avatarBusy}
                      className="flex aspect-square items-center justify-center rounded-md border border-dashed border-zinc-300 text-zinc-400 transition-colors hover:border-zinc-400 hover:text-zinc-600 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-500 dark:hover:border-zinc-400"
                    >
                      <span className="text-lg">+</span>
                    </button>
                    {avatarAssets.map((asset) => {
                      const isSelected = avatarConfig?.avatar === asset.relative_path;
                      return (
                        <div
                          key={asset.relative_path}
                          className={`group relative aspect-square overflow-hidden rounded-md border-2 transition-colors ${isSelected
                            ? "border-zinc-800 dark:border-zinc-200"
                            : "border-transparent hover:border-zinc-300 dark:hover:border-zinc-600"
                            }`}
                        >
                          <img
                            src={resolveAgentAvatarFileUrl(selectedAgentId, asset.relative_path)}
                            alt={asset.relative_path}
                            draggable={false}
                            className="h-full w-full cursor-pointer object-cover"
                            onClick={() => handleSelectCustom(asset.relative_path)}
                          />
                          <button
                            onClick={(e) => {
                              e.stopPropagation();
                              handleDeleteAvatar(asset.relative_path);
                            }}
                            disabled={avatarBusy}
                            className="absolute right-0.5 top-0.5 flex h-4 w-4 items-center justify-center rounded bg-red-500/80 text-[8px] text-white opacity-0 transition-opacity group-hover:opacity-100"
                          >
                            ×
                          </button>
                        </div>
                      );
                    })}
                  </div>
                )}

                {/* Builtin tab */}
                {avatarTab === "builtin" && (
                  <div className="grid grid-cols-4 gap-2">
                    {BUILTIN_ICON_IDS.map((iconId) => {
                      const isSelected = avatarConfig?.builtin_avatar === iconId;
                      return (
                        <button
                          key={iconId}
                          onClick={() => handleSelectBuiltin(iconId)}
                          disabled={avatarBusy}
                          className={`flex items-center justify-center rounded-md p-1 transition-colors ${isSelected
                            ? "bg-zinc-200 dark:bg-zinc-600"
                            : "hover:bg-zinc-100 dark:hover:bg-zinc-700"
                            }`}
                        >
                          <img
                            src={BUILTIN_ICONS[iconId] ?? ""}
                            alt={iconId}
                            draggable={false}
                            className="h-12 w-12 rounded-full object-cover"
                          />
                        </button>
                      );
                    })}
                  </div>
                )}
              </div>
            </>
          )}
        </div>
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium text-zinc-800 dark:text-zinc-200">
            {agentName}
          </p>
          <p className="truncate text-[10px] text-zinc-400 dark:text-zinc-500">
            {selectedAgentId}
          </p>
        </div>
      </div>

      {/* Agent Name */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.agentName")}
        </label>
        <StyledInput
          type="text"
          value={profile.displayName ?? selectedAgent.name ?? ""}
          onChange={(e) =>
            setProfile(selectedAgentId, { displayName: e.target.value || undefined })
          }
          placeholder={selectedAgent.name ?? "Agent name"}
          className="rounded-md bg-modal-surface"
        />
      </div>

      {/* Max Output Tokens */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.maxOutputTokens")}
        </label>
        <StyledInput
          type="number"
          min={0}
          max={131072}
          step={1024}
          value={profile.maxTokens && profile.maxTokens > 0 ? profile.maxTokens : ""}
          onChange={(e) => {
            const v = e.target.value;
            // Empty input → omit the field on the wire (don't clobber
            // the on-disk value). 0 / non-numeric collapses to 0 by
            // input convention, which is also omitted by the
            // `> 0` gate in the old handleApply and matches
            // the pre-write-through behavior.
            saveField(
              "maxTokens",
              v === "" ? undefined : Math.max(0, parseInt(v, 10) || 0),
            );
          }}
          placeholder={`${profile.globalMaxTokens ?? 32768} ${t("agentSetup.defaultModelLimit")}`}
          className="rounded-md bg-modal-surface"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.leaveEmptyDefault")}
        </p>
      </div>

      {/* Max Iterations */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.maxIterations")}
        </label>
        <StyledInput
          type="number"
          min={0}
          max={200}
          value={profile.maxIterations && profile.maxIterations > 0 ? profile.maxIterations : ""}
          onChange={(e) => {
            const v = e.target.value;
            saveField(
              "maxIterations",
              v === "" ? undefined : Math.max(0, parseInt(v, 10) || 0),
            );
          }}
          placeholder={t("agentSetup.defaultIterations")}
          className="rounded-md bg-modal-surface"
        />
      </div>

      {/* Max Sessions (ADR-024) */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.maxSessions")}
        </label>
        <StyledInput
          type="number"
          min={0}
          max={10000}
          value={profile.maxSessions && profile.maxSessions > 0 ? profile.maxSessions : ""}
          onChange={(e) => {
            const v = e.target.value;
            saveField(
              "maxSessions",
              v === "" ? undefined : Math.max(0, parseInt(v, 10) || 0),
            );
          }}
          placeholder="2000 (default)"
          className="rounded-md bg-modal-surface"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.maxSessionsDesc")}
        </p>
      </div>

      {/* Context Window */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.contextWindow")}
        </label>
        <div className="flex items-center gap-3">
          <StyledInput
            type="number"
            min={0}
            max={1000000}
            step={1000}
            value={profile.contextWindow ?? ""}
            placeholder={t("agentSetup.contextWindowPlaceholder")}
            onChange={(e) => {
              const raw = e.target.value;
              if (raw === "" || raw === "0") {
                // 0 = no limit (use model's full window)
                saveField("contextWindow", 0);
              } else {
                const n = parseInt(raw, 10);
                if (!isNaN(n) && n >= 0) {
                  saveField("contextWindow", n);
                }
              }
            }}
            className="w-32 rounded-md bg-modal-surface"
          />
          <span className="text-[10px] text-zinc-400 dark:text-zinc-500">
            {t("agentSetup.tokens")}
          </span>
        </div>
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.contextWindowDesc")}
        </p>
      </div>

      {/* Tool Compression (ADR-052) */}
      <div className="mb-3 space-y-1">
        <Switch
          checked={profile.toolCompressionEnabled ?? true}
          onChange={(checked) => saveField("toolCompressionEnabled", checked)}
          size="sm"
          label={t("agentSetup.toolCompressionEnabled")}
          className="text-[10px] font-medium text-zinc-500 dark:text-zinc-400"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.toolCompressionEnabledDesc")}
        </p>
      </div>


      {/* Approval Timeout */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.approvalTimeout")}
        </label>
        <StyledInput
          type="number"
          min={0}
          max={3600}
          step={30}
          value={profile.approvalTimeoutSecs && profile.approvalTimeoutSecs > 0 ? profile.approvalTimeoutSecs : ""}
          onChange={(e) => {
            const v = e.target.value;
            saveField(
              "approvalTimeoutSecs",
              v === "" ? undefined : Math.max(0, parseInt(v, 10) || 0),
            );
          }}
          placeholder="300 (5 min)"
          className="rounded-md bg-modal-surface"
        />
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.approvalTimeoutDesc")}
        </p>
      </div>

      {/* Shell Command Approval Threshold */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.shellCommandApproval")}
        </label>
        <select
          value={profile.shellApprovalThreshold ?? "medium"}
          onChange={(e) => {
            const v = e.target.value;
            saveField("shellApprovalThreshold", v);
          }}
          className="w-full appearance-none rounded border border-zinc-200 bg-modal-surface px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:text-zinc-200"
          style={{
            backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
            backgroundPosition: 'right 0.5rem center',
            backgroundRepeat: 'no-repeat',
            backgroundSize: '1.5em 1.5em',
          }}
        >
          <option value="medium">{t("agentSetup.approvalMedium")}</option>
          <option value="low">{t("agentSetup.approvalLow")}</option>
          <option value="high">{t("agentSetup.approvalHigh")}</option>
          <option value="never">{t("agentSetup.approvalNever")}</option>
        </select>
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.approvalDesc")}
        </p>
      </div>

      {/* Temperature slider */}
      <div className="mb-3 space-y-1">
        <label className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
          {t("agentSetup.temperature")}
        </label>
        <div className="flex items-center gap-3">
          <input
            type="range"
            min={0}
            max={2}
            step={0.05}
            value={profile.temperature ?? 0.3}
            onChange={(e) => {
              saveField("temperature", parseFloat(e.target.value));
            }}
            className="flex-1 h-1.5 rounded-full appearance-none cursor-pointer
              bg-zinc-200 dark:bg-zinc-700
              accent-zinc-600 dark:accent-zinc-400"
          />
          <span className="w-10 text-right text-xs text-zinc-600 dark:text-zinc-400 tabular-nums">
            {profile.temperature !== undefined ? profile.temperature.toFixed(2) : "—"}
          </span>
        </div>
        <p className="text-[9px] text-zinc-400 dark:text-zinc-500">
          {t("agentSetup.temperatureDesc")}
        </p>
      </div>

      {/* Footer: saving indicator + reset (ADR-052 follow-up) */}
      <div className="mt-4 border-t border-zinc-200 pt-3 dark:border-zinc-700 flex items-center gap-3">
        <span
          className="flex-1 text-[10px] tabular-nums text-zinc-400 dark:text-zinc-500"
          aria-live="polite"
        >
          {savingFields.size > 0
            ? t("agentSetup.savingChanges", { count: savingFields.size })
            : t("agentSetup.allChangesSaved")}
        </span>
        <button
          onClick={() => setShowResetConfirm(true)}
          className="rounded btn-solid px-3 py-1.5 text-xs font-medium"
        >
          {t("agentSetup.resetToDefaults")}
        </button>
      </div>

      <ConfirmDialog
        open={showResetConfirm}
        title={t("agentSetup.resetAgentSetup")}
        message={t("agentSetup.resetConfirm")}
        confirmLabel={t("agentSetup.reset")}
        destructive
        onConfirm={async () => {
          setShowResetConfirm(false);
          // Cancel any in-flight debounced PUTs so they don't overwrite
          // the clear with the pre-reset value.
          const timers = debounceTimersRef.current;
          timers.forEach((timer) => clearTimeout(timer));
          timers.clear();
          // Clear local state first so the UI updates immediately.
          resetProfile(selectedAgentId);
          if (!selectedAgentId) return;
          // Push all wired fields as JSON-null → runtime maps null to
          // `FieldPatch::Clear` for each, which drops the on-disk
          // entry (`skip_serializing_if = Option::is_none`). Next
          // boot falls through to the runtime defaults:
          //   max_output_tokens → manifest / global default
          //   max_iterations    → global default
          //   max_sessions      → 1000
          //   temperature       → 0.3
          //   context_window    → 200K
          //   shell_approval_threshold → manifest default
          //   approval_timeout_secs     → 300
          //   tool_compression_enabled  → true (agent_init.rs)
          try {
            setSavingFields((prev) => {
              const next = new Set(prev);
              next.add("__reset__");
              return next;
            });
            const res = await fetch(
              `${getGatewayUrl()}/api/agents/${selectedAgentId}/config`,
              {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({
                  max_output_tokens: null,
                  max_iterations: null,
                  max_sessions: null,
                  temperature: null,
                  context_window: null,
                  shell_approval_threshold: null,
                  approval_timeout_secs: null,
                  tool_compression_enabled: null,
                }),
              },
            );
            if (!res.ok) {
              log.warn("[AgentSetup] Reset PUT failed:", res.status);
            }
          } catch (err) {
            log.warn("[AgentSetup] Reset PUT error:", err);
          } finally {
            setSavingFields((prev) => {
              const next = new Set(prev);
              next.delete("__reset__");
              return next;
            });
          }
        }}
        onCancel={() => setShowResetConfirm(false)}
      />
    </div>
  );
}

