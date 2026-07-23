import { Minus, Square, X } from "lucide-react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { log } from "../../lib/logger";

export function TitleBar() {
  const isMacOS = navigator.platform.includes("Mac");
  const win = getCurrentWindow();

  const handleMinimize = async () => {
    try {
      await win.minimize();
    } catch (error) {
      log.error("Failed to minimize:", error);
    }
  };

  const handleMaximize = async () => {
    try {
      await win.toggleMaximize();
    } catch (error) {
      log.error("Failed to toggle maximize:", error);
    }
  };

  const handleClose = async () => {
    try {
      await win.close();
    } catch (error) {
      log.error("Failed to close:", error);
    }
  };

  // On macOS, the native traffic lights provide close/minimize/maximize.
  // On Windows/Linux, we render custom buttons.
  //
  // `data-tauri-drag-region` enables native window dragging with zero JS
  // latency — Tauri's webview layer handles mousedown directly, so the
  // cursor stays anchored at the click point.  Double-click to maximize is
  // also handled natively by Tauri, replacing the previous setTimeout-based
  // workaround that caused a 250ms delay and cursor drift on macOS.
  return (
    <div
      data-tauri-drag-region
      className={`flex h-8 w-full items-center justify-between select-none ${
        isMacOS ? "pl-[80px]" : "pl-3"
      }`}
    >
      {/* Left: App title */}
      <div className="flex items-center gap-2">
        <span className="text-xs font-medium text-zinc-700 dark:text-zinc-300">
          ACowork
        </span>
      </div>

      {/* Right: Window controls (Windows/Linux only) */}
      {!isMacOS && (
        <div className="flex items-center gap-1" onMouseDown={(e) => e.stopPropagation()}>
          <button
            className="flex h-8 w-8 items-center justify-center rounded text-zinc-600 hover:bg-zinc-300 dark:text-zinc-400 dark:hover:bg-zinc-700"
            onClick={handleMinimize}
          >
            <Minus className="h-3.5 w-3.5" />
          </button>
          <button
            className="flex h-8 w-8 items-center justify-center rounded text-zinc-600 hover:bg-zinc-300 dark:text-zinc-400 dark:hover:bg-zinc-700"
            onClick={handleMaximize}
          >
            <Square className="h-3 w-3" />
          </button>
          <button
            className="flex h-8 w-8 items-center justify-center rounded text-zinc-600 hover:bg-red-500 hover:text-white dark:text-zinc-400 dark:hover:bg-red-600"
            onClick={handleClose}
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </div>
      )}
    </div>
  );
}
