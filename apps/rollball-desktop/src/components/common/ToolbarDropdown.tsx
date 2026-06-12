import { type ReactNode } from "react";
import { ChevronDown } from "lucide-react";
import { cn } from "../../lib/utils";
import { toolbarButton } from "../../lib/ui-styles";
import { Tooltip } from "./Tooltip";

/**
 * Shared toolbar dropdown trigger — icon + text + chevron + hover tooltip.
 *
 * Text and chevron carry `collapseClass` so CSS container-query rules
 * in globals.css can hide them at specific toolbar widths.
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
}: {
    icon: ReactNode;
    label: string;
    /** CSS class that container-query rules target to hide text + chevron */
    collapseClass: string;
    /** CSS class that container-query rules target to show tooltip */
    tipClass: string;
    open: boolean;
    onToggle: () => void;
    wrapperRef?: React.Ref<HTMLDivElement>;
    buttonClassName?: string;
    children: ReactNode;
    /** Tooltip text (falls back to label if not provided) */
    tooltip?: string;
}) {
    return (
        <div ref={wrapperRef} className="relative inline-block">
            <Tooltip content={tooltip ?? label} tipClass={tipClass}>
                <button
                    type="button"
                    onClick={onToggle}
                    className={cn(
                        toolbarButton,
                        open && "bg-zinc-200 dark:bg-zinc-700 text-zinc-900 dark:text-zinc-100",
                        buttonClassName,
                    )}
                >
                    {icon}
                    <span className={cn(collapseClass, "max-w-[120px] truncate")}>{label}</span>
                    <ChevronDown className={cn("h-3 w-3 text-zinc-400", collapseClass)} />
                </button>
            </Tooltip>
            {children}
        </div>
    );
}
