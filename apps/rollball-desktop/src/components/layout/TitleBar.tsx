import { Minus, Square, X } from "lucide-react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useSettingsStore } from "../../stores/settingsStore";

// ── VS Code style panel toggle icon ──────────────────────────────────
/** VS Code style panel toggle: a box divided by a vertical line into left/right halves.
 *  Right half highlights (filled) when panel is expanded. */
function PanelToggleIcon({ expanded }: { expanded: boolean }) {
  return (
    <svg width="14" height="14" viewBox="0 0 14 14" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
      {/* Outer rectangle */}
      <rect x="1.5" y="1.5" width="11" height="11" rx="1.2" />
      {/* Vertical divider line */}
      <line x1="7" y1="1.5" x2="7" y2="12.5" />
      {/* Right half fill — accent when expanded */}
      <rect x="7" y="1.5" width="5.5" height="11" rx="0"
        fill={expanded ? "var(--color-accent)" : "none"}
      />
    </svg>
  );
}

interface TitleBarProps {
  /** Whether the right panel is currently expanded */
  panelExpanded: boolean;
  /** Toggle the right panel */
  onTogglePanel: () => void;
}

export function TitleBar({ panelExpanded, onTogglePanel }: TitleBarProps) {
  const { opacity, theme } = useSettingsStore();
  const isDark = theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  // Original gray: #E2E3E9 (light) / #292A2C (dark), modulated by opacity
  const bgColor = isDark ? `rgba(41,42,44,${opacity})` : `rgba(226,227,233,${opacity})`;

  const handleMinimize = async () => {
    console.log("[TitleBar] Minimize clicked");
    try {
      const currentWindow = getCurrentWindow();
      console.log("[TitleBar] Window instance:", currentWindow);
      await currentWindow.minimize();
      console.log("[TitleBar] Minimize success");
    } catch (error) {
      console.error("[TitleBar] Failed to minimize:", error);
    }
  };

  const handleMaximize = async () => {
    console.log("[TitleBar] Maximize clicked");
    try {
      const currentWindow = getCurrentWindow();
      console.log("[TitleBar] Window instance:", currentWindow);
      await currentWindow.toggleMaximize();
      console.log("[TitleBar] Maximize success");
    } catch (error) {
      console.error("[TitleBar] Failed to toggle maximize:", error);
    }
  };

  const handleClose = async () => {
    console.log("[TitleBar] Close clicked");
    try {
      const currentWindow = getCurrentWindow();
      console.log("[TitleBar] Window instance:", currentWindow);
      await currentWindow.close();
      console.log("[TitleBar] Close success");
    } catch (error) {
      console.error("[TitleBar] Failed to close:", error);
    }
  };

  return (
    <div
      data-tauri-drag-region
      className="flex h-8 w-full items-center justify-between select-none pl-3"
      style={{
        "-webkit-app-region": "drag",
        backgroundColor: bgColor,
      } as React.CSSProperties}
    >
      {/* Left: App title */}
      <div className="flex items-center gap-2">
        <span className="text-xs font-medium text-zinc-700 dark:text-zinc-300">
          - RollBall.AI -
        </span>
      </div>

      {/* Right: Panel toggle + Window controls */}
      <div className="flex items-center gap-1">
        {/* Right panel toggle — VS Code style, left of window controls */}
        <button
          className="flex h-8 w-8 items-center justify-center rounded text-zinc-500 hover:text-zinc-700 hover:bg-zinc-300 dark:text-zinc-400 dark:hover:text-zinc-200 dark:hover:bg-zinc-700"
          style={{ "-webkit-app-region": "no-drag" } as React.CSSProperties}
          onClick={onTogglePanel}
          aria-label={panelExpanded ? "Collapse right panel" : "Expand right panel"}
          title={panelExpanded ? "Collapse Right Panel" : "Expand Right Panel"}
        >
          <PanelToggleIcon expanded={panelExpanded} />
        </button>

        {/* Minimize */}
        <button
          className="flex h-8 w-8 items-center justify-center rounded text-zinc-600 hover:bg-zinc-300 dark:text-zinc-400 dark:hover:bg-zinc-700"
          style={{ "-webkit-app-region": "no-drag" } as React.CSSProperties}
          onClick={handleMinimize}
        >
          <Minus className="h-3.5 w-3.5" />
        </button>

        {/* Maximize/Restore */}
        <button
          className="flex h-8 w-8 items-center justify-center rounded text-zinc-600 hover:bg-zinc-300 dark:text-zinc-400 dark:hover:bg-zinc-700"
          style={{ "-webkit-app-region": "no-drag" } as React.CSSProperties}
          onClick={handleMaximize}
        >
          <Square className="h-3 w-3" />
        </button>

        {/* Close */}
        <button
          className="flex h-8 w-8 items-center justify-center rounded text-zinc-600 hover:bg-red-500 hover:text-white dark:text-zinc-400 dark:hover:bg-red-600"
          style={{ "-webkit-app-region": "no-drag" } as React.CSSProperties}
          onClick={handleClose}
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </div>
    </div>
  );
}
