import { create } from "zustand";
import { BUILTIN_ICON_IDS } from "../components/common/UserAvatar";
import { normalizeBuiltinAvatarId, pickRandomBuiltinIconId } from "../lib/avatar";

// ── Types ──────────────────────────────────────────────────────────────

export interface AgentProfileSettings {
  /** Custom display name shown in chat bubbles */
  displayName?: string;
  /** Built-in icon ID (e.g. "icon-02"), null = use geometric avatar */
  avatarIconId?: string | null;
  /** Override model ID for this agent */
  modelId?: string;
  /** Override provider ID for this agent */
  providerId?: string;
  /** Max output tokens (0 = use global default) */
  maxTokens?: number;
  /** Max LLM iterations per run (0 = use global default) */
  maxIterations?: number;
  /** LLM temperature (0-2, step 0.1) */
  temperature?: number;
  /** System prompt override */
  systemPrompt?: string;
  /** Shell approval threshold: "low" | "medium" | "high" | "never" */
  shellApprovalThreshold?: string;
  /** Approval timeout in seconds (default 300 = 5 min). 0 = Gateway default. */
  approvalTimeoutSecs?: number;
  /** Gateway global max_output_tokens limit (informational, from ConfigSnapshot) */
  globalMaxTokens?: number;
  /** Current active model name (from ConfigSnapshot) */
  activeModel?: string;
  /** Current active provider name (from ConfigSnapshot) */
  activeProvider?: string;
}

const STORAGE_KEY = "acowork-agent-profiles";

// ── Defaults ───────────────────────────────────────────────────────────

const DEFAULT_SETTINGS: AgentProfileSettings = {
  displayName: undefined,
  avatarIconId: null,
  modelId: undefined,
  providerId: undefined,
  maxTokens: 0,
  maxIterations: 0,
  temperature: 0.7,
  systemPrompt: undefined,
  shellApprovalThreshold: undefined,
  approvalTimeoutSecs: undefined,
};

// ── Store ──────────────────────────────────────────────────────────────

interface AgentProfileStore {
  profiles: Record<string, AgentProfileSettings>;

  getProfile: (agentId: string) => AgentProfileSettings;
  setProfile: (agentId: string, settings: Partial<AgentProfileSettings>) => void;
  resetProfile: (agentId: string) => void;
  /**
   * Idempotently assign a builtin avatar to the agent if it doesn't already
   * have one. Called from the install hook so freshly installed agents show
   * a builtin icon instead of the legacy gradient fallback.
   *
   * Pick order:
   * 1. The manifest's `builtin_avatar` field (if present and valid) — gives
   *    the agent author control over the default icon at packaging time.
   * 2. A random builtin icon — last-resort fallback.
   *
   * Skipped when:
   * - the agent already has a profile iconId set (user has chosen one)
   * - `hasPackagedAvatar` is true (the agent ships its own icon.png/jpg)
   */
  assignRandomAvatarIfMissing: (
    agentId: string,
    hasPackagedAvatar?: boolean,
    builtinAvatarHint?: string | null,
  ) => void;
  /**
   * Bulk variant used after `fetchAgents` to backfill any installed agent
   * that somehow has no profile icon. Idempotent.
   *
   * Each entry may carry a `builtin_avatar` hint from the manifest.
   */
  ensureBuiltinAvatars: (
    agents: ReadonlyArray<{
      agent_id: string;
      avatar?: string | null;
      builtin_avatar?: string | null;
    }>,
  ) => void;
}

function loadProfiles(): Record<string, AgentProfileSettings> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Record<string, Partial<AgentProfileSettings>>;
      const result: Record<string, AgentProfileSettings> = {};
      for (const [agentId, settings] of Object.entries(parsed)) {
        result[agentId] = {
          displayName: settings.displayName,
          avatarIconId: validateIconId(settings.avatarIconId),
          modelId: settings.modelId,
          providerId: settings.providerId,
          maxTokens: typeof settings.maxTokens === "number" && settings.maxTokens > 0 ? settings.maxTokens : 0,
          maxIterations:
            typeof settings.maxIterations === "number" && settings.maxIterations > 0
              ? settings.maxIterations
              // Back-compat: migrate legacy `toolsLimit` field from older localStorage snapshots.
              : typeof (settings as { toolsLimit?: number }).toolsLimit === "number" &&
                (settings as { toolsLimit?: number }).toolsLimit! > 0
                ? (settings as { toolsLimit?: number }).toolsLimit!
                : 0,
          temperature: typeof settings.temperature === "number" ? settings.temperature : 0.7,
          systemPrompt: settings.systemPrompt,
          shellApprovalThreshold: settings.shellApprovalThreshold,
          approvalTimeoutSecs: typeof settings.approvalTimeoutSecs === "number" && settings.approvalTimeoutSecs > 0 ? settings.approvalTimeoutSecs : undefined,
          globalMaxTokens: typeof settings.globalMaxTokens === "number" ? settings.globalMaxTokens : undefined,
          activeModel: typeof settings.activeModel === "string" ? settings.activeModel : undefined,
          activeProvider: typeof settings.activeProvider === "string" ? settings.activeProvider : undefined,
        };
      }
      return result;
    }
  } catch {
    // localStorage unavailable or corrupted; use empty
  }
  return {};
}

function saveProfiles(profiles: Record<string, AgentProfileSettings>) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(profiles));
  } catch {
    // silently ignore persistence failures
  }
}

function validateIconId(id?: unknown): string | null | undefined {
  if (id === null || id === undefined) return id;
  if (typeof id === "string" && BUILTIN_ICON_IDS.includes(id)) return id;
  return null;
}

export const useAgentProfileStore = create<AgentProfileStore>((set, get) => ({
  profiles: loadProfiles(),

  getProfile: (agentId) => {
    const profiles = get().profiles;
    const stored = profiles[agentId];
    if (stored) return stored;
    return { ...DEFAULT_SETTINGS };
  },

  setProfile: (agentId, settings) => {
    set((state) => {
      const existing = state.profiles[agentId] ?? { ...DEFAULT_SETTINGS };
      const updated: Record<string, AgentProfileSettings> = {
        ...state.profiles,
        [agentId]: { ...existing, ...settings },
      };
      saveProfiles(updated);
      return { profiles: updated };
    });
  },

  resetProfile: (agentId) => {
    set((state) => {
      const updated = { ...state.profiles };
      delete updated[agentId];
      saveProfiles(updated);
      return { profiles: updated };
    });
  },

  assignRandomAvatarIfMissing: (agentId, hasPackagedAvatar, builtinAvatarHint) => {
    if (!agentId) return;
    if (hasPackagedAvatar) return;
    const state = get();
    const existing = state.profiles[agentId];
    if (existing && existing.avatarIconId) return;
    // Prefer the manifest's builtin_avatar hint; fall back to a random pick.
    const iconId = normalizeBuiltinAvatarId(builtinAvatarHint) ?? pickRandomBuiltinIconId();
    if (!iconId) return;
    set((s) => {
      const current = s.profiles[agentId] ?? { ...DEFAULT_SETTINGS };
      if (current.avatarIconId) return s; // someone else won the race
      const updated: Record<string, AgentProfileSettings> = {
        ...s.profiles,
        [agentId]: { ...current, avatarIconId: iconId },
      };
      saveProfiles(updated);
      return { profiles: updated };
    });
  },

  ensureBuiltinAvatars: (agents) => {
    if (!agents || agents.length === 0) return;
    const state = get();
    const updates: Record<string, AgentProfileSettings> = {};
    let dirty = false;
    for (const agent of agents) {
      const id = agent.agent_id;
      if (!id) continue;
      if (agent.avatar) continue; // has packaged avatar
      const current = state.profiles[id];
      if (current && current.avatarIconId) continue; // user already set one
      // Prefer the manifest's builtin_avatar hint; fall back to a random pick.
      const iconId = normalizeBuiltinAvatarId(agent.builtin_avatar) ?? pickRandomBuiltinIconId();
      if (!iconId) continue;
      updates[id] = { ...(current ?? { ...DEFAULT_SETTINGS }), avatarIconId: iconId };
      dirty = true;
    }
    if (!dirty) return;
    set((s) => {
      const next = { ...s.profiles, ...updates };
      saveProfiles(next);
      return { profiles: next };
    });
  },
}));
