import { useState, useCallback } from "react";
import { ChevronRight, ChevronDown, Copy, Check } from "lucide-react";

interface CodeBlockProps {
    language: string;
    code: string;
}

export function CodeBlock({ language, code }: CodeBlockProps) {
    const [collapsed, setCollapsed] = useState(false);
    const [copied, setCopied] = useState(false);

    const handleCopy = useCallback(async () => {
        try {
            await navigator.clipboard.writeText(code);
            setCopied(true);
            setTimeout(() => setCopied(false), 2000);
        } catch {
            const ta = document.createElement("textarea");
            ta.value = code;
            document.body.appendChild(ta);
            ta.select();
            document.execCommand("copy");
            document.body.removeChild(ta);
            setCopied(true);
            setTimeout(() => setCopied(false), 2000);
        }
    }, [code]);

    const langLabel = language || "code";

    return (
        <div className="overflow-hidden rounded-lg border border-zinc-200 dark:border-zinc-700">
            {/* Title bar */}
            <div className="flex items-center justify-between border-b border-zinc-200 bg-zinc-100 px-3 py-1.5 dark:border-zinc-700 dark:bg-zinc-800">
                <div className="flex items-center gap-1.5">
                    <button
                        onClick={() => setCollapsed(!collapsed)}
                        className="flex items-center justify-center rounded p-0.5 text-zinc-500 hover:text-zinc-700 hover:bg-zinc-200 dark:text-zinc-400 dark:hover:text-zinc-200 dark:hover:bg-zinc-700"
                        aria-label={collapsed ? "Expand code" : "Collapse code"}
                    >
                        {collapsed ? (
                            <ChevronRight className="h-3.5 w-3.5" />
                        ) : (
                            <ChevronDown className="h-3.5 w-3.5" />
                        )}
                    </button>
                    <span className="text-xs font-medium text-zinc-500 dark:text-zinc-400">
                        {langLabel}
                    </span>
                </div>
                <button
                    onClick={handleCopy}
                    className="flex items-center gap-1 rounded px-1.5 py-0.5 text-xs text-zinc-500 hover:text-zinc-700 hover:bg-zinc-200 dark:text-zinc-400 dark:hover:text-zinc-200 dark:hover:bg-zinc-700"
                    aria-label="Copy code"
                >
                    {copied ? (
                        <>
                            <Check className="h-3 w-3" />
                            Copied
                        </>
                    ) : (
                        <>
                            <Copy className="h-3 w-3" />
                            Copy
                        </>
                    )}
                </button>
            </div>

            {/* Code content — div directly fills the expanded area, no nested pre */}
            {!collapsed && (
                <div className="overflow-x-auto whitespace-pre-wrap bg-zinc-200/40 p-3 font-mono leading-relaxed text-zinc-500 dark:bg-zinc-900/60 dark:text-zinc-400" style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}>
                    {code}
                </div>
            )}
        </div>
    );
}
