/**
 * CommitPicker — small popover for selecting a git revision (commit or
 * Working Tree). Used by the diff banner buttons in FileEditorPanel to
 * change which revisions the side-by-side Diff diff compares.
 *
 * ADR-078 extension: previously the only option was HEAD vs working
 * tree. The banner now has two buttons ("base" / "compare"); clicking
 * either opens this picker so the user can pick any commit on the
 * file's history. The `compare` picker additionally offers "Working
 * Tree" (special ref = "") as a top option so the user can flip back.
 *
 * Position: anchored under `anchorEl`, fixed-width (320px), capped
 * height with internal scroll so long histories stay usable.
 */

import React, { useEffect, useMemo, useState } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useGitStore } from "../../stores/gitStore";

interface CommitPickerProps {
    /** Anchor the popover positions itself under. Required while open. */
    anchorEl: HTMLElement | null;
    /** Active ref for this side — used to highlight the current selection. */
    currentRef: string;
    /** Working tree ref ("") — pass `true` for the compare-side picker,
     *  `false` for the base-side picker (which only accepts commits). */
    allowWorkingTree: boolean;
    /** Fetch path scope — null disables the fetch (caller is closing). */
    agentId: string;
    workspaceId: string;
    relPath: string;
    onSelect: (ref: string, label: string) => void;
    onClose: () => void;
}

/** Working Tree is encoded as the empty string on the wire (see
 *  core/acowork-runtime/src/usecases/git_query_impl.rs `diff()`). The
 *  diff banner shows it as a literal "Working Tree" label. */
const WORKING_TREE_REF = "";
const WORKING_TREE_LABEL_KEY = "gitStatus.workingTreeLabel";

export function CommitPicker({
    anchorEl,
    currentRef,
    allowWorkingTree,
    agentId,
    workspaceId,
    relPath,
    onSelect,
    onClose,
}: CommitPickerProps) {
    const { t } = useTranslation();
    const fetchLog = useGitStore((s) => s.fetchLog);
    const [commits, setCommits] = useState<
        Array<{ hash: string; shortHash: string; subject: string; author: string; date: string }>
    >([]);
    const [loading2, setLoading2] = useState(false);

    // Fetch on mount (or when path changes). 50 mirrors the log page size.
    useEffect(() => {
        let cancelled = false;
        setLoading2(true);
        void fetchLog(agentId, workspaceId, relPath, 50)
            .then((log) => {
                if (cancelled) return;
                setCommits(log.commits);
            })
            .catch((err) => {
                console.error("[CommitPicker] fetchLog failed:", err);
                if (!cancelled) setCommits([]);
            })
            .finally(() => {
                if (!cancelled) setLoading2(false);
            });
        return () => {
            cancelled = true;
        };
    }, [agentId, workspaceId, relPath, fetchLog]);

    // Close on Escape; close on outside click.
    useEffect(() => {
        if (!anchorEl) return;
        const onKey = (e: KeyboardEvent) => {
            if (e.key === "Escape") onClose();
        };
        const onDown = (e: MouseEvent) => {
            const popover = document.getElementById("commit-picker-popover");
            if (popover && popover.contains(e.target as Node)) return;
            if (anchorEl.contains(e.target as Node)) return;
            onClose();
        };
        document.addEventListener("keydown", onKey);
        document.addEventListener("mousedown", onDown);
        return () => {
            document.removeEventListener("keydown", onKey);
            document.removeEventListener("mousedown", onDown);
        };
    }, [anchorEl, onClose]);

    // Position under the anchor.
    const style = useMemo<React.CSSProperties>(() => {
        if (!anchorEl) return { display: "none" };
        const r = anchorEl.getBoundingClientRect();
        return {
            position: "fixed",
            top: r.bottom + 4,
            left: Math.min(r.left, window.innerWidth - 340),
            width: 320,
            zIndex: 50,
        };
    }, [anchorEl]);

    if (!anchorEl) return null;

    return (
        <div
            id="commit-picker-popover"
            role="listbox"
            aria-label={t("gitStatus.commitPickerLabel")}
            style={style}
            className="rounded-md border border-right-panel-border bg-white shadow-lg dark:bg-zinc-800"
        >
            <div className="max-h-72 overflow-y-auto py-1 text-xs">
                {allowWorkingTree && (
                    <button
                        type="button"
                        role="option"
                        aria-selected={currentRef === WORKING_TREE_REF}
                        onClick={() =>
                            onSelect(WORKING_TREE_REF, t(WORKING_TREE_LABEL_KEY))
                        }
                        className={
                            "flex w-full items-center gap-2 px-3 py-1.5 text-left transition-colors " +
                            (currentRef === WORKING_TREE_REF
                                ? "bg-blue-50 font-medium text-blue-700 dark:bg-blue-900/40 dark:text-blue-200"
                                : "text-zinc-700 hover:bg-zinc-100 dark:text-zinc-200 dark:hover:bg-zinc-700")
                        }
                    >
                        <span className="shrink-0 rounded bg-zinc-200 px-1 py-px font-mono text-[10px] text-zinc-600 dark:bg-zinc-600 dark:text-zinc-200">
                            WT
                        </span>
                        <span className="truncate">{t(WORKING_TREE_LABEL_KEY)}</span>
                    </button>
                )}
                {loading2 && (
                    <div className="px-3 py-2 text-zinc-400">
                        {t("gitStatus.loading")}
                    </div>
                )}
                {!loading2 && commits.length === 0 && (
                    <div className="px-3 py-2 text-zinc-400">
                        {t("gitStatus.noCommits")}
                    </div>
                )}
                {commits.map((c) => {
                    const selected = c.hash === currentRef;
                    return (
                        <button
                            key={c.hash}
                            type="button"
                            role="option"
                            aria-selected={selected}
                            title={`${c.hash}\n${c.author}  ${c.date}\n${c.subject}`}
                            onClick={() => onSelect(c.hash, c.shortHash)}
                            className={
                                "flex w-full items-center gap-2 px-3 py-1.5 text-left transition-colors " +
                                (selected
                                    ? "bg-blue-50 dark:bg-blue-900/40"
                                    : "hover:bg-zinc-100 dark:hover:bg-zinc-700")
                            }
                        >
                            <span className="shrink-0 rounded bg-zinc-200 px-1 py-px font-mono text-[10px] text-zinc-600 dark:bg-zinc-600 dark:text-zinc-200">
                                {c.shortHash}
                            </span>
                            <span className="min-w-0 flex-1 truncate text-zinc-700 dark:text-zinc-200">
                                {c.subject}
                            </span>
                            <span className="shrink-0 text-[10px] text-zinc-400 dark:text-zinc-500">
                                {c.author}
                            </span>
                        </button>
                    );
                })}
            </div>
        </div>
    );
}