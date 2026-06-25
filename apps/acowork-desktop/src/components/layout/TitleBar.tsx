import { Minus, Square, X } from "lucide-react";
import { getCurrentWindow } from "@tauri-apps/api/window";

export function TitleBar() {
  const isMacOS = navigator.platform.includes("Mac");
  const win = getCurrentWindow();

  const handleMinimize = async () => {
    try {
      await win.minimize();
    } catch (error) {
      console.error("Failed to minimize:", error);
    }
  };

  const handleMaximize = async () => {
    try {
      await win.toggleMaximize();
    } catch (error) {
      console.error("Failed to toggle maximize:", error);
    }
  };

  const handleClose = async () => {
    try {
      await win.close();
    } catch (error) {
      console.error("Failed to close:", error);
    }
  };

  // JS-based drag — data-tauri-drag-region is unreliable on macOS when
  // transparent:true is set (it can consume the mousedown without actually
  // dragging, blocking our JS handler).  We rely solely on startDragging()
  // which requires the core:window:allow-start-dragging permission.
  const handleDragStart = async (e: React.MouseEvent) => {
    // Only start drag on primary mouse button (left click)
    if (e.button !== 0) return;
    try {
      await win.startDragging();
    } catch (error) {
      console.error("Failed to start drag:", error);
    }
  };

  // On macOS, the native traffic lights provide close/minimize/maximize.
  // On Windows/Linux, we render custom buttons.
  return (
    <div
      onMouseDown={handleDragStart}
      className={`flex h-8 w-full items-center justify-between select-none ${
        isMacOS ? "pl-[80px]" : "pl-3"
      }`}
    >
      {/* Left: App title */}
      <div className="flex items-center gap-2">
        <span className="text-xs font-medium text-zinc-700 dark:text-zinc-300">
          Acowork
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
