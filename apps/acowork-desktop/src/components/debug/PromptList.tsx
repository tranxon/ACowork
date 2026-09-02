/**
 * ADR-063 §3.7 — Debug panel "Prompts" entry list.
 *
 * Lists the 9 package-declared LLM prompt overrides that
 * `core/acowork-runtime/src/http/prompts.rs` exposes via
 * `GET /api/agents/{id}/prompts`. The Debug tab renders this list at the
 * top of its layout (collapsible, default collapsed) so operators can:
 *
 *   1. See at a glance which `prompts/<file>.md` files the package
 *      currently declares (overridden = true) vs. which fall back to the
 *      built-in default (overridden = false).
 *   2. Click an entry to open the file in the existing file-tab editor
 *      via `useFileEditorStore.openFileWithContent` — same interaction
 *      pattern as `AgentSetupTab`'s "edit risk rules file" button. When
 *      the package has no override, the editor is seeded with the
 *      built-in default wrapped in an HTML comment so the user can read
 *      what the LLM is currently using as a reference.
 *   3. Hit the Reload button (top-right) to push on-disk content into
 *      the live `AgentCore` Arc via the prompts namespace
 *      (`POST /api/agents/{id}/prompts/reload`, proxied through
 *      the Gateway — works without DevMode being enabled, see
 *      ADR-063 §3.7.7).
 *
 * The list is always rendered in the Debug tab regardless of DevMode
 * state (per ADR-063 §3.7: "全部 5 个状态始终显示"). When DevMode is
 * off, the other Debug panels are blank stubs and the prompts list
 * occupies the visible area.
 */
import { useEffect, useState, useCallback } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useDebugStore } from "../../stores/debugStore";
import { useAgentStore } from "../../stores/agentStore";
import { getGatewayUrl } from "../../lib/config";
import { ChevronDown, ChevronRight, RefreshCw, FileText, Loader2 } from "lucide-react";
import { log } from "../../lib/logger";
import { cn } from "../../lib/utils";

// ── Wire types (mirror Rust `http/prompts.rs` envelope shapes) ────────

export interface PromptMeta {
  /** Basename without `.md` (e.g. "summary", "compact-template"). */
  name: string;
  /** Relative path inside the package — `prompts/<file>.md`. */
  file: string;
  /** Short, user-facing description of what this prompt is for. */
  purpose: string;
  /** True iff `prompts/<file>.md` exists on disk. */
  overridden: boolean;
  /** True iff this prompt is mandatory (currently only `system.md`). */
  required: boolean;
  /** Size in bytes; 0 when `overridden = false`. */
  size_bytes: number;
  /** Built-in default text used when `overridden = false`. */
  fallback_constant: string;
}

interface ListPromptsResponse {
  agent_id: string;
  prompts: PromptMeta[];
}

interface GetPromptResponse extends PromptMeta {
  agent_id: string;
  /** UTF-8 content; `None` when the package does not declare the override. */
  content?: string | null;
}

interface ReloadPromptsResponse {
  agent_id: string;
  /** Always equals OVERRIDABLE_PROMPTS.len() (= 8). */
  reloaded_count: number;
  /** True iff the main-dialog system prompt (system.md + sections) was also rebuilt and pushed. */
  system_prompt_reloaded: boolean;
}

// ── Component ─────────────────────────────────────────────────────────

interface PromptListProps {
  /** Default open state; defaults to `false` — the list is collapsed on first paint. */
  defaultOpen?: boolean;
  /** Optional override for the active agent id (Debug tab uses `useDebugStore.debugAgentId`). */
  agentIdOverride?: string;
}

/**
 * Wrap a prompt body in a markdown/HTML comment so the editor shows the
 * built-in default as reference text without sending it to the LLM.
 *
 * The body is run through `escapeClosing()` to neutralise any `-->` in
 * the source string — otherwise a stray `-->` inside the default prompt
 * would prematurely terminate the comment block.
 */
function wrapAsComment(body: string): string {
  const escaped = body.replace(/-->/g, "--\\>");
  return `<!--\n${escaped}\n-->\n`;
}

export function PromptList({ defaultOpen = false, agentIdOverride }: PromptListProps) {
  const { t } = useTranslation();
  const debugAgentId = useDebugStore((s) => s.debugAgentId);
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const agentId = agentIdOverride ?? debugAgentId ?? selectedAgentId ?? "";

  const [open, setOpen] = useState(defaultOpen);
  const [prompts, setPrompts] = useState<PromptMeta[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [reloading, setReloading] = useState(false);
  const [reloadNotice, setReloadNotice] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    if (!agentId) return;
    setLoading(true);
    setError(null);
    try {
      const url = `${getGatewayUrl()}/api/agents/${agentId}/prompts`;
      const resp = await fetch(url);
      if (!resp.ok) {
        const text = await resp.text().catch(() => "");
        throw new Error(`HTTP ${resp.status}${text ? `: ${text}` : ""}`);
      }
      const data = (await resp.json()) as ListPromptsResponse;
      setPrompts(data.prompts);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      log.error("[PromptList] refresh failed:", msg);
      setError(msg);
      setPrompts(null);
    } finally {
      setLoading(false);
    }
  }, [agentId]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const openInEditor = useCallback(
    async (meta: PromptMeta) => {
      try {
        const url = `${getGatewayUrl()}/api/agents/${agentId}/prompts/${meta.name}`;
        const resp = await fetch(url);
        if (!resp.ok) {
          const text = await resp.text().catch(() => "");
          throw new Error(`HTTP ${resp.status}${text ? `: ${text}` : ""}`);
        }
        const data = (await resp.json()) as GetPromptResponse;
        // `content: null | undefined` = package does not declare the
        // override; seed the editor with the built-in default wrapped
        // in an HTML comment so the user can read what the LLM is
        // currently using without accidentally saving it as the
        // override value. Saving the file (after deleting the comment)
        // declares the override; the next L2 reload then pushes it
        // into the live `AgentCore` Arc.
        //
        // Required prompts (system.md) are the exception: when missing,
        // seed the editor with the built-in default verbatim as a
        // starting template — the user is expected to create the file,
        // not just read a reference. Saving it (with or without edits)
        // creates `prompts/system.md` and the next reload applies it.
        const seed =
          data.content ??
          (meta.required ? meta.fallback_constant : wrapAsComment(meta.fallback_constant));
        useFileEditorStore.getState().openFileWithContent(
          agentId,
          "__agent_home__",
          `prompts/${meta.file}`,
          seed,
          "markdown",
        );
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        log.error("[PromptList] openInEditor failed:", msg);
        setError(msg);
      }
    },
    [agentId],
  );

  const reload = useCallback(async () => {
    if (!agentId) return;
    setReloading(true);
    setReloadNotice(null);
    setError(null);
    try {
      // ADR-048 §D8: `reload_prompts` lives under the existing debug
      // namespace; the wildcard `/debug/{*rest}` forwards verbatim.
      const url = `${getGatewayUrl()}/api/agents/${agentId}/prompts/reload`;
      const resp = await fetch(url, { method: "POST" });
      if (!resp.ok) {
        const text = await resp.text().catch(() => "");
        throw new Error(`HTTP ${resp.status}${text ? `: ${text}` : ""}`);
      }
      // Success envelope carries no `ok` field (project convention: 2xx
      // returns the data payload, 4xx/5xx returns `{error, message}`).
      // `resp.ok` was already checked above, so reaching here means the
      // reload succeeded — no `data.ok` gate (it would be `undefined`).
      const data = (await resp.json()) as ReloadPromptsResponse;
      // ADR-063 §3.7.6: the reload also rebuilds the main-dialog system
      // prompt (system.md + all prompt sections) and pushes it to live
      // sessions. When that best-effort step failed, surface a hint so
      // the user knows system.md changes need an agent restart.
      setReloadNotice(
        data.system_prompt_reloaded
          ? t("prompts.reloadOk")
          : t("prompts.reloadSystemPromptFailed"),
      );
      // Refresh list so size_bytes / overridden reflect post-reload state.
      void refresh();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      log.error("[PromptList] reload failed:", msg);
      setError(msg);
    } finally {
      setReloading(false);
    }
  }, [agentId, refresh, t]);

  return (
    <div data-testid="prompt-list" className="m-3">
      {/* Top — ToolsTab-style section label above the card. */}
      <span className="block text-[10px] font-medium text-zinc-500 dark:text-zinc-400">
        {t("prompts.title")}
      </span>

      {/* Card — original style (header + body in one unit with
          bg-white/60 and border). The header row stays INSIDE the card
          (chevron + count on the left toggle the body; reload button on
          the right stops propagation so it doesn't toggle). The body
          uses a thin border-t to separate from the header when open. */}
      <div className="mt-1 rounded border border-zinc-200 bg-white/60 dark:border-zinc-700 dark:bg-zinc-900/60">
        <div className="flex w-full items-center gap-2 px-3 py-2">
          <button
            type="button"
            onClick={() => setOpen((v) => !v)}
            className="flex flex-1 items-center gap-2 text-left text-xs font-medium text-zinc-700 dark:text-zinc-300"
          >
            {open ? (
              <ChevronDown className="h-3.5 w-3.5" />
            ) : (
              <ChevronRight className="h-3.5 w-3.5" />
            )}
            <span>{t("prompts.title")}</span>
            {prompts && (
              <span className="ml-1 rounded bg-zinc-100 px-1.5 py-px text-[10px] font-mono text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                {prompts.filter((p) => p.overridden).length}/{prompts.length}
              </span>
            )}
          </button>
          {/* Reload — icon-only, matches the ControlButton style used by
              the debug action block (p-1.5 + 14px icon). */}
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              void reload();
            }}
            disabled={reloading}
            className="rounded p-1.5 transition-colors text-zinc-500 hover:bg-zinc-200 hover:text-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-700 dark:hover:text-zinc-200 disabled:cursor-not-allowed disabled:text-zinc-300 dark:disabled:text-zinc-600"
            aria-label={t("prompts.reloadAria")}
          >
            {reloading ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <RefreshCw className="h-3.5 w-3.5" />
            )}
          </button>
        </div>

        {open && (
          <div className="border-t border-zinc-200 dark:border-zinc-700">
            {loading && (
              <div className="flex items-center gap-1.5 px-3 py-2 text-[10px] text-zinc-400">
                <Loader2 className="h-3 w-3 animate-spin" />
                {t("prompts.loading")}
              </div>
            )}
            {error && (
              <div className="px-3 py-2 text-[10px] text-red-600 dark:text-red-400">
                {t("prompts.error", { message: error })}
              </div>
            )}
            {reloadNotice && (
              <div className="px-3 py-2 text-[10px] text-emerald-600 dark:text-emerald-400">
                {reloadNotice}
              </div>
            )}
            {prompts && !loading && (
              <ul className="divide-y divide-zinc-100 dark:divide-zinc-800">
                {/* Required prompts (system.md) first, then optional
                    overrides — stable order so the mandatory entry is
                    always visible without scrolling. */}
                {[...prompts]
                  .sort((a, b) => Number(b.required) - Number(a.required))
                  .map((p) => (
                  <li key={p.name}>
                    <button
                      type="button"
                      onClick={() => void openInEditor(p)}
                      className="flex w-full items-start gap-2 px-3 py-1.5 text-left hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
                      aria-label={t("prompts.openAria", { name: p.name })}
                    >
                      <FileText
                        className={cn(
                          "mt-0.5 h-3 w-3 flex-shrink-0",
                          p.overridden
                            ? "text-emerald-500 dark:text-emerald-400"
                            : "text-zinc-400 dark:text-zinc-500",
                        )}
                      />
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-2">
                          <span className="font-mono text-[11px] font-medium text-zinc-800 dark:text-zinc-200">
                            {p.name}
                          </span>
                          {p.required && !p.overridden && (
                            <span className="rounded bg-amber-100 px-1 py-px text-[9px] font-medium uppercase text-amber-700 dark:bg-amber-900/40 dark:text-amber-300">
                              {t("prompts.badgeMissing")}
                            </span>
                          )}
                          {p.required && (
                            <span className="rounded bg-red-100 px-1 py-px text-[9px] font-medium uppercase text-red-700 dark:bg-red-900/40 dark:text-red-300">
                              {t("prompts.badgeRequired")}
                            </span>
                          )}
                          {p.overridden ? (
                            <span className="rounded bg-emerald-100 px-1 py-px text-[9px] font-medium uppercase text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300">
                              {t("prompts.badgeOverridden")}
                            </span>
                          ) : (
                            !p.required && (
                              <span className="rounded bg-zinc-100 px-1 py-px text-[9px] font-medium uppercase text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                                {t("prompts.badgeBuiltin")}
                              </span>
                            )
                          )}
                        </div>
                        <div className="mt-0.5 text-[10px] text-zinc-500 dark:text-zinc-400">
                          {p.purpose}
                        </div>
                      </div>
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </div>

      {/* Bottom — ToolsTab-style help hint under the card. */}
      <p className="mt-1 text-[9px] text-zinc-400 dark:text-zinc-500">
        {t("prompts.help")}
      </p>
    </div>
  );
}
