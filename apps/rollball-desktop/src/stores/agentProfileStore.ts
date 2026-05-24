import { create } from "zustand";
import { BUILTIN_ICON_IDS } from "../components/common/UserAvatar";

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
  /** Active tool names (from manifest + overrides) */
  activeTools?: string[];
  /** Shell approval threshold: "low" | "medium" | "high" | "never" */
  shellApprovalThreshold?: string;
  /** Gateway global max_output_tokens limit (informational, from ConfigSnapshot) */
  globalMaxTokens?: number;
  /** Available model names from Gateway (informational, from ConfigSnapshot) */
  availableModels?: string[];
  /** Current active model name (from ConfigSnapshot) */
  activeModel?: string;
  /** Current active provider name (from ConfigSnapshot) */
  activeProvider?: string;
}

const STORAGE_KEY = "rollball-agent-profiles";

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
  activeTools: undefined,
};

// ── Store ──────────────────────────────────────────────────────────────

interface AgentProfileStore {
  profiles: Record<string, AgentProfileSettings>;

  getProfile: (agentId: string) => AgentProfileSettings;
  setProfile: (agentId: string, settings: Partial<AgentProfileSettings>) => void;
  resetProfile: (agentId: string) => void;
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
          activeTools: Array.isArray(settings.activeTools) ? settings.activeTools : undefined,
          shellApprovalThreshold: settings.shellApprovalThreshold,
          globalMaxTokens: typeof settings.globalMaxTokens === "number" ? settings.globalMaxTokens : undefined,
          availableModels: Array.isArray(settings.availableModels) ? settings.availableModels : undefined,
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
}));
