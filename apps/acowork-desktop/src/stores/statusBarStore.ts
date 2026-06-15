import { create } from "zustand";

export type StatusType = "info" | "error" | "warning";

interface StatusBarState {
    message: string;
    type: StatusType;
    visible: boolean;
    setStatus: (message: string, type?: StatusType) => void;
    clearStatus: () => void;
}

export const useStatusBarStore = create<StatusBarState>((set) => ({
    message: "",
    type: "info",
    visible: false,
    setStatus: (message, type = "info") => set({ message, type, visible: true }),
    clearStatus: () => set({ message: "", type: "info", visible: false }),
}));
