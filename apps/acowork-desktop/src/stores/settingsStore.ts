import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { Theme, GatewayMode } from "../lib/types";
import {
  DEFAULT_GATEWAY_URL,
  DEFAULT_THEME,
  DEFAULT_FONT_SIZE,
  DEFAULT_LOG_LEVEL,
  DEFAULT_CONTENT_WIDTH,
  DEFAULT_OPACITY,
  DEFAULT_ACCENT_COLOR,
  DEFAULT_GATEWAY_MODE,
  DEFAULT_LOG_FILE_SIZE_MB,
  DEFAULT_LOG_FILE_COUNT,
} from "../lib/defaults";
import {
  DEFAULT_ACCENT_PRESET,
  getAccentPresetByHex,
  type AccentPreset,
} from "../lib/accentPresets";

/**
 * Push the current gateway config to the Rust backend so that:
 *   - All Tauri HTTP commands use the correct base URL (previously they
 *     were hardcoded to 127.0.0.1:19876 in Rust)
 *   - The Rust side knows whether to skip spawning a local Gateway on
 *     the next boot (remote mode)
 *
 * Best-effort: errors are logged but never thrown, because settings
 * persistence must not be blocked by transient Tauri command failures
 * (e.g. during page reload while the Rust side is still booting).
 */
async function pushGatewayConfigToRust(mode: GatewayMode, url: string): Promise<void> {
  try {
    await invoke("set_gateway_config", {
      config: { mode, url },
    });
  } catch (err) {
    console.warn("Failed to push gateway config to Rust:", err);
  }
}

const STORAGE_KEY_THEME = "acowork-theme";
const STORAGE_KEY_FONT_SIZE = "acowork-font-size";
const STORAGE_KEY_LOG_LEVEL = "acowork-log-level";
const STORAGE_KEY_CONTENT_WIDTH = "acowork-content-width";
const STORAGE_KEY_OPACITY = "acowork-opacity";
const STORAGE_KEY_ACCENT_COLOR = "acowork-accent-color";
const STORAGE_KEY_GATEWAY_URL = "acowork-gateway-url";
const STORAGE_KEY_GATEWAY_MODE = "acowork-gateway-mode";
const STORAGE_KEY_LOG_FILE_SIZE = "acowork-log-file-size";
const STORAGE_KEY_LOG_FILE_COUNT = "acowork-log-file-count";

/** Read current OS theme preference from matchMedia */
function readOsTheme(): "light" | "dark" {
  if (typeof window === "undefined" || !window.matchMedia) return "light";
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

/** Apply theme to DOM by toggling .dark class on <html>.
 * For "system" mode, `osTheme` must be passed so the correct value
 * is applied even when the OS theme changed since the last call. */
function applyTheme(theme: Theme, osTheme: "light" | "dark") {
  const effective = theme === "system" ? osTheme : theme;
  document.documentElement.classList.toggle("dark", effective === "dark");
}

/** Apply fontSize to CSS custom property on root */
function applyFontSize(size: number) {
  document.documentElement.style.setProperty("--ui-font-size", `${size}rem`);
}

/** Apply contentWidth to CSS custom property on root */
function applyContentWidth(width: number) {
  document.documentElement.style.setProperty("--content-max-width", `${width}%`);
}

/** Apply user-controlled opacity to the DOM.
 *
 * Writes the raw 0..1 value to `--app-opacity` on `:root` for any
 * component that wants to bind to it.  The actual visual tint is
 * applied by AppLayout's `glassBg` (rgba with this alpha), not by
 * this variable — the html/body layers are intentionally transparent
 * so the OS-native vibrancy shows through. */
function applyOpacity(opacity: number) {
  document.documentElement.style.setProperty("--app-opacity", String(opacity));
}

/** Apply accent preset to the DOM.
 *
 * Two layers of effect:
 *   1. `--color-accent` CSS variable — used by buttons, links, sliders, etc.
 *   2. `accent-{id}` class on <html>  — selects the matching per-accent
 *      block in globals.css that overrides `--glass-tint-light/dark`
 *      with the preset's hue-shifted near-neutral.
 *
 * The macOS native vibrancy tint (set via the `set_window_effect`
 * Tauri command) is owned by AppLayout — its useEffect on
 * `[isDark, accentColor]` re-invokes the command whenever either
 * changes, passing the matching RGBA tuples from `accentPresets.ts`.
 * That keeps the Rust side stateless w.r.t. accent selection.
 */
function applyAccentPreset(preset: AccentPreset) {
  document.documentElement.style.setProperty("--color-accent", preset.hex);
  const classList = document.documentElement.classList;
  // Remove any existing `accent-*` class, then add the current one.
  // We iterate instead of using a regex because classList doesn't
  // support wildcard removal in older browsers (and Safari < 17
  // doesn't support `className.replace` with regex reliably either).
  for (const cls of Array.from(classList)) {
    if (cls.startsWith("accent-")) classList.remove(cls);
  }
  classList.add(`accent-${preset.id}`);
}

/** Read persisted theme from localStorage, fallback to "system" */
function getPersistedTheme(): Theme {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_THEME);
    if (stored === "light" || stored === "dark" || stored === "system") return stored;
  } catch {
    // localStorage unavailable (SSR / privacy mode)
  }
  return DEFAULT_THEME;
}

/** Read persisted font size from localStorage, fallback to 0.875 (M) */
function getPersistedFontSize(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_FONT_SIZE);
    if (stored) {
      const val = parseFloat(stored);
      if (!isNaN(val) && val > 0) return val;
    }
  } catch { }
  return DEFAULT_FONT_SIZE;
}

/** Read persisted log level from localStorage, fallback to "info" */
function getPersistedLogLevel(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_LOG_LEVEL);
    if (stored) return stored;
  } catch { }
  return DEFAULT_LOG_LEVEL;
}

/** Read persisted content width from localStorage, fallback to 90 */
function getPersistedContentWidth(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_CONTENT_WIDTH);
    if (stored) {
      const val = parseInt(stored, 10);
      if (!isNaN(val) && val >= 40 && val <= 100) return val;
    }
  } catch { }
  return DEFAULT_CONTENT_WIDTH;
}

/** Read persisted accent color from localStorage, fallback to default blue.
 *
 * Always returns a valid `#rrggbb` hex string.  Unknown / malformed values
 * are coerced to `DEFAULT_ACCENT_COLOR` so downstream code can rely on the
 * format.  The matching `AccentPreset` is resolved lazily via
 * `getAccentPresetByHex()` in `setAccentColor` — see `applyAccentPreset`.
 */
function getPersistedAccentColor(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_ACCENT_COLOR);
    if (stored && /^#[0-9a-fA-F]{6}$/.test(stored)) return stored;
  } catch { }
  return DEFAULT_ACCENT_COLOR;
}

/** Read persisted gateway URL from localStorage, fallback to DEFAULT_GATEWAY_URL */
function getPersistedGatewayUrl(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_GATEWAY_URL);
    if (stored) return stored;
  } catch { }
  return DEFAULT_GATEWAY_URL;
}

/** Read persisted gateway mode from localStorage, fallback to "local" */
function getPersistedGatewayMode(): GatewayMode {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_GATEWAY_MODE);
    if (stored === "local" || stored === "remote") return stored;
  } catch { }
  return DEFAULT_GATEWAY_MODE;
}

/** Read persisted log file size from localStorage, fallback to 10 (MB) */
function getPersistedLogFileSizeMb(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_LOG_FILE_SIZE);
    if (stored) {
      const val = parseInt(stored, 10);
      if (!isNaN(val) && val >= 0) return val;
    }
  } catch { }
  return DEFAULT_LOG_FILE_SIZE_MB;
}

/** Read persisted log file count from localStorage, fallback to 20 */
function getPersistedLogFileCount(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_LOG_FILE_COUNT);
    if (stored) {
      const val = parseInt(stored, 10);
      if (!isNaN(val) && val >= 0) return val;
    }
  } catch { }
  return DEFAULT_LOG_FILE_COUNT;
}

/** Read persisted opacity from localStorage, fallback to 1.0 (opaque) */
function getPersistedOpacity(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_OPACITY);
    if (stored) {
      const val = parseFloat(stored);
      if (!isNaN(val) && val >= 0.0 && val <= 1.0) return val;
    }
  } catch { }
  return DEFAULT_OPACITY;
}

interface SettingsStore {
  theme: Theme;
  osTheme: "light" | "dark";
  fontSize: number;
  contentWidth: number;
  opacity: number;
  accentColor: string;
  gatewayUrl: string;
  gatewayMode: GatewayMode;
  logLevel: string;
  logFileSizeMb: number;
  logFileCount: number;
  setTheme: (theme: Theme) => void;
  setFontSize: (size: number) => void;
  setContentWidth: (width: number) => void;
  setOpacity: (opacity: number) => void;
  setAccentColor: (color: string) => void;
  setGatewayUrl: (url: string) => void;
  setGatewayMode: (mode: GatewayMode) => void;
  setLogLevel: (level: string) => void;
  setLogFileSizeMb: (size: number) => void;
  setLogFileCount: (count: number) => void;
}

export const useSettingsStore = create<SettingsStore>((set, get) => {
  // Initialize from persisted values and apply theme to DOM immediately
  const initialTheme = getPersistedTheme();
  const initialOsTheme = readOsTheme();
  const initialFontSize = getPersistedFontSize();
  const initialLogLevel = getPersistedLogLevel();
  const initialOpacity = getPersistedOpacity();
  const initialContentWidth = getPersistedContentWidth();
  const initialAccentColor = getPersistedAccentColor();
  applyTheme(initialTheme, initialOsTheme);
  applyFontSize(initialFontSize);
  applyOpacity(initialOpacity);
  applyContentWidth(initialContentWidth);
  applyAccentPreset(getAccentPresetByHex(initialAccentColor) ?? DEFAULT_ACCENT_PRESET);

  // Subscribe to OS theme changes so theme="system" stays in sync.
  // Without this listener the .dark class on <html> is frozen at app
  // start, so Tailwind dark variants and any component reading the
  // resolved theme (AppLayout glass, NavBar, SetiIcon) stay stale when
  // the user switches macOS appearance while the app is running.
  if (typeof window !== "undefined" && window.matchMedia) {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handleChange = (e: MediaQueryListEvent) => {
      const nextOsTheme: "light" | "dark" = e.matches ? "dark" : "light";
      const current = get();
      if (current.osTheme === nextOsTheme) return;
      // Always update osTheme so any component subscribed via the store
      // re-renders with the new effective theme.
      // If current.theme === "system", also re-apply to DOM so the
      // .dark class on <html> flips and Tailwind dark variants kick in.
      const applyNeeded = current.theme === "system";
      set({ osTheme: nextOsTheme });
      if (applyNeeded) applyTheme(current.theme, nextOsTheme);
    };
    if (mq.addEventListener) {
      mq.addEventListener("change", handleChange);
    } else if ((mq as any).addListener) {
      // Safari < 14 fallback (not strictly needed since Tauri requires
      // macOS 11+/Safari 14+, but kept for safety)
      (mq as any).addListener(handleChange);
    }
  }

  return {
    theme: initialTheme,
    osTheme: initialOsTheme,
    fontSize: initialFontSize,
    contentWidth: initialContentWidth,
    opacity: initialOpacity,
    accentColor: initialAccentColor,
    gatewayUrl: getPersistedGatewayUrl(),
    gatewayMode: getPersistedGatewayMode(),
    logLevel: initialLogLevel,
    logFileSizeMb: getPersistedLogFileSizeMb(),
    logFileCount: getPersistedLogFileCount(),

    setTheme: (theme) => {
      const osTheme = get().osTheme;
      applyTheme(theme, osTheme);
      try { localStorage.setItem(STORAGE_KEY_THEME, theme); } catch { }
      set({ theme });
    },

    setFontSize: (fontSize) => {
      applyFontSize(fontSize);
      try { localStorage.setItem(STORAGE_KEY_FONT_SIZE, String(fontSize)); } catch { }
      set({ fontSize });
    },

    setContentWidth: (contentWidth) => {
      applyContentWidth(contentWidth);
      try { localStorage.setItem(STORAGE_KEY_CONTENT_WIDTH, String(contentWidth)); } catch { }
      set({ contentWidth });
    },

    setOpacity: (opacity) => {
      applyOpacity(opacity);
      try { localStorage.setItem(STORAGE_KEY_OPACITY, String(opacity)); } catch { }
      set({ opacity });
    },

    setAccentColor: (accentColor) => {
      // Resolve the preset by hex; fall back to the default if the
      // stored value is no longer a known preset (e.g. a removed accent).
      const preset = getAccentPresetByHex(accentColor) ?? DEFAULT_ACCENT_PRESET;
      applyAccentPreset(preset);
      try { localStorage.setItem(STORAGE_KEY_ACCENT_COLOR, accentColor); } catch { }
      set({ accentColor });
    },

    setGatewayUrl: (gatewayUrl) => {
      try { localStorage.setItem(STORAGE_KEY_GATEWAY_URL, gatewayUrl); } catch { }
      set({ gatewayUrl });
      // Sync to Rust so subsequent Tauri commands use the new URL
      pushGatewayConfigToRust(get().gatewayMode, gatewayUrl);
    },
    setGatewayMode: (gatewayMode) => {
      try { localStorage.setItem(STORAGE_KEY_GATEWAY_MODE, gatewayMode); } catch { }
      set({ gatewayMode });
      // Sync to Rust. For local→remote this also stops any locally-
      // spawned Gateway. For remote→local the user must press Start
      // (or reload) — we don't auto-spawn here.
      pushGatewayConfigToRust(gatewayMode, get().gatewayUrl);
    },
    setLogLevel: (logLevel) => {
      try { localStorage.setItem(STORAGE_KEY_LOG_LEVEL, logLevel); } catch { }
      set({ logLevel });
    },
    setLogFileSizeMb: (logFileSizeMb) => {
      try { localStorage.setItem(STORAGE_KEY_LOG_FILE_SIZE, String(logFileSizeMb)); } catch { }
      set({ logFileSizeMb });
    },
    setLogFileCount: (logFileCount) => {
      try { localStorage.setItem(STORAGE_KEY_LOG_FILE_COUNT, String(logFileCount)); } catch { }
      set({ logFileCount });
    },
  };
});
