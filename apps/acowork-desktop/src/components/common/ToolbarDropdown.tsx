import { type ReactNode } from "react";
import { ChevronDown } from "lucide-react";
import { cn } from "../../lib/utils";
import { toolbarButton } from "../../lib/ui-styles";
import { Tooltip } from "./Tooltip";

/**
 * Shared toolbar dropdown trigger — icon + text + chevron + hover tooltip.
 *
 * Text/chevron collapse can be driven two ways:
 *  1. CSS container queries via `collapseClass` (legacy, static breakpoints)
 *  2. JS-driven `textHidden` prop set by a ResizeObserver in the parent
 *     (preferred — adapts to which buttons are actually rendered)
 *
 * The tooltip carries `tipClass` and is shown on hover only when text is hidden.
 */
export function ToolbarDropdownTrigger({
    icon,
    label,
    collapseClass,
    tipClass,
    open,
    onToggle,
    wrapperRef,
    buttonClassName,
    children,
    tooltip,
    textHidden,
}: {
    icon: ReactNode;
    label: string;
    /** CSS class that container-query rules target to hide text + chevron */
    collapseClass?: string;
    /** CSS class that container-query rules target to show tooltip */
    tipClass?: string;
    open: boolean;
    onToggle: () => void;
    wrapperRef?: React.Ref<HTMLDivElement>;
    buttonClassName?: string;
    children: ReactNode;
    /** Tooltip text (falls back to label if not provided) */
    tooltip?: string;
    /** When true, force-hide the label text and chevron (JS-driven collapse) */
    textHidden?: boolean;
}) {
    return (
        <div ref={wrapperRef} className="relative inline-block min-w-0">
            <Tooltip content={tooltip ?? label} tipClass={tipClass}>
                <button
                    type="button"
                    onClick={onToggle}
                    className={cn(
                        toolbarButton,
                        "min-w-0",
                        open && "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100",
                        buttonClassName,
                    )}
                >
                    <span className="shrink-0">{icon}</span>
                    <span
                        data-toolbar-text=""
                        className={cn(collapseClass, "min-w-0 max-w-[120px] truncate")}
                        style={{ fontSize: "0.75rem", display: textHidden ? "none" : undefined }}
                    >{label}</span>
                    <ChevronDown
                        data-toolbar-chevron=""
                        className={cn("h-3 w-3 shrink-0 text-zinc-400", collapseClass)}
                        style={{ display: textHidden ? "none" : undefined }}
                    />
                </button>
            </Tooltip>
            {children}
        </div>
    );
}
