import { create } from "zustand";
import type { AvatarType, BoringAvatarVariant, ColorPalette, UserProfile } from "../lib/types";
import { COLOR_PALETTES } from "../lib/types";

const STORAGE_KEY = "rollball-user-profile";

// ── Defaults ───────────────────────────────────────────────────────────

const DEFAULT_PROFILE: UserProfile = {
  displayName: "我",
  avatarType: "icon",
  avatarVariant: "beam",
  avatarSeed: "user",
  avatarIcon: null,
  colorPalette: "rainbow",
  avatarColors: [],
};

// ── Persistence helpers ────────────────────────────────────────────────

function loadProfile(): UserProfile {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Partial<UserProfile>;
      return {
        displayName: parsed.displayName ?? DEFAULT_PROFILE.displayName,
        avatarType: validateAvatarType(parsed.avatarType),
        avatarVariant: validateVariant(parsed.avatarVariant),
        avatarSeed: parsed.avatarSeed ?? DEFAULT_PROFILE.avatarSeed,
        avatarIcon: parsed.avatarIcon ?? DEFAULT_PROFILE.avatarIcon,
        colorPalette: validatePalette(parsed.colorPalette),
        avatarColors: Array.isArray(parsed.avatarColors) ? parsed.avatarColors : [],
      };
    }
  } catch {
    // localStorage unavailable or corrupted; use defaults
  }
  return { ...DEFAULT_PROFILE };
}

function saveProfile(profile: UserProfile) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(profile));
  } catch {
    // silently ignore persistence failures
  }
}

function validateAvatarType(v?: unknown): AvatarType {
  if (v === "boring" || v === "icon" || v === "letter") return v;
  return "icon";
}

function validateVariant(v?: unknown): BoringAvatarVariant {
  const valid = ["beam", "marble", "pixel", "sunset", "ring", "bauhaus"];
  if (typeof v === "string" && valid.includes(v)) return v as BoringAvatarVariant;
  return "beam";
}

function validatePalette(v?: unknown): ColorPalette {
  const valid: ColorPalette[] = ["rainbow", "ocean", "forest", "sunset", "neon"];
  if (typeof v === "string" && valid.includes(v as ColorPalette)) return v as ColorPalette;
  return "rainbow";
}

// ── Store ──────────────────────────────────────────────────────────────

interface UserProfileState {
  profile: UserProfile;
  /** Update profile partially and persist */
  setProfile: (partial: Partial<UserProfile>) => void;
  /** Get effective colors (custom or palette default) */
  getColors: () => string[];
  /** Reset to defaults */
  resetProfile: () => void;
}

export const useUserProfileStore = create<UserProfileState>((set, get) => ({
  profile: loadProfile(),

  setProfile: (partial) => {
    const next = { ...get().profile, ...partial };
    saveProfile(next);
    set({ profile: next });
  },

  getColors: () => {
    const p = get().profile;
    if (p.avatarColors.length > 0) return p.avatarColors;
    return COLOR_PALETTES[p.colorPalette] ?? COLOR_PALETTES.rainbow;
  },

  resetProfile: () => {
    const next = { ...DEFAULT_PROFILE };
    saveProfile(next);
    set({ profile: next });
  },
}));
