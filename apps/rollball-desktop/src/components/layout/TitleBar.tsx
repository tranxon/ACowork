import { Minus, Square, X } from "lucide-react";
import { getCurrentWindow } from "@tauri-apps/api/window";

export function TitleBar() {
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
      className="flex h-7 w-full items-center justify-between select-none bg-[#E2E3E9] px-3 dark:bg-[#292A2C]"
      style={{ "-webkit-app-region": "drag" } as React.CSSProperties}
    >
      {/* Left: App title */}
      <div className="flex items-center gap-2">
        <span className="text-xs font-medium text-zinc-700 dark:text-zinc-300">
          Rollball
        </span>
      </div>

      {/* Right: Window controls */}
      <div className="flex items-center gap-1">
        {/* Minimize */}
        <button
          className="flex h-7 w-7 items-center justify-center rounded text-zinc-600 hover:bg-zinc-300 dark:text-zinc-400 dark:hover:bg-zinc-700"
          style={{ "-webkit-app-region": "no-drag" } as React.CSSProperties}
          onClick={handleMinimize}
        >
          <Minus className="h-3.5 w-3.5" />
        </button>

        {/* Maximize/Restore */}
        <button
          className="flex h-7 w-7 items-center justify-center rounded text-zinc-600 hover:bg-zinc-300 dark:text-zinc-400 dark:hover:bg-zinc-700"
          style={{ "-webkit-app-region": "no-drag" } as React.CSSProperties}
          onClick={handleMaximize}
        >
          <Square className="h-3 w-3" />
        </button>

        {/* Close */}
        <button
          className="flex h-7 w-7 items-center justify-center rounded text-zinc-600 hover:bg-red-500 hover:text-white dark:text-zinc-400 dark:hover:bg-red-600"
          style={{ "-webkit-app-region": "no-drag" } as React.CSSProperties}
          onClick={handleClose}
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </div>
    </div>
  );
}
