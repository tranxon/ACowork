import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./i18n"; // i18n initialization (must run before any useTranslation call)
import "./styles/globals.css";

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

  // Block Ctrl+<key> combinations (but not Ctrl+Alt or Ctrl+Meta)
  if (e.ctrlKey && !e.altKey && !e.metaKey) {
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
