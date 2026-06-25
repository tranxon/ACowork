import { useEffect, useLayoutEffect, useRef, useState, useCallback, useMemo, Children, isValidElement } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { invoke } from "@tauri-apps/api/core";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useSkillStore } from "../../stores/skillStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { ToolApprovalNeededEvent } from "../../lib/types";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { fetchProviderModels } from "../../lib/gateway-api";
import { syncAgentUI } from "../../lib/agent-start";
import { toolbarButton } from "../../lib/ui-styles";
import { AddProviderFlow } from "../harness/AddProviderFlow";
import { Bot, Play, Send, ChevronDown, ChevronRight, ChevronsDown, Wrench, AlertTriangle, X, Square, Copy, Plus, Cpu, Loader, Pencil, Paperclip, Image, Brain, Circle, CircleDot } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatMessage, VaultKeyEntry, ModelEntry } from "../../lib/types";
import { ThinkBlock } from "./ThinkBlock";
import { ExploreBlock } from "./ExploreBlock";
import { CodeBlock } from "./CodeBlock";
import { MermaidBlock } from "./MermaidBlock";
import { ContextUsageIcon } from "./ContextUsageIcon";
import { CompactionCard } from "./CompactionCard";

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
function StreamMarkdown({ content }: { content: string }) {
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
}

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
import { AskQuestionCard } from "./AskQuestionCard";
import { DebugPausedBanner } from "./DebugPausedBanner";
import { RetryWaitBanner } from "./RetryWaitBanner";
import { SessionTabBar } from "./SessionTabBar";
import { SkillsPanel } from "../skills/SkillsPanel";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";
import { UserAvatar } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";
import { DocumentChip } from "./DocumentChip";
import { AttachedContextChips } from "./AttachedContextChips";
import { ToolbarDropdownTrigger } from "../common/ToolbarDropdown";
import { Tooltip } from "../common/Tooltip";

// Module-level: persists across ChatPanel mount/unmount cycles
// so nav-back (Settings→Chat) doesn't trigger full reinit
let lastInitAgentId: string | null = null;
/** Tracks the last session ID for which messages were loaded.
 *  Prevents redundant reload when remounting after navigation. */
let lastLoadedSessionId: string | null = null;

export function ChatPanel() {
  const { t } = useTranslation();
  const { selectedAgentId, startAgent } = useAgentStore();
  const selectedAgent = useAgentStore((s) => selectedAgentId ? s.agents[selectedAgentId]?.meta : undefined);

  // Per-agent + per-session state selectors
  // Two-level mapping: agentStates[agentId].sessionStates[sessionId]
  const sessionState = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId] ?? null;
  });
  const messages = sessionState?.messages ?? [];
  const streamingMessageId = sessionState?.streamingMessageId ?? null;
  const thinkingMessageId = sessionState?.thinkingMessageId ?? null;
  const iterationLimitPaused = sessionState?.iterationLimitPaused ?? null;
  const pendingApproval = sessionState?.pendingApproval ?? {};
  const pendingQuestion = sessionState?.pendingQuestion ?? null;
  const isReasoning = sessionState?.isReasoning ?? false;
  const isLoadingSession = sessionState?.isLoadingSession ?? false;
  const loadError = sessionState?.loadError ?? null;
  const todos = sessionState?.todos ?? [];

  // Derive sending from current session: pendingSend (optimistic) OR sessionStatus (backend truth).
  // This is per-session — no cross-session state leakage.
  const sending = sessionState
    ? (sessionState.pendingSend
      || sessionState.sessionStatus?.status === "streaming"
      || sessionState.sessionStatus?.status === "waiting_approval"
      || sessionState.sessionStatus?.status === "paused")
    : false;
  const currentModel = sessionState?.model ?? null;
  const currentProvider = sessionState?.provider ?? null;
  const currentReasoningEffort = sessionState?.reasoningEffort ?? null;

  // Global state and actions — selectors to avoid full-store re-render
  const wsMap = useChatStore((s) => s.wsMap);
  const availableModels = useChatStore((s) => s.availableModels);
  const isLoadingMore = useChatStore((s) => s.isLoadingMore);
  // Stable function refs
  const {
    connectStream,
    sendMessage,
    sendStop,
    setCurrentModel,
    setReasoningEffort,
    setAvailableModels,
    continueExecution,
    resolveApproval,
    resolveApprovalByToolCallId,
  } = useChatStore.getState();
  const currentSessionId = useChatStore((s) => selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null);
  const gatewayStatus = useGatewayStore((s) => s.status);
  const { activeSkill, clearActiveSkill } = useSkillStore();
  const [inputValue, setInputValue] = useState("");
  const [queuedMessages, setQueuedMessages] = useState<string[]>([]);
  /** Pending file uploads: chips shown above the textarea */
  const [pendingFiles, setPendingFiles] = useState<Array<{
    tempId: string;
    filename: string;
    format: string;
    size: number;
    status: "uploading" | "success" | "error";
    documentId?: string;
    errorMessage?: string;
  }>>([]);
  /** Pending image selections: thumbnails shown above the textarea */
  const [pendingImages, setPendingImages] = useState<Array<{
    tempId: string;
    filename: string;
    base64Url: string;
    width: number;
    height: number;
  }>>([]);
  const [showImageUnsupportedDialog, setShowImageUnsupportedDialog] = useState(false);
  const [imageCapableModels, setImageCapableModels] = useState<ModelEntry[]>([]);
  const [hasLlmConfig, setHasLlmConfig] = useState<boolean | null>(null); // null = checking
  const [todosCollapsed, setTodosCollapsed] = useState(false);
  const [showScrollToBottom, setShowScrollToBottom] = useState(false);

  // Auto-collapse todo list when all tasks are completed
  useEffect(() => {
    if (todos.length === 0) return;
    if (todos.every(t => t.status === "completed")) {
      setTodosCollapsed(true);
    }
  }, [todos]);

  const messagesEndRef = useRef<HTMLDivElement>(null);
  const messagesContainerRef = useRef<HTMLDivElement>(null);
  const prevScrollHeightRef = useRef<number>(0);
  const isLoadingMoreRef = useRef<boolean>(false);
  const isInitialLoadRef = useRef<boolean>(false);
  const initAbortedRef = useRef(false);
  /** Tracks previous running state to detect genuine agent stop vs transient remount. */
  const wasRunningRef = useRef(false);
  /** True immediately after the user sends a message — used to force-scroll to
   *  bottom (jump, not smooth) in the next useLayoutEffect, even when the user
   *  had scrolled far up into history. */
  const userJustSentRef = useRef(false);
  /** True while the user is at / near the bottom and hasn't manually scrolled
   *  away.  Used by the ResizeObserver to keep content pinned when virtual
   *  items grow (e.g. thinking block streams in). */
  const pinnedToBottomRef = useRef(false);
  /** Tracks whether the thinking indicator was visible in the previous render.
   *  When it transitions from false → true (first appearance in a turn),
   *  we force-scroll to bottom so the expanded thinking content is visible. */
  const thinkingWasShowingRef = useRef(false);
  /** True only during the first useEffect run of this mount. */
  const justMountedRef = useRef(true);

  const agentDisplayName = useAgentStore((s) => selectedAgentId ? s.agents[selectedAgentId]?.profile?.displayName : undefined) ?? selectedAgent?.display_name ?? selectedAgent?.name;

  // Group consecutive messages for display
  // - Consecutive think + tool_call + tool_result → explore_group (aggregated)
  // - assistant reply (non-think) → display as-is
  // - Everything else → display as-is
  const displayMessages = useMemo(() => {
    const grouped: Array<
      | ChatMessage
      | { type: 'explore_group'; items: ChatMessage[] }
    > = [];

    let exploreBuffer: ChatMessage[] = [];

    const flushExplore = () => {
      if (exploreBuffer.length > 0) {
        grouped.push({ type: 'explore_group', items: [...exploreBuffer] });
        exploreBuffer = [];
      }
    };

    for (const msg of messages) {
      if (msg.type === 'tool_call' || msg.type === 'tool_result') {
        exploreBuffer.push(msg);
      } else if (msg.type === 'thought') {
        exploreBuffer.push(msg);
      } else if (msg.type === 'document_upload' || msg.type === 'system' || msg.type === 'compaction') {
        // Document upload, system messages, and compaction summary cards:
        // flush explore, pass through as-is. compaction must NOT be merged
        // into a tool/explore group — it is a standalone visual unit and
        // also a logical "context boundary" between rounds.
        flushExplore();
        grouped.push(msg);
      } else if (msg.type === 'assistant') {
        // Streaming: if content starts with a think tag but no closing tag yet,
        // treat entire content as thinking
        if (msg.id === streamingMessageId) {
          const trimmed = msg.content.trimStart();
          if (trimmed.startsWith('<think>') && !trimmed.includes('</think>')) {
            const thinkContent = trimmed.slice(7);
            if (thinkContent) {
              exploreBuffer.push({ ...msg, type: 'thought' as any, content: thinkContent });
            }
            continue;
          }
        }

        const { thinkContent, replyContent } = parseThinkContent(msg.content);
        if (thinkContent) {
          exploreBuffer.push({ ...msg, type: 'thought' as any, content: thinkContent });
        }
        if (replyContent.trim()) {
          flushExplore();
          grouped.push({ ...msg, content: replyContent });
        } else if (!thinkContent) {
          // Empty message (streaming)
          flushExplore();
          grouped.push(msg);
        }
      } else {
        // user message or other
        flushExplore();
        grouped.push(msg);
      }
    }

    flushExplore();
    return grouped;
  }, [messages, streamingMessageId]);

  // Show thinking indicator below virtualized message list when waiting for first token
  const showThinkingItem = isReasoning && !streamingMessageId && !thinkingMessageId;
  // Show compacting indicator below messages when compaction is in progress
  const isCompacting = sessionState?.isCompacting ?? false;
  const showCompactingItem = isCompacting && !streamingMessageId && !thinkingMessageId && !isReasoning;

  // Virtual scrolling: only render visible items (messages + optional thinking/compacting indicator)
  const virtualCount = displayMessages.length + (showThinkingItem ? 1 : 0) + (showCompactingItem ? 1 : 0);
  const virtualizer = useVirtualizer({
    count: virtualCount,
    getScrollElement: () => messagesContainerRef.current,
    estimateSize: () => 80,
    overscan: 5,
    gap: 4,
  });

  // Load available models: configured providers (from vault) + capabilities (from models API)
  const loadModels = useCallback(async () => {
    try {
      const keys = await invoke<VaultKeyEntry[]>("list_keys");

      // Build (provider, configuredModelIds, modelCapabilities) tuples, skipping empty entries
      const entries = keys.map(key => ({
        provider: key.provider,
        modelIds: key.models?.length
          ? key.models
          : key.default_model ? [key.default_model] : [],
        modelCapabilities: key.model_capabilities,
      })).filter(e => e.modelIds.length > 0);

      // Fetch capabilities for all providers in parallel
      const results = await Promise.allSettled(
        entries.map(e => fetchProviderModels(e.provider))
      );

      const allModels: ModelEntry[] = [];
      entries.forEach((entry, i) => {
        const apiModels = results[i].status === "fulfilled"
          ? (results[i].value.models ?? [])
          : [];
        for (const modelId of entry.modelIds) {
          const info = apiModels.find(m => m.id === modelId);
          allModels.push({
            name: modelId,
            provider: entry.provider,
            tool_call: info?.tool_call ?? undefined,
            reasoning: info?.reasoning ?? undefined,
            input_modalities: info?.input_modalities ?? undefined,
            default_reasoning_effort: entry.modelCapabilities?.[modelId]?.default_reasoning_effort ?? undefined,
          });
        }
      });

      // Deduplicate by model name + provider
      const uniqueModels = allModels.filter(
        (m, i, arr) => arr.findIndex(x => x.name === m.name && x.provider === m.provider) === i
      );
      setAvailableModels(uniqueModels);
      setHasLlmConfig(keys.length > 0);
    } catch {
      // Gateway may not be running
    }
  }, [setAvailableModels]);

  useEffect(() => {
    loadModels();
  }, [gatewayStatus, loadModels]);

  // Listen for models-added event from AddProviderFlow
  useEffect(() => {
    const handler = () => loadModels();
    window.addEventListener('models-added', handler);
    return () => window.removeEventListener('models-added', handler);
  }, [loadModels]);

  // Connect WebSocket when agent changes + restore per-agent model + init session
  useEffect(() => {
    // Skip re-init if this agent was already initialized and is still running.
    // This prevents redundant clearMessages + reload when selectedAgent.running
    // is re-evaluated without actually changing (e.g. agent list refresh).
    if (selectedAgentId && selectedAgentId === lastInitAgentId && selectedAgent?.running && selectedAgent?.ready) {
      // Still ensure WebSocket is connected — the early return used to skip
      // connectStream, causing ~18s of "streaming unavailable" until
      // waitForAgentReady's poll loop finally caught up.
      if (!wsMap[selectedAgentId]) {
        connectStream(selectedAgentId, getGatewayUrl());
      }
      return;
    }

    // DEFENSIVE: If we reached here for the SAME agent (e.g. remount after
    // Settings→Chat navigation where selectedAgent?.running was temporarily
    // falsy), skip all destructive init to preserve WebSocket-streamed messages.
    const isSameAgentRemount = !!(selectedAgentId && selectedAgentId === lastInitAgentId);

    // Remember the current session for the agent we're leaving (saved in store for remount survival)
    const leavingAgentId = lastInitAgentId;
    const leavingSessionId = leavingAgentId ? useChatStore.getState().getActiveSessionId(leavingAgentId) : null;
    if (leavingAgentId && leavingSessionId) {
      useAgentStore.getState().saveSessionForAgent(leavingAgentId, leavingSessionId);
    }

    if (!isSameAgentRemount) {
      // Allow reload for new agent/session
      lastLoadedSessionId = null;
      // Reset session list state for the new agent
      useAgentStore.getState().reset();
    }

    if (selectedAgentId && selectedAgent?.running && selectedAgent?.ready) {
      if (!isSameAgentRemount) {
        lastInitAgentId = selectedAgentId;
        // Guard against double-connect: waitForAgentReady in AgentList already
        // calls connectStream when the agent becomes ready. If the WebSocket
        // is already open (e.g. ready flag arrived before this effect fired),
        // skip the redundant connectStream call.
        if (!wsMap[selectedAgentId]) {
          connectStream(selectedAgentId, getGatewayUrl());
        }
      }
      // Load available model list; per-session model comes from model_confirmed events.
      loadModels();

      if (!isSameAgentRemount) {
        // Only load session messages on first switch to this agent if no messages yet.
        // Per-session state preserves messages across remounts, so skip reload if already loaded.
        const agent = useChatStore.getState().agentStates[selectedAgentId];
        const activeSessId = agent?.activeSessionId;
        const agentMessages = activeSessId ? agent?.sessionStates[activeSessId]?.messages : undefined;
        if (!agentMessages || agentMessages.length === 0) {
          // 3. Fetch sessions and 4. restore previously selected session (or latest)
          const initSession = async () => {
            isInitialLoadRef.current = true;
            initAbortedRef.current = false;

            // Retry fetching sessions until Agent is ready (max 10 attempts, 1s interval)
            const maxRetries = 10;
            let sessions = useAgentStore.getState().agents[selectedAgentId]?.sessions ?? [];

            for (let i = 0; i < maxRetries; i++) {
              if (initAbortedRef.current) return;
              await useAgentStore.getState().fetchSessions(selectedAgentId);
              sessions = useAgentStore.getState().agents[selectedAgentId]?.sessions ?? [];
              if (sessions.length > 0) break;
              if (i < maxRetries - 1) {
                await new Promise(resolve => setTimeout(resolve, 1000));
              }
            }

            if (initAbortedRef.current) return;

            if (sessions.length === 0) {
              isInitialLoadRef.current = false;
              return;
            }

            // Restore previously selected session for this agent, fallback to latest
            const rememberedSessionId = useAgentStore.getState().agents[selectedAgentId]?.rememberedSessionId;
            const targetSession = rememberedSessionId
              ? sessions.find((s) => s.session_id === rememberedSessionId) ?? sessions[0]
              : sessions[0];
            if (targetSession) {
              // switchSession first (clears old messages), then loadSessionMessages
              // so that messages are not cleared after loading.
              await useAgentStore.getState().switchSession(targetSession.session_id, selectedAgentId);
              await useChatStore
                .getState()
                .loadSessionMessages(selectedAgentId, targetSession.session_id);
            }
            isInitialLoadRef.current = false;
          };
          void initSession();
        } else {
          // Messages already cached — restore session list and selection without reloading
          const restoreSessionSelection = async () => {
            await useAgentStore.getState().fetchSessions(selectedAgentId);
            const rememberedId = useAgentStore.getState().agents[selectedAgentId]?.rememberedSessionId;
            const sessions = useAgentStore.getState().agents[selectedAgentId]?.sessions ?? [];
            if (rememberedId && sessions.some(s => s.session_id === rememberedId)) {
              useAgentStore.getState().switchSession(rememberedId, selectedAgentId);
            }
          };
          void restoreSessionSelection();
        }
      }
    } else if (!isSameAgentRemount) {
      lastInitAgentId = null;
    }
    // Only reset lastInitAgentId on genuine agent stop (running true→false
    // within the same component lifecycle), not during a remount transient.
    if (!selectedAgent?.running && wasRunningRef.current && !justMountedRef.current) {
      lastInitAgentId = null;
    }
    wasRunningRef.current = selectedAgent?.running ?? false;
    justMountedRef.current = false;
    return () => {
      initAbortedRef.current = true;
      // Do NOT disconnect the old agent's ws — keep it alive for reuse.
      // Only clear reconnect timers for the old agent to avoid stale reconnects.
      // The ws connections are per-agent and managed in wsMap.
    };
  }, [selectedAgentId, selectedAgent?.running, selectedAgent?.ready, connectStream, loadModels]);

  // Load messages when active session changes (from SessionPanel or createSession)
  useEffect(() => {
    if (!currentSessionId || !selectedAgentId) return;

    // Skip if agent initialization is in progress — initSession already calls loadSessionMessages
    if (isInitialLoadRef.current) return;

    // Guard: only proceed if this session belongs to the current agent's session list.
    const session = useAgentStore
      .getState()
      .agents[selectedAgentId]?.sessions.find((s) => s.session_id === currentSessionId);
    if (!session) return;

    // If this session was already loaded (e.g. returning from Settings navigation
    // while WebSocket was still streaming), skip reload to avoid overwriting
    // in-flight messages.
    if (currentSessionId === lastLoadedSessionId) return;

    // CRITICAL: If the session already has messages AND the session is actively streaming
    // (streamingMessageId is set, or sessionStatus is streaming/waiting_approval/paused),
    // skip loadSessionMessages. Loading from the backend would overwrite in-memory
    // streaming messages with only-persisted messages, causing thinking/chat bubbles to disappear.
    const agent = useChatStore.getState().agentStates[selectedAgentId];
    const sessState = agent?.sessionStates[currentSessionId];
    const isSessionStreaming = sessState?.streamingMessageId != null
      || sessState?.sessionStatus?.status === "streaming"
      || sessState?.sessionStatus?.status === "waiting_approval"
      || sessState?.sessionStatus?.status === "paused";
    if (sessState && sessState.messages.length > 0 && isSessionStreaming) {
      lastLoadedSessionId = currentSessionId;
      return;
    }

    lastLoadedSessionId = currentSessionId;

    // Mark as initial load to trigger scroll-to-bottom after messages are loaded
    isInitialLoadRef.current = true;
    void useChatStore
      .getState()
      .loadSessionMessages(selectedAgentId, currentSessionId)
      .finally(() => {
        isInitialLoadRef.current = false;
      });
  }, [currentSessionId, selectedAgentId]);

  // Initial load / agent switch / session switch: scroll to bottom synchronously before paint
  const prevDisplayCountRef = useRef(0);
  const prevScrollAgentRef = useRef<string | null>(null);
  const prevScrollSessionRef = useRef<string | null>(null);
  useLayoutEffect(() => {
    // Reset count tracking when agent OR session changes, so we jump to bottom
    // (instead of smooth-scrolling, or failing to scroll when new count <= old count)
    if (
      prevScrollAgentRef.current !== selectedAgentId ||
      prevScrollSessionRef.current !== currentSessionId
    ) {
      prevDisplayCountRef.current = 0;
      prevScrollAgentRef.current = selectedAgentId ?? null;
      prevScrollSessionRef.current = currentSessionId ?? null;
    }
    const prevCount = prevDisplayCountRef.current;

    if (isLoadingMoreRef.current) {
      // Loading more: restore scroll position to keep view stable
      if (prevScrollHeightRef.current > 0 && displayMessages.length > 0) {
        const prevOffset = prevScrollHeightRef.current;
        prevScrollHeightRef.current = 0;
        isLoadingMoreRef.current = false;
        virtualizer.scrollToOffset(prevOffset, { align: "start" });
      }
      prevDisplayCountRef.current = virtualCount;
      return;
    }

    if (virtualCount > 0) {
      if (prevCount === 0) {
        // Agent switch or initial load: jump to bottom instantly (before paint)
        virtualizer.scrollToIndex(virtualCount - 1, { align: "end" });
        pinnedToBottomRef.current = true;
      } else if (virtualCount > prevCount) {
        // New message arrived or thinking indicator appeared
        if (userJustSentRef.current) {
          // User just sent — jump to bottom so they see the response immediately,
          // even if they had scrolled far up into history.
          userJustSentRef.current = false;
          virtualizer.scrollToIndex(virtualCount - 1, { align: "end" });
          pinnedToBottomRef.current = true;
        } else if (!thinkingWasShowingRef.current && showThinkingItem) {
          // Thinking block first appeared in this turn — jump to bottom so
          // the expanded content is visible without manual scrolling.
          virtualizer.scrollToIndex(virtualCount - 1, { align: "end" });
          pinnedToBottomRef.current = true;
        } else {
          // Auto-generated (streaming chunk, thinking toggle, etc.) — smooth scroll
          virtualizer.scrollToIndex(virtualCount - 1, { align: "end", behavior: "smooth" });
        }
      }
    }

    prevDisplayCountRef.current = virtualCount;
    thinkingWasShowingRef.current = showThinkingItem;
  }, [messages, virtualCount, virtualizer, selectedAgentId, currentSessionId, showThinkingItem]);

  // Sticky-bottom: when the virtualizer re-measures a bottom item (e.g.
  // thinking block content streams in), the scroll position drifts above
  // the true bottom because the initial jump used estimateSize.
  //
  // Watching virtualizer.getTotalSize() catches every re-measurement —
  // useVirtualizer tracks measurements in React state, so getTotalSize()
  // returns a new value after recalculation, triggering this effect before
  // the next paint.
  //
  // IMPORTANT: Only force scroll when new items were added (virtualCount
  // increased).  When existing items grow (e.g. mermaid diagram finishes
  // rendering), the height change can push content down — forcing a scroll
  // in that case creates visible jank.  The virtualizer's automatic
  // re-measurement handles size changes of existing items just fine.
  const prevStickyCountRef = useRef(0);
  const totalSize = virtualizer.getTotalSize();
  useLayoutEffect(() => {
    const countChanged = virtualCount !== prevStickyCountRef.current;
    prevStickyCountRef.current = virtualCount;
    if (pinnedToBottomRef.current && virtualCount > 0 && countChanged) {
      virtualizer.scrollToIndex(virtualCount - 1, { align: "end" });
    }
  }, [totalSize, virtualCount, virtualizer]);

  const scrollToBottom = useCallback(() => {
    pinnedToBottomRef.current = true;
    virtualizer.scrollToIndex(virtualCount - 1, { align: "end", behavior: "smooth" });
  }, [virtualizer, virtualCount]);

  // Scroll handler: load more messages when scrolled to top,
  // and show/hide the "scroll to bottom" button.
  const handleScroll = useCallback(() => {
    const container = messagesContainerRef.current;
    if (!container || !selectedAgentId) return;

    // ── Scroll-to-bottom button visibility ──
    const distFromBottom = container.scrollHeight - container.scrollTop - container.clientHeight;
    setShowScrollToBottom(distFromBottom > container.clientHeight);

    // When the user manually scrolls away from the bottom, stop pinning
    // so the ResizeObserver doesn't steal their scroll position.
    if (distFromBottom > 120) {
      pinnedToBottomRef.current = false;
    } else if (distFromBottom < 5) {
      pinnedToBottomRef.current = true;
    }

    const { isLoadingMore } = useChatStore.getState();
    const agent = useChatStore.getState().agentStates[selectedAgentId];
    const activeSessId = agent?.activeSessionId;
    const sessState = activeSessId ? agent?.sessionStates[activeSessId] : undefined;
    const hasMoreMessages = sessState?.hasMoreMessages ?? false;
    const currentSessionId = selectedAgentId ? useChatStore.getState().getActiveSessionId(selectedAgentId) : null;
    if (isLoadingMore || !hasMoreMessages || !currentSessionId) return;

    // Trigger when within 50px of the top
    if (container.scrollTop < 50) {
      // Store current scroll offset for position restoration after prepending messages
      prevScrollHeightRef.current = virtualizer.scrollOffset ?? 0;
      isLoadingMoreRef.current = true;
      void useChatStore
        .getState()
        .loadMoreMessages(selectedAgentId, currentSessionId);
    }
  }, [selectedAgentId, virtualizer]);

  const handleSend = () => {
    const content = inputValue.trim();
    const hasSuccessfulFiles = pendingFiles.some((f) => f.status === "success");
    const hasUploadingFiles = pendingFiles.some((f) => f.status === "uploading");
    const hasImages = pendingImages.length > 0;

    // Block send: no content AND no files AND no images, or files still uploading
    if ((!content && !hasSuccessfulFiles && !hasImages) || sending || !selectedAgentId || hasUploadingFiles) return;

    // Collect successfully uploaded document IDs and metadata for optimistic bubbles
    const documentIds = pendingFiles
      .filter((f) => f.status === "success" && f.documentId)
      .map((f) => f.documentId!);
    const documents = pendingFiles
      .filter((f) => f.status === "success" && f.documentId)
      .map((f) => ({
        id: f.documentId!,
        filename: f.filename,
        format: f.format,
        size: f.size,
      }));

    // Build image parts from pending images (for multimodal content_parts)
    const imageParts = pendingImages.map((img) => ({
      url: img.base64Url,
      width: img.width,
      height: img.height,
    }));

    // sendMessage is async but we fire-and-forget here —
    // the store handles all state updates internally
    userJustSentRef.current = true;
    void sendMessage(content, selectedAgentId, activeSkill?.name, documentIds.length > 0 ? documentIds : undefined, documents.length > 0 ? documents : undefined, imageParts.length > 0 ? imageParts : undefined).then(() => {
      clearActiveSkill();
    });
    setInputValue("");
    // Clear pending files and images after send
    setPendingFiles([]);
    setPendingImages([]);
  };

  // Stop button dual-action:
  //   input has content → send to queue (no stop, message waits for next loop)
  //   input empty       → stop current loop
  const handleStop = () => {
    const content = inputValue.trim();
    if (content && selectedAgentId) {
      // Add to queue — message waits in the queue box above the input area.
      setQueuedMessages(prev => [...prev, content]);
      setInputValue("");
    } else if (queuedMessages.length > 0 && selectedAgentId) {
      // Click with queued messages: send all queued + stop current loop.
      userJustSentRef.current = true;
      for (const msg of queuedMessages) {
        void sendMessage(msg, selectedAgentId, activeSkill?.name).then(() => {
          clearActiveSkill();
        });
      }
      setQueuedMessages([]);
      sendStop(selectedAgentId);
    } else if (selectedAgentId) {
      // No queued messages: just stop
      sendStop(selectedAgentId);
    }
  };

  const handleRemoveQueued = (index: number) => {
    setQueuedMessages(prev => prev.filter((_, i) => i !== index));
  };

  const handleEditQueued = (index: number) => {
    setInputValue(queuedMessages[index]);
    setQueuedMessages(prev => prev.filter((_, i) => i !== index));
  };

  // File upload handler: open file dialog, then upload via Tauri command
  const handleFileUpload = async () => {
    // Import dialog dynamically to avoid build issues
    const { open } = await import("@tauri-apps/plugin-dialog");
    const selected = await open({
      title: "Select a document",
      filters: [{
        name: "Documents",
        extensions: ["pdf", "docx", "pptx", "xlsx"],
      }],
      multiple: false,
    });

    if (!selected) return;

    const filePath = selected as string;
    if (!filePath) return;

    const filename = filePath.replace(/^.*[\\/]/, "");
    const ext = filename.split(".").pop()?.toLowerCase() ?? "";
    if (!["pdf", "docx", "pptx", "xlsx"].includes(ext)) return;

    const tempId = `file-${Date.now()}`;

    // Check prerequisites before adding chip
    if (!currentSessionId) {
      setPendingFiles(prev => [...prev, {
        tempId,
        filename,
        format: ext,
        size: 0,
        status: "error",
        errorMessage: "No active session",
      }]);
      return;
    }
    if (!selectedAgentId) {
      setPendingFiles(prev => [...prev, {
        tempId,
        filename,
        format: ext,
        size: 0,
        status: "error",
        errorMessage: "No agent selected",
      }]);
      return;
    }

    // Add pending chip with uploading status
    setPendingFiles(prev => [...prev, {
      tempId,
      filename,
      format: ext,
      size: 0,
      status: "uploading",
    }]);

    try {
      const result = await invoke<{
        document_id: string;
        filename: string;
        format: string;
        size_bytes: number;
      }>("upload_document", {
        sessionId: currentSessionId,
        filePath,
      });

      // Update chip to success
      setPendingFiles(prev => prev.map((f) =>
        f.tempId === tempId
          ? { ...f, status: "success", documentId: result.document_id, size: result.size_bytes }
          : f
      ));
    } catch (err) {
      const msg = err instanceof Error ? err.message : typeof err === "string" ? err : "Upload failed";
      console.error("[ChatPanel] Document upload failed:", err);
      // Update chip to error
      setPendingFiles(prev => prev.map((f) =>
        f.tempId === tempId ? { ...f, status: "error", errorMessage: msg } : f
      ));
    }
  };

  // Remove a pending file chip
  const handleRemoveFile = (tempId: string) => {
    setPendingFiles(prev => prev.filter((f) => f.tempId !== tempId));
  };

  // Select image file via Tauri dialog, read as base64, and get dimensions
  const handleImageSelect = async () => {
    if (!currentSessionId || !selectedAgentId) return;

    // Check if current model supports image input
    const currentEntry = availableModels.find(
      m => m.name === currentModel && m.provider === currentProvider
    );
    const supportsImage = currentEntry?.input_modalities?.includes('image');
    if (!supportsImage) {
      // Find models that support image — including other providers
      const imageModels = availableModels.filter(m => m.input_modalities?.includes('image'));
      if (imageModels.length === 0) {
        console.warn("[ChatPanel] No image-capable models available — skipping dialog");
        return;
      }
      setImageCapableModels(imageModels);
      setShowImageUnsupportedDialog(true);
      return;
    }

    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const selected = await open({
        title: t("chatPanel.selectImageTitle"),
        filters: [{
          name: "Images",
          extensions: ["png", "jpg", "jpeg", "gif", "webp"],
        }],
        multiple: false,
      });
      if (!selected) return;
      const filePath = selected as string;
      if (!filePath) return;

      // Read file bytes via Tauri FS plugin (bypasses asset protocol scope limitations)
      const filename = filePath.replace(/^.*[\\/]/, "");
      const { readFile } = await import("@tauri-apps/plugin-fs");
      const bytes = await readFile(filePath);

      // Convert bytes to base64 data URL
      const ext = filename.split(".").pop()?.toLowerCase() ?? "";
      const mimeMap: Record<string, string> = { png: "image/png", gif: "image/gif", webp: "image/webp", jpg: "image/jpeg", jpeg: "image/jpeg" };
      const mime = mimeMap[ext] ?? "image/jpeg";
      const chunks: string[] = [];
      const CHUNK_SIZE = 8192;
      for (let i = 0; i < bytes.length; i += CHUNK_SIZE) {
        chunks.push(String.fromCharCode(...bytes.subarray(i, i + CHUNK_SIZE)));
      }
      const base64 = btoa(chunks.join(""));
      const dataUrl = `data:${mime};base64,${base64}`;

      // Get image dimensions
      const dims = await new Promise<{ width: number; height: number }>((resolve, reject) => {
        const img = new window.Image();
        img.onload = () => resolve({ width: img.naturalWidth, height: img.naturalHeight });
        img.onerror = () => reject(new Error("Failed to load image for dimension detection"));
        img.src = dataUrl;
      });

      const tempId = `img-${Date.now()}`;
      setPendingImages(prev => [...prev, {
        tempId,
        filename,
        base64Url: dataUrl,
        width: dims.width,
        height: dims.height,
      }]);
    } catch (err) {
      console.error("[ChatPanel] Image selection failed:", err);
    }
  };

  // Remove a pending image thumbnail
  const handleRemoveImage = (tempId: string) => {
    setPendingImages(prev => prev.filter((img) => img.tempId !== tempId));
  };

  // Tool approval: send decision to Gateway API directly, then clear inline state
  const handleToolApprove = async (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => {
    const agentId = String(approval.agent_id ?? selectedAgentId ?? "");
    const requestId = String(approval.request_id ?? "");
    const sessionId = approval.session_id;
    try {
      const url = `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/approval`;
      await fetch(url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          request_id: requestId,
          action,
          ...(sessionId ? { session_id: sessionId } : {}),
        }),
      });
    } catch (err) {
      console.error("[ChatPanel] Failed to send approval:", err);
    }
    // Clear the specific approval from the pending map by tool_call_id
    if (selectedAgentId && approval.tool_call_id) {
      resolveApprovalByToolCallId(selectedAgentId, approval.tool_call_id);
    } else {
      resolveApproval(selectedAgentId ?? "");
    }
  };

  // Ask question answer: send answer to Gateway API, then clear pendingQuestion
  const handleQuestionAnswer = async (requestId: string, answer: string) => {
    if (!selectedAgentId) return;
    const agentId = String(selectedAgentId);
    const sessionId = selectedAgentId ? useChatStore.getState().getActiveSessionId(selectedAgentId) : null;
    try {
      const url = `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/question`;
      await fetch(url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ request_id: requestId, answer, session_id: sessionId }),
      });
    } catch (err) {
      console.error("[ChatPanel] Failed to send question answer:", err);
    }
    // Clear pending question state regardless of result
    useChatStore.getState().resolveQuestion(agentId);
  };

  // Auto-send queued messages when agent finishes execution
  useEffect(() => {
    if (!sending && queuedMessages.length > 0 && selectedAgentId) {
      const msgs = [...queuedMessages];
      setQueuedMessages([]);
      for (const msg of msgs) {
        void sendMessage(msg, selectedAgentId);
      }
    }
  }, [sending]);

  // ── Empty state: no agents at all ──
  if (Object.keys(useAgentStore.getState().agents).length === 0) {
    return (
      <div className="flex flex-1 items-center justify-center bg-zinc-50 dark:bg-zinc-900">
        <div className="text-center">
          <Bot className="mx-auto h-12 w-12 text-zinc-300 dark:text-zinc-600" />
          <p className="mt-3 text-sm text-zinc-400 dark:text-zinc-500">No agents available</p>
          <p className="mt-1 text-xs text-zinc-400 dark:text-zinc-600">Connect to Gateway and install the System Agent</p>
        </div>
      </div>
    );
  }

  // ── No agent selected ──
  if (!selectedAgent) {
    return (
      <div className="flex flex-1 items-center justify-center bg-zinc-50 dark:bg-zinc-900">
        <div className="text-center">
          <Bot className="mx-auto h-12 w-12 text-zinc-300 dark:text-zinc-600" />
          <p className="mt-3 text-sm text-zinc-400 dark:text-zinc-500">Select an agent to start chatting</p>
          <p className="mt-1 text-xs text-zinc-400 dark:text-zinc-600">or install a new agent from the sidebar</p>
        </div>
      </div>
    );
  }

  // ── Agent not running ──
  if (!selectedAgent.running) {
    return (
      <div className="flex flex-1 items-center justify-center bg-zinc-50 dark:bg-zinc-900">
        <div className="text-center">
          <Tooltip content={t("chatPanel.startAgent")} variant="plain">
            <button
              onClick={async () => {
                await startAgent(selectedAgent.agent_id);
                await syncAgentUI(selectedAgent.agent_id);
              }}
              className="mx-auto flex h-20 w-20 items-center justify-center rounded-full btn-solid"
            >
              <Play className="h-8 w-8" />
            </button>
          </Tooltip>
          <p className="mt-3 text-xs text-zinc-400 dark:text-zinc-500">{agentDisplayName} is sleeping</p>
        </div>
      </div>
    );
  }

  // ── Chat view ──
  const inputDisabled = gatewayStatus !== "connected";

  return (
    <>
      <div
        className="flex flex-1 min-w-[288px] flex-col bg-[#FAFAFA] dark:bg-zinc-900 rounded-lg overflow-hidden"
      >
        {/* LLM config warning */}
        {hasLlmConfig === false && (
          <div className="flex items-center gap-2 border-b border-amber-200 bg-amber-50 px-4 py-2 rounded-t-lg dark:border-amber-900 dark:bg-amber-950">
            <AlertTriangle className="h-4 w-4 text-amber-600 dark:text-amber-400" />
            <span className="text-xs text-amber-700 dark:text-amber-300">
              {t("chatPanel.llmNotConfigured")}
            </span>
          </div>
        )}
        {/* ADR-015: Session tab bar */}
        {selectedAgentId && <SessionTabBar agentId={selectedAgentId} />}
        {/* Messages area with drawer overlay */}
        <div className="relative flex-1 overflow-hidden">
          <div
            ref={messagesContainerRef}
            onScroll={handleScroll}
            className="h-full overflow-y-auto px-4 py-3 select-text cursor-text"
            role="log"
            aria-label="Chat messages"
          >
            {/* Loading more indicator at top */}
            {isLoadingMore && (
              <div className="flex items-center justify-center py-2">
                <Loader className="h-4 w-4 animate-spin text-zinc-400 dark:text-zinc-500" />
                <span className="ml-1.5 text-[10px] text-zinc-400 dark:text-zinc-500">Loading more...</span>
              </div>
            )}

            {/* Loading session indicator */}
            {isLoadingSession && messages.length === 0 && (
              <div className="flex h-full items-center justify-center">
                <div className="text-center">
                  <Loader className="mx-auto h-8 w-8 animate-spin text-zinc-400 dark:text-zinc-500" />
                  <p className="mt-3 text-xs text-zinc-400 dark:text-zinc-500">Loading conversation...</p>
                </div>
              </div>
            )}

            {loadError && !isLoadingSession && (
              <div className="flex h-full flex-col items-center justify-center gap-3 px-4">
                <div className="text-sm text-red-500 dark:text-red-400">{t("chatPanel.sessionLoadFailed")}</div>
                <div className="max-w-xs text-center text-xs text-zinc-500 dark:text-zinc-400">
                  {loadError}
                </div>
                <button
                  onClick={() => {
                    const sessionId = selectedAgentId ? useChatStore.getState().getActiveSessionId(selectedAgentId) : null;
                    const agentId = useAgentStore.getState().selectedAgentId;
                    if (sessionId && agentId) {
                      useChatStore.getState().loadSessionMessages(agentId, sessionId);
                    }
                  }}
                  className="rounded-md bg-zinc-100 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                >
                  {t("chatPanel.retry")}
                </button>
              </div>
            )}
            {!loadError && !isLoadingSession && messages.length === 0 && (
              <div className="flex h-full items-center justify-center text-xs text-zinc-400 dark:text-zinc-500">
                Start a conversation with {selectedAgent.name}
              </div>
            )}
            {/* Virtualized message list — only renders visible items */}
            {displayMessages.length > 0 && (
              <div
                style={{
                  height: virtualizer.getTotalSize(),
                  width: '100%',
                  position: 'relative',
                }}
              >
                {virtualizer.getVirtualItems().map((virtualRow) => {
                  // --- Compacting indicator (extra virtual item, above thinking if both shown) ---
                  if (showCompactingItem && virtualRow.index === displayMessages.length) {
                    return (
                      <div
                        key={virtualRow.key}
                        ref={virtualizer.measureElement}
                        data-index={virtualRow.index}
                        style={{
                          position: 'absolute',
                          top: 0,
                          left: 0,
                          width: '100%',
                          transform: `translateY(${virtualRow.start}px)`,
                        }}
                      >
                        <div className="flex items-center gap-1.5 px-4 py-1.5 select-none">
                          <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                          <span className="thinking-shimmer" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>Context compacting...</span>
                        </div>
                      </div>
                    );
                  }

                  // --- Thinking indicator (extra virtual item below messages / compacting) ---
                  if (showThinkingItem && virtualRow.index === displayMessages.length + (showCompactingItem ? 1 : 0)) {
                    return (
                      <div
                        key={virtualRow.key}
                        ref={virtualizer.measureElement}
                        data-index={virtualRow.index}
                        style={{
                          position: 'absolute',
                          top: 0,
                          left: 0,
                          width: '100%',
                          transform: `translateY(${virtualRow.start}px)`,
                        }}
                      >
                        <div className="flex items-center gap-1.5 px-4 py-1.5 select-none">
                          <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                          <span className="thinking-shimmer" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>working ...</span>
                        </div>
                      </div>
                    );
                  }

                  // --- Regular message item ---
                  const item = displayMessages[virtualRow.index];
                  const displayItem = item as any;

                  return (
                    <div
                      key={virtualRow.key}
                      ref={virtualizer.measureElement}
                      data-index={virtualRow.index}
                      style={{
                        position: 'absolute',
                        top: 0,
                        left: 0,
                        width: '100%',
                        transform: `translateY(${virtualRow.start}px)`,
                      }}
                      className=""
                    >
                      {/* Explore group - aggregated think + tool calls/results */}
                      {displayItem.type === 'explore_group' && (() => {
                        const nextItem = displayMessages[virtualRow.index + 1];
                        const hasFollowUpReply = nextItem !== undefined && (nextItem as any).type !== 'explore_group';
                        return (
                          <ExploreBlock
                            items={displayItem.items}
                            isStreaming={displayItem.items.some(
                              (m: ChatMessage) => m.id === streamingMessageId || m.id === thinkingMessageId
                            )}
                            pendingApproval={pendingApproval}
                            currentSessionId={currentSessionId}
                            onApprove={(action, approval) => handleToolApprove(action, approval)}
                            hasFollowUpReply={hasFollowUpReply}
                          />
                        );
                      })()}

                      {/* Regular message */}
                      {displayItem.type !== 'explore_group' && (
                        <MessageBubble message={item as ChatMessage} isStreaming={(item as ChatMessage).id === streamingMessageId} agentId={selectedAgentId ?? ""} />
                      )}
                    </div>
                  );
                })}
              </div>
            )}
            {/* Debug paused banner — shown when the agent is in dev_mode and
                the debugger is currently in Stepping/Paused state. Provides
                F5 (resume) and F10 (step) actions directly from the chat. */}
            <DebugPausedBanner />
            {/* 429 Retry wait banner — countdown + Skip Wait button, shown when
                LLM provider returns 429 with Retry-After > 10s */}
            <RetryWaitBanner />
            {/* Iteration limit pause — hint + Continue button */}
            {iterationLimitPaused && (
              <div className="flex flex-col items-start gap-1.5">
                <span
                  className="text-zinc-600 dark:text-zinc-400"
                  style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)" }}
                >
                  {iterationLimitPaused.message}
                </span>
                <button
                  onClick={() => {
                    if (selectedAgentId) {
                      userJustSentRef.current = true;
                      continueExecution(selectedAgentId);
                    }
                  }}
                  className="flex w-fit max-w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-white transition-opacity hover:opacity-90"
                  style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)", backgroundColor: "var(--color-accent)" }}
                >
                  <Play className="h-3.5 w-3.5" />
                  <span>
                    Continue ({iterationLimitPaused.iteration}/{iterationLimitPaused.maxIterations})
                  </span>
                </button>
              </div>
            )}
            {/* Ask question card — shown when LLM asks the user a question */}
            {pendingQuestion && (
              <AskQuestionCard
                event={pendingQuestion}
                agentId={selectedAgentId ?? ""}
                sessionId={currentSessionId}
                onAnswer={handleQuestionAnswer}
              />
            )}
            <div ref={messagesEndRef} />
          </div>
          {/* Scroll-to-bottom button — visible when scrolled up > 1 screen */}
          {showScrollToBottom && (
            <button
              onClick={scrollToBottom}
              className="absolute bottom-3 right-4 z-10 rounded-full bg-zinc-100 dark:bg-zinc-700 border border-zinc-200 dark:border-zinc-600 shadow-md p-1.5 hover:bg-zinc-200 dark:hover:bg-zinc-600 transition-all animate-in fade-in zoom-in"
              aria-label="Scroll to bottom"
            >
              <ChevronsDown className="h-4 w-4 text-zinc-500 dark:text-zinc-400" />
            </button>
          )}
        </div>

        {/* Todo list box — above the message queue, same collapsible style.
          Shows current task list from todo_write tool calls. */}
        {todos.length > 0 && (
          <div className="mx-5 mb-0 rounded-t-md border border-b-0 border-zinc-200 dark:border-zinc-800 bg-zinc-50/80 dark:bg-zinc-800/60 overflow-hidden">
            <button
              className="flex items-center w-full px-2.5 py-1.5 border-b border-zinc-200 dark:border-zinc-800 hover:bg-zinc-100 dark:hover:bg-zinc-700/30 transition-colors"
              onClick={() => setTodosCollapsed(!todosCollapsed)}
            >
              {todosCollapsed ? (
                <ChevronRight className="h-3 w-3 mr-1 text-zinc-400 dark:text-zinc-500 shrink-0" />
              ) : (
                <ChevronDown className="h-3 w-3 mr-1 text-zinc-400 dark:text-zinc-500 shrink-0" />
              )}
              <span className="text-[10px] font-medium text-zinc-400 dark:text-zinc-500 uppercase tracking-wider">
                {t("chatPanel.taskList", { completed: todos.filter(t => t.status === "completed").length, total: todos.length })}
              </span>
            </button>
            {!todosCollapsed && (
              <div className="max-h-[7.5rem] overflow-y-auto">
                {todos.map((item) => {
                  const isCompleted = item.status === "completed";
                  const isInProgress = item.status === "in_progress";
                  return (
                    <div
                      key={item.id}
                      className="flex items-start gap-1.5 px-2.5 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-700/40 border-b border-zinc-100 dark:border-zinc-700/30 last:border-b-0"
                    >
                      <span className={cn(
                        "shrink-0 mt-0.5 select-none",
                        isCompleted
                          ? "text-zinc-400 dark:text-zinc-500"
                          : isInProgress
                            ? "text-zinc-500 dark:text-zinc-300"
                            : "text-zinc-400 dark:text-zinc-500"
                      )}>
                        {isCompleted ? (
                          <CircleDot className="h-3.5 w-3.5" strokeWidth={2.25} />
                        ) : isInProgress ? (
                          <Loader className="h-3.5 w-3.5 animate-spin" strokeWidth={2.25} />
                        ) : (
                          <Circle className="h-3.5 w-3.5" strokeWidth={2.25} />
                        )}
                      </span>
                      <span className={cn(
                        "flex-1 min-w-0 text-xs leading-relaxed",
                        isCompleted
                          ? "text-zinc-400 dark:text-zinc-500 line-through"
                          : isInProgress
                            ? "text-zinc-700 dark:text-zinc-200 font-medium"
                            : "text-zinc-600 dark:text-zinc-300"
                      )}>
                        {item.content}
                      </span>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        )}

        {/* Queued messages box — separate box above the input area,
          flush against input, slightly narrower for layered depth */}
        {queuedMessages.length > 0 && (
          <div className={cn(
            "mx-5 mb-0 border border-b-0 border-zinc-200 dark:border-zinc-800 bg-zinc-50/80 dark:bg-zinc-800/60 overflow-hidden",
            todos.length > 0 ? "" : "rounded-t-md"
          )}>
            <div className="flex items-center px-2.5 py-1.5 border-b border-zinc-200 dark:border-zinc-800">
              <span className="text-[10px] font-medium text-zinc-400 dark:text-zinc-500 uppercase tracking-wider">
                {t("chatPanel.messageQueue", { count: queuedMessages.length })}
              </span>
            </div>
            <div className="max-h-[7.5rem] overflow-y-auto">
              {queuedMessages.map((msg, i) => (
                <div
                  key={i}
                  className="group flex items-start gap-1.5 px-2.5 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-700/40 border-b border-zinc-100 dark:border-zinc-700/30 last:border-b-0"
                >
                  <span className="shrink-0 text-[10px] mt-0.5 text-zinc-400 dark:text-zinc-500 select-none">{i + 1}.</span>
                  <span className="flex-1 min-w-0 text-xs text-zinc-700 dark:text-zinc-300 truncate leading-relaxed">
                    {msg}
                  </span>
                  <div className="flex items-center gap-0.5 shrink-0 opacity-0 group-hover:opacity-100 transition-opacity">
                    <button
                      type="button"
                      onClick={() => handleEditQueued(i)}
                      className="rounded-sm p-0.5 text-zinc-400 hover:text-[var(--color-accent)] hover:bg-[var(--color-accent)]/10"
                      aria-label={`Edit message ${i + 1}`}
                    >
                      <Pencil size={12} />
                    </button>
                    <button
                      type="button"
                      onClick={() => handleRemoveQueued(i)}
                      className="rounded-sm p-0.5 text-zinc-400 hover:text-red-500 hover:bg-red-50 dark:hover:text-red-400 dark:hover:bg-red-900/30"
                      aria-label={`Remove message ${i + 1}`}
                    >
                      <X size={12} />
                    </button>
                  </div>
                </div>
              ))}
            </div>
          </div>
        )}

        {/* Unified input container with toolbar */}
        <div className="mx-3 mb-3 rounded-md border border-zinc-200 dark:border-zinc-700 bg-[#FAFAFA] dark:bg-zinc-900">
          {/* Active skill badge */}
          {activeSkill && (
            <div className="flex items-center gap-1 px-3 pt-2">
              <span className="inline-flex items-center gap-1 rounded bg-[var(--color-accent)]/10 px-1.5 py-0.5 text-xs font-medium border border-[var(--color-accent)]/20" style={{ color: "var(--color-accent)" }}>
                /{activeSkill.name}
                <button
                  type="button"
                  onClick={clearActiveSkill}
                  className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-[var(--color-accent)]/15"
                  aria-label="Clear active skill"
                >
                  <X size={12} />
                </button>
              </span>
            </div>
          )}

          {/* Pending file chips */}
          {pendingFiles.length > 0 && (
            <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
              {pendingFiles.map((file) => (
                <DocumentChip
                  key={file.tempId}
                  filename={file.filename}
                  format={file.format}
                  size={file.size > 0 ? file.size : undefined}
                  status={file.status}
                  errorMessage={file.errorMessage}
                  onRemove={() => handleRemoveFile(file.tempId)}
                />
              ))}
            </div>
          )}
          {/* Pending image thumbnails */}
          {pendingImages.length > 0 && (
            <div className="flex flex-wrap items-center gap-2 px-3 pt-2">
              {pendingImages.map((img) => (
                <div
                  key={img.tempId}
                  className="group relative h-14 w-14 shrink-0 overflow-hidden rounded-md border border-zinc-200 dark:border-zinc-700"
                >
                  <img
                    src={img.base64Url}
                    alt={img.filename}
                    className="h-full w-full object-cover"
                  />
                  <button
                    type="button"
                    onClick={() => handleRemoveImage(img.tempId)}
                    className="absolute -right-0.5 -top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-red-500 text-white opacity-0 transition-opacity group-hover:opacity-100"
                    aria-label={`Remove ${img.filename}`}
                  >
                    <X size={10} />
                  </button>
                </div>
              ))}
            </div>
          )}
          {/* Attached context chips (from right-click "Add to Chat") */}
          <AttachedContextChips />
          {/* Textarea area — borderless, transparent background */}
          <textarea
            value={inputValue}
            onChange={(e) => setInputValue(e.target.value)}
            placeholder={
              gatewayStatus !== "connected"
                ? "Gateway not connected"
                : !wsMap[selectedAgentId!] || wsMap[selectedAgentId!].readyState !== WebSocket.OPEN
                  ? activeSkill
                    ? t("chatPanel.inputParamsConnecting")
                    : "Type a message... (Connecting to agent...)"
                  : activeSkill
                    ? t("chatPanel.inputParams")
                    : "Type a message... (Enter to send, Shift+Enter for new line)"
            }
            disabled={inputDisabled}
            className="w-full resize-none border-0 bg-transparent p-3 pb-2 outline-none placeholder:text-zinc-400 dark:placeholder:text-zinc-500 dark:text-zinc-100 disabled:cursor-not-allowed disabled:opacity-50 max-h-48 overflow-y-auto min-h-[4.5rem]"
            style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                // Let handleSend() itself decide whether to proceed
                handleSend();
              }
            }}
          />

          {/* Bottom toolbar — @container for responsive button text collapse */}
          <div className="@container/tb flex items-center justify-between gap-2 px-3 pb-2 min-w-[264px]">
            {/* Left: feature buttons */}
            <div className="flex items-center gap-1 min-w-0 overflow-visible">
              {/* Model switcher — only enabled when agent is running */}
              {availableModels.length > 1 && selectedAgent?.running && (
                <ModelMenu
                  models={availableModels}
                  currentModel={currentModel}
                  currentProvider={currentProvider}
                  onSelect={(m, p) => selectedAgentId && setCurrentModel(m, p, selectedAgentId)}
                />
              )}
              {/* Reasoning effort toggle — shown when session has a non-null reasoningEffort (null = provider doesn't support reasoning) */}
              {selectedAgent?.running && currentReasoningEffort != null && (
                <ReasoningEffortMenu
                  effort={currentReasoningEffort}
                  onChange={(e) => selectedAgentId && setReasoningEffort(e, selectedAgentId)}
                />
              )}
              {/* Workspace button */}
              <WorkspaceSelector />
              {/* Skills dropdown */}
              <SkillsPanel />
              {/* File upload button */}
              <Tooltip content={t("chatPanel.uploadHint")}>
                <button
                  className={toolbarButton}
                  onClick={handleFileUpload}
                  disabled={!currentSessionId || !selectedAgentId}
                  aria-label={t("chatPanel.uploadFile")}
                >
                  <Paperclip size={14} />
                </button>
              </Tooltip>
              {/* Image upload button */}
              <Tooltip content={t("chatPanel.uploadImageHint")}>
                <button
                  className={toolbarButton}
                  onClick={handleImageSelect}
                  disabled={!currentSessionId || !selectedAgentId}
                  aria-label={t("chatPanel.selectImage")}
                >
                  <Image size={14} />
                </button>
              </Tooltip>
            </div>

            {/* Right: send/stop button + context usage icon */}

            <div className="flex shrink-0 items-center gap-1">
              {/* Context usage icon — shown when session is active */}
              {selectedAgentId && currentSessionId && <ContextUsageIcon agentId={selectedAgentId} sessionId={currentSessionId} />}

              {/* Send/Stop button with tooltip above */}
              <Tooltip content={sending
                ? (inputValue.trim()
                  ? t("chatPanel.addToQueue")
                  : queuedMessages.length > 0
                    ? t("chatPanel.sendQueuedAndStop")
                    : t("chatPanel.stop"))
                : t("chatPanel.sendMessage")}>
                <button
                  className={`rounded-md p-1.5 transition-colors ${sending
                    ? "text-[var(--color-accent)] hover:bg-[var(--color-accent)]/10"
                    : "text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 hover:text-zinc-700 dark:hover:text-zinc-200 disabled:opacity-50"
                    }`}
                  onClick={sending ? handleStop : handleSend}
                  disabled={
                    sending
                      ? false
                      : (inputDisabled
                        || (!inputValue.trim() && !pendingFiles.some(f => f.status === "success") && pendingImages.length === 0)
                        || pendingFiles.some(f => f.status === "uploading"))
                  }
                  aria-label={sending ? (inputValue.trim() ? t("chatPanel.addToQueue") : queuedMessages.length > 0 ? t("chatPanel.sendQueuedAndStop") : t("chatPanel.stop")) : t("chatPanel.sendMessage")}
                >
                  {sending ? <Square size={16} fill="currentColor" /> : <Send size={16} />}
                </button>
              </Tooltip>
            </div>
          </div>
        </div>
      </div>

      {/* Image unsupported dialog */}
      <UnsupportedImageDialog
        open={showImageUnsupportedDialog}
        models={imageCapableModels}
        onSelect={(model: string, provider: string) => {
          if (selectedAgentId) {
            setCurrentModel(model, provider, selectedAgentId);
            setShowImageUnsupportedDialog(false);
          }
        }}
        onClose={() => setShowImageUnsupportedDialog(false)}
      />
    </>
  );
}

/**
 * Parse <think>...</think> tags from assistant content.
 *
 * Returns the think content, reply content, and whether the think tag is closed.
 * If the content does not start with <think>, all content is treated as reply.
 * The <think> and </think> tags are stripped from the output.
 * Handles multiple <think> blocks by extracting the first one and stripping all others.
 */
function parseThinkContent(content: string): {
  thinkContent: string | null;
  replyContent: string;
  thinkClosed: boolean;
} {
  // Find the first <think> block
  const firstThinkStart = content.indexOf("<think>");

  if (firstThinkStart === -1) {
    // No <think> tag found — treat entire content as reply
    return { thinkContent: null, replyContent: content, thinkClosed: false };
  }

  // Find the closing </think> for the first <think>
  const firstThinkEnd = content.indexOf("</think>", firstThinkStart);

  if (firstThinkEnd === -1) {
    // <think> tag is still open — everything after <think> is think content
    const thinkContent = content.slice(firstThinkStart + 7); // length of "<think>"
    return { thinkContent, replyContent: "", thinkClosed: false };
  }

  // Extract think content (between first <think> and its closing </think>)
  const thinkContent = content.slice(firstThinkStart + 7, firstThinkEnd);

  // Extract reply content (after the first </think>)
  // Also strip any remaining <think>...</think> tags from the reply
  let replyContent = content.slice(firstThinkEnd + 8); // length of "</think>"

  // Remove any remaining <think>...</think> blocks from reply content
  const thinkRegex = new RegExp('<think>[\\s\\S]*?</think>', 'g');
  replyContent = replyContent.replace(thinkRegex, "");
  // Remove any unclosed <think> at the end
  const lastUnclosedThink = replyContent.lastIndexOf("<think>");
  if (lastUnclosedThink !== -1 && replyContent.indexOf("</think>", lastUnclosedThink + 7) === -1) {
    replyContent = replyContent.slice(0, lastUnclosedThink);
  }

  // Trim leading whitespace/newlines from reply content
  replyContent = replyContent.trimStart();

  return { thinkContent, replyContent, thinkClosed: true };
}

/** Shell tools (bash, powershell, shell) need Terminal icon and command preview. */

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
          className="fixed z-[100] min-w-[120px] rounded-md border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
          style={{ left: contextMenu.x, top: contextMenu.y }}
          onContextMenu={(e) => e.stopPropagation()}
        >
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
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

/** Single message bubble */
function MessageBubble({ message, isStreaming, agentId }: { message: ChatMessage; isStreaming: boolean; agentId: string }) {
  const [expanded, setExpanded] = useState(false);
  // Use CSS custom property for font size — set once in store, global effect
  const fontSizeStyle = { fontSize: "var(--ui-font-size, 0.875rem)" };
  // Live names — subscribe to profile stores so name edits update all messages instantly
  // (instead of relying on the senderDisplayName snapshot captured at message creation time)
  const userDisplayName = useUserProfileStore((s) => s.profile.displayName);
  const userAvatarUrl = useUserProfileStore((s) => s.profile.backendAvatarUrl);
  const userBuiltinAvatarId = useUserProfileStore((s) => s.profile.backendBuiltinAvatarId);
  const agentProfileName = useAgentStore((s) => s.agents[agentId]?.profile?.displayName);
  const agentInfo = useAgentStore((s) => s.agents[agentId]?.meta);
  const liveAgentName = agentProfileName ?? agentInfo?.display_name ?? agentInfo?.name ?? message.senderDisplayName;
  const liveUserName = userDisplayName ?? message.senderDisplayName;

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
            avatarUrl={userAvatarUrl ?? null}
            builtinAvatarId={userBuiltinAvatarId ?? null}
            size={40}
            className="shrink-0 mt-1"
          />
        </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "assistant") {
    const showPlaceholder = !message.content;

    return (
      <MessageContentWrapper>
        <div className="flex items-start gap-2">
          <AgentAvatar
            agentId={agentId}
            displayName={liveAgentName}
            avatarUrl={agentInfo?.avatar}
            version={agentInfo?.version}
            builtinAvatarId={agentInfo?.builtin_avatar ?? null}
            size={40}
            className="shrink-0 mt-1"
          />
          <div className="min-w-0 flex-1 flex flex-col">
            <div className="flex items-center gap-1.5 mt-[5px]">
              {liveAgentName && (
                <span className="text-xs font-medium text-zinc-400 dark:text-zinc-500">{liveAgentName}</span>
              )}
              {message.senderRole && (
                <span className="rounded bg-chat-badge px-1 py-0 text-[10px] font-medium text-zinc-500 dark:text-zinc-400">{message.senderRole}</span>
              )}
            </div>
            <div className="mt-[6px] max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
              {message.content && (
                <div className="prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text break-words [&_th]:bg-chat-title [&_td]:bg-chat-body [&_tbody_tr]:!bg-transparent" style={fontSizeStyle}>
                  <StreamMarkdown content={message.content} />
                </div>
              )}
              {showPlaceholder && (
                <span className="inline-flex items-center gap-1.5">
                  <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                  <span className="text-zinc-400">Thinking...</span>
                </span>
              )}
              {isStreaming && <span className="ml-0.5 inline-block animate-pulse">▌</span>}
            </div>
          </div>
        </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "thought") {
    return (
      <MessageContentWrapper>
        <div className="flex items-start gap-2">
          <AgentAvatar
            agentId={agentId}
            displayName={liveAgentName}
            avatarUrl={agentInfo?.avatar}
            version={agentInfo?.version}
            builtinAvatarId={agentInfo?.builtin_avatar ?? null}
            size={40}
            className="shrink-0 mt-1"
          />
          <div className="min-w-0 flex-1 flex flex-col">
            <div className="flex items-center gap-1.5 mt-[5px]">
              {liveAgentName && (
                <span className="text-xs font-medium text-zinc-400 dark:text-zinc-500">{liveAgentName}</span>
              )}
              {message.senderRole && (
                <span className="rounded bg-chat-badge px-1 py-0 text-[10px] font-medium text-zinc-500 dark:text-zinc-400">{message.senderRole}</span>
              )}
            </div>
            <div className="mt-[6px] max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
              <ThinkBlock
                content={message.content}
                isStreaming={isStreaming}
                hasReplyStarted={!isStreaming}
                startTime={message.startTime}
                endTime={message.endTime}
              />
            </div>
          </div>
        </div>
      </MessageContentWrapper>
    );
  }

  if (message.type === "error") {
    return (
      <MessageContentWrapper>
        <div className="flex items-start gap-2">
          <AgentAvatar
            agentId={agentId}
            displayName={liveAgentName}
            avatarUrl={agentInfo?.avatar}
            version={agentInfo?.version}
            builtinAvatarId={agentInfo?.builtin_avatar ?? null}
            size={40}
            className="shrink-0 mt-1"
          />
          <div className="min-w-0 flex-1 flex flex-col">
            <div className="flex items-center gap-1.5 mt-[5px]">
              {liveAgentName && (
                <span className="text-xs font-medium text-zinc-400 dark:text-zinc-500">{liveAgentName}</span>
              )}
              {message.senderRole && (
                <span className="rounded bg-chat-badge px-1 py-0 text-[10px] font-medium text-zinc-500 dark:text-zinc-400">{message.senderRole}</span>
              )}
            </div>
            <div className="mt-[6px] max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 dark:text-zinc-200 select-text break-words overflow-hidden" style={fontSizeStyle}>
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
        <CompactionCard
          summary={message.content}
          meta={message.compactionMeta}
          timestampMs={message.timestamp}
        />
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
}

/** Dialog shown when user tries to upload an image but the current model doesn't support it */
function UnsupportedImageDialog({
  open,
  models,
  onSelect,
  onClose,
}: {
  open: boolean;
  models: ModelEntry[];
  onSelect: (model: string, provider: string) => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  if (!open) return null;

  return (
    <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50" onClick={onClose}>
      <div
        className="w-[400px] overflow-hidden rounded-md bg-white shadow-xl dark:bg-zinc-800 flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="shrink-0 px-6 pt-6 pb-2 text-sm font-semibold text-zinc-900 dark:text-zinc-100">
          {t("chatPanel.imageUnsupportedTitle")}
        </h3>
        <p className="px-6 pb-4 text-xs text-zinc-500 dark:text-zinc-400">
          {t("chatPanel.imageUnsupportedDesc")}
        </p>

        <div className="max-h-[240px] overflow-y-auto px-6 pb-2">
          {models.map((m) => (
            <button
              key={`${m.name}::${m.provider}`}
              type="button"
              onClick={() => {
                onSelect(m.name, m.provider);
              }}
              className="flex w-full items-center justify-between px-2.5 py-1.5 text-xs transition-colors rounded-md hover:bg-zinc-50 dark:hover:bg-zinc-700/50 text-zinc-600 dark:text-zinc-300"
            >
              <span className="flex items-center gap-1.5 min-w-0">
                <Image size={12} className="shrink-0 text-blue-400" />
                <span className="font-medium truncate">
                  {(() => {
                    if (!m.name.includes('/')) return m.name;
                    const parts = m.name.split('/');
                    const prefix = parts[0];
                    const modelName = parts.slice(1).join('/');
                    return modelName.length > prefix.length ? modelName : m.name;
                  })()}
                </span>
                <span className="flex items-center gap-0.5 shrink-0">
                  {m.tool_call && <Wrench size={10} className="text-zinc-400" />}
                  {m.reasoning && <Brain size={10} className="text-purple-400" />}
                </span>
              </span>
              <span className="text-[10px] text-zinc-400 dark:text-zinc-500 shrink-0 ml-2">
                {m.provider}
              </span>
            </button>
          ))}
        </div>

        <div className="shrink-0 flex items-center justify-end gap-2 border-t border-zinc-100 dark:border-zinc-800 px-6 py-4">
          <button
            onClick={onClose}
            className="rounded-md px-4 py-1.5 text-xs font-medium text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
          >
            {t("chatPanel.close")}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Popup-style model selector with provider shown in gray */
function ModelMenu({
  models,
  currentModel,
  currentProvider,
  onSelect,
}: {
  models: { name: string; provider: string; tool_call?: boolean; reasoning?: boolean; input_modalities?: string[] }[];
  currentModel: string | null;
  currentProvider: string | null;
  onSelect: (model: string, provider: string) => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [showAddDialog, setShowAddDialog] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // Calculate menu width based on longest model name + provider
  const menuWidth = useMemo(() => {
    const CHAR_WIDTH = 7.5; // Approximate px per character for text-xs
    const PADDING = 30; // Left + right padding (12.5px each side)
    const GAP = 12; // Space between model and provider (~2 chars)
    let maxWidth = 0;

    for (const m of models) {
      const displayName = m.name.includes('/') && m.name.split('/')[0].length < m.name.split('/').slice(1).join('/').length
        ? m.name.split('/').slice(1).join('/')
        : m.name;
      const itemWidth = displayName.length * CHAR_WIDTH + m.provider.length * CHAR_WIDTH + GAP + PADDING;
      if (itemWidth > maxWidth) maxWidth = itemWidth;
    }

    return Math.max(maxWidth, 180); // Minimum 180px
  }, [models]);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  const modelDisplayName = (() => {
    if (!currentModel || !currentModel.includes('/')) return currentModel ?? "Model";
    const parts = currentModel.split('/');
    const prefix = parts[0];
    const modelName = parts.slice(1).join('/');
    return modelName.length > prefix.length ? modelName : currentModel;
  })();

  return (
    <ToolbarDropdownTrigger
      icon={<Cpu size={14} />}
      label={modelDisplayName}
      collapseClass="tb-model-text"
      tipClass="tb-model-tip"
      tooltip={t("chatPanel.selectModel")}
      open={open}
      onToggle={() => setOpen(!open)}
      wrapperRef={ref}
    >
      {/* Popup menu */}
      {open && (
        <div
          className={cn(
            "absolute bottom-full left-0 z-50 mb-1 overflow-hidden rounded-md border shadow-lg",
            "border-zinc-200 bg-white dark:border-zinc-700 dark:bg-zinc-800",
          )}
          style={{ width: `${menuWidth}px` }}
        >
          {/* Model list */}
          <div className="max-h-[240px] overflow-y-auto">
            {models.map((m) => {
              const isActive = m.name === currentModel && m.provider === currentProvider;
              return (
                <button
                  key={`${m.name}::${m.provider}`}
                  type="button"
                  onClick={() => {
                    onSelect(m.name, m.provider);
                    setOpen(false);
                  }}
                  className={cn(
                    "flex w-full items-center justify-between px-2.5 py-1.5 text-xs transition-colors",
                    isActive
                      ? "text-zinc-900 dark:text-white"
                      : "text-zinc-600 hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50",
                  )}
                >
                  <span className="flex items-center gap-1 min-w-0">
                    <span className={cn("font-medium truncate")} style={isActive ? { color: "var(--color-accent)" } : undefined}>
                      {/* Strip provider prefix from model name if format is provider/model and model is longer */}
                      {(() => {
                        if (!m.name.includes('/')) return m.name;
                        const parts = m.name.split('/');
                        const prefix = parts[0];
                        const modelName = parts.slice(1).join('/');
                        // Only strip if model name is longer than prefix (avoid stripping model/provider)
                        return modelName.length > prefix.length ? modelName : m.name;
                      })()}
                    </span>
                    <span className="flex items-center gap-0.5 ml-2">
                      {m.tool_call && <Wrench size={10} className="text-zinc-400" />}
                      {m.reasoning && <Brain size={10} className="text-purple-400" />}
                      {m.input_modalities?.includes('image') && <Image size={10} className="text-blue-400" />}
                    </span>
                  </span>
                  <span className="text-[10px] text-zinc-400 dark:text-zinc-500 shrink-0 ml-2">
                    {m.provider}
                  </span>
                </button>
              );
            })}
          </div>

          {/* Divider */}
          <div className="border-t border-zinc-200 dark:border-zinc-700" />

          {/* Add Models button — same style as Install Agent */}
          <button
            type="button"
            onClick={() => {
              setShowAddDialog(true);
              setOpen(false);
            }}
            className="mx-1.5 mt-2 mb-1.5 flex w-[calc(100%-0.75rem)] items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-3 py-[var(--ui-btn-py)] text-xs font-medium text-zinc-700 transition-colors hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
          >
            <Plus className="h-3.5 w-3.5" />
            Add Model
          </button>
        </div>
      )}

      {/* Add Provider Flow */}
      <AddProviderFlow
        open={showAddDialog}
        onClose={() => setShowAddDialog(false)}
        onSuccess={() => {
          window.dispatchEvent(new Event('models-added'));
        }}
      />
    </ToolbarDropdownTrigger>
  );
}

/** Reasoning effort selector — popup with Auto/Off/Low/Medium/High */
function ReasoningEffortMenu({
  effort,
  onChange,
}: {
  effort: string | null;
  onChange: (effort: string) => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  // Values are lowercase to match backend ReasoningEffort serde serialization.
  const OPTIONS: { value: string; label: string; color: string }[] = [
    { value: "auto", label: "Auto", color: "#22c55e" },
    { value: "off", label: "Off", color: "#9ca3af" },
    { value: "low", label: "Low", color: "#3b82f6" },
    { value: "medium", label: "Medium", color: "#8b5cf6" },
    { value: "high", label: "High", color: "#ef4444" },
  ];

  const currentOpt = OPTIONS.find((o) => o.value === effort);
  const effortLabel = currentOpt?.label ?? "Auto";
  const currentColor = currentOpt?.color ?? "#22c55e";

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  return (
    <ToolbarDropdownTrigger
      icon={<Brain size={14} style={{ color: currentColor }} />}
      label={effortLabel}
      collapseClass="tb-model-text"
      tipClass="tb-model-tip"
      tooltip={t("chatPanel.selectReasoningEffort") ?? "Reasoning effort"}
      open={open}
      onToggle={() => setOpen(!open)}
      wrapperRef={ref}
    >
      {open && (
        <div
          className={cn(
            "absolute bottom-full left-0 z-50 mb-1 overflow-hidden rounded-md border shadow-lg",
            "border-zinc-200 bg-white dark:border-zinc-700 dark:bg-zinc-800",
          )}
          style={{ width: "140px" }}
        >
          {OPTIONS.map((opt) => {
            const isActive = opt.value === effort;
            return (
              <button
                key={opt.value}
                type="button"
                onClick={() => {
                  onChange(opt.value);
                  setOpen(false);
                }}
                className={cn(
                  "flex w-full items-center gap-2 px-2.5 py-1.5 text-xs transition-colors",
                  isActive
                    ? "text-zinc-900 dark:text-white"
                    : "text-zinc-600 hover:bg-zinc-50 dark:text-zinc-300 dark:hover:bg-zinc-700/50",
                )}
              >
                <span
                  className="h-2 w-2 rounded-full shrink-0"
                  style={{ backgroundColor: opt.color }}
                />
                <span
                  className={cn("font-medium", isActive && "text-[var(--color-accent)]")}
                >
                  {opt.label}
                </span>
              </button>
            );
          })}
        </div>
      )}
    </ToolbarDropdownTrigger>
  );
}

