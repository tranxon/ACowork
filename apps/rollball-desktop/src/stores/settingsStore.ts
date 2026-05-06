import { create } from "zustand";
import type { Theme } from "../lib/types";
import { DEFAULT_GATEWAY_URL } from "../lib/config";

const STORAGE_KEY_THEME = "rollball-theme";
const STORAGE_KEY_FONT_SIZE = "rollball-font-size";
const STORAGE_KEY_LOG_LEVEL = "rollball-log-level";

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

/** Read persisted font size from localStorage, fallback to 1.0 */
function getPersistedFontSize(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_FONT_SIZE);
    if (stored) {
      const val = parseFloat(stored);
      if (!isNaN(val) && val > 0) return val;
    }
  } catch {}
  return 1.0;
}

/** Read persisted log level from localStorage, fallback to "info" */
function getPersistedLogLevel(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_LOG_LEVEL);
    if (stored) return stored;
  } catch {}
  return "info";
}

interface SettingsStore {
  theme: Theme;
  fontSize: number;
  gatewayUrl: string;
  logLevel: string;
  setTheme: (theme: Theme) => void;
  setFontSize: (size: number) => void;
  setGatewayUrl: (url: string) => void;
  setLogLevel: (level: string) => void;
}

export const useSettingsStore = create<SettingsStore>((set) => {
  // Initialize from persisted values and apply theme to DOM immediately
  const initialTheme = getPersistedTheme();
  const initialFontSize = getPersistedFontSize();
  const initialLogLevel = getPersistedLogLevel();
  applyTheme(initialTheme);
  applyFontSize(initialFontSize);

  return {
    theme: initialTheme,
    fontSize: initialFontSize,
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

    setGatewayUrl: (gatewayUrl) => set({ gatewayUrl }),
    setLogLevel: (logLevel) => {
      try { localStorage.setItem(STORAGE_KEY_LOG_LEVEL, logLevel); } catch {}
      set({ logLevel });
    },
  };
});
