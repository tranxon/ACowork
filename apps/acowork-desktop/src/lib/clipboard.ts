// src/lib/clipboard.ts
//
// Unified clipboard helpers used by every context-menu "Copy" item and any
// other ad-hoc copy surface (CodeBlock, ErrorBox, AppLayout status, etc.).
//
// Why a single helper instead of 5 inlined `navigator.clipboard.writeText`
// blocks?
//
//   1. `main.tsx` patches `navigator.clipboard.writeText` to silently swallow
//      `NotAllowedError` (the Tauri WKWebView quirk — see main.tsx header
//      comment). That patch ONLY catches the error; it does NOT provide a
//      usable fallback. So callers that need "actually copy something" must
//      layer their own `execCommand("copy")` fallback on top.
//
//   2. Every context-menu "Copy" item has a recurring two-step pattern:
//        a. Snapshot the user's current text selection at the moment they
//           right-click. (Reading `window.getSelection()` at click time is
//           unreliable — `<button>` focus on mouseup clears the selection in
//           WebKit, so `selectedText` ends up empty by the time the button's
//           onClick fires. See the MessageBubble bug report.)
//        b. Fall back to a caller-supplied string when the selection is
//           empty, so the menu item works even without a prior text-drag.
//
//   This module owns that two-step pattern + the navigator.clipboard +
//   execCommand fallback. Callers stay focused on *what* to copy, not
//   *how*.
//
// Functions:
//   - `snapshotSelection()`              — read selection RIGHT NOW
//   - `copyText(text)`                    — best-effort copy one string
//   - `copySelectionOrFallback(fallback)` — snapshot selection, fall back
//                                           when empty, copy result
//   - `wasCopiedRecently()`               — cheap heuristic for toast/UI
//                                           feedback (not used yet, kept
//                                           for future surfaces)

/**
 * Read the current document selection as plain text.
 *
 * IMPORTANT: only call this at the moment of the user gesture that needs it
 * (typically the right-click handler). Calling it later — e.g. inside a
 * button onClick — is unreliable in WKWebView because clicking a `<button>`
 * shifts focus to the button, which in turn clears the page's text
 * selection. The MessageBubble bug exists exactly because the original
 * implementation deferred this read to button click time.
 */
export function snapshotSelection(): string {
  if (typeof window === "undefined") return "";
  return window.getSelection()?.toString() ?? "";
}

/**
 * Copy `text` to the system clipboard.
 *
 * Tries `navigator.clipboard.writeText` first (already patched by main.tsx
 * to swallow NotAllowedError silently), then falls back to the classic
 * `document.execCommand("copy")` textarea trick — which works in WKWebView
 * when triggered from a user gesture even when the async clipboard API
 * refuses.
 *
 * Returns `true` when something was written, `false` when the text was
 * empty or both paths threw. Never throws — callers can fire-and-forget.
 */
export async function copyText(text: string): Promise<boolean> {
  if (!text) return false;

  // Path 1: async Clipboard API. Already NotAllowedError-suppressed by
  // main.tsx so the `.catch` here only fires for non-permission failures.
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // fall through to execCommand
  }

  // Path 2: hidden-textarea + execCommand. Works in Tauri WebView as long
  // as we are still inside the original user-gesture event chain (we are,
  // because this function is invoked synchronously from button onClick).
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.left = "-9999px";
    ta.style.top = "0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}

/**
 * Copy the user's current text selection, or `fallback` when the selection
 * is empty/whitespace-only.
 *
 * This is the single helper every context-menu "Copy" item should call:
 *
 *   onClick: () => {
 *     void copySelectionOrFallback("the whole message body");
 *   }
 *
 * Callers SHOULD snapshot the selection at right-click time (so the menu
 * knows whether to enable the item) — `useContextMenu` exposes
 * `selectionAtOpen` for exactly this.
 */
export async function copySelectionOrFallback(fallback: string): Promise<boolean> {
  const selection = snapshotSelection();
  const text = selection.trim() || fallback;
  return copyText(text);
}