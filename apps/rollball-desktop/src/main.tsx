import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./i18n"; // i18n initialization (must run before any useTranslation call)
import "./styles/globals.css";

// ═══ Monaco Editor bootstrap — MUST run before any component uses Monaco ═══
//
// 1. Tell @monaco-editor/react to use the locally-installed monaco-editor
//    instead of loading scripts from CDN (which may fail in Tauri's WebView).
// 2. Configure MonacoEnvironment.getWorker so that language-service workers
//    (TypeScript, JSON, CSS, HTML, editor) are resolved through Vite's
//    ?worker import pipeline rather than fetched as loose scripts.
import { loader } from "@monaco-editor/react";
import * as monaco from "monaco-editor";

loader.config({ monaco });

// Vite-compatible worker resolution: each language label maps to a
// monaco-editor worker entry that Vite bundles as a separate chunk.
(window as any).MonacoEnvironment = {
	getWorker(_workerId: string, label: string) {
		switch (label) {
			case "json":
				return new Worker(
					new URL("monaco-editor/esm/vs/language/json/json.worker.js", import.meta.url),
					{ type: "module" },
				);
			case "css":
			case "scss":
			case "less":
				return new Worker(
					new URL("monaco-editor/esm/vs/language/css/css.worker.js", import.meta.url),
					{ type: "module" },
				);
			case "html":
			case "handlebars":
			case "razor":
				return new Worker(
					new URL("monaco-editor/esm/vs/language/html/html.worker.js", import.meta.url),
					{ type: "module" },
				);
			case "typescript":
			case "javascript":
				return new Worker(
					new URL("monaco-editor/esm/vs/language/typescript/ts.worker.js", import.meta.url),
					{ type: "module" },
				);
			default:
				return new Worker(
					new URL("monaco-editor/esm/vs/editor/editor.worker.js", import.meta.url),
					{ type: "module" },
				);
		}
	},
};
// ═══ End Monaco bootstrap ═══

// Import settingsStore early so theme is applied to DOM before first paint.
// The store initializer calls applyTheme() which toggles the .dark class
// based on the persisted preference from localStorage.
import "./stores/settingsStore";

// Disable native browser context menu to prevent accidental page refresh
// and other browser actions that would restart the entire app.
// Custom context menus (ChatPanel, AgentList) handle their own preventDefault.
document.addEventListener("contextmenu", (e) => e.preventDefault());

// Block native browser keyboard shortcuts so they can be redefined by the app.
//
// Architecture — two-layer design with clean module boundaries:
//
//   Layer 1 (this handler):
//     Bubble-phase listener on `window`. Only fires when the event was NOT
//     handled by any inner component. If Monaco Editor (or any other inner
//     handler) matched the keybinding, it calls stopPropagation() and the
//     event never reaches here — no explicit component detection needed.
//
//   Layer 2 (components, e.g. FileEditorPanel):
//     Register keybindings via Monaco's addCommand() API. When Ctrl+P is
//     pressed inside the editor, Monaco fires the registered handler which
//     opens the Command Palette and stops propagation.
//
// Clipboard (C/V/X/Z/A) and selection shortcuts are intentionally NOT blocked.
// F12 is NOT blocked — needed for DevTools in debug/development mode.
const BLOCKED_SHORTCUTS = new Set([
  "p", // Print
  "s", // Save page
  "f", // Browser find
  "h", // History
  "j", // Downloads
  "u", // View source
  "w", // Close tab
  "t", // New tab
  "n", // New window
  "k", // Browser search
]);

window.addEventListener("keydown", (e: KeyboardEvent) => {
  // Skip if already handled by an inner component (e.g. Monaco Editor).
  // Monaco calls preventDefault() + stopPropagation() on matched keybindings,
  // which prevents the event from bubbling here. This check covers the edge
  // case where an inner handler called preventDefault() without stopPropagation().
  if (e.defaultPrevented) return;

  // Block Ctrl+<key> combinations (but not Ctrl+Alt, Ctrl+Meta, or Ctrl+Shift)
  if (e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey) {
    if (BLOCKED_SHORTCUTS.has(e.key.toLowerCase())) {
      e.preventDefault();
      return;
    }
    if (e.key.toLowerCase() === "r") {
      e.preventDefault(); // Ctrl+R — page refresh
      return;
    }
  }

  // Block F5 (page refresh)
  if (e.key === "F5") {
    e.preventDefault();
  }
});

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
