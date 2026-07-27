import { create } from "zustand";
import { type LspStatus } from "../lib/lspUtils";

/**
 * Cross-component transient signals for the currently active file tab.
 *
 * Why this store exists (instead of `AppLayout` calling `useLspClientPool`
 * directly or `react-drilling` the values up from `FileEditorPanel`):
 *
 *   - `useLspClientPool` is **not** a module-level singleton — each call
 *     constructs a fresh `LspPoolManager` (see `hooks/useLspClientPool.ts`
 *     line 184–187) and opens its own LSP WebSocket. Subscribing from a
 *     second caller would create a duplicate connection per language —
 *     unacceptable.
 *
 *   - Cursor position changes on every selection move (mouse click,
 *     arrow key, drag-select). It is already triggering a re-render of
 *     `FileEditorPanel` via local `useState`. Mirroring the change into a
 *     Zustand store costs the same single synchronous write but lets the
 *     cluster subscribe with its own selector so unrelated components do
 *     not re-render.
 *
 *   - Keeping the cluster a *pure renderer* (props in / DOM out) is what
 *     makes it easy to test, snapshot, and reuse — see the
 *     `FileStatusCluster` component contract.
 *
 * Reset semantics: every field is keyed on "the file panel currently has
 * an active file". When the panel unmounts or `activeFile` becomes `null`,
 * `FileEditorPanel` resets the relevant slices to their idle sentinel
 * (see the `editorStatusReset` effect in `FileEditorPanel`).
 */

export interface EditorCursorPosition {
    /** 1-based line number (Monaco's coordinate space). */
    line: number;
    /** 1-based column number. */
    column: number;
}

interface EditorStatusState {
    /** Latest cursor position reported by Monaco's `onDidChangeCursorSelection`. */
    cursor: EditorCursorPosition;
    /** Number of characters currently selected; `0` when no active selection. */
    selectedCount: number;

    /**
     * LSP signals for the active file's language. `null` / `""` when no
     * editor is mounted, so the cluster can render nothing rather than a
     * stale "ready" pill from a previously-closed file.
     */
    lspEnabled: boolean;
    lspLanguage: string | null;
    lspStatus: LspStatus | null;
    lspStatusMessage: string;

    /**
     * Push a new cursor position. Called on every Monaco
     * `onDidChangeCursorSelection`.
     */
    setCursor: (cursor: EditorCursorPosition) => void;
    /**
     * Push the count of selected characters. `0` for empty selections.
     */
    setSelectedCount: (count: number) => void;
    /**
     * Mirror the four LSP signals that `FileEditorPanel` reads from
     * `useLspClientPool`. Bundled into one action so callers do not have
     * to remember to keep partial updates coherent (e.g. setting `status`
     * without clearing `message`).
     */
    setLspSignals: (signals: {
        enabled: boolean;
        language: string | null;
        status: LspStatus | null;
        statusMessage: string;
    }) => void;
    /**
     * Reset everything to the idle sentinel — called when the panel
     * unmounts or becomes empty so the cluster cannot show stale data
     * from a previously-closed file.
     */
    resetToIdle: () => void;
}

const INITIAL_STATE: Pick<
    EditorStatusState,
    "cursor" | "selectedCount" | "lspEnabled" | "lspLanguage" | "lspStatus" | "lspStatusMessage"
> = {
    cursor: { line: 1, column: 1 },
    selectedCount: 0,
    lspEnabled: false,
    lspLanguage: null,
    lspStatus: null,
    lspStatusMessage: "",
};

export const useEditorStatusStore = create<EditorStatusState>((set) => ({
    ...INITIAL_STATE,

    setCursor: (cursor) => set({ cursor }),
    setSelectedCount: (selectedCount) => set({ selectedCount }),
    setLspSignals: ({ enabled, language, status, statusMessage }) =>
        set({
            lspEnabled: enabled,
            lspLanguage: language,
            lspStatus: status,
            lspStatusMessage: statusMessage,
        }),
    resetToIdle: () => set({ ...INITIAL_STATE }),
}));
