import { create } from "zustand";

/** Right-side results panel tabs — keep in sync with AppLayout.PanelTab. */
export type PanelTab = "debug" | "status" | "setup" | "tools" | "memory" | "workspace";

/**
 * Live bounding rect of the file editor panel, reported by
 * `useReportFilePanelBounds` from inside `FileEditorPanel`.
 *
 * Coordinates are measured against `window` (i.e. `getBoundingClientRect()`)
 * so a consumer rendered in the global status bar can use them directly with
 * `position: absolute; left: ${bounds.left}px; right: ${windowW - bounds.right}px`.
 *
 * `mounted = false` means no file is open (the panel is unmounted in
 * `AppLayout` when `hasOpenFiles` is false); consumers should hide
 * themselves rather than render at stale (0,0) coordinates.
 */
export interface FilePanelBounds {
    left: number;
    right: number;
    mounted: boolean;
}

interface LayoutState {
    /** Currently active tab in the right-side results panel. */
    activePanelTab: PanelTab;
    setActivePanelTab: (tab: PanelTab) => void;

    /** Whether the right-side results panel is collapsed. */
    resultsCollapsed: boolean;
    /**
     * Update the collapsed state. Accepts either a boolean or an updater
     * function (mirrors React's `setState(prev => !prev)` API) so call sites
     * can avoid stale-closure bugs.
     */
    setResultsCollapsed: (collapsed: boolean | ((prev: boolean) => boolean)) => void;

    /**
     * Monotonically-increasing counter for "show me the workspace panel" requests.
     * AppLayout consumes each new value once (via a local ref) to expand the
     * results panel and switch the active tab to "workspace", even when the
     * user clicks the trigger repeatedly for the same file.
     */
    workspacePanelRequestSeq: number;
    requestShowWorkspacePanel: () => void;

    /**
     * Live bounding rect of the file editor panel. Updated by
     * `useReportFilePanelBounds` running inside `FileEditorPanel`.
     */
    filePanelBounds: FilePanelBounds;
    /**
     * Replace the file panel bounds wholesale. The reporting hook coalesces
     * updates via `requestAnimationFrame` so a drag of the resize handle does
     * not flood the store at refresh-rate cadence.
     */
    setFilePanelBounds: (bounds: FilePanelBounds) => void;
}

export const useLayoutStore = create<LayoutState>((set) => ({
    activePanelTab: "workspace",
    setActivePanelTab: (tab) => set({ activePanelTab: tab }),

    resultsCollapsed: false,
    setResultsCollapsed: (collapsed) =>
        set((state) => ({
            resultsCollapsed:
                typeof collapsed === "function"
                    ? (collapsed as (prev: boolean) => boolean)(state.resultsCollapsed)
                    : collapsed,
        })),

    workspacePanelRequestSeq: 0,
    requestShowWorkspacePanel: () =>
        set((state) => ({ workspacePanelRequestSeq: state.workspacePanelRequestSeq + 1 })),

    filePanelBounds: { left: 0, right: 0, mounted: false },
    setFilePanelBounds: (bounds) => set({ filePanelBounds: bounds }),
}));