import { useRef, useState, useLayoutEffect } from "react";
import type { ModelEntry } from "../../lib/types";

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
 * All per-session mutable state that does NOT need to trigger React re-renders.
 * Stored in a single ref object; reset atomically on session change.
 */
export interface SessionScope {
  // ── Scroll & Virtual ──
  prevScrollHeight: number;
  isLoadingMore: boolean;
  /** Tracks which session ID is being initial-loaded (or "__init__" for agent init). */
  isInitialLoad: string | null;
  userJustSent: boolean;
  pinnedToBottom: boolean;
  thinkingWasShowing: boolean;
  /** Previous display count for scroll-on-new-message logic. */
  prevDisplayCount: number;
  /** Previous virtualCount for sticky-bottom logic. */
  prevStickyCount: number;

}

/** Factory: returns a fresh default SessionScope. */
export function createDefaultSessionScope(): SessionScope {
  return {
    prevScrollHeight: 0,
    isLoadingMore: false,
    isInitialLoad: null,
    userJustSent: false,
    pinnedToBottom: false,
    thinkingWasShowing: false,
    prevDisplayCount: 0,
    prevStickyCount: 0,
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
}

/**
 * Consolidates all per-session refs and state into a single hook.
 *
 * On session change (currentSessionId differs from previous render):
 *  - The entire SessionScope ref is replaced with a fresh default.
 *  - All state values are reset to their initial defaults.
 *  - The optional `onSessionChange` callback is invoked (e.g. for
 *    virtualizer.measure() to clear measurement cache).
 *
 * This eliminates the class of bugs where per-session refs/state leak
 * across session switches because individual variables were not reset.
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

  // ── Reset on session change ──
  useLayoutEffect(() => {
    if (currentSessionId !== prevSessionRef.current) {
      prevSessionRef.current = currentSessionId;

      // Reset all ref-based state atomically
      scopeRef.current = createDefaultSessionScope();

      // Reset all state-based values
      setInputValue("");
      setPendingFiles([]);
      setPendingImages([]);
      setShowImageUnsupportedDialog(false);
      setImageCapableModels([]);
      setTodosCollapsed(false);
      setShowScrollToBottom(false);

      // Notify parent (e.g. to clear virtualizer measurement cache)
      onSessionChange?.();
    }
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
  };
}
