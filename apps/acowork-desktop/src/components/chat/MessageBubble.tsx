import React, { useEffect, useRef, useState, useCallback, Children, isValidElement } from "react";
import { Copy, ChevronDown, ChevronRight, Wrench, AlertTriangle } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatMessage } from "../../lib/types";
import { useTranslation } from "../../i18n/useTranslation";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { ThinkBlock } from "./ThinkBlock";
import { useStreamingContent } from "./useStreamingContent";
import { CodeBlock } from "./CodeBlock";
import { MermaidBlock } from "./MermaidBlock";
import { CompactionCard } from "./CompactionCard";
import { UserAvatar } from "../common/UserAvatar";
import { DocumentChip } from "./DocumentChip";

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
  /** Intercept link clicks: open in a preview tab instead of navigating the webview (which crashes). */
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
        // Case 2: Local file paths — open in file preview tab
        const sessionId = useChatStore.getState().getActiveSessionId(agentId);
        if (!sessionId) return;
        const workspaceId = useWorkspaceStore.getState().getSessionWorkspaceId(sessionId);
        const relPath = href.replace(/^\//, "");
        useFileEditorStore.getState().openPreview(agentId, workspaceId, relPath);
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
function MessageContentWrapper({ children }: { children: React.ReactNode }) {
  const { t } = useTranslation();
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);
  const wrapperRef = useRef<HTMLDivElement>(null);

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const selection = window.getSelection();
    const selectedText = selection?.toString().trim();

    // Only show context menu if there's selected text
    if (selectedText) {
      setContextMenu({ x: e.clientX, y: e.clientY });
    }
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

  return (
    <>
      <div ref={wrapperRef} onContextMenu={handleContextMenu}>{children}</div>
      {contextMenu && (
        <div
          ref={wrapperRef}
          className="context-menu context-menu--compact"
          style={{ left: contextMenu.x, top: contextMenu.y }}
          onContextMenu={(e) => e.stopPropagation()}
        >
          <button
            type="button"
            className="context-menu-item"
            onClick={handleCopy}
          >
            <Copy size={14} />
            <span>{t("chatPanel.copy")}</span>
          </button>
        </div>
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
  // ADR-027: Streaming content lives in an external mutable Map, read via
  // useSyncExternalStore.  The ChatMessage ref in React state is stable
  // across polls — only the mutable Map update triggers a re-render of
  // this single bubble.
  const streaming = useStreamingContent(currentSessionId, message.id);
  const displayContent = streaming?.content ?? message.content;
  const isStreaming = streaming?.isStreaming ?? false;
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  // Use CSS custom property for font size — set once in store, global effect
  const fontSizeStyle = { fontSize: "var(--ui-font-size, 0.875rem)" };
  // Live names — received as props from ChatPanel so React.memo can detect
  // profile changes (name/avatar edits update all rendered bubbles instantly).
  const liveUserName = liveUserNameProp ?? message.senderDisplayName;

  if (message.type === "user") {
    return (
      <MessageContentWrapper>
        <div className="flex items-start justify-end gap-2">
          <div className="min-w-0 flex-1 flex flex-col items-end">
            {liveUserName && (
              <span className="mt-[2px] text-xs text-zinc-400 dark:text-zinc-500">{liveUserName}</span>
            )}
            {/* Document chips attached to this message */}
            {message.documents && message.documents.length > 0 && (
              <div className="mt-[6px] flex flex-wrap justify-end gap-1.5 max-w-[85%]">
                {message.documents.map((doc, i) => (
                  <DocumentChip
                    key={`${doc.documentId ?? i}`}
                    filename={doc.filename}
                    format={doc.format}
                    size={doc.size}
                    status="success"
                  />
                ))}
              </div>
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
    const showPlaceholder = !displayContent;

    return (
      <MessageContentWrapper>
        <div className="min-w-0 flex flex-col ml-12">
<div className="max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
              {displayContent && (
                <div className="prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text break-words [&_th]:bg-chat-title [&_td]:bg-chat-body [&_tbody_tr]:!bg-transparent" style={fontSizeStyle}>
                  <StreamMarkdown content={displayContent} />
                </div>
              )}
              {!displayContent && showPlaceholder && (
                <span className="inline-flex items-center gap-1.5">
                  <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                  <span className="text-zinc-400">{t("chatPanel.thinking")}</span>
                </span>
              )}
              {isStreaming && <span className="ml-0.5 inline-block animate-pulse">▌</span>}
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
                content={displayContent || message.content}
                isStreaming={isStreaming}
                hasReplyStarted={!isStreaming}
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

  if (message.type === "document_upload") {
    return (
      <MessageContentWrapper>
        <div className="flex justify-center">
          <DocumentChip
            filename={message.content.replace(/^Uploaded file: /, "").replace(/ \(.*, \d+ bytes\)$/, "")}
            format={message.documentFormat ?? "unknown"}
            size={message.documentSize}
            status="success"
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
  // ADR-027: chatStore keeps message object references stable for streaming
  // messages (only appended on first appearance, never mutated).  Reference
  // equality correctly skips re-renders for both settled and streaming
  // messages alike.  Streaming content changes are delivered via
  // useSyncExternalStore → useStreamingContent, which triggers a granular
  // re-render of only the affected MessageBubble.
  //
  // isStreaming is intentionally NOT compared here — it's derived internally
  // from useStreamingContent, not received as a prop.
  return prev.message === next.message
    && prev.liveUserName === next.liveUserName
    && prev.liveUserAvatarUrl === next.liveUserAvatarUrl
    && prev.liveUserBuiltinAvatarId === next.liveUserBuiltinAvatarId;
});

export { MessageBubble };
