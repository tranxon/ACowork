import { useEffect, useLayoutEffect, useRef, useState, useCallback, useMemo, Children, isValidElement, type ReactNode } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { invoke } from "@tauri-apps/api/core";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useSessionStore } from "../../stores/sessionStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSkillStore } from "../../stores/skillStore";
import { useAgentProfileStore } from "../../stores/agentProfileStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import type { ToolApprovalNeededEvent } from "../../lib/types";
import { cn } from "../../lib/utils";
import { getGatewayUrl } from "../../lib/config";
import { needsApiKey, keyPlaceholder } from "../../lib/providers";
import { fetchProviderModels, fetchProviders } from "../../lib/gateway-api";
import { emitAgentConfigRefresh } from "../../lib/refresh";
import { syncAgentUI } from "../../lib/agent-start";
import { toolbarButton, toolbarButtonActive } from "../../lib/ui-styles";
import { Bot, Play, Send, ChevronDown, ChevronRight, Wrench, AlertTriangle, X, Square, Copy, Plus, RefreshCw, Cpu, Loader, Pencil, Paperclip, Image, Brain } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatMessage, VaultKeyEntry, ModelInfo, ModelEntry, ModelCapabilitiesMap } from "../../lib/types";
import { ThinkBlock } from "./ThinkBlock";
import { ExploreBlock } from "./ExploreBlock";
import { CodeBlock } from "./CodeBlock";

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
      const code = Children.toArray(codeContent).join("");
      return <CodeBlock language={language} code={code} />;
    }
    return <pre>{children}</pre>;
  },
};
import { AskQuestionCard } from "./AskQuestionCard";
import { SessionTabBar } from "./SessionTabBar";
import { SkillsPanel } from "../skills/SkillsPanel";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";
import { UserAvatar } from "../common/UserAvatar";
import { AgentAvatar } from "../common/AgentAvatar";
import { DocumentChip } from "./DocumentChip";

// Module-level: persists across ChatPanel mount/unmount cycles
// so nav-back (Settings→Chat) doesn't trigger full reinit
let lastInitAgentId: string | null = null;
/** Tracks the last session ID for which messages were loaded.
 *  Prevents redundant reload when remounting after navigation. */
let lastLoadedSessionId: string | null = null;

export function ChatPanel() {
  const { agents, selectedAgentId, startAgent } = useAgentStore();

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

  // Global state and actions
  const { wsMap, connectStream, sendMessage, sendInterrupt, availableModels, setCurrentModel, setAvailableModels, continueExecution, resolveApproval, resolveApprovalByToolCallId } = useChatStore();
  const currentSessionId = useSessionStore((s) => s.currentSessionId);
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
  /** True only during the first useEffect run of this mount. */
  const justMountedRef = useRef(true);

  const selectedAgent = agents.find((a) => a.agent_id === selectedAgentId);
  const agentDisplayName = useAgentProfileStore((s) => selectedAgentId ? s.profiles[selectedAgentId]?.displayName : undefined) ?? selectedAgent?.display_name ?? selectedAgent?.name;

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
      } else if (msg.type === 'document_upload' || msg.type === 'system') {
        // Document upload and system messages: flush explore, pass through as-is
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

  // Virtual scrolling: only render visible items (messages + optional thinking indicator)
  const virtualCount = displayMessages.length + (showThinkingItem ? 1 : 0);
  const virtualizer = useVirtualizer({
    count: virtualCount,
    getScrollElement: () => messagesContainerRef.current,
    estimateSize: () => 80,
    overscan: 5,
    gap: 4,
  });

  // Load available models from Vault keys (capabilities come from persisted model_capabilities)
  const loadModels = useCallback(async () => {
    try {
      const keys = await invoke<VaultKeyEntry[]>("list_keys");
      const allModels: ModelEntry[] = [];
      for (const key of keys) {
        const provider = key.provider;
        const capsMap = key.model_capabilities;
        if (key.models?.length) {
          for (const model of key.models) {
            const caps = capsMap?.[model];
            allModels.push({
              name: model,
              provider,
              tool_call: caps?.supports_tool_calling ?? undefined,
              reasoning: caps?.supports_reasoning ?? undefined,
              input_modalities: caps?.modalities?.input ?? undefined,
            });
          }
        } else if (key.default_model) {
          const caps = capsMap?.[key.default_model];
          allModels.push({
            name: key.default_model,
            provider,
            tool_call: caps?.supports_tool_calling ?? undefined,
            reasoning: caps?.supports_reasoning ?? undefined,
            input_modalities: caps?.modalities?.input ?? undefined,
          });
        }
      }
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

  // Listen for models-added event from AddModelDialog
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
    const leavingSessionId = useSessionStore.getState().currentSessionId;
    if (leavingAgentId && leavingSessionId) {
      useSessionStore.getState().saveSessionForAgent(leavingAgentId, leavingSessionId);
    }

    if (!isSameAgentRemount) {
      // Allow reload for new agent/session
      lastLoadedSessionId = null;
      // Reset session state (sessions, currentSessionId, etc.)
      useSessionStore.getState().reset();
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
            let sessions = useSessionStore.getState().sessions;

            for (let i = 0; i < maxRetries; i++) {
              if (initAbortedRef.current) return;
              await useSessionStore.getState().fetchSessions(selectedAgentId);
              sessions = useSessionStore.getState().sessions;
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
            const rememberedSessionId = useSessionStore.getState().agentSessionMap[selectedAgentId];
            const targetSession = rememberedSessionId
              ? sessions.find((s) => s.session_id === rememberedSessionId) ?? sessions[0]
              : sessions[0];
            if (targetSession) {
              // switchSession first (clears old messages), then loadSessionMessages
              // so that messages are not cleared after loading.
              await useSessionStore.getState().switchSession(targetSession.session_id, selectedAgentId);
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
            await useSessionStore.getState().fetchSessions(selectedAgentId);
            const rememberedId = useSessionStore.getState().agentSessionMap[selectedAgentId];
            const sessions = useSessionStore.getState().sessions;
            if (rememberedId && sessions.some(s => s.session_id === rememberedId)) {
              useSessionStore.getState().switchSession(rememberedId, selectedAgentId);
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
    const session = useSessionStore
      .getState()
      .sessions.find((s) => s.session_id === currentSessionId);
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
      } else if (virtualCount > prevCount) {
        // New message arrived or thinking indicator appeared: smooth scroll to bottom
        virtualizer.scrollToIndex(virtualCount - 1, { align: "end", behavior: "smooth" });
      }
    }

    prevDisplayCountRef.current = virtualCount;
  }, [messages, virtualCount, virtualizer, selectedAgentId, currentSessionId, showThinkingItem]);

  // Scroll handler: load more messages when scrolled to top
  const handleScroll = useCallback(() => {
    const container = messagesContainerRef.current;
    if (!container || !selectedAgentId) return;

    const { isLoadingMore } = useChatStore.getState();
    const agent = useChatStore.getState().agentStates[selectedAgentId];
    const activeSessId = agent?.activeSessionId;
    const sessState = activeSessId ? agent?.sessionStates[activeSessId] : undefined;
    const hasMoreMessages = sessState?.hasMoreMessages ?? false;
    const currentSessionId = useSessionStore.getState().currentSessionId;
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
    void sendMessage(content, selectedAgentId, activeSkill?.name, documentIds.length > 0 ? documentIds : undefined, documents.length > 0 ? documents : undefined, imageParts.length > 0 ? imageParts : undefined).then(() => {
      clearActiveSkill();
    });
    setInputValue("");
    // Clear pending files and images after send
    setPendingFiles([]);
    setPendingImages([]);
  };

  // Stop button dual-action:
  //   input has content → send to queue (no interrupt, message waits for next loop)
  //   input empty       → interrupt current loop
  const handleStop = () => {
    const content = inputValue.trim();
    if (content && selectedAgentId) {
      // Add to queue — message waits in the queue box above the input area.
      setQueuedMessages(prev => [...prev, content]);
      setInputValue("");
    } else if (queuedMessages.length > 0 && selectedAgentId) {
      // Click with queued messages: send all queued + interrupt current loop.
      for (const msg of queuedMessages) {
        void sendMessage(msg, selectedAgentId, activeSkill?.name).then(() => {
          clearActiveSkill();
        });
      }
      setQueuedMessages([]);
      sendInterrupt();
    } else {
      // No queued messages: just interrupt
      sendInterrupt();
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
        title: "选择图片",
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
    const sessionId = useSessionStore.getState().currentSessionId;
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
  if (agents.length === 0) {
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
          <button
            onClick={async () => {
              await startAgent(selectedAgent.agent_id);
              await syncAgentUI(selectedAgent.agent_id);
            }}
            className="mx-auto flex h-20 w-20 items-center justify-center rounded-full btn-solid"
            title="Start Agent"
          >
            <Play className="h-8 w-8" />
          </button>
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
        className="flex flex-1 min-w-0 flex-col bg-[#FAFAFA] dark:bg-zinc-900"
      >
        {/* LLM config warning */}
        {hasLlmConfig === false && (
          <div className="flex items-center gap-2 border-b border-amber-200 bg-amber-50 px-4 py-2 dark:border-amber-900 dark:bg-amber-950">
            <AlertTriangle className="h-4 w-4 text-amber-600 dark:text-amber-400" />
            <span className="text-xs text-amber-700 dark:text-amber-300">
              No LLM provider configured. Please add an API key in Settings → Providers.
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
            {useChatStore.getState().isLoadingMore && (
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
                <div className="text-sm text-red-500 dark:text-red-400">Session 加载失败</div>
                <div className="max-w-xs text-center text-xs text-zinc-500 dark:text-zinc-400">
                  {loadError}
                </div>
                <button
                  onClick={() => {
                    const sessionId = useSessionStore.getState().currentSessionId;
                    const agentId = useAgentStore.getState().selectedAgentId;
                    if (sessionId && agentId) {
                      useChatStore.getState().loadSessionMessages(agentId, sessionId);
                    }
                  }}
                  className="rounded-md bg-zinc-100 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-600"
                >
                  重试
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
                  // --- Thinking indicator (extra virtual item below messages) ---
                  if (showThinkingItem && virtualRow.index === displayMessages.length) {
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
                          <span className="thinking-shimmer" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>thinking ...</span>
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
        </div>

        {/* Todo list box — above the message queue, same collapsible style.
          Shows current task list from todo_write tool calls. */}
        {todos.length > 0 && (
          <div className="mx-5 mb-0 rounded-t-lg border border-b-0 border-zinc-200 dark:border-zinc-800 bg-zinc-50/80 dark:bg-zinc-800/60 overflow-hidden">
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
                任务列表 ({todos.filter(t => t.status === "completed").length}/{todos.length})
              </span>
            </button>
            {!todosCollapsed && (
              <div className="max-h-[7.5rem] overflow-y-auto">
                {todos.map((item) => {
                  const statusMark = item.status === "completed" ? "x" : item.status === "in_progress" ? "-" : " ";
                  const isCompleted = item.status === "completed";
                  return (
                    <div
                      key={item.id}
                      className="flex items-start gap-1.5 px-2.5 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-700/40 border-b border-zinc-100 dark:border-zinc-700/30 last:border-b-0"
                    >
                      <span className={cn(
                        "shrink-0 text-[10px] mt-0.5 select-none font-mono",
                        isCompleted ? "text-zinc-400 dark:text-zinc-500" : "text-zinc-500 dark:text-zinc-400"
                      )}>
                        [{statusMark}]
                      </span>
                      <span className={cn(
                        "flex-1 min-w-0 text-xs leading-relaxed",
                        isCompleted
                          ? "text-zinc-400 dark:text-zinc-500 line-through"
                          : item.status === "in_progress"
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
            todos.length > 0 ? "" : "rounded-t-lg"
          )}>
            <div className="flex items-center px-2.5 py-1.5 border-b border-zinc-200 dark:border-zinc-800">
              <span className="text-[10px] font-medium text-zinc-400 dark:text-zinc-500 uppercase tracking-wider">
                消息队列 ({queuedMessages.length})
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
                      className="rounded-sm p-0.5 text-zinc-400 hover:text-blue-500 hover:bg-blue-50 dark:hover:text-blue-400 dark:hover:bg-blue-900/30"
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
        <div className="mx-3 mb-3 rounded-xl border border-zinc-200 dark:border-zinc-700 bg-[#FAFAFA] dark:bg-zinc-900">
          {/* Active skill badge */}
          {activeSkill && (
            <div className="flex items-center gap-1 px-3 pt-2">
              <span className="inline-flex items-center gap-1 rounded bg-[var(--color-accent)]/10 px-1.5 py-0.5 text-xs font-medium border border-[var(--color-accent)]/20" style={{ color: "var(--color-accent)" }}>
                /{activeSkill.name}
                <button
                  type="button"
                  onClick={clearActiveSkill}
                  className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-blue-100 dark:hover:bg-blue-800"
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
                  className="group relative h-14 w-14 shrink-0 overflow-hidden rounded-lg border border-zinc-200 dark:border-zinc-700"
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
          {/* Textarea area — borderless, transparent background */}
          <textarea
            value={inputValue}
            onChange={(e) => setInputValue(e.target.value)}
            placeholder={
              gatewayStatus !== "connected"
                ? "Gateway not connected"
                : !wsMap[selectedAgentId!] || wsMap[selectedAgentId!].readyState !== WebSocket.OPEN
                  ? activeSkill
                    ? "输入参数... (Connecting to agent...)"
                    : "Type a message... (Connecting to agent...)"
                  : activeSkill
                    ? "输入参数... (Enter to send, Shift+Enter for new line)"
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

          {/* Bottom toolbar */}
          <div className="flex items-center justify-between px-3 pb-2">
            {/* Left: feature buttons */}
            <div className="flex items-center gap-1">
              {/* Model switcher — only enabled when agent is running */}
              {availableModels.length > 1 && selectedAgent?.running && (
                <ModelMenu
                  models={availableModels}
                  currentModel={currentModel}
                  currentProvider={currentProvider}
                  onSelect={(m, p) => selectedAgentId && setCurrentModel(m, p, selectedAgentId)}
                />
              )}
              {/* Workspace button */}
              <WorkspaceSelector />
              {/* Skills dropdown */}
              <SkillsPanel />
              {/* File upload button */}
              <div className="group relative">
                <button
                  className={toolbarButton}
                  onClick={handleFileUpload}
                  disabled={!currentSessionId || !selectedAgentId}
                  aria-label="上传文件"
                >
                  <Paperclip size={14} />
                </button>
                {/* Tooltip */}
                <div className="pointer-events-none absolute bottom-full left-1/2 -translate-x-1/2 mb-1.5 hidden group-hover:block z-50">
                  <div className="whitespace-nowrap rounded-md bg-zinc-800 dark:bg-zinc-200 px-2.5 py-1.5 text-[11px] leading-tight text-white dark:text-zinc-800 shadow-lg">
                    上传文件 (PDF/DOCX/PPTX/XLSX)
                  </div>
                </div>
              </div>
              {/* Image upload button */}
              <div className="group relative">
                <button
                  className={toolbarButton}
                  onClick={handleImageSelect}
                  disabled={!currentSessionId || !selectedAgentId}
                  aria-label="上传图片"
                >
                  <Image size={14} />
                </button>
                {/* Tooltip */}
                <div className="pointer-events-none absolute bottom-full left-1/2 -translate-x-1/2 mb-1.5 hidden group-hover:block z-50">
                  <div className="whitespace-nowrap rounded-md bg-zinc-800 dark:bg-zinc-200 px-2.5 py-1.5 text-[11px] leading-tight text-white dark:text-zinc-800 shadow-lg">
                    上传图片 (PNG/JPG/GIF/WebP)
                  </div>
                </div>
              </div>
            </div>

            {/* Right: send/stop button */}

            {/* Send/Stop button with tooltip above */}
            <div className="group relative">
              <button
                className={`rounded-lg p-1.5 transition-colors ${sending
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
                aria-label={sending ? (inputValue.trim() ? "Add to queue" : queuedMessages.length > 0 ? "Send queued & stop" : "Stop") : "Send message"}
              >
                {sending ? <Square size={16} fill="currentColor" /> : <Send size={16} />}
              </button>
              {/* Tooltip — shown above the button on hover */}
              <div className="pointer-events-none absolute bottom-full left-1/2 -translate-x-1/2 mb-1.5 hidden group-hover:block z-50">
                <div className="whitespace-nowrap rounded-md bg-zinc-800 dark:bg-zinc-200 px-2.5 py-1.5 text-[11px] leading-tight text-white dark:text-zinc-800 shadow-lg">
                  {sending
                    ? (inputValue.trim()
                      ? "加入队列"
                      : queuedMessages.length > 0
                        ? "发送队列 & 停止"
                        : "停止")
                    : "发送消息"}
                </div>
              </div>
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
          className="fixed z-[100] min-w-[120px] rounded-lg border border-zinc-200 bg-white py-1 shadow-lg dark:border-zinc-700 dark:bg-zinc-800"
          style={{ left: contextMenu.x, top: contextMenu.y }}
          onContextMenu={(e) => e.stopPropagation()}
        >
          <button
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-zinc-700 hover:bg-zinc-100 dark:text-zinc-300 dark:hover:bg-zinc-700"
            onClick={handleCopy}
          >
            <Copy size={14} />
            <span>复制</span>
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
  // Agent icon from profile settings
  const agentIconId = useAgentProfileStore((s) => s.profiles[agentId]?.avatarIconId);
  // Live names — subscribe to profile stores so name edits update all messages instantly
  // (instead of relying on the senderDisplayName snapshot captured at message creation time)
  const userDisplayName = useUserProfileStore((s) => s.profile.displayName);
  const agentProfileName = useAgentProfileStore((s) => s.profiles[agentId]?.displayName);
  const agentInfo = useAgentStore((s) => s.agents.find((a) => a.agent_id === agentId));
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
              <div className="mt-[6px] max-w-[85%] rounded-lg rounded-br-sm bg-[var(--color-accent)]/15 px-4 py-2.5 text-zinc-900 dark:text-zinc-200 select-text whitespace-pre-wrap break-words max-h-48 overflow-y-auto" style={fontSizeStyle}>
                {message.content}
              </div>
            )}
          </div>
          <UserAvatar
            displayName={liveUserName}
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
            iconId={agentIconId}
            size={40}
            className="shrink-0 mt-1"
          />
          <div className="min-w-0 flex-1 flex flex-col items-start">
            <div className="flex items-center gap-1.5 mt-[5px]">
              {liveAgentName && (
                <span className="text-xs font-medium text-zinc-400 dark:text-zinc-500">{liveAgentName}</span>
              )}
              {message.senderRole && (
                <span className="rounded bg-zinc-200 px-1 py-0 text-[10px] font-medium text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400">{message.senderRole}</span>
              )}
            </div>
            <div className="mt-[6px] max-w-[var(--content-max-width)] rounded-lg rounded-bl-sm bg-zinc-100 px-4 py-2.5 dark:bg-zinc-800 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
              {message.content && (
                <div className="prose prose-sm prose-zinc max-w-none prose-h1:text-lg prose-h2:text-base prose-h3:text-sm prose-h4:text-sm prose-headings:font-semibold select-text break-words [&_table]:bg-zinc-200/20 [&_tbody_tr]:!bg-transparent dark:[&_table]:bg-zinc-900/30" style={fontSizeStyle}>
                  <ReactMarkdown remarkPlugins={[remarkGfm]} components={markdownComponents}>{message.content}</ReactMarkdown>
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
            iconId={agentIconId}
            size={40}
            className="shrink-0 mt-1"
          />
          <div className="min-w-0 flex-1 flex flex-col items-start">
            <div className="flex items-center gap-1.5 mt-[5px]">
              {liveAgentName && (
                <span className="text-xs font-medium text-zinc-400 dark:text-zinc-500">{liveAgentName}</span>
              )}
              {message.senderRole && (
                <span className="rounded bg-zinc-200 px-1 py-0 text-[10px] font-medium text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400">{message.senderRole}</span>
              )}
            </div>
            <div className="mt-[6px] max-w-[var(--content-max-width)] rounded-lg rounded-bl-sm bg-zinc-100 px-4 py-2.5 dark:bg-zinc-800 dark:text-zinc-200 select-text break-words" style={fontSizeStyle}>
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
            iconId={agentIconId}
            size={40}
            className="shrink-0 mt-1"
          />
          <div className="min-w-0 flex-1 flex flex-col items-start">
            <div className="flex items-center gap-1.5 mt-[5px]">
              {liveAgentName && (
                <span className="text-xs font-medium text-zinc-400 dark:text-zinc-500">{liveAgentName}</span>
              )}
              {message.senderRole && (
                <span className="rounded bg-zinc-200 px-1 py-0 text-[10px] font-medium text-zinc-500 dark:bg-zinc-700 dark:text-zinc-400">{message.senderRole}</span>
              )}
            </div>
            <div className="mt-[6px] max-w-[var(--content-max-width)] rounded-lg rounded-bl-sm bg-zinc-100 px-4 py-2.5 dark:bg-zinc-800 dark:text-zinc-200 select-text break-words overflow-hidden" style={fontSizeStyle}>
              <div className="flex items-start gap-2 min-w-0">
                <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-500" />
                <div className="min-w-0 whitespace-pre-wrap break-words">{message.content}</div>
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
          <div className="rounded bg-zinc-100 px-3 py-1 text-xs text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400 select-text">
            {message.content}
          </div>
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
        <div className="flex w-full items-center gap-2 rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-1.5 text-left text-xs text-zinc-500 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800">
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
            className="flex w-full items-center gap-2 rounded-lg border border-zinc-200 bg-zinc-50 px-3 py-1.5 text-left text-xs text-zinc-500 transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-400 dark:hover:bg-zinc-800"
            onClick={() => setExpanded(!expanded)}
          >
            <Wrench className="h-3 w-3 shrink-0" />
            <span className="font-medium">{message.toolName}</span>
            <span className="text-zinc-400 dark:text-zinc-500">→ Result</span>
            <span className="ml-auto text-[10px] text-zinc-400 dark:text-zinc-500">Click to view</span>
            {expanded ? <ChevronDown className="ml-2 h-3 w-3 shrink-0" /> : <ChevronRight className="ml-2 h-3 w-3 shrink-0" />}
          </button>
          {expanded && (
            <pre className="mt-1 max-w-full overflow-x-auto rounded-lg bg-zinc-50 p-3 text-xs text-zinc-600 dark:bg-zinc-800/50 dark:text-zinc-400 select-text">
              {message.content}
            </pre>
          )}
        </div>
      </MessageContentWrapper>
    );
  }

  return null;
}

/** Add Key dialog — exact copy from SettingsPage with provider as dropdown */
function AddModelDialog({
  open,
  onClose,
  onSuccess,
}: {
  open: boolean;
  onClose: () => void;
  onSuccess: () => void;
}) {
  const [provider, setProvider] = useState("minimax");
  const [key, setKey] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [models, setModels] = useState<string[]>([]);
  const [availableModels, setAvailableModels] = useState<ModelInfo[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelSearchTerm, setModelSearchTerm] = useState("");
  const [modelCapabilityFilter, setModelCapabilityFilter] = useState<string[]>([]);
  const [contextWindow, setContextWindow] = useState("");
  const [maxOutputTokens, setMaxOutputTokens] = useState("");
  const [supportsToolCalling, setSupportsToolCalling] = useState(true);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ success: boolean; message: string } | null>(null);
  const [dynamicProviders, setDynamicProviders] = useState<Array<{ id: string; name: string; api?: string }>>([]);
  const [providersLoading, setProvidersLoading] = useState(false);
  const [keys, setKeys] = useState<VaultKeyEntry[]>([]);


  // Fetch existing keys to check which models are already added
  useEffect(() => {
    if (!open) return;
    const loadKeys = async () => {
      try {
        const result = await invoke<VaultKeyEntry[]>("list_keys");
        setKeys(result);
      } catch {
        // Ignore errors
      }
    };
    loadKeys();
  }, [open]);

  // Fetch dynamic providers from Gateway cache on open
  useEffect(() => {
    if (!open) return;
    const loadProviders = async () => {
      setProvidersLoading(true);
      try {
        const CACHE_KEY = "rollball_models_cache";
        const cachedData = localStorage.getItem(CACHE_KEY);
        if (cachedData) {
          const parsed = JSON.parse(cachedData);
          const providers = (parsed.providers || []).map((p: any) => ({
            id: p.id,
            name: p.name || p.id,
            api: p.api,
          }));
          if (providers.length > 0) {
            setDynamicProviders(providers);
            setProvidersLoading(false);
            return;
          }
        }
        const providers = await fetchProviders();
        setDynamicProviders(providers);
      } catch {
        setDynamicProviders([]);
      }
      setProvidersLoading(false);
    };
    loadProviders();
  }, [open]);

  // Fetch models when provider changes
  useEffect(() => {
    if (!open || !provider) return;
    const loadModels = async () => {
      setModelsLoading(true);
      try {
        const data = await fetchProviderModels(provider);
        setAvailableModels(data.models ?? []);
      } catch {
        setAvailableModels([]);
      }
      setModelsLoading(false);
    };
    loadModels();
  }, [provider, open]);

  // Reset state when provider changes
  useEffect(() => {
    if (!provider) return;
    const dynamicProvider = dynamicProviders.find((p) => p.id === provider);
    setBaseUrl(dynamicProvider?.api ?? "");
    setModels([]);
    setModelSearchTerm("");
    setModelCapabilityFilter([]);
    setContextWindow("");
    setMaxOutputTokens("");
    setSupportsToolCalling(true);
  }, [provider]);

  if (!open) return null;

  const toggleModel = (modelId: string, currentList: string[], setList: (v: string[]) => void) => {
    if (currentList.includes(modelId)) {
      setList(currentList.filter((m) => m !== modelId));
    } else {
      setList([...currentList, modelId]);
    }
  };

  const handleSave = async () => {
    if (needsApiKey(provider) && !key.trim()) {
      setTestResult({ success: false, message: "Please enter an API Key first" });
      return;
    }

    setSaving(true);
    setTesting(true);
    setTestResult(null);

    try {
      // First test the API key
      if (needsApiKey(provider)) {
        // Temporarily add the key
        await invoke("add_key", {
          provider,
          key,
          baseUrl: baseUrl || undefined,
        });

        // Try to fetch models to verify the key works
        await fetchProviderModels(provider);

        setTestResult({ success: true, message: "API Key is valid!" });

        // Remove the temporary key
        await invoke("remove_key", { provider });
      }
    } catch (e: any) {
      const errorMsg = e?.message || e?.toString() || "Test failed";
      setTestResult({ success: false, message: errorMsg });
      setTesting(false);
      setSaving(false);
      return;
    }

    setTesting(false);

    // Test passed, proceed with saving
    try {
      // Get effective values (prefer models.dev data if available)
      const primaryModel = models.length > 0 ? models[0] : "";
      const modelInfo = availableModels.find(m => m.id === primaryModel);
      const hasModelsDevData = !!(modelInfo && (modelInfo.context_window || modelInfo.max_tokens));
      const effectiveContextWindow = hasModelsDevData
        ? (modelInfo?.context_window?.toString() ?? contextWindow)
        : contextWindow;
      const effectiveMaxOutputTokens = hasModelsDevData
        ? (modelInfo?.max_tokens?.toString() ?? maxOutputTokens)
        : maxOutputTokens;
      const effectiveSupportsToolCalling = hasModelsDevData
        ? (modelInfo?.tool_call ?? supportsToolCalling)
        : supportsToolCalling;

      // Rust requires context_window to be present (u64, not Option)
      // Default to 128000 if not specified (safe default for most models)
      const ctxWindow = effectiveContextWindow ? parseInt(effectiveContextWindow) : 128000;
      const maxOutTokens = effectiveMaxOutputTokens ? parseInt(effectiveMaxOutputTokens) : 0;

      // Build per-model capabilities map
      const modelCapabilities: ModelCapabilitiesMap = {};
      for (const modelId of models) {
        const mi = availableModels.find(m => m.id === modelId);
        modelCapabilities[modelId] = {
          context_window: mi?.context_window ?? ctxWindow,
          max_output_tokens: mi?.max_tokens ?? maxOutTokens,
          supports_tool_calling: mi?.tool_call ?? effectiveSupportsToolCalling,
          supports_reasoning: mi?.reasoning ?? undefined,
          modalities: mi?.input_modalities?.length ? { input: mi.input_modalities } : undefined,
        };
      }

      await invoke("add_key", {
        provider,
        key,
        baseUrl: baseUrl || undefined,
        models: models.length > 0 ? models : undefined,
        modelCapabilities,
      });
      emitAgentConfigRefresh();
      onSuccess();
      onClose();
    } catch (e) {
      alert(`Failed to add: ${e}`);
    } finally {
      setSaving(false);
    }
  };

  const showBaseUrl = true;

  return (
    <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50" onClick={onClose}>
      <div
        className="w-[440px] max-h-[85vh] overflow-hidden rounded-lg bg-white shadow-xl dark:bg-zinc-800 flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="shrink-0 px-6 pt-6 pb-3 text-sm font-semibold">Add Model</h3>

        <div className="flex-1 overflow-y-auto px-6">
          <div className="space-y-2">
            {/* Provider dropdown */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">Provider</label>
              {providersLoading ? (
                <div className="flex items-center gap-2 rounded-md border border-zinc-200 px-3 py-2 text-xs text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900">
                  <RefreshCw className="h-3 w-3 animate-spin" />
                  Loading providers...
                </div>
              ) : (
                <select
                  value={provider}
                  onChange={(e) => setProvider(e.target.value)}
                  className="w-full appearance-none rounded-md border border-zinc-200 bg-white px-3 py-2 pr-3 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  style={{
                    backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
                    backgroundPosition: 'right 0.5rem center',
                    backgroundRepeat: 'no-repeat',
                    backgroundSize: '1.5em 1.5em',
                  }}
                >
                  <optgroup label="All Providers">
                    {dynamicProviders.map((p) => (
                      <option key={p.id} value={p.id}>{p.name}</option>
                    ))}
                  </optgroup>
                </select>
              )}
            </div>

            {/* API Key */}
            {needsApiKey(provider) && (
              <div>
                <label className="mb-1 block text-xs text-zinc-500">API Key</label>
                <input
                  type="password"
                  value={key}
                  onChange={(e) => setKey(e.target.value)}
                  placeholder={keyPlaceholder(provider)}
                  className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                />
              </div>
            )}

            {showBaseUrl && (
              <div>
                <label className="mb-1 block text-xs text-zinc-500">Base URL</label>
                <input
                  type="text"
                  value={baseUrl}
                  onChange={(e) => setBaseUrl(e.target.value)}
                  placeholder="https://..."
                  className="w-full rounded-md border border-zinc-200 px-3 py-2 font-mono text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                />
              </div>
            )}

            {/* Model selection */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">
                Model {models.length > 0 && <span className="text-accent-green">({models.length} selected)</span>}
              </label>

              {/* Capability filters */}
              <div className="mb-2 flex gap-2">
                <button
                  onClick={() => setModelCapabilityFilter(
                    modelCapabilityFilter.includes('tool_call')
                      ? modelCapabilityFilter.filter(f => f !== 'tool_call')
                      : [...modelCapabilityFilter, 'tool_call']
                  )}
                  className={cn(
                    "rounded px-2 py-0.5 text-[10px] font-medium",
                    modelCapabilityFilter.includes('tool_call')
                      ? "bg-accent-green/10 text-accent-green"
                      : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                  )}
                >
                  🔧 Tool Calling
                </button>
                <button
                  onClick={() => setModelCapabilityFilter(
                    modelCapabilityFilter.includes('reasoning')
                      ? modelCapabilityFilter.filter(f => f !== 'reasoning')
                      : [...modelCapabilityFilter, 'reasoning']
                  )}
                  className={cn(
                    "rounded px-2 py-0.5 text-[10px] font-medium",
                    modelCapabilityFilter.includes('reasoning')
                      ? "bg-purple-100 text-purple-700 dark:bg-purple-900 dark:text-purple-300"
                      : "bg-zinc-100 text-zinc-600 hover:bg-zinc-200 dark:bg-zinc-700 dark:text-zinc-400"
                  )}
                >
                  🧠 Reasoning
                </button>
              </div>

              {/* Selected models as tags */}
              {models.length > 0 && (
                <div className="mb-1 flex flex-wrap gap-1">
                  {models.map((m) => (
                    <span key={m} className="inline-flex items-center gap-1 rounded bg-accent-green/10 px-2 py-0.5 text-xs text-accent-green">
                      {m}
                      <button onClick={() => setModels(models.filter((x) => x !== m))} className="text-accent-green/60 hover:text-accent-green">×</button>
                    </span>
                  ))}
                </div>
              )}

              {/* Search */}
              <input
                type="text"
                value={modelSearchTerm}
                onChange={(e) => setModelSearchTerm(e.target.value)}
                placeholder="Search models..."
                className="w-full rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
              />

              {/* Model list */}
              <div className="mt-1 max-h-40 overflow-y-auto rounded border border-zinc-200 dark:border-zinc-700">
                {modelsLoading ? (
                  <div className="px-3 py-2 text-xs text-zinc-400">Loading models...</div>
                ) : (
                  availableModels
                    .filter((m) => {
                      const matchesSearch = !modelSearchTerm ||
                        m.id.toLowerCase().includes(modelSearchTerm.toLowerCase()) ||
                        m.name.toLowerCase().includes(modelSearchTerm.toLowerCase());

                      const matchesCapabilities = modelCapabilityFilter.length === 0 ||
                        modelCapabilityFilter.every(filter => {
                          if (filter === 'tool_call') return m.tool_call === true;
                          if (filter === 'reasoning') return m.reasoning === true;
                          return true;
                        });

                      return matchesSearch && matchesCapabilities;
                    })
                    .map((m) => (
                      <label
                        key={m.id}
                        className="flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs hover:bg-zinc-50 dark:hover:bg-zinc-700"
                      >
                        <input
                          type="checkbox"
                          checked={models.includes(m.id)}
                          disabled={keys.some(k => k.provider === provider && k.models?.includes(m.id))}
                          onChange={() => toggleModel(m.id, models, setModels)}
                          className="accent-accent-green disabled:opacity-50"
                        />
                        <div className="flex flex-1 flex-col gap-0.5">
                          <span className="truncate">{m.name || m.id}</span>
                          <div className="flex items-center gap-2 text-[10px] text-zinc-400">
                            {keys.some(k => k.provider === provider && k.models?.includes(m.id)) && (
                              <span className="text-green-600 dark:text-green-400">✓ Added</span>
                            )}
                            {m.context_window && (
                              <span>{(m.context_window / 1000).toFixed(0)}K context</span>
                            )}
                            {m.max_tokens && (
                              <span>{(m.max_tokens / 1000).toFixed(1)}K max output</span>
                            )}
                            {m.reasoning && <span>🧠 reasoning</span>}
                            {m.tool_call && <span>🔧 tools</span>}
                          </div>
                        </div>
                      </label>
                    ))
                )}
                {!modelsLoading && availableModels.length === 0 && (
                  <div className="px-3 py-2 text-xs text-zinc-400">No models found. Select provider first.</div>
                )}
              </div>

              {/* Manual model input */}
              <div className="mt-2 flex gap-1">
                <input
                  type="text"
                  placeholder="Or type a custom model name..."
                  className="flex-1 rounded-md border border-zinc-200 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      const val = (e.target as HTMLInputElement).value.trim();
                      if (val && !models.includes(val)) {
                        setModels([...models, val]);
                        (e.target as HTMLInputElement).value = "";
                      }
                    }
                  }}
                />
              </div>
            </div>

            {/* Model Capabilities */}
            {models.length > 0 && (() => {
              const primaryModel = models[0];
              const modelInfo = availableModels.find(m => m.id === primaryModel);
              const hasModelsDevData = !!(modelInfo && (modelInfo.context_window || modelInfo.max_tokens));
              const autoContextWindow = modelInfo?.context_window?.toString() ?? "";
              const autoMaxOutputTokens = modelInfo?.max_tokens?.toString() ?? "";
              const autoSupportsToolCalling = modelInfo?.tool_call ?? true;
              const displayContextWindow = hasModelsDevData ? autoContextWindow : contextWindow;
              const displayMaxOutputTokens = hasModelsDevData ? autoMaxOutputTokens : maxOutputTokens;
              const displaySupportsToolCalling = hasModelsDevData ? autoSupportsToolCalling : supportsToolCalling;
              return (
                <div>
                  <label className="mb-1 block text-xs text-zinc-500">
                    Model Capabilities
                    {hasModelsDevData && <span className="ml-1 text-[10px] text-zinc-400">(from models.dev)</span>}
                    {!hasModelsDevData && <span className="ml-1 text-[10px] text-amber-500">(manual input required)</span>}
                  </label>
                  <div className="flex gap-2">
                    <div className="flex-1">
                      <label className="mb-0.5 block text-[10px] text-zinc-400">Context Window</label>
                      <input
                        type="number"
                        value={displayContextWindow}
                        onChange={(e) => setContextWindow(e.target.value)}
                        readOnly={hasModelsDevData}
                        placeholder="e.g. 128000"
                        className={cn(
                          "w-full rounded-md border border-zinc-200 px-3 py-2 text-xs",
                          hasModelsDevData
                            ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                            : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                        )}
                      />
                    </div>
                    <div className="flex-1">
                      <label className="mb-0.5 block text-[10px] text-zinc-400">Max Output Tokens</label>
                      <input
                        type="number"
                        value={displayMaxOutputTokens}
                        onChange={(e) => setMaxOutputTokens(e.target.value)}
                        readOnly={hasModelsDevData}
                        placeholder="e.g. 4096"
                        className={cn(
                          "w-full rounded-md border border-zinc-200 px-3 py-2 text-xs",
                          hasModelsDevData
                            ? "bg-zinc-50 text-zinc-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-500"
                            : "dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200",
                        )}
                      />
                    </div>
                  </div>
                  <div className="mt-1.5 flex items-center gap-2">
                    <label className="flex items-center gap-1.5 text-xs text-zinc-500">
                      <input
                        type="checkbox"
                        checked={displaySupportsToolCalling}
                        onChange={(e) => setSupportsToolCalling(e.target.checked)}
                        disabled={hasModelsDevData}
                        className="accent-accent-green"
                      />
                      Supports Tool Calling
                    </label>
                  </div>
                </div>
              );
            })()}



            {/* Test result */}
            {testResult && (
              <div className={cn(
                "rounded-md px-3 py-2 text-xs",
                testResult.success
                  ? "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400"
                  : "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400"
              )}>
                {testResult.message}
              </div>
            )}
          </div>
        </div>

        <div className="shrink-0 flex items-center justify-between gap-2 border-t border-zinc-100 dark:border-zinc-800 px-6 py-4">
          {/* Test result on the left */}
          <div className="flex-1 min-w-0">
            {testResult && (
              <div className={cn(
                "rounded-md px-3 py-1.5 text-xs truncate",
                testResult.success
                  ? "bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400"
                  : "bg-red-50 text-red-700 dark:bg-red-900/20 dark:text-red-400"
              )}>
                {testResult.message}
              </div>
            )}
            {testing && (
              <div className="text-xs text-zinc-400">Testing...</div>
            )}
          </div>

          {/* Buttons on the right with equal width */}
          <div className="flex gap-2 shrink-0">
            <button
              onClick={onClose}
              className="w-20 rounded-md px-3 py-1.5 text-xs font-medium text-center text-zinc-600 hover:bg-zinc-100 dark:text-zinc-400 dark:hover:bg-zinc-700"
            >
              Cancel
            </button>
            <button
              onClick={handleSave}
              disabled={(needsApiKey(provider) ? !key.trim() : false) || saving}
              className="w-20 rounded-md btn-solid px-3 py-1.5 text-xs font-medium text-center disabled:opacity-50"
            >
              {saving ? "Saving..." : "Save"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
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
  if (!open) return null;

  return (
    <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50" onClick={onClose}>
      <div
        className="w-[400px] overflow-hidden rounded-lg bg-white shadow-xl dark:bg-zinc-800 flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="shrink-0 px-6 pt-6 pb-2 text-sm font-semibold text-zinc-900 dark:text-zinc-100">
          当前模型不支持图片输入
        </h3>
        <p className="px-6 pb-4 text-xs text-zinc-500 dark:text-zinc-400">
          请切换到以下支持图片输入的模型：
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
            关闭
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

  return (
    <div ref={ref} className="relative inline-block">
      {/* Trigger button */}
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className={cn(
          toolbarButton,
          open && toolbarButtonActive,
        )}
      >
        <Cpu size={14} />
        <span>
          {(() => {
            if (!currentModel || !currentModel.includes('/')) return currentModel ?? "Model";
            const parts = currentModel.split('/');
            const prefix = parts[0];
            const modelName = parts.slice(1).join('/');
            // Only strip if model name is longer than prefix (avoid stripping model/provider)
            return modelName.length > prefix.length ? modelName : currentModel;
          })()}
        </span>
        <ChevronDown className="h-3 w-3 text-zinc-400" />
      </button>

      {/* Popup menu */}
      {open && (
        <div
          className={cn(
            "absolute bottom-full left-0 z-50 mb-1 overflow-hidden rounded-lg border shadow-lg",
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

      {/* Add Model Dialog */}
      <AddModelDialog
        open={showAddDialog}
        onClose={() => setShowAddDialog(false)}
        onSuccess={() => {
          // Trigger reload of models from parent component
          window.dispatchEvent(new Event('models-added'));
        }}
      />
    </div>
  );
}

