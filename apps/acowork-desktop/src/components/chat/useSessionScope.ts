import { useRef, useState, useLayoutEffect } from "react";
import type { ModelEntry } from "../../lib/types";
import { log } from "../../lib/logger";

// ── Types ────────────────────────────────────────────────────────────

export interface PendingFile {
  tempId: string;
  filename: string;
  format: string;
  size: number;
  status: "uploading" | "success" | "error";
  documentId?: string;
  errorMessage?: string;
}

export interface PendingImage {
  tempId: string;
  filename: string;
  base64Url: string;
  width: number;
  height: number;
}

/**
 * Per-session mutable ref state. Stores values that need to persist across
 * renders within a session but don't trigger re-renders when written.
 *
 * IMPORTANT: This ref only holds PER-SESSION state.  Per-user-intent UI
 * state (such as scroll-pinned-to-bottom, which survives across session
 * switches and is owned by ChatPanel via its own ref) MUST NOT live here.
 *
 * All fields are reset atomically when the active session changes within
 * the same component instance, so per-session state never leaks across
 * sessions.  A fresh component instance starts with default values
 * (created by useRef(default)), so no reset is needed on first mount.
 */
export interface SessionScope {
  // ── Scroll & Virtual ──
  /** Tracks which session ID is being initial-loaded. `null` when no load is in flight. */
  isInitialLoad: string | null;
  /** True for the render immediately after the user sends a message. */
  userJustSent: boolean;
  /** True while the LLM is in its "thinking" prelude (before any text streamed). */
  thinkingWasShowing: boolean;
  /** Previous display count for scroll-on-new-message logic. */
  prevDisplayCount: number;

}

/** Factory: returns a fresh default SessionScope. */
export function createDefaultSessionScope(): SessionScope {
  return {
    isInitialLoad: null,
    userJustSent: false,
    thinkingWasShowing: false,
    prevDisplayCount: 0,
  };
}

// ── Hook ─────────────────────────────────────────────────────────────

export interface SessionScopeAPI {
  /** Mutable scope ref — read/write without triggering re-renders. */
  scope: React.MutableRefObject<SessionScope>;

  // ── State (triggers re-render on change) ──
  inputValue: string;
  setInputValue: (v: string) => void;

  pendingFiles: PendingFile[];
  setPendingFiles: React.Dispatch<React.SetStateAction<PendingFile[]>>;

  pendingImages: PendingImage[];
  setPendingImages: React.Dispatch<React.SetStateAction<PendingImage[]>>;

  showImageUnsupportedDialog: boolean;
  setShowImageUnsupportedDialog: (v: boolean) => void;

  imageCapableModels: ModelEntry[];
  setImageCapableModels: React.Dispatch<React.SetStateAction<ModelEntry[]>>;

  todosCollapsed: boolean;
  setTodosCollapsed: (v: boolean) => void;

  showScrollToBottom: boolean;
  setShowScrollToBottom: (v: boolean) => void;
  showScrollToTop: boolean;
  setShowScrollToTop: (v: boolean) => void;
}

/**
 * Consolidates all per-session refs and state into a single hook.
 *
 * On GENUINE SESSION CHANGE within the same component instance (current
 * session ID differs from the previous render), the entire SessionScope
 * ref is replaced with a fresh default and all state values are reset.
 * This eliminates the class of bugs where per-session refs/state leak
 * across session switches because individual variables were not reset.
 *
 * On FRESH MOUNT (first render of a new component instance), no reset
 * is needed: scopeRef is already initialized to its default state by
 * useRef(createDefaultSessionScope()), and useState values start at their
 * initial defaults.
 */
export function useSessionScope(
  currentSessionId: string | null,
  onSessionChange?: () => void,
): SessionScopeAPI {
  const scopeRef = useRef<SessionScope>(createDefaultSessionScope());
  const prevSessionRef = useRef<string | null>(null);

  // ── State values (trigger re-render) ──
  const [inputValue, setInputValue] = useState("");
  const [pendingFiles, setPendingFiles] = useState<PendingFile[]>([]);
  const [pendingImages, setPendingImages] = useState<PendingImage[]>([]);
  const [showImageUnsupportedDialog, setShowImageUnsupportedDialog] = useState(false);
  const [imageCapableModels, setImageCapableModels] = useState<ModelEntry[]>([]);
  const [todosCollapsed, setTodosCollapsed] = useState(false);
  const [showScrollToBottom, setShowScrollToBottom] = useState(false);
  const [showScrollToTop, setShowScrollToTop] = useState(false);

  // ── Reset on session change ──
  useLayoutEffect(() => {
    log.debug("[useSessionScope] reset-effect fire", {
      currentSessionId,
      prevSessionRef: prevSessionRef.current,
    });
    if (currentSessionId === prevSessionRef.current) return;

    // Capture the previous value BEFORE overwriting so we can distinguish a
    // fresh mount (prev was null) from a genuine session change (prev was a
    // non-null session ID different from the new one).
    const isFreshMount = prevSessionRef.current === null;
    prevSessionRef.current = currentSessionId;

    if (isFreshMount) {
      log.debug("[useSessionScope] fresh mount — no reset performed", {
        currentSessionId,
      });
      // Fresh mount: scope is already in default state (useRef(default)) and
      // useState values already hold their initial defaults.  No reset needed.
      // (Per-user-intent scroll state such as pinnedToBottom is owned OUTSIDE
      // this hook — by ChatPanel via its own ref — so it is not affected.)
      return;
    }

    // Genuine session change within the same component instance — reset all
    // per-session state to clear leaks across sessions.
    scopeRef.current = createDefaultSessionScope();
    setInputValue("");
    setPendingFiles([]);
    setPendingImages([]);
    setShowImageUnsupportedDialog(false);
    setImageCapableModels([]);
    setTodosCollapsed(false);
    setShowScrollToBottom(false);
    setShowScrollToTop(false);
    onSessionChange?.();
  }, [currentSessionId, onSessionChange]);

  return {
    scope: scopeRef,
    inputValue,
    setInputValue,
    pendingFiles,
    setPendingFiles,
    pendingImages,
    setPendingImages,
    showImageUnsupportedDialog,
    setShowImageUnsupportedDialog,
    imageCapableModels,
    setImageCapableModels,
    todosCollapsed,
    setTodosCollapsed,
    showScrollToBottom,
    setShowScrollToBottom,
    showScrollToTop,
    setShowScrollToTop,
  };
}