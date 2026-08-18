import React, { useEffect, useRef, useState, useCallback, Children, isValidElement } from "react";
import { createPortal } from "react-dom";
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
import { useContextMenuPosition } from "../../hooks/useContextMenuPosition";
import { MermaidBlock } from "./MermaidBlock";
import { CompactionCard } from "./CompactionCard";
import { UserAvatar } from "../common/UserAvatar";
import { AttachmentChipRow } from "./AttachmentChipRow";
import { openAttachedRef } from "../../lib/openWorkspaceRef";
import { pickOpenActionForPath } from "../editor/markdownLinkResolver";
import type { AttachedItem } from "../../lib/types";

// ── Utilities ─────────────────────────────────────────────────────────

/** Shape of a single item rendered inside the bubble context menu.  Kept
 *  intentionally small so callers can compose their own item lists without
 *  having to know about the wrapper's internal state (selection, copy). */
export interface BubbleMenuItem {
  /** Stable key for React reconciliation. */
  key: string;
  /** Lucide icon shown on the left of the label. */
  icon: React.ReactNode;
  /** Visible label — usually a `t("...")` translation. */
  label: string;
  /** Click handler.  The wrapper closes the menu after invoking it. */
  onClick: () => void;
  /** Disabled state — rendered with reduced opacity and `not-allowed`. */
  disabled?: boolean;
  /** Optional colour variant — reuses the global context-menu classes. */
  variant?: "default" | "danger" | "warning";
}

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
        const sessionId = useChatStore.getState().getActiveSessionId(agentId);
        if (!sessionId) return;
        const workspaceId = useWorkspaceStore.getState().getSessionWorkspaceId(sessionId);
        const relPath = href.replace(/^\//, "");
        const action = pickOpenActionForPath(relPath);
        const store = useFileEditorStore.getState();
        if (action === "openPreview") {
          void store.openPreview(agentId, workspaceId, relPath);
        } else {
          void store.openFile(agentId, workspaceId, relPath);
        }
      }
    };
    return (
      <a href={href} onClick={handleClick} {...rest}>
        {children}
      </a>
    );
  },
};

// ── Message Wrapper ───────────────────────────────────────────────────

/** Wrapper that provides right-click context menu for copying text */
interface MessageContentWrapperProps {
  children: React.ReactNode;
  /** Extra menu items appended below the default Copy action.  Use for
   *  message-type-specific affordances (e.g. "Resend" on user bubbles).
   *  Items are rendered in the order provided. */
  extraMenuItems?: BubbleMenuItem[];
}

/**
 * Wraps a message bubble's visual content and attaches a right-click context
 * menu anchored at the cursor.
 *
 * The menu is rendered through `createPortal` to `document.body` so that
 * `position: fixed` is anchored to the **viewport** rather than the nearest
 * transformed ancestor.  `VirtualMessageList` translates each row with
 * `transform: translateY(...)`, which under CSS Containing Block rules
 * makes `position: fixed` resolve against the row container instead of the
 * viewport — the symptom users reported was the menu "floating far from
 * the bubble".  Same trick used in `FileTreeNode.tsx:417`.
 */
function MessageContentWrapper({
  children,
  extraMenuItems,
}: MessageContentWrapperProps) {
  const { t } = useTranslation();
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);
  const wrapperRef = useRef<HTMLDivElement>(null);
  // Viewport-aware positioning: shared hook handles flip-above + edge-clamp.
  const { menuRef, style: contextMenuStyle } = useContextMenuPosition({
    pointer: contextMenu,
  });

  // Track whether the last right-click hit text that produced a selection
  // — used to enable/disable the Copy item without re-reading the
  // selection on every render.
  const [hasSelection, setHasSelection] = useState(false);

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    // Always show the menu on right-click; some items (e.g. "Resend") do
    // not require a text selection.  Copy stays disabled when nothing is
    // selected — see `BubbleMenuItem.disabled` below.
    const selection = window.getSelection();
    setHasSelection(!!selection?.toString().trim());
    setContextMenu({ x: e.clientX, y: e.clientY });
  }, []);

  const handleCopy = useCallback(async () => {
    const selection = window.getSelection();
    const selectedText = selection?.toString();
    if (selectedText) {
      try {
        await navigator.clipboard.writeText(selectedText);
      } catch (err) {
        // Fallback for older browsers
        const textArea = document.createElement("textarea");
        textArea.value = selectedText;
        textArea.style.position = "fixed";
        textArea.style.left = "-9999px";
        document.body.appendChild(textArea);
        textArea.select();
        document.execCommand("copy");
        document.body.removeChild(textArea);
      }
    }
    setContextMenu(null);
  }, []);

  // Wrap any extra item click so the menu always closes — the wrapper
  // owns the close-on-action contract so callers can stay focused on
  // *what* happens, not *when* the menu disappears.
  const invokeAndClose = useCallback((item: BubbleMenuItem) => {
    return () => {
      if (item.disabled) return;
      item.onClick();
      setContextMenu(null);
    };
  }, []);

  // Close context menu on outside click (but not on right-click)
  useEffect(() => {
    if (!contextMenu) return;

    const handleClick = (e: MouseEvent) => {
      // Check if click is outside the context menu
      const target = e.target as Node;
      if (wrapperRef.current && !wrapperRef.current.contains(target)) {
        setContextMenu(null);
      }
    };

    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setContextMenu(null);
      }
    };

    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [contextMenu]);

  // Stable key for the Copy action — keeps React happy across re-renders.
  const copyKey = "copy";

  // Default items: Copy (always first).  Extra items appended below.
  const copyItem: BubbleMenuItem = {
    key: copyKey,
    icon: <Copy size={14} />,
    label: t("chatPanel.copy"),
    onClick: handleCopy,
    disabled: !hasSelection,
  };

  const allItems: BubbleMenuItem[] = [copyItem, ...(extraMenuItems ?? [])];

  // Portal target must exist in the DOM — guard for SSR / very-early mount.
  const portalTarget = typeof document !== "undefined" ? document.body : null;

  return (
    <>
      <div ref={wrapperRef} onContextMenu={handleContextMenu}>{children}</div>
      {contextMenu && portalTarget && createPortal(
        <div
          ref={menuRef}
          className="context-menu context-menu--compact"
          style={contextMenuStyle}
          onContextMenu={(e) => e.stopPropagation()}
        >
          {allItems.map((item) => {
            const variantClass =
              item.variant === "danger"
                ? "context-menu-item--danger"
                : item.variant === "warning"
                  ? "context-menu-item--warning"
                  : undefined;
            return (
              <button
                key={item.key}
                type="button"
                className={`context-menu-item${variantClass ? ` ${variantClass}` : ""}`}
                onClick={invokeAndClose(item)}
                disabled={item.disabled}
                aria-disabled={item.disabled ? "true" : undefined}
              >
                <span className="context-menu-item__icon">{item.icon}</span>
                <span>{item.label}</span>
              </button>
            );
          })}
        </div>,
        portalTarget,
      )}
    </>
  );
}

// ── Message Bubble ────────────────────────────────────────────────────

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

  // Stable items array — rebuilt only when content (or language) changes.
  // Wrapped in useMemo so React.memo peer bubbles don't see prop churn.
  const userExtraMenuItems = React.useMemo<BubbleMenuItem[]>(() => {
    if (!message.content) return [];
    return [{
      key: "resend",
      icon: <RotateCcw size={14} />,
      label: t("chatPanel.resend"),
      onClick: handleResend,
    }];
  }, [message.content, handleResend, t]);

  if (message.type === "user") {
    return (
      <MessageContentWrapper extraMenuItems={userExtraMenuItems}>
        <div className="flex items-start justify-end gap-2">
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
      </MessageContentWrapper>
    );
  }

  if (message.type === "assistant") {
    if (!displayContent) return null;
    return (
      <MessageContentWrapper>
        <div className="min-w-0 flex flex-col ml-12">
<div className="max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
              <div className="prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text break-words [&_th]:bg-chat-title [&_td]:bg-chat-body [&_tbody_tr]:!bg-transparent" style={fontSizeStyle}>
                <StreamMarkdown content={displayContent} />
              </div>
            </div>
          </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "thought") {
    return (
      <MessageContentWrapper>
        <div className="min-w-0 flex flex-col ml-12">
<div className="max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
              <ThinkBlock
                content={message.content}
                startTime={message.startTime}
                endTime={message.endTime}
              />
            </div>
          </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "error") {
    return (
      <MessageContentWrapper>
        <div className="min-w-0 flex flex-col ml-12">
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
      </MessageContentWrapper>
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
        <MessageContentWrapper>
          <div className="flex justify-center">
            <div className="rounded bg-chat-bubble px-3 py-1 text-xs text-zinc-500 dark:text-zinc-400 select-text">
              {message.content}
            </div>
          </div>
        </MessageContentWrapper>
      );
    }

    if (metaType === "file_upload") {
      return (
        <MessageContentWrapper>
          <div className="flex justify-center">
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
        </MessageContentWrapper>
      );
    }

    if (metaType === "image_upload") {
      // Need the selected agent ID for the blob fetch. Use the global
      // store rather than threading a prop through every bubble.
      const selectedAgentId = useAgentStore.getState().selectedAgentId;
      return (
        <MessageContentWrapper>
          <div className="flex justify-center">
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
        </MessageContentWrapper>
      );
    }

    if (metaType === "attached_file") {
      return (
        <MessageContentWrapper>
          <div className="flex justify-center">
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
        </MessageContentWrapper>
      );
    }

    if (metaType === "attached_selection") {
      return (
        <MessageContentWrapper>
          <div className="flex justify-center">
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
        </MessageContentWrapper>
      );
    }

    if (metaType === "attached_folder") {
      return (
        <MessageContentWrapper>
          <div className="flex justify-center">
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
        </MessageContentWrapper>
      );
    }

    // Fallback: generic system message (non-attachment system entries
    // like session notifications, etc.).
    return (
      <MessageContentWrapper>
        <div className="flex justify-center">
          <div className="rounded bg-chat-bubble px-3 py-1 text-xs text-zinc-500 dark:text-zinc-400 select-text">
            {message.content}
          </div>
        </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "compaction") {
    return (
      <MessageContentWrapper>
        {/* ml-12 mirrors assistant/thought/error: content starts to the right of the avatar. */}
        <div className="ml-12">
          <CompactionCard
            summary={message.content}
            meta={message.compactionMeta}
            timestampMs={message.timestamp}
          />
        </div>
      </MessageContentWrapper>
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
      <MessageContentWrapper>
        <div className="flex justify-start">
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
      </MessageContentWrapper>
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
