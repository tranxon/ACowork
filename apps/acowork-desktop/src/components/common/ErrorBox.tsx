import { useCallback, useState, useEffect, useRef } from "react";
import { AlertTriangle, Check, ChevronDown, ChevronRight, Copy, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";

interface ErrorBoxProps {
    /** Short headline message — shown in the box body. */
    message: string;
    /** Optional long-form details (stack trace, raw error body, etc.).
     *  When provided, a "Show details" disclosure is rendered. */
    details?: string;
    /** Optional close handler. When provided, an X button is rendered. */
    onClose?: () => void;
    /** Extra classes for the outer container. */
    className?: string;
}

/**
 * ErrorBox — standardized inline error display.
 *
 * Replaces the dozens of hand-rolled `<div class="bg-red-50 ... text-red-700">{error}</div>`
 * blocks across the app. Adds:
 *   - `user-select: text` + `data-error` (CSS hook to opt out of body user-select:none)
 *   - One-click Copy button with copied feedback
 *   - Optional collapsible details for long error bodies
 *   - Optional close button
 *   - role="alert" for accessibility
 */
export function ErrorBox({ message, details, onClose, className }: ErrorBoxProps) {
    const { t } = useTranslation();
    const [copied, setCopied] = useState(false);
    const [showDetails, setShowDetails] = useState(false);
    const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

    useEffect(() => {
        return () => {
            if (timerRef.current) clearTimeout(timerRef.current);
        };
    }, []);

    const handleCopy = useCallback(async () => {
        // Combine message + details so the user gets the full context.
        const full = details ? `${message}\n\n${details}` : message;
        try {
            await navigator.clipboard.writeText(full);
            setCopied(true);
            if (timerRef.current) clearTimeout(timerRef.current);
            timerRef.current = setTimeout(() => setCopied(false), 1500);
        } catch {
            // Clipboard API can fail in some sandboxed contexts; fail silently.
        }
    }, [message, details]);

    const hasDetails = Boolean(details);

    return (
        <div
            role="alert"
            data-error="true"
            className={cn(
                "rounded-md border border-red-200 bg-red-50 p-3 text-xs text-red-700 select-text",
                "dark:border-red-800 dark:bg-red-900/20 dark:text-red-400",
                className,
            )}
        >
            <div className="flex items-start gap-2">
                <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-red-500 dark:text-red-400" />
                <div className="min-w-0 flex-1">
                    <p className="break-words whitespace-pre-wrap">{message}</p>
                    {hasDetails && (
                        <button
                            type="button"
                            onClick={() => setShowDetails((v) => !v)}
                            className="mt-1 inline-flex items-center gap-0.5 text-[11px] font-medium text-red-700 hover:text-red-900 dark:text-red-400 dark:hover:text-red-300"
                        >
                            {showDetails ? (
                                <ChevronDown className="h-3 w-3" />
                            ) : (
                                <ChevronRight className="h-3 w-3" />
                            )}
                            {showDetails ? t("common.hideDetails") : t("common.showDetails")}
                        </button>
                    )}
                </div>
                <div className="flex shrink-0 items-center gap-1">
                    <button
                        type="button"
                        onClick={handleCopy}
                        aria-label={t("common.ariaLabelCopyError")}
                        title={t("common.copy")}
                        className={cn(
                            "inline-flex h-6 items-center gap-1 rounded px-1.5 text-[11px] font-medium transition-colors",
                            copied
                                ? "text-green-700 dark:text-green-400"
                                : "text-red-700 hover:bg-red-100 dark:text-red-400 dark:hover:bg-red-900/40",
                        )}
                    >
                        {copied ? (
                            <>
                                <Check className="h-3 w-3" />
                                {t("common.copied")}
                            </>
                        ) : (
                            <>
                                <Copy className="h-3 w-3" />
                                {t("common.copy")}
                            </>
                        )}
                    </button>
                    {onClose && (
                        <button
                            type="button"
                            onClick={onClose}
                            aria-label={t("common.ariaLabelDismiss")}
                            className="inline-flex h-6 w-6 items-center justify-center rounded text-red-700 hover:bg-red-100 dark:text-red-400 dark:hover:bg-red-900/40"
                        >
                            <X className="h-3 w-3" />
                        </button>
                    )}
                </div>
            </div>
            {hasDetails && showDetails && (
                <pre className="mt-2 max-h-48 overflow-auto rounded bg-red-100/60 p-2 text-[11px] leading-relaxed text-red-800 break-all whitespace-pre-wrap dark:bg-red-950/40 dark:text-red-300">
                    {details}
                </pre>
            )}
        </div>
    );
}