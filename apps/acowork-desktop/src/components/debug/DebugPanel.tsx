import { useEffect, useState, useCallback } from "react";
import { cn } from "../../lib/utils";
import { Tooltip } from "../common/Tooltip";
import { useTranslation } from "../../i18n/useTranslation";
import { redistributeTokensByBytes } from "../../lib/contextUsageBreakdown";
import {
  ChevronDown,
  ChevronRight,
  Loader,
  Rewind,
  Edit3,
  Check,
  X,
  Copy,
} from "lucide-react";

interface SectionContentType {
  content: string;
  hash: string;
  token_count: number;
}

/** Mirrors backend `SectionMeta` (ADR-054: each entry carries its key). */
export interface SectionMetaType {
  key: string;
  size_bytes: number;
  token_estimate: number;
  hash: string;
}

export type { SectionContentType };

export const SECTION_LABELS: Record<string, string> = {
  system_prompt: "System Prompt",
  workspace_context: "Workspace Context",
  environment: "Environment",
  tool_definitions: "Tool Definitions",
  skill_instructions: "Skill Instructions",
  // ADR-051 P3: this section is the AUTO-INJECTED memory (retrieve_and_inject
  // runs every user turn before the LLM call) — it is NOT a memory_recall
  // tool call, so the chat conversation will not show it as a tool step.
  retrieved_memory: "Retrieved Memory",
  identity_context: "Identity Context",
  // ADR-054 step 3: sections previously merged/lost, now standalone.
  workspace_prompt_file: "Workspace Prompt File",
  // ADR-060: Block C — an independent User-role message (Ephemeral cache
  // breakpoint) emitted AFTER the history block, not a system-prompt
  // sub-item anymore. Content/key unchanged; only grouping/label moved.
  todo_context: "Active Task List",
  ambiguous_confirmation_hint: "Memory Conflicts Hint",
  // ADR-054 step 4: lazy-loaded; refreshed at iteration end so it includes
  // the current iteration's assistant reply (not just the pre-LLM history).
  messages: "Conversation Messages",
};

export const SECTION_ORDER = [
  // Strictly follows ContextBuilder::build() injection order so the UI
  // reproduces the system prompt the LLM actually sees (ADR-054 §3.2).
  "system_prompt",
  "identity_context",
  "workspace_context",
  "retrieved_memory",
  "ambiguous_confirmation_hint",
  "skill_instructions",
  "environment",
  "workspace_prompt_file",
  "tool_definitions",
  "messages",
  // ADR-060: todo_context is Block C — after the history/messages block.
  "todo_context",
];

export function formatBytes(bytes: number): string {
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

/**
 * Format a temperature value for the request-params metadata bar.
 * Shows at most 3 decimal places (kills float noise like
 * 0.10000000000000001) but keeps at least 2 (0.1 → "0.10").
 */
export function formatTemperature(value: number): string {
  const fixed = value.toFixed(3);
  const trimmed = fixed.replace(/0+$/, "").replace(/\.$/, "");
  const [int, frac] = trimmed.split(".");
  return frac && frac.length >= 2 ? trimmed : `${int}.${(frac ?? "").padEnd(2, "0")}`;
}

// ── Conversation messages viewer (ADR-054 step 4) ──────────────────────
//
// The `messages` section's content is a JSON array of ChatMessage
// (lazy-loaded from `getSection(iteration, "messages")`). Rendered as a
// scrollable list with per-role badges and collapsible tool_calls. This
// mirrors the wire shape of `acowork_core::providers::traits::ChatMessage`
// (role / content / name / tool_calls / reasoning_content / content_parts).

interface WireChatMessage {
  role?: string;
  content?: string;
  name?: string | null;
  tool_calls?: Array<{
    id?: string;
    type?: string;
    function?: { name?: string; arguments?: string };
  }>;
  reasoning_content?: string | null;
}

const ROLE_BADGE_CLASSES: Record<string, string> = {
  user: "bg-blue-100 text-blue-700 dark:bg-blue-900/40 dark:text-blue-300",
  assistant: "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300",
  tool: "bg-amber-100 text-amber-700 dark:bg-amber-900/40 dark:text-amber-300",
  system: "bg-zinc-200 text-zinc-600 dark:bg-zinc-700 dark:text-zinc-300",
};

function MessagesView({ content }: { content?: string }) {
  if (!content) {
    return (
      <div className="flex items-center gap-1.5 text-[10px] text-zinc-400">
        <Loader className="h-2.5 w-2.5 animate-spin" />
        Loading messages...
      </div>
    );
  }
  let messages: WireChatMessage[] = [];
  try {
    const parsed = JSON.parse(content);
    if (Array.isArray(parsed)) messages = parsed;
  } catch {
    // Not valid JSON — fall back to plain text (e.g. error payload).
    return (
      <pre className="max-h-64 overflow-y-auto whitespace-pre-wrap text-[10px] leading-relaxed text-zinc-600 dark:text-zinc-400">
        {content.slice(0, 4000)}
        {content.length > 4000 && <span className="text-zinc-400">... (truncated)</span>}
      </pre>
    );
  }
  return (
    <div className="max-h-64 space-y-1 overflow-y-auto pr-1">
      {messages.length === 0 && (
        <div className="text-[10px] text-zinc-400">(empty conversation)</div>
      )}
      {messages.map((m, i) => {
        const role = m.role ?? "unknown";
        return (
          <div
            key={i}
            className="rounded border-[0.5px] border-zinc-300 bg-zinc-50 px-1.5 py-0.5 dark:border-zinc-600 dark:bg-zinc-800/60"
          >
            <div className="flex flex-wrap items-center gap-1.5 text-[9px]">
              <span
                className={cn(
                  "rounded px-1 py-px font-medium uppercase",
                  ROLE_BADGE_CLASSES[role] ?? "bg-zinc-200 text-zinc-600 dark:bg-zinc-700 dark:text-zinc-300"
                )}
              >
                {role}
              </span>
              {m.name && <span className="font-mono text-zinc-400">{m.name}</span>}
              <span className="ml-auto font-mono text-zinc-400">#{i}</span>
            </div>
            {m.reasoning_content && (
              <details className="text-[9px] text-zinc-400">
                <summary className="cursor-pointer">reasoning_content</summary>
                <pre className="mt-0.5 whitespace-pre-wrap text-[10px] text-zinc-500 dark:text-zinc-400">
                  {m.reasoning_content}
                </pre>
              </details>
            )}
            <pre className="whitespace-pre-wrap text-[10px] leading-snug text-zinc-600 dark:text-zinc-400">
              {m.content ?? ""}
            </pre>
            {m.tool_calls && m.tool_calls.length > 0 && (
              <details className="text-[9px] text-zinc-400">
                <summary className="cursor-pointer">
                  tool_calls ({m.tool_calls.length})
                </summary>
                <pre className="mt-0.5 overflow-x-auto whitespace-pre-wrap text-[10px] text-zinc-500 dark:text-zinc-400">
                  {JSON.stringify(m.tool_calls, null, 2).slice(0, 2000)}
                </pre>
              </details>
            )}
          </div>
        );
      })}
    </div>
  );
}


// ── Sub-components ─────────────────────────────────────────────────────

export function ControlButton({
  children,
  onClick,
  title,
  active,
  disabled,
}: {
  children: React.ReactNode;
  onClick: () => void;
  title: string;
  active?: boolean;
  disabled?: boolean;
}) {
  return (
    <Tooltip content={title} variant="plain">
      <button
        onClick={onClick}
        disabled={disabled}
        className={cn(
          "rounded p-1.5 transition-colors",
          disabled
            ? "cursor-not-allowed text-zinc-300 dark:text-zinc-600"
            : active
              ? "bg-amber-100 text-amber-700 dark:bg-amber-900/30 dark:text-amber-400"
              : "text-zinc-500 hover:bg-zinc-200 hover:text-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
        )}
      >
        {children}
      </button>
    </Tooltip>
  );
}

export function StateLabel({
  label,
  value,
  highlight,
}: {
  label: string;
  value: string;
  highlight?: boolean;
}) {
  return (
    <>
      <span className="text-text-secondary">{label}</span>
      <span
        className={cn(
          "text-right font-mono",
          highlight ? "text-[var(--color-accent)] dark:text-[var(--color-accent)]" : "text-zinc-700 dark:text-zinc-300"
        )}
      >
        {value}
      </span>
    </>
  );
}

export function SnapshotNode({
  snapshot,
  expandedSections,
  sectionCache,
  editingSection,
  onToggleSection,
  onStartEdit,
  onCancelEdit,
  onSaveEdit,
  onEditChange,
  onRewind,
  getSection,
  /**
   * Real billed tokens for this iteration (`prompt_tokens + completion_tokens`
   * from the LLM's `UsageInfo`). When provided, per-section token counts
   * are redistributed in proportion to `size_bytes` so they sum exactly
   * to this number — instead of summing to the heuristic
   * `total_token_estimate` (which drifts because `token_estimate` is
   * computed independently per section via `count_text`).
   *
   * Pass `undefined` to keep the legacy heuristic display.
   */
  realTotalTokens,
}: {
  snapshot: {
    iteration: number;
    built_at: string;
    /** ADR-054: content-addressed list (was a Record of 7 hardcoded keys). */
    sections: SectionMetaType[];
    total_token_estimate: number;
    phase: string;
    /** ADR-054 step 2: control params of the ChatRequest that built this snapshot. */
    request_params?: {
      model?: string;
      temperature?: number | null;
      max_tokens?: number | null;
      reasoning_effort?: string | null;
      thinking_mode?: string | null;
    } | null;
  };
  expandedSections: Set<string>;
  sectionCache: Map<string, { content: string; hash: string; token_count: number }>;
  editingSection: { iteration: number; section: string; original: string; current: string } | null;
  onToggleSection: (section: string) => void;
  onStartEdit: (section: string, original: string) => void;
  onCancelEdit: () => void;
  onSaveEdit: (section: string, content: string) => void;
  onEditChange: (content: string) => void;
  onRewind: (iteration: number) => void;
  getSection: (iteration: number, section: string) => Promise<SectionContentType | null>;
  realTotalTokens?: number;
}) {
  const { t } = useTranslation();
  const [collapsed, setCollapsed] = useState(true);
  const [copied, setCopied] = useState(false);

  // ADR: when the caller hands us a real, billed token total for this
  // iteration, redistribute it across sections by `size_bytes` byte share.
  // The result sums exactly to `realTotalTokens` and replaces the
  // per-section `token_estimate` heuristics that the backend produces
  // independently via `count_text` (which drift and compound).
  //
  // Falls back to an empty map when the caller didn't supply an anchor or
  // when the snapshot has no byte data -- in those cases the per-row
  // display falls back to `section.token_estimate` (legacy behaviour).
  const realTokenBySection = realTotalTokens !== undefined && realTotalTokens > 0
    ? redistributeTokensByBytes(realTotalTokens, snapshot.sections)
    : {};
  const displayTotalTokens = realTotalTokens ?? snapshot.total_token_estimate;
  const usingRealAnchor = Object.keys(realTokenBySection).length > 0;

  const handleCopy = useCallback(async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = document.createElement("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      document.body.removeChild(ta);
    }
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  }, []);

  // Reset copied state when editing ends
  useEffect(() => {
    if (!editingSection) setCopied(false);
  }, [editingSection]);

  return (
    <div className="border-b border-zinc-100 dark:border-zinc-800">
      {/* Iteration header */}
      <div
        role="button"
        tabIndex={0}
        onClick={() => setCollapsed(!collapsed)}
        onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') setCollapsed(!collapsed); }}
        className="flex w-full items-center gap-2 rounded-md bg-zinc-50 px-2.5 py-1.5 text-left transition-colors hover:bg-zinc-100 dark:bg-zinc-800/30 dark:hover:bg-zinc-800/50 cursor-pointer"
      >
        {collapsed ? (
          <ChevronRight className="h-3.5 w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
        ) : (
          <ChevronDown className="h-3.5 w-3.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
        )}
        <span className="text-xs font-medium text-zinc-500 dark:text-zinc-400">
          Iteration #{snapshot.iteration}
        </span>
        <span className="ml-1 text-[10px] text-zinc-400 dark:text-zinc-500">
          ~{displayTotalTokens.toLocaleString()} tok
        </span>
        <Tooltip content={t("debugPanel.rewindToIteration", { iteration: snapshot.iteration })} variant="plain">
          <button
            onClick={(e) => {
              e.stopPropagation();
              onRewind(snapshot.iteration);
            }}
            className="ml-auto rounded p-0.5 text-zinc-400 transition-colors hover:bg-zinc-100 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
          >
          <Rewind className="h-3 w-3" />
        </button>
        </Tooltip>
      </div>

      {/* ADR-054 step 2: request params metadata bar — only non-empty
          entries are shown; the whole bar is hidden when nothing is set.
          One param per line so narrow panels don't wrap mid-value. */}
      {!collapsed &&
        (() => {
          const rp = snapshot.request_params;
          const items: string[] = [];
          if (rp?.model) items.push(`Model: ${rp.model}`);
          if (rp?.temperature != null) items.push(`Temperature: ${formatTemperature(rp.temperature)}`);
          if (rp?.max_tokens != null) items.push(`max_tokens: ${rp.max_tokens}`);
          if (rp?.reasoning_effort) items.push(`reasoning: ${rp.reasoning_effort}`);
          if (rp?.thinking_mode) items.push(`thinking: ${rp.thinking_mode}`);
          if (items.length === 0) return null;
          return (
            <div className="mx-2 mt-1 overflow-x-auto rounded border-[0.5px] border-zinc-200 bg-zinc-100/60 px-2 py-1 font-mono text-[10px] text-zinc-500 dark:border-zinc-700 dark:bg-zinc-800/40 dark:text-zinc-400">
              {items.map((item) => (
                <div key={item} className="whitespace-nowrap leading-4">
                  {item}
                </div>
              ))}
            </div>
          );
        })()}

      {/* Sections — ADR-054: render whatever the backend produced, sorted
          by SECTION_ORDER (build() injection order); unknown keys sort last
          and fall back to the raw key as label. */}
      {!collapsed && (
        <div className="ml-2 mt-1 rounded-md border-l-2 border-zinc-300 bg-zinc-50 pl-2 pr-1.5 py-1.5 space-y-0.5 dark:border-zinc-600 dark:bg-zinc-800/30">
          {[...snapshot.sections]
            .sort((a, b) => {
              const ia = SECTION_ORDER.indexOf(a.key);
              const ib = SECTION_ORDER.indexOf(b.key);
              return (ia === -1 ? 999 : ia) - (ib === -1 ? 999 : ib);
            })
            .map((section) => {
            const sectionKey = section.key;
            const cacheKey = `${snapshot.iteration}:${sectionKey}`;
            const isExpanded = expandedSections.has(cacheKey);
            const cachedContent = sectionCache.get(cacheKey);

            return (
              <div key={sectionKey}>
                {/* Section header */}
                <div className="flex w-full items-center gap-1.5 rounded-md bg-zinc-50 pl-2 pr-2 py-1.5 text-left transition-colors hover:bg-zinc-100 dark:bg-zinc-800/40 dark:hover:bg-zinc-800/60">
                  <button
                    onClick={() => onToggleSection(sectionKey)}
                    className="flex flex-1 items-center gap-1.5"
                  >
                    {isExpanded ? (
                      <ChevronDown className="h-2.5 w-2.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
                    ) : (
                      <ChevronRight className="h-2.5 w-2.5 shrink-0 text-zinc-400 dark:text-zinc-500" />
                    )}
                    <span className="text-[11px] text-zinc-500 dark:text-zinc-400">
                      {SECTION_LABELS[sectionKey] ?? sectionKey}
                    </span>
                    <span className="ml-auto text-[10px] text-zinc-400 dark:text-zinc-500">
                      {formatBytes(section.size_bytes)} / ~{(usingRealAnchor ? (realTokenBySection[section.key] ?? section.token_estimate) : section.token_estimate).toLocaleString()} tok
                    </span>
                  </button>
                  {/* Edit button — opens inline editor with the section's full content.
                      The messages section is read-only in ADR-054 step 4
                      (patch support is out of scope) — no editor for it. */}
                  {sectionKey !== "messages" && (
                  <Tooltip content={t("debugPanel.editSection")} variant="plain">
                  <button
                    onClick={async () => {
                      const cacheKey = `${snapshot.iteration}:${sectionKey}`;
                      const cached = sectionCache.get(cacheKey);
                      if (cached) {
                        onStartEdit(sectionKey, cached.content);
                      } else {
                        // Lazy-load the content first
                        const loaded = await getSection(snapshot.iteration, sectionKey);
                        if (loaded) {
                          onStartEdit(sectionKey, loaded.content);
                        }
                      }
                    }}
                    className="rounded p-0.5 text-zinc-400 transition-colors hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
                  >
                    <Edit3 className="h-2.5 w-2.5" />
                  </button>
                  </Tooltip>
                  )}
                </div>

                {/* Section content (lazy-loaded or inline-editing) */}
                {isExpanded && (
                  <div className="mx-2 mb-1.5 rounded border-[0.5px] border-zinc-300 bg-zinc-50 p-2 text-zinc-600 dark:border-zinc-600 dark:bg-zinc-900/40 dark:text-zinc-400">
                    {/* ADR-054 step 4: messages render as a conversation
                        list, not raw JSON text. */}
                    {sectionKey === "messages" ? (
                      <MessagesView content={cachedContent?.content} />
                    ) : editingSection &&
                      editingSection.iteration === snapshot.iteration &&
                      editingSection.section === sectionKey ? (
                      <div className="flex flex-col gap-1.5">
                        <textarea
                          value={editingSection.current}
                          onChange={(e) => onEditChange(e.target.value)}
                          className="max-h-48 min-h-40 w-full resize-y rounded border-[0.5px] border-[var(--color-accent)]/30 bg-modal-surface px-2 py-1 font-mono text-[10px] leading-relaxed text-zinc-700 outline-none dark:border-[var(--color-accent)]/50 dark:text-zinc-300"
                          autoFocus
                        />
                        <div className="flex items-center gap-1">
                          <button
                            onClick={() => onSaveEdit(sectionKey, editingSection.current)}
                            className="flex items-center gap-0.5 rounded bg-[var(--color-accent)] px-2 py-0.5 text-[10px] text-white transition-opacity hover:opacity-90"
                          >
                            <Check className="h-2.5 w-2.5" />
                            Apply
                          </button>
                          <button
                            onClick={onCancelEdit}
                            className="flex items-center gap-0.5 rounded px-2 py-0.5 text-[10px] text-zinc-500 transition-colors hover:bg-zinc-200 dark:text-zinc-400 dark:hover:bg-zinc-700"
                          >
                            <X className="h-2.5 w-2.5" />
                            Cancel
                          </button>
                          <Tooltip content={t("debugPanel.copyContent")} variant="plain">
                            <button
                              onClick={() => handleCopy(editingSection.current)}
                              className="ml-auto flex items-center gap-0.5 rounded px-2 py-0.5 text-[10px] text-zinc-500 transition-colors hover:bg-zinc-200 dark:text-zinc-400 dark:hover:bg-zinc-700"
                            >
                            {copied ? (
                              <>
                                <Check className="h-2.5 w-2.5" />
                                Copied
                              </>
                            ) : (
                              <>
                                <Copy className="h-2.5 w-2.5" />
                                Copy
                              </>
                            )}
                          </button>
                          </Tooltip>
                        </div>
                      </div>
                    ) : cachedContent ? (
                      <>
                        <div className="mb-1 flex items-center gap-2 text-[10px] text-zinc-400">
                          <span>{cachedContent.token_count} tokens</span>
                          <span className="font-mono">{cachedContent.hash.slice(0, 8)}</span>
                        </div>
                        <pre className="max-h-32 overflow-y-auto whitespace-pre-wrap text-[10px] leading-relaxed text-zinc-600 dark:text-zinc-400">
                          {cachedContent.content.slice(0, 2000)}
                          {cachedContent.content.length > 2000 && (
                            <span className="text-zinc-400">... (truncated)</span>
                          )}
                        </pre>
                      </>
                    ) : (
                      <div className="flex items-center gap-1.5 text-[10px] text-zinc-400">
                        <Loader className="h-2.5 w-2.5 animate-spin" />
                        Loading section...
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
