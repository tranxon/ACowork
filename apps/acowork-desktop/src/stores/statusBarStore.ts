import { create } from "zustand";

export type StatusType = "info" | "error" | "warning";

/// Unique identifier for who set the status bar message.
///
/// Used by `clearStatus(source)` to clear messages by origin rather
/// than by fragile text matching against the displayed string.
export type StatusSource = "gateway" | "mqtt" | "debug" | "generic";

interface StatusBarState {
    message: string;
    type: StatusType;
    visible: boolean;
    source: StatusSource | null;
    setStatus: (message: string, type?: StatusType, source?: StatusSource) => void;
    /// Clear the status bar.
    ///
    /// If `source` is provided, only clears if the current message's
    /// source matches.  This prevents one subsystem from accidentally
    /// clearing a warning set by another subsystem.
    /// If `source` is omitted, clears unconditionally.
    clearStatus: (source?: StatusSource) => void;
}

export const useStatusBarStore = create<StatusBarState>((set, get) => ({
    message: "",
    type: "info",
    visible: false,
    source: null,
    setStatus: (message, type = "info", source) => set({ message, type, visible: true, source: source ?? null }),
    clearStatus: (source) => {
        if (source !== undefined && get().source !== source) return;
        set({ message: "", type: "info", visible: false, source: null });
    },
}));
