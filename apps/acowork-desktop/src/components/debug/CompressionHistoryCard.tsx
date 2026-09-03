//! Compression History card (ADR-061 §15-16).
//!
//! DevMode diagnostics: lists every LLM-driven context compaction event
//! of the active session (level / before → after tokens / ratio / model).
//!
//! Pure frontend — no runtime changes: compaction events are persisted
//! as `kind="compaction"` JSONL entries (with `CompactionEventMeta` in
//! `metadata`) and served by the existing
//! `GET /api/agents/{id}/sessions/{sid}/messages` API (the same source
//! the chat stream's `CompactionCard` renders from).
//!
//! UI mirrors `SnapshotNode`'s collapse/expand style: each row collapses
//! to a single-line summary (time / level / ratio) and expands into a
//! vertical key-value block on click. Avoids the narrow-panel cramping
//! of the previous 6-column table.
import { useCallback, useEffect, useState } from "react";
import { ChevronDown, ChevronRight, Loader, RefreshCw } from "lucide-react";

import { getGatewayUrl } from "../../lib/config";
import { useTranslation } from "../../i18n/useTranslation";
import type { CompactionEventMeta, ConversationEntry } from "../../lib/types";

interface CompactionRow {
  ts: number;
  meta: CompactionEventMeta;
}

const HISTORY_LIMIT = 500;

function formatTokens(n: number): string {
  return n.toLocaleString("en-US");
}

function formatRatio(before: number, after: number): string {
  if (before <= 0) return "—%";
  const ratio = 1 - after / before;
  return `${(ratio * 100).toFixed(1)}%`;
}

function shortId(id?: string): string {
  if (!id) return "—";
  return id.length > 12 ? `${id.slice(0, 6)}…${id.slice(-4)}` : id;
}

function formatTime(ts: number): string {
  return new Date(ts).toLocaleTimeString("en-US", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function CompressionHistoryCard({
  agentId,
  sessionId,
}: {
  agentId: string | null;
  sessionId: string | null;
}) {
  const { t } = useTranslation();
  const [rows, setRows] = useState<CompactionRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Indices of expanded rows. Use a Set so multiple rows can be open at
  // once (mirrors SnapshotNode, which keeps each snapshot independent).
  const [expanded, setExpanded] = useState<Set<number>>(() => new Set());

  const load = useCallback(async () => {
    if (!agentId || !sessionId) {
      setRows([]);
      setError(null);
      return;
    }
    setRows(null);
    setError(null);
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/messages?limit=${HISTORY_LIMIT}&tail=true`,
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { messages: ConversationEntry[] };
      const compactions = (data.messages ?? [])
        .filter((e) => e.kind === "compaction")
        .map((e) => ({
          ts: new Date(e.ts).getTime(),
          meta: (e.metadata ?? {}) as unknown as CompactionEventMeta,
        }))
        .sort((a, b) => a.ts - b.ts);
      setRows(compactions);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [agentId, sessionId]);

  useEffect(() => {
    void load();
  }, [load]);

  const toggleExpanded = useCallback((idx: number) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(idx)) next.delete(idx);
      else next.add(idx);
      return next;
    });
  }, []);

  return (
    <div className="rounded-md border border-zinc-200 bg-modal-surface p-3 dark:border-zinc-700">
      <div className="mb-2 flex items-center justify-between">
        <span className="text-xs font-medium text-zinc-500 dark:text-zinc-400">
          {t("resultsPanel.compressionHistory", { count: rows?.length ?? 0 })}
        </span>
        <button
          type="button"
          onClick={() => void load()}
          title={t("resultsPanel.buttonRefresh")}
          className="rounded p-1 text-zinc-500 transition-colors hover:bg-zinc-200 hover:text-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
        >
          <RefreshCw className="h-3.5 w-3.5" />
        </button>
      </div>

      {rows === null && !error && (
        <div className="flex items-center justify-center gap-2 py-3 text-xs text-zinc-400">
          <Loader className="h-3.5 w-3.5 animate-spin" />
          {t("resultsPanel.loadingCompressionHistory")}
        </div>
      )}

      {error && (
        <div className="py-3 text-center text-xs text-red-500">
          {t("resultsPanel.compressionHistoryError")}: {error}
        </div>
      )}

      {rows !== null && !error && rows.length === 0 && (
        <div className="py-3 text-center text-xs text-zinc-400">
          {t("resultsPanel.noCompressionEvents")}
        </div>
      )}

      {rows !== null && !error && rows.length > 0 && (
        <div className="-mx-1">
          {rows.map((row, i) => {
            const m = row.meta;
            const isOpen = expanded.has(i);
            const time = formatTime(row.ts);
            const before = m.before_tokens ?? 0;
            const after = m.after_tokens ?? 0;
            const ratio = formatRatio(before, after);
            return (
              <div key={i} className="border-b border-zinc-100 dark:border-zinc-800">
                {/* Collapsed header — main info only */}
                <div
                  role="button"
                  tabIndex={0}
                  onClick={() => toggleExpanded(i)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      toggleExpanded(i);
                    }
                  }}
                  className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left transition-colors hover:bg-zinc-100 dark:hover:bg-zinc-800/50 cursor-pointer"
                >
                  {isOpen ? (
                    <ChevronDown className="h-3.5 w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
                  ) : (
                    <ChevronRight className="h-3.5 w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
                  )}
                  <span className="font-mono text-[11px] text-zinc-500 dark:text-zinc-400">
                    {time}
                  </span>
                  <span className={cnLevel(m.level)}>
                    Lv{m.level}
                  </span>
                  <span className="ml-auto font-mono text-[11px] text-zinc-700 dark:text-zinc-300">
                    {ratio}
                  </span>
                </div>

                {/* Expanded details — vertical key/value list, one per row */}
                {isOpen && (
                  <div className="mx-2 mb-2 mt-0.5 overflow-x-auto rounded border-[0.5px] border-zinc-200 bg-zinc-100/60 px-2 py-1 font-mono text-[10px] text-zinc-500 dark:border-zinc-700 dark:bg-zinc-800/40 dark:text-zinc-400">
                    <div className="whitespace-nowrap leading-4">
                      {t("resultsPanel.compTime")}: {time}
                    </div>
                    <div className="whitespace-nowrap leading-4">
                      {t("resultsPanel.compLevel")}: {m.level}
                    </div>
                    <div className="whitespace-nowrap leading-4">
                      {t("resultsPanel.compTokens")}: {formatTokens(before)} → {formatTokens(after)}
                    </div>
                    <div className="whitespace-nowrap leading-4">
                      {t("resultsPanel.compRatio")}: {ratio}
                    </div>
                    {m.model && (
                      <div
                        className="whitespace-nowrap leading-4"
                        title={m.model}
                      >
                        {t("resultsPanel.compModel")}: {m.model}
                      </div>
                    )}
                    {(m.compacted_from_id || m.compacted_to_id) && (
                      <div className="whitespace-nowrap leading-4">
                        {t("resultsPanel.compRange")}: {shortId(m.compacted_from_id)} → {shortId(m.compacted_to_id)}
                      </div>
                    )}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

/** Level badge: 8 (minimal form) is the most aggressive level — tint it
 *  amber so an unexpected deep compaction stands out at a glance. */
function cnLevel(level: number): string {
  if (level >= 8) {
    return "rounded bg-amber-100 px-1 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-900/30 dark:text-amber-400";
  }
  if (level >= 5) {
    return "rounded bg-orange-50 px-1 py-0.5 text-[10px] font-medium text-orange-600 dark:bg-orange-900/20 dark:text-orange-400";
  }
  return "rounded bg-zinc-100 px-1 py-0.5 text-[10px] font-medium text-zinc-600 dark:bg-zinc-800 dark:text-zinc-300";
}