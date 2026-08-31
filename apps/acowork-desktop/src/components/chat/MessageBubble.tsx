import React, { useState, useCallback, useEffect, useRef, Children, isValidElement, useMemo } from "react";
import { Copy, ChevronDown, ChevronRight, Wrench, AlertTriangle, RotateCcw } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatMessage } from "../../lib/types";
import { useTranslation } from "../../i18n/useTranslation";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { ThinkBlock } from "./ThinkBlock";
import { CodeBlock } from "./CodeBlock";
import { MermaidBlock } from "./MermaidBlock";
import { CompactionCard } from "./CompactionCard";
import { UserAvatar } from "../common/UserAvatar";
import { AttachmentChipRow } from "./AttachmentChipRow";
import { openAttachedRef } from "../../lib/openWorkspaceRef";
import { pickOpenActionForPath } from "../editor/markdownLinkResolver";
import type { AttachedItem } from "../../lib/types";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../common/ContextMenu";
import { copySelectionOrFallback, snapshotSelection } from "../../lib/clipboard";

// ── Utilities ─────────────────────────────────────────────────────────

/**
 * Strip common leading whitespace from multi-line strings.
 * Useful when a code block arrives indented inside a list item.
 */
function dedent(code: string): string {
  const lines = code.split("\n");
  const nonEmpty = lines.filter((l) => l.trim().length > 0);
  if (nonEmpty.length === 0) return code.trim();

  const minIndent = Math.min(
    ...nonEmpty.map((l) => l.match(/^ */)?.[0].length ?? 0),
  );
  return lines.map((l) => l.slice(minIndent)).join("\n").trim();
}

/**
 * Split streaming markdown content by mermaid code blocks so that
 * ReactMarkdown never sees the ```mermaid fences — it would otherwise
 * misparse them during streaming (e.g. swallowing the first diagram
 * into a larger "markdown"-language code block).
 *
 * Text segments → ReactMarkdown
 * Mermaid blocks → MermaidBlock (no fence, no indentation confusion)
 */
const StreamMarkdown = React.memo(function StreamMarkdown({ content }: { content: string }) {
  // Split on ```mermaid ... ``` (non-greedy, handles indented closing fences)
  const segments = content.split(/(```mermaid\n[\s\S]*?\n[ \t]*```)/g).filter(Boolean);

  if (segments.length <= 1) {
    // Fast path: no mermaid blocks at all
    return <ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>{content}</ReactMarkdown>;
  }

  return (
    <>
      {segments.map((seg, i) => {
        const mermaidMatch = seg.match(/^```mermaid\n([\s\S]*?)\n[ \t]*```$/);
        if (mermaidMatch) {
          const code = dedent(mermaidMatch[1]);
          return <MermaidBlock key={i} chart={code} />;
        }
        return (
          <ReactMarkdown key={i} remarkPlugins={[remarkGfm]} components={markdownComponents}>
            {seg}
          </ReactMarkdown>
        );
      })}
    </>
  );
}, (prev, next) => prev.content === next.content);

/** ReactMarkdown component overrides — code blocks with title bar */
const markdownComponents = {
  pre: ({ children }: { children?: React.ReactNode }) => {
    const childArray = Children.toArray(children);
    const codeEl = childArray.find(
      (child): child is React.ReactElement<{ className?: string; children?: React.ReactNode }> =>
        isValidElement(child) && child.type === "code"
    );
    if (codeEl) {
      const { className, children: codeContent } = codeEl.props;
      const language = className?.replace(/^language-/, "") || "";
      const code = dedent(Children.toArray(codeContent).join(""));
      return <CodeBlock language={language} code={code} />;
    }
    return <pre>{children}</pre>;
  },
  /** Intercept link clicks: open in the fileTab instead of navigating the
   *  webview (which would crash). Image extensions open in preview mode,
   *  every other supported text/source format (including JSON, Markdown,
   *  HTML, source code) opens in Monaco — same rule as the workspace tree
   *  and the chat banner chip. Previously this unconditionally opened
   *  `openPreview`, which falls through to `MarkdownPreviewView` for
   *  non-image / non-HTML files and freezes on multi-MB JSON. */
  a: ({ href, children, ...rest }: React.AnchorHTMLAttributes<HTMLAnchorElement>) => {
    const handleClick = (e: React.MouseEvent) => {
      if (!href) return;
      // Always prevent default to avoid Tauri webview navigation crash
      e.preventDefault();
      const agentId = useAgentStore.getState().selectedAgentId;
      if (!agentId) return;

      if (/^https?:\/\//i.test(href)) {
        // Case 1: http/https URLs — open in URL preview tab
        useFileEditorStore.getState().openUrl(agentId, href);
      } else {
        // Case 2: Local file paths — preview vs. Monaco decided by the
        // shared helper. The `href` is treated as a workspace-relative or
        // absolute path; `pickOpenActionForPath` only checks the extension.
        //
        // Strip a `#L42` / `#L42-L67` line-fragment before resolving: those
        // markers are IDE conventions understood by Monaco's cursor-jump
        // wiring, but `/workspaces/file` treats the path verbatim and a
        // path like `src/foo.ts#L42` is NOT a valid filesystem path on the
        // runtime side, so it would 404 every click. The fragment is
        // preserved separately and forwarded to `openFile(..., line)` so
        // the editor still lands on the correct line.
        const sessionId = useChatStore.getState().getActiveSessionId(agentId);
        if (!sessionId) return;
        const workspaceId = useWorkspaceStore.getState().getSessionWorkspaceId(sessionId);
        const [pathPart, fragmentPart = ""] = href.split("#", 2);
        const relPath = pathPart.replace(/^\//, "");
        // Match `#L42` or `#L42-L67`. Anything else (custom anchors, GitHub
        // permalinks, etc.) is ignored — file opens with no cursor jump.
        const lineMatch = fragmentPart.match(/^L(\d+)(?:-L(\d+))?$/);
        const cursorLine = lineMatch ? parseInt(lineMatch[1], 10) : undefined;
        const action = pickOpenActionForPath(relPath);
        const store = useFileEditorStore.getState();
        if (action === "openPreview") {
          // Previews don't honor `cursorLine` — they have no cursor to
          // place. Drop it silently rather than threading a no-op param.
          void store.openPreview(agentId, workspaceId, relPath);
        } else {
          void store.openFile(agentId, workspaceId, relPath, cursorLine);
        }
      }
    };
    return (
      <a href={href} onClick={handleClick} {...rest}>
        {children}
      </a>
    );
  },
  /** Wrap <table> in a horizontally-scrollable container so an
   *  oversized column (URL / hash / path) doesn't squeeze the
   *  title column mid-word. The wrapper is a pure scroll viewport —
   *  visual chrome (border / rounded corners) lives on the <table>
   *  itself in globals.css; the <th> cells stay nowrap so headers
   *  never break. */
  table: ({ children, ...rest }: React.TableHTMLAttributes<HTMLTableElement>) => (
    <div className="prose-table-scroll">
      <table {...rest}>{children}</table>
    </div>
  ),
};

// ── Message Bubble ─────���──────────────────────────────────────────────

/** Single message bubble */
const MessageBubble = React.memo(function MessageBubble({
  message,
  currentSessionId,
  liveUserName: liveUserNameProp,
  liveUserAvatarUrl,
  liveUserBuiltinAvatarId,
}: {
  message: ChatMessage;
  currentSessionId: string;
  liveUserName?: string;
  liveUserAvatarUrl?: string | null;
  liveUserBuiltinAvatarId?: string | null;
}) {
  // Convergent model: content always comes from the frozen message.
  const displayContent = message.content;
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  // Use CSS custom property for font size — set once in store, global effect
  const fontSizeStyle = { fontSize: "var(--ui-font-size, 0.875rem)" };
  // Live names — received as props from ChatPanel so React.memo can detect
  // profile changes (name/avatar edits update all rendered bubbles instantly).
  const liveUserName = liveUserNameProp ?? message.senderDisplayName;

  // ── Right-click context menu ──────────────────────────────────────────
  // Unified hook + component handles open/close/portal/position/escape.
  // The legacy in-component implementation carried two bugs:
  //   1. `handleContextMenu` read `window.getSelection()` at right-click
  //      time and used it to gate the Copy item — but a bare right-click
  //      creates no selection, so the item was always disabled until the
  //      user pre-drag-selected some text first.
  //   2. `handleCopy` re-read `window.getSelection()` inside the button's
  //      onClick. WKWebView / WebKit clear the page selection the moment
  //      a `<button>` gains focus on mouseup, so by the time onClick
  //      fired the selection string was empty and nothing got copied.
  // Both bugs are fixed here by snapshotting the selection at
  // right-click time (in `useContextMenu`) and forwarding it via
  // `selectionAtOpen`. See `src/lib/clipboard.ts` and
  // `src/components/common/ContextMenu/useContextMenu.ts` for the
  // cross-cutting implementation.
  const menu = useContextMenu();

  // ── Attachment chip handlers ────────────────────────────────────────
  // Stable per (currentSessionId) so re-renders don't churn React.memo
  // peers. Reads agentId / workspaceId via `getState()` to avoid wiring
  // every store into the bubble's reactive deps — same pattern used by
  // the `<a>` markdown-link handler further down.
  const handleChipClick = useCallback((item: AttachedItem) => {
    const agentId = useAgentStore.getState().selectedAgentId;
    if (!agentId) return;
    const workspaceId = useWorkspaceStore.getState().getSessionWorkspaceId(currentSessionId);
    void openAttachedRef({ item, agentId, currentWorkspaceId: workspaceId });
  }, [currentSessionId]);

  const handleChipRemove = useCallback(() => {
    const agentId = useAgentStore.getState().selectedAgentId;
    if (!agentId) return;
    useChatStore.getState().removeMessageAttachment(agentId, currentSessionId, message.id);
  }, [currentSessionId, message.id]);

  // ── Resend handler (user bubbles only) ─────────────────────────────
  // Reuses `sendMessage` which already handles message-id allocation,
  // optimistic insert, attached-item clearing and MQTT publish.  Reading
  // agentId via `getState()` keeps this callback's deps narrow and
  // stable — same pattern used by the chip handlers above.
  const handleResend = useCallback(() => {
    const agentId = useAgentStore.getState().selectedAgentId;
    if (!agentId) return;
    // Resend only carries the text body.  Any original attachments are
    // intentionally dropped — they were tied to a previous send and would
    // double-charge the upload pipeline on a fresh re-send.
    if (!message.content) return;
    void useChatStore.getState().sendMessage(message.content, agentId);
  }, [message.content]);

  // ── Copy handler ────────────────────────────────────────────────────
  // Copies whatever the user had selected when the menu opened. If the
  // user right-clicked without selecting anything first, fall back to the
  // whole message body — matches the macOS / Windows "right-click →
  // Copy" expectation. The selection snapshot is captured in
  // `useContextMenu.openAt` BEFORE button focus can clear it (the root
  // cause of the original bug).
  const handleCopy = useCallback((selectionAtOpen: string) => {
    void copySelectionOrFallback(selectionAtOpen || (message.content ?? ""));
  }, [message.content]);

  // ── Menu items ──────────────────────────────────────────────────────
  // useMemo keeps the array stable across renders — important because
  // `<ContextMenu>` is a child of the React.memo-wrapped MessageBubble;
  // a fresh array each render would force ContextMenu to re-render every
  // time even when nothing relevant changed. Items only depend on
  // content (Copy / Resend need it) and the two translation keys.
  const items = useMemo<ContextMenuItem[]>(() => {
    const list: ContextMenuItem[] = [];

    // Copy is always present on bubbles that have content. The Copy
    // action uses the selection-at-open snapshot from the click context,
    // so it works whether or not the user pre-selected text.
    if (message.content) {
      list.push({
        key: "copy",
        icon: <Copy size={14} />,
        label: t("chatPanel.copy"),
        onClick: ({ selectionAtOpen }) => {
          // WebKit guard (see `useEffect` above): we snapshotted the
          // selection at the CAPTURE phase of right-button mousedown,
          // BEFORE WebKit's auto word-select ran. If the snapshot equals
          // the contextmenu-time selection, the user had nothing
          // pre-selected and the non-empty value is purely WebKit noise
          // — pass an empty string so `handleCopy` falls back to the
          // whole message body.
          const hadUserSelection =
            selectionAtMousedownRef.current.length > 0 &&
            selectionAtMousedownRef.current !== selectionAtOpen;
          const userSelection = hadUserSelection ? selectionAtOpen : "";
          void handleCopy(userSelection);
        },
      });
    }

    // Resend is user-bubble-only. No disabled state — either the
    // message has content (we got past the `if` above) or the item is
    // not rendered at all.
    if (message.type === "user" && message.content) {
      list.push({
        key: "resend",
        icon: <RotateCcw size={14} />,
        label: t("chatPanel.resend"),
        onClick: () => handleResend(),
      });
    }

    return list;
  }, [message.content, message.type, handleCopy, handleResend, t]);

  // Common right-click trigger — bound to whichever layout wrapper the
  // message type uses (see each branch below). No payload: items read
  // what they need from their closures (message, handlers).
  const onContextMenu = useCallback(
    (e: React.MouseEvent) => menu.openAt(e),
    [menu],
  );

  // ── WebKit auto-selection guard ───────────────────────────────────────
  // Bug: WebKit / WKWebView (macOS Tauri) auto-selects the word under
  // the cursor on right-click as a native context-menu prep step. By the
  // time `contextmenu` fires, `window.getSelection()` is non-empty even
  // when the user did NOT pre-select anything — and `copySelectionOrFallback`
  // never falls back to the whole message body. Symptom: right-clicking a
  // user bubble copies just one or two characters.
  //
  // Detection strategy: WebKit's word-select happens as the default
  // action of the right-click `mousedown`. To capture the user's TRUE
  // selection (the one that existed BEFORE WebKit auto-selects), we have
  // to listen on the DOCUMENT at the CAPTURE phase — React's synthetic
  // event on the bubble div fires during bubbling, which is AFTER the
  // default action has already mutated the selection. Capture-phase
  // listeners fire before any default action.
  //
  // We compare the capture-phase selection against the contextmenu-phase
  // selection:
  //   - equal  → user had nothing pre-selected; the non-empty value is
  //              purely WebKit noise → fall back to `message.content`.
  //   - differ → user did pre-select; the contextmenu value is what they
  //              want (WebKit may have extended the range, which is fine).
  //
  // The capture listener is bound for the lifetime of the bubble; the
  // cost is one selection-read per mousedown, which is negligible.
  const selectionAtMousedownRef = useRef<string>("");
  useEffect(() => {
    const onDocMouseDownCapture = (e: MouseEvent) => {
      // Right button only — left clicks should not interfere with
      // in-progress text drag-selections the user is still making.
      if (e.button !== 2) return;
      selectionAtMousedownRef.current = snapshotSelection();
    };
    document.addEventListener("mousedown", onDocMouseDownCapture, true);
    return () => {
      document.removeEventListener("mousedown", onDocMouseDownCapture, true);
    };
  }, []);

  if (message.type === "user") {
    return (
      <>
        <div className="flex items-start justify-end gap-2" onContextMenu={onContextMenu}>
          <div className="min-w-0 flex-1 flex flex-col items-end">
            {liveUserName && (
              <span className="mt-[2px] text-xs text-zinc-400 dark:text-zinc-500">{liveUserName}</span>
            )}
            {message.content && (
              <div className="mt-[6px] max-w-[85%] rounded-md rounded-br-sm bg-chat-user px-4 py-2.5 text-chat-user-text select-text whitespace-pre-wrap break-words max-h-48 overflow-y-auto" style={fontSizeStyle}>
                {message.content}
              </div>
            )}
          </div>
          <UserAvatar
            displayName={liveUserName}
            avatarUrl={liveUserAvatarUrl ?? null}
            builtinAvatarId={liveUserBuiltinAvatarId ?? null}
            size={40}
            className="shrink-0 mt-1"
          />
        </div>
        <ContextMenu
          isOpen={menu.isOpen}
          menuProps={menu.menuProps}
          items={items}
          payload={undefined}
          selectionAtOpen={menu.selectionAtOpen}
          onClose={menu.close}
          compact
        />
      </>
    );
  }

  if (message.type === "assistant") {
    if (!displayContent) return null;
    return (
      <>
        <div className="min-w-0 flex flex-col ml-12" onContextMenu={onContextMenu}>
          <div className="max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
            <div className="prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text break-words [&_th]:bg-chat-title [&_td]:bg-chat-body [&_tbody_tr]:!bg-transparent" style={fontSizeStyle}>
              <StreamMarkdown content={displayContent} />
            </div>
          </div>
        </div>
        <ContextMenu
          isOpen={menu.isOpen}
          menuProps={menu.menuProps}
          items={items}
          payload={undefined}
          selectionAtOpen={menu.selectionAtOpen}
          onClose={menu.close}
          compact
        />
      </>
    );
  }

  if (message.type === "thought") {
    return (
      <>
        <div className="min-w-0 flex flex-col ml-12" onContextMenu={onContextMenu}>
          <div className="max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
            <ThinkBlock
              content={message.content}
              startTime={message.startTime}
              endTime={message.endTime}
            />
          </div>
        </div>
        <ContextMenu
          isOpen={menu.isOpen}
          menuProps={menu.menuProps}
          items={items}
          payload={undefined}
          selectionAtOpen={menu.selectionAtOpen}
          onClose={menu.close}
          compact
        />
      </>
    );
  }

  if (message.type === "error") {
    return (
      <>
        <div className="min-w-0 flex flex-col ml-12" onContextMenu={onContextMenu}>
          <div className="max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 dark:text-zinc-200 select-text break-words overflow-hidden" style={fontSizeStyle}>
            <div className="flex items-start gap-2 min-w-0">
              <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-500" />
              <div className="min-w-0 flex-1">
                <div className="whitespace-pre-wrap break-words">{message.content}</div>
                {message.errorDetail && (
                  <details className="mt-2">
                    <summary className="cursor-pointer text-xs text-zinc-400 hover:text-zinc-300 dark:text-zinc-500 dark:hover:text-zinc-400 select-none">
                      Details
                    </summary>
                    <pre className="mt-1 max-h-40 overflow-auto rounded bg-black/5 dark:bg-white/5 p-2 text-xs text-zinc-500 dark:text-zinc-400 whitespace-pre-wrap break-all">
                      {message.errorDetail}
                    </pre>
                  </details>
                )}
              </div>
            </div>
          </div>
        </div>
        <ContextMenu
          isOpen={menu.isOpen}
          menuProps={menu.menuProps}
          items={items}
          payload={undefined}
          selectionAtOpen={menu.selectionAtOpen}
          onClose={menu.close}
          compact
        />
      </>
    );
  }

  if (message.type === "system") {
    const meta = message.metadata;
    const metaType = meta?.type as string | undefined;

    // ADR-046 §2.5: 5 attachment-type system entries. Each renders a
    // single `AttachmentChipRow` chip. The raw metadata from the JSONL
    // entry is reconstructed into the `AttachedItem` discriminated union.
    if (!meta) {
      return (
        <>
          <div className="flex justify-center" onContextMenu={onContextMenu}>
            <div className="rounded bg-chat-bubble px-3 py-1 text-xs text-zinc-500 dark:text-zinc-400 select-text">
              {message.content}
            </div>
          </div>
          <ContextMenu
            isOpen={menu.isOpen}
            menuProps={menu.menuProps}
            items={items}
            payload={undefined}
            selectionAtOpen={menu.selectionAtOpen}
            onClose={menu.close}
            compact
          />
        </>
      );
    }

    if (metaType === "file_upload") {
      return (
        <>
          <div className="flex justify-center" onContextMenu={onContextMenu}>
            <AttachmentChipRow
              item={{
                type: "file_upload",
                documentId: (meta.document_id as string) ?? "",
                filename: (meta.filename as string) ?? "",
                format: (meta.format as string) ?? "",
                sizeBytes: (meta.size_bytes as number) ?? 0,
              }}
              compact
              onRemove={handleChipRemove}
              pending={!!message._isOptimistic}
            />
          </div>
          <ContextMenu
            isOpen={menu.isOpen}
            menuProps={menu.menuProps}
            items={items}
            payload={undefined}
            selectionAtOpen={menu.selectionAtOpen}
            onClose={menu.close}
            compact
          />
        </>
      );
    }

    if (metaType === "image_upload") {
      // Need the selected agent ID for the blob fetch. Use the global
      // store rather than threading a prop through every bubble.
      const selectedAgentId = useAgentStore.getState().selectedAgentId;
      return (
        <>
          <div className="flex justify-center" onContextMenu={onContextMenu}>
            <AttachmentChipRow
              item={{
                type: "image_upload",
                documentId: (meta.document_id as string) ?? "",
                filename: (meta.filename as string) ?? "",
                format: (meta.format as string) ?? "",
                sizeBytes: (meta.size_bytes as number) ?? 0,
                width: typeof meta.width === "number" ? meta.width : undefined,
                height: typeof meta.height === "number" ? meta.height : undefined,
              }}
              agentId={selectedAgentId}
              compact
              onRemove={handleChipRemove}
              pending={!!message._isOptimistic}
            />
          </div>
          <ContextMenu
            isOpen={menu.isOpen}
            menuProps={menu.menuProps}
            items={items}
            payload={undefined}
            selectionAtOpen={menu.selectionAtOpen}
            onClose={menu.close}
            compact
          />
        </>
      );
    }

    if (metaType === "attached_file") {
      return (
        <>
          <div className="flex justify-center" onContextMenu={onContextMenu}>
            <AttachmentChipRow
              item={{
                type: "attached_file",
                absPath: (meta.abs_path as string) ?? "",
                name: (meta.name as string) ?? (meta.abs_path as string) ?? "",
              }}
              compact
              onChipClick={handleChipClick}
              onRemove={handleChipRemove}
              pending={!!message._isOptimistic}
            />
          </div>
          <ContextMenu
            isOpen={menu.isOpen}
            menuProps={menu.menuProps}
            items={items}
            payload={undefined}
            selectionAtOpen={menu.selectionAtOpen}
            onClose={menu.close}
            compact
          />
        </>
      );
    }

    if (metaType === "attached_selection") {
      return (
        <>
          <div className="flex justify-center" onContextMenu={onContextMenu}>
            <AttachmentChipRow
              item={{
                type: "attached_selection",
                absPath: (meta.abs_path as string) ?? "",
                name: (meta.name as string) ?? (meta.abs_path as string) ?? "",
                startLine: (meta.start_line as number) ?? 1,
                endLine: (meta.end_line as number) ?? 1,
              }}
              compact
              onChipClick={handleChipClick}
              onRemove={handleChipRemove}
              pending={!!message._isOptimistic}
            />
          </div>
          <ContextMenu
            isOpen={menu.isOpen}
            menuProps={menu.menuProps}
            items={items}
            payload={undefined}
            selectionAtOpen={menu.selectionAtOpen}
            onClose={menu.close}
            compact
          />
        </>
      );
    }

    if (metaType === "attached_folder") {
      return (
        <>
          <div className="flex justify-center" onContextMenu={onContextMenu}>
            <AttachmentChipRow
              item={{
                type: "attached_folder",
                absPath: (meta.abs_path as string) ?? "",
                name: (meta.name as string) ?? (meta.abs_path as string) ?? "",
              }}
              compact
              onChipClick={handleChipClick}
              onRemove={handleChipRemove}
              pending={!!message._isOptimistic}
            />
          </div>
          <ContextMenu
            isOpen={menu.isOpen}
            menuProps={menu.menuProps}
            items={items}
            payload={undefined}
            selectionAtOpen={menu.selectionAtOpen}
            onClose={menu.close}
            compact
          />
        </>
      );
    }

    // Fallback: generic system message (non-attachment system entries
    // like session notifications, etc.).
    return (
      <>
        <div className="flex justify-center" onContextMenu={onContextMenu}>
          <div className="rounded bg-chat-bubble px-3 py-1 text-xs text-zinc-500 dark:text-zinc-400 select-text">
            {message.content}
          </div>
        </div>
        <ContextMenu
          isOpen={menu.isOpen}
          menuProps={menu.menuProps}
          items={items}
          payload={undefined}
          selectionAtOpen={menu.selectionAtOpen}
          onClose={menu.close}
          compact
        />
      </>
    );
  }

  if (message.type === "compaction") {
    return (
      <>
        <div className="ml-12" onContextMenu={onContextMenu}>
          <CompactionCard
            summary={message.content}
            meta={message.compactionMeta}
            timestampMs={message.timestamp}
          />
        </div>
        <ContextMenu
          isOpen={menu.isOpen}
          menuProps={menu.menuProps}
          items={items}
          payload={undefined}
          selectionAtOpen={menu.selectionAtOpen}
          onClose={menu.close}
          compact
        />
      </>
    );
  }

  if (message.type === "tool_call") {
    return (
      <div className="flex justify-start">
        <div className="flex w-full items-center gap-2 rounded-md border border-zinc-200 bg-zinc-50 px-3 py-1.5 text-left text-xs text-zinc-500 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800">
          <button
            className="flex flex-1 items-start gap-2 min-w-0"
            onClick={() => setExpanded(!expanded)}
          >
            <Wrench className="mt-0.5 h-3 w-3 shrink-0" />
            <span className="font-medium">{message.toolName}</span>
            <span className="min-w-0 break-all text-zinc-400 dark:text-zinc-500">{message.content}</span>
            {expanded ? <ChevronDown className="ml-auto h-3 w-3 shrink-0" /> : <ChevronRight className="ml-auto h-3 w-3 shrink-0" />}
          </button>
        </div>
      </div>
    );
  }

  if (message.type === "tool_result") {
    return (
      <>
        <div className="flex justify-start" onContextMenu={onContextMenu}>
          <button
            className="flex w-full items-center gap-2 rounded-md border border-zinc-200 bg-zinc-50 px-3 py-1.5 text-left text-xs text-zinc-500 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800"
            onClick={() => setExpanded(!expanded)}
          >
            <Wrench className="h-3 w-3 shrink-0" />
            <span className="font-medium">{message.toolName}</span>
            <span className="text-zinc-400 dark:text-zinc-500">→ Result</span>
            <span className="ml-auto text-[10px] text-zinc-400 dark:text-zinc-500">Click to view</span>
            {expanded ? <ChevronDown className="ml-2 h-3 w-3 shrink-0" /> : <ChevronRight className="ml-2 h-3 w-3 shrink-0" />}
          </button>
          {expanded && (
            <pre className="mt-1 max-w-full overflow-x-auto rounded-md bg-zinc-50 p-3 text-xs text-zinc-600 dark:bg-zinc-800/50 dark:text-zinc-400 select-text">
              {message.content}
            </pre>
          )}
        </div>
        <ContextMenu
          isOpen={menu.isOpen}
          menuProps={menu.menuProps}
          items={items}
          payload={undefined}
          selectionAtOpen={menu.selectionAtOpen}
          onClose={menu.close}
          compact
        />
      </>
    );
  }

  return null;
}, (prev, next) => {
  // Convergent model: message objects are frozen records from HTTP.
  // Reference equality is sufficient.
  return prev.message === next.message
    && prev.liveUserName === next.liveUserName
    && prev.liveUserAvatarUrl === next.liveUserAvatarUrl
    && prev.liveUserBuiltinAvatarId === next.liveUserBuiltinAvatarId;
});

export { MessageBubble };