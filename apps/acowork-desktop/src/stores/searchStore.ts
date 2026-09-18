/**
 * Global search dialog (ADR-081, Ctrl+Shift+F) open state.
 *
 * Tiny store shared by the window-level keydown (AppLayout) and the
 * Monaco action (FileEditorPanel) so the shortcut opens the same dialog
 * no matter where focus lives. No business logic here — the dialog
 * component owns all search state.
 */

import { create } from "zustand";

interface SearchStoreState {
    open: boolean;
    openDialog: () => void;
    closeDialog: () => void;
    toggleDialog: () => void;
}

export const useSearchStore = create<SearchStoreState>((set, get) => ({
    open: false,
    openDialog: () => set({ open: true }),
    closeDialog: () => set({ open: false }),
    toggleDialog: () => set({ open: !get().open }),
}));

/** `true` when `e` is Ctrl+Shift+F / Cmd+Shift+F. */
export function isGlobalSearchShortcut(e: KeyboardEvent): boolean {
    return (e.ctrlKey || e.metaKey) && e.shiftKey && e.key.toLowerCase() === "f";
}
