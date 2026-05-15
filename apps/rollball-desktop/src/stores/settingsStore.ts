import { create } from "zustand";
import type { Theme } from "../lib/types";
import { DEFAULT_GATEWAY_URL } from "../lib/config";

const STORAGE_KEY_THEME = "rollball-theme";
const STORAGE_KEY_FONT_SIZE = "rollball-font-size";
const STORAGE_KEY_LOG_LEVEL = "rollball-log-level";
const STORAGE_KEY_CONTENT_WIDTH = "rollball-content-width";
const STORAGE_KEY_OPACITY = "rollball-opacity";
const STORAGE_KEY_ACCENT_COLOR = "rollball-accent-color";

const DEFAULT_ACCENT_COLOR = "#00C375";

/** Apply theme to DOM by toggling .dark class on <html> */
function applyTheme(theme: Theme) {
  if (theme === "dark") {
    document.documentElement.classList.add("dark");
  } else if (theme === "light") {
    document.documentElement.classList.remove("dark");
  } else {
    // "system" — follow OS preference
    const prefersDark = window.matchMedia("(prefers-color-scheme: dark)").matches;
    document.documentElement.classList.toggle("dark", prefersDark);
  }
}

/** Apply fontSize to CSS custom property on root */
function applyFontSize(size: number) {
  document.documentElement.style.setProperty("--ui-font-size", `${size}rem`);
}

/** Apply contentWidth to CSS custom property on root */
function applyContentWidth(width: number) {
  document.documentElement.style.setProperty("--content-max-width", `${width}%`);
}

/** Apply opacity to CSS custom property on root */
function applyOpacity(opacity: number) {
  document.documentElement.style.setProperty("--app-opacity", String(opacity));
}

/** Apply accent color to CSS custom property on root */
function applyAccentColor(color: string) {
  document.documentElement.style.setProperty("--color-accent-green", color);
}

/** Read persisted theme from localStorage, fallback to "system" */
function getPersistedTheme(): Theme {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_THEME);
    if (stored === "light" || stored === "dark" || stored === "system") return stored;
  } catch {
    // localStorage unavailable (SSR / privacy mode)
  }
  return "system";
}

/** Read persisted font size from localStorage, fallback to 0.875 (M) */
function getPersistedFontSize(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_FONT_SIZE);
    if (stored) {
      const val = parseFloat(stored);
      if (!isNaN(val) && val > 0) return val;
    }
  } catch {}
  return 0.875;
}

/** Read persisted log level from localStorage, fallback to "info" */
function getPersistedLogLevel(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_LOG_LEVEL);
    if (stored) return stored;
  } catch {}
  return "info";
}

/** Read persisted content width from localStorage, fallback to 90 */
function getPersistedContentWidth(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_CONTENT_WIDTH);
    if (stored) {
      const val = parseInt(stored, 10);
      if (!isNaN(val) && val >= 40 && val <= 100) return val;
    }
  } catch {}
  return 90;
}

/** Read persisted accent color from localStorage, fallback to #00C375 */
function getPersistedAccentColor(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_ACCENT_COLOR);
    if (stored && /^#[0-9a-fA-F]{6}$/.test(stored)) return stored;
  } catch {}
  return DEFAULT_ACCENT_COLOR;
}

/** Read persisted opacity from localStorage, fallback to 1.0 (opaque) */
function getPersistedOpacity(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_OPACITY);
    if (stored) {
      const val = parseFloat(stored);
      if (!isNaN(val) && val >= 0.0 && val <= 1.0) return val;
    }
  } catch {}
  return 1.0;
}

interface SettingsStore {
  theme: Theme;
  fontSize: number;
  contentWidth: number;
  opacity: number;
  accentColor: string;
  gatewayUrl: string;
  logLevel: string;
  setTheme: (theme: Theme) => void;
  setFontSize: (size: number) => void;
  setContentWidth: (width: number) => void;
  setOpacity: (opacity: number) => void;
  setAccentColor: (color: string) => void;
  setGatewayUrl: (url: string) => void;
  setLogLevel: (level: string) => void;
}

export const useSettingsStore = create<SettingsStore>((set) => {
  // Initialize from persisted values and apply theme to DOM immediately
  const initialTheme = getPersistedTheme();
  const initialFontSize = getPersistedFontSize();
  const initialLogLevel = getPersistedLogLevel();
  const initialOpacity = getPersistedOpacity();
  const initialContentWidth = getPersistedContentWidth();
  const initialAccentColor = getPersistedAccentColor();
  applyTheme(initialTheme);
  applyFontSize(initialFontSize);
  applyOpacity(initialOpacity);
  applyContentWidth(initialContentWidth);
  applyAccentColor(initialAccentColor);

  return {
    theme: initialTheme,
    fontSize: initialFontSize,
    contentWidth: initialContentWidth,
    opacity: initialOpacity,
    accentColor: initialAccentColor,
    gatewayUrl: DEFAULT_GATEWAY_URL,
    logLevel: initialLogLevel,

    setTheme: (theme) => {
      applyTheme(theme);
      try { localStorage.setItem(STORAGE_KEY_THEME, theme); } catch {}
      set({ theme });
    },

    setFontSize: (fontSize) => {
      applyFontSize(fontSize);
      try { localStorage.setItem(STORAGE_KEY_FONT_SIZE, String(fontSize)); } catch {}
      set({ fontSize });
    },

    setContentWidth: (contentWidth) => {
      applyContentWidth(contentWidth);
      try { localStorage.setItem(STORAGE_KEY_CONTENT_WIDTH, String(contentWidth)); } catch {}
      set({ contentWidth });
    },

    setOpacity: (opacity) => {
      applyOpacity(opacity);
      try { localStorage.setItem(STORAGE_KEY_OPACITY, String(opacity)); } catch {}
      set({ opacity });
    },

    setAccentColor: (accentColor) => {
      applyAccentColor(accentColor);
      try { localStorage.setItem(STORAGE_KEY_ACCENT_COLOR, accentColor); } catch {}
      set({ accentColor });
    },

    setGatewayUrl: (gatewayUrl) => set({ gatewayUrl }),
    setLogLevel: (logLevel) => {
      try { localStorage.setItem(STORAGE_KEY_LOG_LEVEL, logLevel); } catch {}
      set({ logLevel });
    },
  };
});
