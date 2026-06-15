import type { ReactNode } from "react";
import { cn } from "../../lib/utils";
import { Tooltip } from "./Tooltip";

interface NavButtonProps {
    active: boolean;
    onClick: () => void;
    tooltip: string;
    tooltipPosition?: "left" | "right";
    className?: string;
    children: ReactNode;
}

/**
 * Reusable navigation bar icon button.
 * Used by both the left (NavBar) and right (RightNavBar) navigation panels.
 * Matches the VS Code-style icon nav pattern: 40×40 rounded button,
 * accent color when active, gray when inactive, with a hover background.
 */
export function NavButton({ active, onClick, tooltip, tooltipPosition = "right", className, children }: NavButtonProps) {
    return (
        <Tooltip content={tooltip} variant="plain" position={tooltipPosition}>
            <button
                onClick={onClick}
                className={cn(
                    "flex h-10 w-10 items-center justify-center rounded-lg transition-colors duration-150",
                    active
                        ? "hover:bg-[#D8D9DC] dark:hover:bg-[#3D3D3F]"
                        : "text-zinc-500 hover:text-zinc-600 hover:bg-[#D8D9DC] dark:text-zinc-400 dark:hover:text-zinc-300 dark:hover:bg-[#3D3D3F]",
                    className,
                )}
                style={active ? { color: "var(--color-accent)" } : undefined}
            >
                {children}
            </button>
        </Tooltip>
    );
}
