import React, { useEffect, useInsertionEffect, useLayoutEffect, useRef, useState, useCallback, useMemo } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSkillStore } from "../../stores/skillStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { ToolApprovalNeededEvent, AttachedItem } from "../../lib/types";
import { isProcessing, getProcessingPhase } from "../../lib/types";
import { cn } from "../../lib/utils";
import { fetchProviderModels } from "../../lib/gateway-api";
import { startAgentAndSyncUI } from "../../lib/agent-start";
import { toolbarButton } from "../../lib/ui-styles";
import { AddProviderFlow } from "../harness/AddProviderFlow";
import { Bot, Play, Send, ChevronDown, ChevronRight, ChevronLeft, ChevronsDown, ChevronsUp, Wrench, AlertTriangle, X, Square, Plus, Layers, Loader, Pencil, Paperclip, Image, Brain, Circle, CircleDot } from "lucide-react";
import type { ChatMessage, VaultKeyEntry, ModelEntry } from "../../lib/types";
import { ContextUsageIcon } from "./ContextUsageIcon";
import { useSessionScope } from "./useSessionScope";
import { VirtualMessageList, type VirtualMessageListHandle } from "./VirtualMessageList";
import { useLiveStream, getChatAdapterSession } from "./chatAdapterStore";
import { useChatListAdapter } from "./chatListAdapter";
import { useScrollController } from "./useScrollController";

/**
 * Measure natural dimensions of an image whose src is already settable in the
 * DOM (data URL, asset protocol URL, blob URL). Returns the naturalWidth /
 * naturalHeight that the browser exposes once the image has loaded. ADR-046
 * uses these to populate `image_upload.width` / `.height` so the runtime
 * doesn't have to re-decode the blob just to render a thumbnail.
 */
function measureImage(src: string): Promise<{ width: number; height: number }> {
  return new Promise((resolve, reject) => {
    const img = new window.Image();
    img.onload = () =>
      resolve({ width: img.naturalWidth, height: img.naturalHeight });
    img.onerror = () => reject(new Error("Failed to load image for dimension detection"));
    img.src = src;
  });
}

/**
 * Merge an internal ref (used for click-outside detection) with an external
 * ref (used by the parent for layout measurement). Both refs are kept in sync
 * on every commit.
 */
function useMergedRef<T>(
    internalRef: React.RefObject<T | null>,
    externalRef?: React.Ref<T | null>,
): React.RefCallback<T | null> {
    return useCallback(
        (el: T | null) => {
            internalRef.current = el;
            if (typeof externalRef === "function") {
                externalRef(el);
            } else if (externalRef && typeof externalRef === "object") {
                (externalRef as React.MutableRefObject<T | null>).current = el;
            }
        },
        [internalRef, externalRef],
    );
}

import { AskQuestionCard } from "./AskQuestionCard";
import { DebugPausedBanner } from "./DebugPausedBanner";
import { RetryWaitBanner } from "./RetryWaitBanner";
import { SessionTabBar } from "./SessionTabBar";
import { SkillsPanel } from "../skills/SkillsPanel";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";
import { DocumentChip } from "./DocumentChip";
import { AttachedContextChips } from "./AttachedContextChips";
import { ToolbarDropdownTrigger } from "../common/ToolbarDropdown";
import { Tooltip } from "../common/Tooltip";
import { log } from "../../lib/logger";

// CHAT_BOTTOM_THRESHOLD_PX removed - pinned-to-bottom detection is now
// owned by useScrollController's state machine (PIN_THRESHOLD_PX).

// Stable empty array reference for the `messages` Zustand selector.
// Returning `[]` literals from a selector creates a new reference on every
// `getSnapshot` call, which trips useSyncExternalStore's "The result of
// getSnapshot should be cached" check and produces a "Maximum update depth
// exceeded" infinite re-render loop during transient states (mount, agent
// switch, session switch) where the agent's session entry does not yet
// exist. The same pattern is already used in ResultsPanel.tsx.
const EMPTY_MESSAGES: ChatMessage[] = [];

/**
 * Data-driven scroll snapshot.  Stored per session key when the user
 * navigates away, consumed by useScrollController on return.
 *
 *   - atBottom: true when the user was viewing the latest content
 *     (or the session was streaming).  On return, the controller
 *     calls scrollToBottom().
 *   - firstVisibleBlockId: content-derived blockId of the first
 *     visible block when the user left.  Used to restore the exact
 *     reading position via vml.scrollToBlockId().  Stable across
 *     session switches (survives VML remount, page reload, etc.).
 */
interface ChatScrollSnapshot {
  atBottom: boolean;
  firstVisibleBlockId: string | null;
  /** Adapter messageOffset at save time. Pagination cursor (data, not pixels). */
  messageOffset: number | null;
  /** Index of the first visible block in the adapter's blocks array at save time.
   *  Combined with messageOffset, approximates the absolute message index to
   *  locate the correct page on restore (merged cache may span multiple pages). */
  firstVisibleBlockIndex: number | null;
}

// Bounded LRU: cap snapshot entries to MAX_SCROLL_SNAPSHOTS so a long-lived
// session that toggles Settings/Harness/Docs many times can't grow this Map
// without bound.  Each snapshot is ~50 bytes; 64 entries ≈ 3.2 KB, negligible.
// Eviction uses Map insertion order (JS Map preserves insertion order), so
// the OLDEST snapshot is dropped when the cap is exceeded — fine because
// the user just toggled away from the settings nav-back use case which is
// the only consumer of these snapshots.
const MAX_SCROLL_SNAPSHOTS = 64;
const chatScrollSnapshots = new Map<string, ChatScrollSnapshot>();

function setScrollSnapshot(key: string, snapshot: ChatScrollSnapshot): void {
  chatScrollSnapshots.set(key, snapshot);
  if (chatScrollSnapshots.size > MAX_SCROLL_SNAPSHOTS) {
    const oldestKey = chatScrollSnapshots.keys().next().value;
    if (oldestKey !== undefined) chatScrollSnapshots.delete(oldestKey);
  }
}

export function ChatPanel() {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  const selectedAgent = useAgentStore((s) => selectedAgentId ? s.agents[selectedAgentId]?.meta : undefined);

  // ── Toolbar responsive collapse ──────────────────────────────────
  // The bottom toolbar (model / think / workspace / skills + upload buttons)
  // must keep all buttons non-overlapping when the panel width narrows.
  // We measure each button's full width vs. its icon-only width and greedily
  // fold labels — starting from the leftmost button (model) and moving
  // rightward (effort → ws → sk) — until the row fits.
  const toolbarRef = useRef<HTMLDivElement>(null);
  // Per-button refs kept ONLY for the inner Menu components' click-outside
  // detection (ModelMenu/ReasoningEffortMenu use useMergedRef internally).
  // The toolbar collapse measurement no longer reads these — it queries
  // the DOM via [data-toolbar-btn] for a timing-independent answer.
  const modelBtnRef = useRef<HTMLDivElement>(null);
  const effortBtnRef = useRef<HTMLDivElement>(null);
  const wsBtnRef = useRef<HTMLDivElement>(null);
  const skBtnRef = useRef<HTMLDivElement>(null);
  const [textHidden, setTextHidden] = useState<Record<string, boolean>>({
    model: false,
    effort: false,
    ws: false,
    sk: false,
  });

  // ── Toolbar responsive collapse ───────────────────────────────
  //
  // IMPLEMENTATION: a `ref` callback (NOT a useEffect on toolbarRef).
  //
  // Why ref callback instead of useEffect?
  //
  // ModelMenu and ReasoningEffortMenu are conditionally rendered:
  //   - ModelMenu:     `availableModels.length > 1 && selectedAgent?.running`
  //   - ReasoningMenu: `selectedAgent?.running && currentReasoningEffort != null`
  //
  // On cold start the toolbar div itself may render before its
  // conditionally-rendered children, or vice versa, depending on the
  // order of state hydration.  useEffect with `[]` runs once on mount
  // and then never again — so if it runs BEFORE the model/effort buttons
  // exist, ResizeObserver is attached to a half-formed DOM and stays
  // attached even when the buttons arrive later, BUT the observer's
  // initial synchronous callback already fired with the half-formed
  // state and cached "no buttons → no fold" forever.
  //
  // A ref callback fires every time React attaches the ref to a DOM
  // node — and crucially, when the toolbar div re-mounts due to React
  // reconciler activity (StrictMode double-invoke, or any parent state
  // that causes ChatPanel's children to re-render and remount the
  // toolbar subtree), the OLD ref is called with `null` (cleanup) and
  // the NEW ref is called with the new node (setup).  This naturally
  // tears down and re-installs the observer across all "toolbar
  // identity" changes, which is exactly what we need.
  //
  // Inside the ref callback we ALSO install a MutationObserver on the
  // toolbar subtree to catch button-set changes that don't cause the
  // toolbar itself to remount (agent becomes running → model menu
  // appears inside the toolbar).
  const toolbarRefCallback = useCallback((node: HTMLDivElement | null) => {
    // Always update the shared ref so other code can read it.
    toolbarRef.current = node;
    if (!node) return;

    // ── Imperative, fully self-contained measurement engine ──────
    //
    // This closure captures nothing from React component state.  It
    // reads/writes the DOM directly.  State sync to React happens
    // through setTextHidden only at the end of a measurement, so
    // even if React is in the middle of a render, the DOM stays
    // visually consistent (we apply visibility BEFORE setting state).
    let measuring = false;
    let rafId = 0;

    const measure = () => {
      if (measuring) return;
      measuring = true;
      try {
        const els = Array.from(
          node.querySelectorAll<HTMLElement>("[data-toolbar-btn]"),
        );
        if (els.length === 0) return;

        const spans = els.map((el) => ({
          id: el.dataset.toolbarBtn as string,
          text: el.querySelector<HTMLElement>("[data-toolbar-text]"),
          chev: el.querySelector<HTMLElement>("[data-toolbar-chevron]"),
          // icon-only width (text hidden) — measured first so it's stable.
          iconWidth: el.offsetWidth,
        }));

        // Reveal all text/chevron to read true full widths.
        spans.forEach(({ text, chev }) => {
          if (text) text.style.display = "";
          if (chev) chev.style.display = "";
        });
        const fullWidths = els.map((el) => el.offsetWidth);

        const GAP = 4;
        const totalGaps = (els.length - 1) * GAP;

        // Layout constants: container padding (px-3 = 12px each side),
        // gap-2 between the left cluster and right cluster,
        // ~60px for the right cluster (ContextUsageIcon + send button),
        // ~68px for the upload buttons (paperclip + image).
        const PAD_X = 24;
        const FLEX_GAP = 8;
        const RIGHT_CLUSTER = 60;
        const UPLOAD_BUTTONS = 68;
        const available = node.offsetWidth
                        - PAD_X - FLEX_GAP - RIGHT_CLUSTER - UPLOAD_BUTTONS;

        // Progressive collapse from LEFT to RIGHT (matches the
        // documented product behavior: model hides first, then effort,
        // then ws, then sk — preserving the rightmost buttons which
        // are usually the most context-relevant).
        //
        // Algorithm:
        //   1. Start with all buttons fully visible.
        //   2. If totalFull > available, fold the leftmost button.
        //   3. Recheck; if still overflowing, fold the next leftmost.
        //   4. Stop when it fits OR every button is folded.
        //
        // Folding a button changes total width by exactly
        // (fullWidth - iconWidth) — the button stays in the row,
        // its trailing gap-1 to the next button is unchanged.

        const fold = new Array<boolean>(spans.length).fill(false);
        let current = fullWidths.reduce((a, b) => a + b, 0) + totalGaps;
        for (let i = 0; i < spans.length; i++) {
          if (current <= available) break;
          fold[i] = true;
          current -= (fullWidths[i] - spans[i].iconWidth);
        }

        // Apply visibility to DOM synchronously (user sees correct
        // layout this frame), THEN sync React state.
        spans.forEach(({ text, chev }, i) => {
          const display = fold[i] ? "none" : "";
          if (text) text.style.display = display;
          if (chev) chev.style.display = display;
        });

        setTextHidden((prev) => {
          let changed = false;
          const next = { ...prev };
          spans.forEach(({ id }, i) => {
            if (next[id] !== fold[i]) {
              next[id] = fold[i];
              changed = true;
            }
          });
          return changed ? next : prev;
        });
      } finally {
        measuring = false;
      }
    };

    const scheduleMeasure = () => {
      if (rafId !== 0) return;
      rafId = requestAnimationFrame(() => {
        rafId = 0;
        measure();
      });
    };

    // ResizeObserver: container width changes.
    const ro = new ResizeObserver(() => scheduleMeasure());
    ro.observe(node);

    // MutationObserver: button-set changes. `attributes: true` covers
    // WorkspaceSelector's text change (it updates the label when the
    // user picks a different workspace) which DOES affect width.
    const mo = new MutationObserver((mutations) => {
      const relevant = mutations.some(
        (m) =>
          m.type === "childList" ||
          m.type === "characterData" ||
          (m.type === "attributes" && m.attributeName === "data-toolbar-text"),
      );
      if (relevant) scheduleMeasure();
    });
    mo.observe(node, {
      childList: true,
      subtree: true,
      characterData: true,
      attributes: true,
      attributeFilter: ["data-toolbar-text"],
    });

    // Schedule the initial measurement for the next frame so React
    // has time to commit children (especially the conditionally-
    // rendered model/effort buttons).
    scheduleMeasure();

    // Stash teardown on the node so the next ref callback can clean
    // up.  Without this, a re-mount would leak observers.
    (node as unknown as { __toolbarTeardown__?: () => void }).__toolbarTeardown__ = () => {
      ro.disconnect();
      mo.disconnect();
      if (rafId !== 0) {
        cancelAnimationFrame(rafId);
        rafId = 0;
      }
    };
  }, []);

  // Per-agent + per-session state selectors.
  // messages and sessionStatus are split into granular selectors because
  // they change at different frequencies: messages updates every ~500ms
  // poll cycle during streaming, while sessionStatus only changes on
  // state transitions (idle→streaming→idle).  Keeping them separate
  // prevents sessionStatus-derived values (sending, etc.) from
  // re-evaluating on every poll tick.
  //
  // ADR-050 C2: `currentSessionId` is read EARLY so the live-stream
  // subscription (`useLiveStream`) can target the right session.  The
  // old code bound currentSessionId further below, after a long list
  // of toolbar refs; we keep the early read here so all subsequent
  // selectors (including the adapter live hook) see a stable session
  // id.  This is the same `useChatStore` selector that used to live
  // ~310 lines lower.
  const currentSessionId = useChatStore((s) => selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null);
  const messages = useChatStore((s) => {
    if (!selectedAgentId) return EMPTY_MESSAGES;
    const agent = s.agentStates[selectedAgentId];
    if (!currentSessionId) return EMPTY_MESSAGES;
    return agent?.sessionStates[currentSessionId]?.messages ?? EMPTY_MESSAGES;
  });
  // ADR-050 C2: live-stream state (optimisticEntries + isThinking +
  // thinkingContent + assistantStreamingContent + isPinnedToBottom)
  // now lives in chatAdapterStore.  ChatPanel subscribes via
  // `useLiveStream(selectedAgentId, currentSessionId)` and the same
  // shape is forwarded to the v1 components (VirtualMessageList /
  // ExploreBlock) until C5 fully takes over.
  const liveState = useLiveStream(selectedAgentId, currentSessionId);
  // ADR-050 C5: optimisticEntries is no longer needed at ChatPanel level;
  // the send-message duplicate guard reads from adapterSession directly.
  const sessionStatus = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!currentSessionId) return null;
    return agent?.sessionStates[currentSessionId]?.sessionStatus ?? null;
  });
  // Remaining session fields — change infrequently, single selector is fine.
  const sessionState = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!currentSessionId) return null;
    return agent?.sessionStates[currentSessionId] ?? null;
  });
  const iterationLimitPaused = sessionState?.iterationLimitPaused ?? null;
  const loopDetectedPaused = sessionState?.loopDetectedPaused ?? null;
  const serverError = sessionState?.serverError ?? null;
  const pendingApproval = sessionState?.pendingApproval ?? {};
  const pendingQuestions = sessionState?.pendingQuestions ?? [];
  const [currentQuestionIndex, setCurrentQuestionIndex] = useState(0);
  // Reset to first question when the list changes (new question arrives)
  useEffect(() => {
    if (pendingQuestions.length === 0) {
      setCurrentQuestionIndex(0);
    } else if (currentQuestionIndex >= pendingQuestions.length) {
      setCurrentQuestionIndex(pendingQuestions.length - 1);
    }
  }, [pendingQuestions.length, currentQuestionIndex]);
  const isLoadingSession = sessionState?.isLoadingSession ?? false;
  const loadError = sessionState?.loadError ?? null;
  const todos = sessionState?.todos ?? [];
  /** Per-session queued messages — persisted in chatStore across agent switches */
  const queuedMessages = sessionState?.queuedMessages ?? [];
  // ADR-050 C5: isAssistantReplying no longer drives a trailing virtual
  // item; the v2 adapter folds the live assistant stream into blocks.
  const isThinking = liveState.isThinking;
  const thinkingStartTime = liveState.thinkingStartTime;
  const thinkingContent = liveState.thinkingContent;
  // ADR-050 C5: assistantStreamingContent / assistantStreamingStartTime
  // are no longer passed to VML — the v2 adapter folds the live assistant
  // stream into blocks (isLive: true) rendered via MessageBubble.

  // ADR-021 + ADR-049: "sending" is derived purely from sessionStatus
  // (backend source of truth). No optimistic flags — the backend pushes
  // session_state within ~50ms.
  //
  // ADR-049: instead of enumerating 4 status strings, use the pure
  // `isProcessing()` derived from `getProcessingPhase()`. The TypeScript
  // compiler will fail if any new processing phase is added without
  // updating the indicator bindings (single source of truth principle).
  const sending = isProcessing(sessionStatus);
  // ADR-049: phase is the single source of truth for indicator visibility.
  // UI banners (waiting / tool_executing / waiting_approval) are derived
  // directly from this — no flag composition. `paused` is intentionally
  // not handled here: DebugPausedBanner / RetryWaitBanner /
  // iterationLimitPaused / loopDetectedPaused already cover all 4
  // backend paths to the `Paused` state.
  const phase = getProcessingPhase(sessionStatus);
  const currentModel = sessionState?.model ?? null;
  const currentProvider = sessionState?.provider ?? null;
  const currentReasoningEffort = sessionState?.reasoningEffort ?? null;

  // User profile fields — subscribed at ChatPanel level and passed as props
  // to MessageBubble so React.memo can detect profile changes (name/avatar
  // edits should update all rendered message bubbles instantly).
  const userDisplayName = useUserProfileStore((s) => s.profile.displayName);
  const userAvatarUrl = useUserProfileStore((s) => s.profile.backendAvatarUrl);
  const userBuiltinAvatarId = useUserProfileStore((s) => s.profile.backendBuiltinAvatarId);

  // Global state and actions — selectors to avoid full-store re-render
  const mqttConnected = useChatStore((s) => s.mqttConnected);
  const availableModels = useChatStore((s) => s.availableModels);
  // Stable function refs
  const {
    sendMessage,
    sendStop,
    setCurrentModel,
    setReasoningEffort,
    setAvailableModels,
    continueExecution,
    resolveApproval,
    resolveApprovalByToolCallId,
    clearServerError,
  } = useChatStore.getState();
  // (currentSessionId is already declared above — ChatPanel hoists it
  // to the top of the per-session block so the live-stream subscription
  // can target the right session without a placeholder re-subscribe.)
  const currentScrollKey = selectedAgentId && currentSessionId ? `${selectedAgentId}:${currentSessionId}` : null;
  const gatewayStatus = useGatewayStore((s) => s.status);
  const { activeSkill, clearActiveSkill } = useSkillStore();

  // ── Per-session scope ──────────────────────────────────────────────
  // All mutable state that is scoped to a single session lives in this
  // hook.  On session change the entire scope is atomically reset to
  // defaults, eliminating the class of bugs where per-session refs/state
  // leak across session switches.
  const session = useSessionScope(currentSessionId);

  // ── ADR-041 C4: ChatListAdapter ──────────────────────────────────
  // The single bridge between chatStore (data) and VirtualMessageList (render).
  // Owns: block folding, bidirectional pagination, scroll anchoring,
  // sticky-bottom state, and ensure-renderable (onLayout).
  const adapter = useChatListAdapter(selectedAgentId, currentSessionId);

  // adapterRef/sessionRef no longer needed - handleScroll and pagination
  // timer are owned by useScrollController, which has direct access to
  // adapter and containerRef.

  const [hasLlmConfig, setHasLlmConfig] = useState<boolean | null>(null); // null = checking

  // Auto-collapse todo list when all tasks are completed
  useEffect(() => {
    if (todos.length === 0) return;
    if (todos.every(t => t.status === "completed")) {
      session.setTodosCollapsed(true);
    }
  }, [todos, session.setTodosCollapsed]);

  const messagesContainerRef = useRef<HTMLDivElement>(null);
  // Last known scrollTop — captured BEFORE DOM mutation each commit.
  //
  // useInsertionEffect runs in the commit phase, BEFORE the mutation step
  // (VML unmount/mount).  At this point the previous VirtualMessageList is
  // still in the DOM and container.scrollTop reflects the user’s real
  // position.  This ref is read by the snapshot-write cleanup below, which
  // runs AFTER mutation — by then the previous VML has been removed, the
  // container is empty, and the browser has clamped container.scrollTop to 0.
  //
  // Without this ref, every session switch would snapshot scrollOffset=0 and
  // the user would be restored to the TOP of every previously-visited session.
  const lastScrollTopRef = useRef(0);
  const lastScrollHeightRef = useRef(0);
  /** Content-derived blockId of the first visible block (data-driven restoration). */
  const lastFirstVisibleBlockIdRef = useRef<string | null>(null);
  /** Index of the first visible block in the adapter's blocks array (for page hint). */
  const lastFirstVisibleBlockIdxRef = useRef<number | null>(null);
  /** Adapter messageOffset at the time of the last scroll event (for snapshot). */
  const lastMessageOffsetRef = useRef<number | null>(null);
  useInsertionEffect(() => {
    // GUARD: only update refs when the current session has rendered blocks.
    // During a session switch the new session's adapter has NO blocks yet
    // (data not loaded).  Without this guard, useInsertionEffect (which
    // fires BEFORE useLayoutEffect cleanup) would overwrite the refs with
    // 0/null — causing the snapshot-write cleanup to save garbage
    // (scrollOffset=0, firstVisibleBlockIndex=null) instead of the old
    // session's actual position.
    //
    // For small sessions (content fits viewport, no scrollbar), onScroll
    // never fires — this effect is the ONLY ref updater.  The guard
    // `adapter.blocks.length > 0` correctly allows updates for small
    // sessions WITH data while blocking the empty-session-switch render.
    if (adapter.blocks.length > 0) {
      const container = messagesContainerRef.current;
      if (container) {
        lastScrollTopRef.current = container.scrollTop;
        lastScrollHeightRef.current = container.scrollHeight;
      }
      const fvbId = vmlRef.current?.getFirstVisibleBlockId();
      if (fvbId != null) lastFirstVisibleBlockIdRef.current = fvbId;
      const fvbIdx = vmlRef.current?.getFirstVisibleBlockIndex();
      if (fvbIdx != null) lastFirstVisibleBlockIdxRef.current = fvbIdx;
      lastMessageOffsetRef.current = adapter.messageOffset;
    }
  });
  /** Timestamp of the last compositionEnd event. On macOS WKWebView, compositionEnd
   *  fires BEFORE the keydown(Enter) that confirmed the IME selection, so
   *  isComposing is already false when keydown runs. We use a time-window
   *  check instead: if compositionEnd happened within the last 300ms, the
   *  Enter was almost certainly an IME confirmation, not a send intent. */
  const lastCompositionEndRef = useRef(0);
  /**
   * Imperative handle to VirtualMessageList. Exposes data-derived queries
   * about the rendered MessageBlock layout. Used here by handleScroll to pick
   * the "anchorToUser" block (the first visible block at the moment the
   * user scrolls near the top) BEFORE triggering loadMoreOlderMessages.
   * No pixel math, no estimateSize-based heuristic — a pure data lookup.
   */
  const vmlRef = useRef<VirtualMessageListHandle | null>(null);

  const agentDisplayName = useAgentStore((s) => selectedAgentId ? s.agents[selectedAgentId]?.profile?.displayName : undefined) ?? selectedAgent?.display_name ?? selectedAgent?.name;

  // Read saved scroll snapshot for data-driven restoration.
  // The snapshot carries { atBottom, firstVisibleBlockId } and is consumed
  // by useScrollController to call scrollToBottom() or scrollToBlockId().
  const scrollSnapshot = currentScrollKey
    ? chatScrollSnapshots.get(currentScrollKey)
    : undefined;
  // ADR-041 C4 / ADR-050 C5: messageBlocks comes from the v2 adapter.
  // No trailing extra items (replying / compacting / working indicators)
  // - the virtualizer count === messageBlocks.length.  Live streaming
  // content is folded into blocks by the adapter (isLive: true).
  const messageBlocks = adapter.blocks;

  // ScrollController owns pagination, scroll-arrow visibility, and init-scroll
  // restoration via the data-driven snapshot { atBottom, firstVisibleBlockId }.
  const scrollController = useScrollController({
    containerRef: messagesContainerRef,
    adapter,
    vmlRef,
    sessionKey: currentScrollKey,
    initialAtBottom: scrollSnapshot?.atBottom,
    initialFirstVisibleBlockId: scrollSnapshot?.firstVisibleBlockId,
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


  // Persist scroll state across top-level navigation.  AppLayout unmounts the
  // whole chat subtree for Settings/Harness/Docs/Projects, so component-local
  // refs and the virtualizer's internal offset are lost otherwise.
  //
  // deps=[currentScrollKey]: only save snapshot when session changes or when the
  // component unmounts.  Without deps the cleanup would fire on *every* re-render
  // (e.g. loadSessionMessages → isLoadingSession → re-render), overwriting the
  // unmount snapshot with a transient scrollOffset=0.
  useLayoutEffect(() => {
    return () => {
      const key = currentScrollKey;
      if (!key) return;

      // Read from the guarded refs.  These hold the OLD session's
      // last-known values because:
      //   - onScroll updates them during user interaction (primary)
      //   - useInsertionEffect updates them ONLY when the container has
      //     content (guard prevents overwrite on session-switch render)
      const scrollOffset = lastScrollTopRef.current;
      const scrollHeight = lastScrollHeightRef.current;
      const firstVisibleBlockId = lastFirstVisibleBlockIdRef.current;
      const container = messagesContainerRef.current;
      const clientHeight = container?.clientHeight ?? 0;
      const distFromBottom = scrollHeight - scrollOffset - clientHeight;
      setScrollSnapshot(key, {
        atBottom: sending || distFromBottom <= 120,
        firstVisibleBlockId,
        messageOffset: lastMessageOffsetRef.current,
        firstVisibleBlockIndex: lastFirstVisibleBlockIdxRef.current,
      });
    };
  }, [currentScrollKey]);

  // ── Atomized mount effect ──────────────────────────────────────────
  // Every ChatPanel mount restores session state from the backend.
  // Reconnect stream (idempotent), load messages if needed, refresh session state.
  const prevAgentIdRef = useRef<string | null>(null);

  useEffect(() => {
    if (!selectedAgentId) return;
    const agentMeta = useAgentStore.getState().agents[selectedAgentId]?.meta;
    if (!agentMeta?.running || !agentMeta?.ready) return;

    const currentSessId = useChatStore.getState().agentStates[selectedAgentId]?.activeSessionId;
    if (!currentSessId) {
      log.debug("[ChatPanel:mount] no active session, deferring...");
      return;
    }

    // Close session panel on agent switch (UX nicety)
    if (prevAgentIdRef.current !== null && prevAgentIdRef.current !== selectedAgentId) {
      useAgentStore.getState().reset();
    }
    prevAgentIdRef.current = selectedAgentId;

    const chatStore = useChatStore.getState();
    const ss0 = chatStore.agentStates[selectedAgentId]?.sessionStates[currentSessId];
    const existingMessages = ss0?.messages;
    // ADR-050 C2: optimistic overlay now lives in chatAdapterStore.
    // The mount guard's "has data?" check is widened to include the
    // adapter's optimistic entries (most often a freshly-sent user
    // message that the HTTP refresh hasn't echoed back yet).
    const adapterSession = getChatAdapterSession(selectedAgentId, currentSessId);
    const existingOptimistic = adapterSession.optimisticEntries;
    // Treat the union as "has data" — a freshly-sent optimistic user
    // message MUST NOT cause a redundant reload. The load path will
    // overwrite `messages[]` with whatever the server says; the merge
    // step inside `loadSessionMessages` will then reconcile the
    // overlay (P0-2 invariant).
    const hasMessages = !!(
      (existingMessages && existingMessages.length > 0) ||
      (existingOptimistic && existingOptimistic.length > 0)
    );

    log.debug("[ChatPanel:mount] atomized restore start", {
      agentId: selectedAgentId,
      sessionId: currentSessId,
      hasMessages,
    });

    // ADR-033: connectStream removed — MQTT connection is managed by Rust backend.
    if (!hasMessages) {
      // 2a. No messages in store — load from backend (first mount or new session).
      // The scroll controller handles positioning after blocks arrive:
      // scrollToBottom() for atBottom/no-snapshot, scrollToBlockId() for browsing.
      // If the user was browsing history, load the page at their saved position
      // so the controller can restore via scrollToBlockId.  Otherwise load tail.
      const mountSnap = chatScrollSnapshots.get(`${selectedAgentId}:${currentSessId}`);
      const mountHint = (mountSnap && !mountSnap.atBottom && mountSnap.firstVisibleBlockId
        && mountSnap.messageOffset != null && mountSnap.firstVisibleBlockIndex != null)
        ? mountSnap.messageOffset + mountSnap.firstVisibleBlockIndex : null;
      const mountLoad = (mountHint != null && mountSnap)
        ? adapter.loadPageForBlockId(mountSnap.firstVisibleBlockId!, mountHint)
        : adapter.loadInitialPage();
      session.scope.current.isInitialLoad = currentSessId;
      mountLoad
        .then(() => chatStore.loadSession(selectedAgentId, currentSessId))
        .finally(() => {
          session.scope.current.isInitialLoad = null;
          log.debug("[ChatPanel:mount] atomized restore done (full)", {
            agentId: selectedAgentId,
            sessionId: currentSessId,
            messageCount: useChatStore.getState().agentStates[selectedAgentId]?.sessionStates[currentSessId]?.messages?.length ?? 0,
          });
        });
    } else {
      // 2b. Messages already in store (nav-back: same agent, same session).
      //     No reload needed — messages survive in zustand across unmount.
      chatStore.loadSession(selectedAgentId, currentSessId);
      log.debug("[ChatPanel:mount] atomized restore done (incremental)", {
        agentId: selectedAgentId,
        sessionId: currentSessId,
        messageCount: existingMessages.length,
      });
    }
  }, [selectedAgentId, selectedAgent?.running, selectedAgent?.ready]);

  // ── Session switch effect ─────────────────────────────────────────
  // When the user picks a different session from the session panel,
  // ChatPanel stays mounted — only activeSessionId changes in chatStore.
  // Load messages for the newly-active session, and release the old
  // session's messages to free memory.
  const prevSessionIdRef = useRef<string | null>(null);
  useEffect(() => {
    if (!selectedAgentId || !currentSessionId) return;

    // Release the previous session's messages (memory cleanup).
    // We do this BEFORE loading the new session so the memory is freed
    // before the new data arrives.
    const prevId = prevSessionIdRef.current;
    if (prevId && prevId !== currentSessionId) {
      useChatStore.getState().clearSessionMessages(selectedAgentId, prevId);
    }
    prevSessionIdRef.current = currentSessionId;

    // Guard: mount effect (above) already handles the initial session load.
    // If it set isInitialLoad, it means a load is in progress for this session.
    if (session.scope.current.isInitialLoad === currentSessionId) {
      log.debug("[ChatPanel:session-switch] skipped (mount effect loading this session)");
      return;
    }

    const chatStore = useChatStore.getState();
    const ss1 = chatStore.agentStates[selectedAgentId]?.sessionStates[currentSessionId];
    const existingMessages = ss1?.messages;
    // ADR-050 C2: see the same pattern in the mount effect above —
    // the "has data?" check now reads the optimistic overlay from
    // chatAdapterStore instead of from chatStore.
    const adapterSession = getChatAdapterSession(selectedAgentId, currentSessionId);
    const existingOptimistic = adapterSession.optimisticEntries;
    // Same union rule as the mount effect: don't treat "only optimistic"
    // as "empty cache". A session that has an in-flight optimistic
    // insert must NOT trigger a redundant reload — the optimistic user
    // is real state and the next `scheduleRefresh` will reconcile.
    const hasMessages = !!(
      (existingMessages && existingMessages.length > 0) ||
      (existingOptimistic && existingOptimistic.length > 0)
    );
    if (hasMessages) {
      // Messages (and/or optimistic overlay) already cached — just
      // refresh session state.
      chatStore.loadSession(selectedAgentId, currentSessionId);
      return;
    }

    log.debug("[ChatPanel:session-switch] loading messages", {
      agentId: selectedAgentId,
      sessionId: currentSessionId,
    });

      // The scroll controller handles positioning after blocks arrive.
      const targetSnap = chatScrollSnapshots.get(`${selectedAgentId}:${currentSessionId}`);
      const targetHint = (targetSnap && !targetSnap.atBottom && targetSnap.firstVisibleBlockId
        && targetSnap.messageOffset != null && targetSnap.firstVisibleBlockIndex != null)
        ? targetSnap.messageOffset + targetSnap.firstVisibleBlockIndex : null;
      const loadPromise = (targetHint != null && targetSnap)
        ? adapter.loadPageForBlockId(targetSnap.firstVisibleBlockId!, targetHint)
        : adapter.loadInitialPage();
      session.scope.current.isInitialLoad = currentSessionId;
    loadPromise
      .then(() => chatStore.loadSession(selectedAgentId, currentSessionId))
      .finally(() => {
        session.scope.current.isInitialLoad = null;
      });
  }, [currentSessionId, selectedAgentId]);

  // ── Scroll restoration ──
// Handled by ScrollController init-scroll: scrollToBottom() when atBottom
// or no snapshot, scrollToBlockId() when restoring a browsing position.

  // ── Retry session load ──────────────────────────────────────────
  // Called from VirtualMessageList when user clicks retry on load error.
  const handleRetryLoadSession = useCallback(() => {
    if (!selectedAgentId || !currentSessionId) return;
    const retrySnap = chatScrollSnapshots.get(`${selectedAgentId}:${currentSessionId}`);
    const retryHint = (retrySnap && !retrySnap.atBottom && retrySnap.firstVisibleBlockId
      && retrySnap.messageOffset != null && retrySnap.firstVisibleBlockIndex != null)
      ? retrySnap.messageOffset + retrySnap.firstVisibleBlockIndex : null;
    if (retryHint != null && retrySnap) {
      adapter.loadPageForBlockId(retrySnap.firstVisibleBlockId!, retryHint);
    } else {
      adapter.loadInitialPage();
    }
  }, [selectedAgentId, currentSessionId, adapter]);

  // ADR-041 C4: Pagination state (hasOlder, hasNewer, isLoading) and
  // actions (loadBefore, loadAfter, jumpToLatest) are now managed by the
  // adapter. ChatPanel no longer reads messageOffset/messageLimit/messageTotal
  // or defines handleNeedMore.

  // ── Scroll actions ──
  // scrollToBottom, scrollToTop, handleScroll, and the pagination timer
  // are all owned by useScrollController.  The controller's jumpToBottom
  // and jumpToTop set the state machine to 'jumping' (preventing
  // concurrent scroll operations) and delegate to adapter.jumpToLatest/
  // jumpToOldest.  The jump target effect in the controller handles the
  // actual scroll after data arrives.

  const handleSend = async () => {
    const content = session.inputValue.trim();
    const hasItems = session.pendingAttachedItems.some(
      (it) => it.status === "success" && it.item !== undefined,
    );
    const hasUploading = session.pendingAttachedItems.some(
      (it) => it.status === "uploading",
    );

    // Block send: no content AND no resolved attachments, or attachments still uploading
    if ((!content && !hasItems) || sending || !selectedAgentId || hasUploading) return;

    // Collect resolved (success) AttachedItem[] for the optimistic bubble
    // and the MQTT `attached_items` payload. Backfill workspace refs
    // (`attached_file` / `attached_selection` / `attached_folder`) are
    // already AttachedItem-shaped so no post-processing is needed.
    const attachedItems = session.pendingAttachedItems
      .filter((it) => it.status === "success" && it.item !== undefined)
      .map((it) => it.item!) as AttachedItem[];

    // sendMessage is async but we fire-and-forget here.
    // The store handles all state updates internally.
    //
    // If the user is scrolled up, jump to the latest page first.  This
    // resets messageOffset to 0 so the optimistic insert in sendMessage
    // (`appendOptimisticUserMessage` in chatStore) merges into a
    // contiguous tail window when the next HTTP response lands.  We
    // delegate to scrollController.jumpToBottom (same code path as the
    // arrow button) so the state machine transitions through
    // "jumping" while the data loads, blocking the pagination timer
    // from racing with ensureLatestInCache.
    //
    // Input is cleared immediately for responsiveness; the actual send
    // waits for the jump to complete.
    session.setInputValue("");
    session.setPendingAttachedItems([]);

    if (selectedAgentId && currentSessionId) {
      const ss = useChatStore.getState().getSessionState(selectedAgentId, currentSessionId);
      if (ss.messageOffset > 0) {
        await scrollController.jumpToBottom();
      }
    }

    void sendMessage(content, selectedAgentId, activeSkill?.name, attachedItems.length > 0 ? attachedItems : undefined).then(() => {
      clearActiveSkill();
    });
  };

  // Stop button dual-action:
  //   input has content → send to queue (no stop, message waits for next loop)
  //   input empty       → stop current loop
  const handleStop = async () => {
    const content = session.inputValue.trim();
    if (content && selectedAgentId && currentSessionId) {
      // Add to queue — message waits in the queue box above the input area.
      useChatStore.getState().addQueuedMessage(selectedAgentId, currentSessionId, content);
      session.setInputValue("");
    } else if (queuedMessages.length > 0 && selectedAgentId && currentSessionId) {
      // Click with queued messages: send all queued + stop current loop.
      const msgs = [...queuedMessages];
      useChatStore.getState().setQueuedMessages(selectedAgentId, currentSessionId, []);

      // Jump to bottom first if scrolled up, so optimistic inserts
      // don't break the sliding window (same as handleSend).  Use
      // scrollController.jumpToBottom so the state machine goes
      // through "jumping" while ensureLatestInCache runs.
      const ss = useChatStore.getState().getSessionState(selectedAgentId, currentSessionId);
      if (ss.messageOffset > 0) {
        await scrollController.jumpToBottom();
      }

      for (const msg of msgs) {
        void sendMessage(msg, selectedAgentId, activeSkill?.name).then(() => {
          clearActiveSkill();
        });
      }
      sendStop(selectedAgentId);
    } else if (selectedAgentId) {
      // No queued messages: just stop
      sendStop(selectedAgentId);
    }
  };

  // Continue button (iterationLimit / loopDetected pause): jump to
  // bottom first if scrolled up so the resumed response lands in
  // view.  Same code path as the down-arrow button — state machine
  // goes through "jumping" while ensureLatestInCache runs, blocking
  // the pagination timer from racing with the jump.
  const handleContinue = useCallback(async () => {
    if (!selectedAgentId) return;
    if (currentSessionId) {
      const ss = useChatStore.getState().getSessionState(selectedAgentId, currentSessionId);
      if (ss.messageOffset > 0) {
        await scrollController.jumpToBottom();
      }
    }
    continueExecution(selectedAgentId);
  }, [selectedAgentId, currentSessionId, continueExecution, scrollController]);

  const handleRemoveQueued = (index: number) => {
    if (selectedAgentId && currentSessionId) {
      useChatStore.getState().removeQueuedMessage(selectedAgentId, currentSessionId, index);
    }
  };

  const handleEditQueued = (index: number) => {
    session.setInputValue(queuedMessages[index]);
    if (selectedAgentId && currentSessionId) {
      useChatStore.getState().removeQueuedMessage(selectedAgentId, currentSessionId, index);
    }
  };

  // File upload handler: opens the dialog and dispatches to the shared
  // upload pipeline. The pipeline handles both documents and images and
  // emits a single `pendingAttachedItems` entry.
  const handleFileUpload = async () => {
    // ADR-046: single upload entry-point for documents AND images. The
    // runtime decodes the format string and persists the blob; the desktop
    // wraps the response into a `FileUploadItem` / `ImageUploadItem`
    // metadata envelope to attach to the next user message.
    const { open } = await import("@tauri-apps/plugin-dialog");
    const selected = await open({
      title: "Select a document or image",
      filters: [{
        name: "Attachments",
        extensions: ["pdf", "docx", "pptx", "xlsx", "png", "jpg", "jpeg", "gif", "webp"],
      }],
      multiple: false,
    });

    if (!selected) return;
    const filePath = selected as string;
    if (!filePath) return;
    await uploadFileAtPath(filePath);
  };

  // Shared upload pipeline used by handleFileUpload (document-or-image dialog)
  // and handleImageSelect (image-only dialog). The runtime accepts both via
  // the same `upload_file` command.
  const uploadFileAtPath = async (filePath: string) => {
    const filename = filePath.replace(/^.*[\\/]/, "");
    const ext = filename.split(".").pop()?.toLowerCase() ?? "";
    if (!["pdf", "docx", "pptx", "xlsx", "png", "jpg", "jpeg", "gif", "webp"].includes(ext)) return;

    const tempId = `att-${Date.now()}`;
    const format = ext;
    const isImage = ["png", "jpg", "jpeg", "gif", "webp"].includes(ext);

    // Prerequisites — emit error chip and bail before invoking the backend.
    if (!currentSessionId) {
      session.setPendingAttachedItems(prev => [...prev, {
        tempId,
        status: "error",
        errorMessage: "No active session",
      }]);
      return;
    }
    if (!selectedAgentId) {
      session.setPendingAttachedItems(prev => [...prev, {
        tempId,
        status: "error",
        errorMessage: "No agent selected",
      }]);
      return;
    }

    // Add pending chip with uploading status. `localUrl` stays undefined for
    // documents; for images the renderer reads from the original file path
    // via the asset protocol while the upload is in flight.
    const localUrl = isImage ? convertFileSrc(filePath) : undefined;
    session.setPendingAttachedItems(prev => [...prev, {
      tempId,
      status: "uploading",
      localUrl,
    }]);

    try {
      // ADR-046: image width/height are pre-measured by the desktop via
      // `new Image()` and sent as multipart fields. Documents omit them.
      let width: number | undefined;
      let height: number | undefined;
      if (isImage && localUrl) {
        const dims = await measureImage(localUrl);
        width = dims.width;
        height = dims.height;
      }

      const result = await invoke<{
        documentId: string;
        filename: string;
        format: string;
        sizeBytes: number;
        width?: number;
        height?: number;
      }>("upload_file", {
        agentId: selectedAgentId,
        sessionId: currentSessionId,
        filePath,
        format,
        width,
        height,
      });

      const item: AttachedItem = isImage
        ? {
            type: "image_upload",
            documentId: result.documentId,
            filename: result.filename,
            format: result.format,
            sizeBytes: result.sizeBytes,
            ...(width !== undefined ? { width } : {}),
            ...(height !== undefined ? { height } : {}),
          }
        : {
            type: "file_upload",
            documentId: result.documentId,
            filename: result.filename,
            format: result.format,
            sizeBytes: result.sizeBytes,
          };

      session.setPendingAttachedItems(prev => prev.map((p) =>
        p.tempId === tempId ? { ...p, status: "success", item } : p
      ));
    } catch (err) {
      const msg = err instanceof Error ? err.message : typeof err === "string" ? err : "Upload failed";
      log.error("[ChatPanel] Attachment upload failed:", err);
      session.setPendingAttachedItems(prev => prev.map((p) =>
        p.tempId === tempId ? { ...p, status: "error", errorMessage: msg } : p
      ));
    }
  };

  // Remove a pending attachment chip
  const handleRemovePending = (tempId: string) => {
    session.setPendingAttachedItems(prev => prev.filter((p) => p.tempId !== tempId));
  };

  // Select image file via Tauri dialog, read as base64, and get dimensions
  const handleImageSelect = async () => {
    if (!currentSessionId || !selectedAgentId) return;

    // ADR-046: gate image selection on multimodal-supporting model — the
    // runtime still accepts the upload, but the LLM prompt layer only
    // consumes the dimensions; a non-vision model would treat the image as
    // a placeholder reference. We surface the warning here before the user
    // commits to a useless upload.
    const currentEntry = availableModels.find(
      (m) => m.name === currentModel && m.provider === currentProvider,
    );
    const supportsImage = currentEntry?.input_modalities?.includes("image");
    if (!supportsImage) {
      const imageModels = availableModels.filter((m) =>
        m.input_modalities?.includes("image"),
      );
      if (imageModels.length === 0) {
        log.warn(
          "[ChatPanel] No image-capable models available — skipping dialog",
        );
        return;
      }
      session.setImageCapableModels(imageModels);
      session.setShowImageUnsupportedDialog(true);
      return;
    }

    // ADR-046: image and document upload share the same code path. We just
    // narrow the dialog filter to image extensions — the runtime handler
    // already accepts both, and the resulting `image_upload` envelope
    // (carrying `width`/`height` for the thumbnail) lands in the same
    // `pendingAttachedItems` queue as document uploads.
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const selected = await open({
        title: t("chatPanel.selectImageTitle"),
        filters: [
          {
            name: "Images",
            extensions: ["png", "jpg", "jpeg", "gif", "webp"],
          },
        ],
        multiple: false,
      });
      if (!selected) return;
      const filePath = selected as string;
      if (!filePath) return;
      await uploadFileAtPath(filePath);
    } catch (err) {
      log.error("[ChatPanel] Image selection failed:", err);
    }
  };

  // ADR-045: cancel an in-flight tool execution by tool_call_id.
  // The selected agent's active session is cancelled; we don't need toolCallId
  // here because the wire message already carries it.
  const handleToolCancel = useCallback(
    (toolCallId: string) => {
      if (!currentSessionId || !selectedAgentId) {
        console.warn('[ChatPanel] handleToolCancel: missing agent/session', {
          currentSessionId,
          selectedAgentId,
        });
        return;
      }
      useChatStore.getState().cancelTool(selectedAgentId, currentSessionId, toolCallId);
    },
    [currentSessionId, selectedAgentId],
  );

  // ADR-045: per-tool heartbeat state derived from session.
  // We subscribe to the session's toolProgress map and re-render only when
  // its identity changes (i.e. when a new tool starts OR the map gains a key).
  // The high-frequency heartbeat updates within an existing entry do NOT
  // re-render this component — only ExploreBlock re-renders for those,
  // because ExploreBlock subscribes directly via props.
  const toolProgressByToolCallId = useChatStore((s) => {
    if (!selectedAgentId || !currentSessionId) return undefined;
    return s.agentStates[selectedAgentId]?.sessionStates[currentSessionId]?.toolProgress;
  });

  // Tool approval: send decision via MQTT, then clear inline state
  const handleToolApprove = async (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => {
    const agentId = String(approval.agent_id ?? selectedAgentId ?? "");
    const requestId = String(approval.request_id ?? "");
    const sessionId = approval.session_id;
    try {
      await invoke("mqtt_publish_control", {
        agentId,
        command: "approval_decision",
        payloadJson: {
          session_id: sessionId ?? "",
          request_id: requestId,
          approved: action === "allow",
          allow_all_session: false,
          reason: "",
        },
      });
    } catch (err) {
      log.error("[ChatPanel] Failed to send approval:", err);
    }
    // Clear the specific approval from the pending map by tool_call_id
    if (selectedAgentId && approval.tool_call_id) {
      resolveApprovalByToolCallId(selectedAgentId, approval.tool_call_id);
    } else {
      resolveApproval(selectedAgentId ?? "");
    }
  };

  // Ask question answer: send answer via MQTT, then clear the answered question from the queue
  const handleQuestionAnswer = async (requestId: string, answer: string) => {
    if (!selectedAgentId) return;
    const agentId = String(selectedAgentId);
    const sessionId = selectedAgentId ? useChatStore.getState().getActiveSessionId(selectedAgentId) : null;
    try {
      await invoke("mqtt_publish_control", {
        agentId,
        command: "question_answer",
        payloadJson: {
          session_id: sessionId ?? "",
          request_id: requestId,
          answer,
        },
      });
    } catch (err) {
      log.error("[ChatPanel] Failed to send question answer:", err);
    }
    // Clear the answered question from the queue by requestId
    useChatStore.getState().resolveQuestion(agentId, requestId);
  };

  // Auto-send queued messages when agent finishes execution
  useEffect(() => {
    if (!sending && queuedMessages.length > 0 && selectedAgentId && currentSessionId) {
      const msgs = [...queuedMessages];
      useChatStore.getState().setQueuedMessages(selectedAgentId, currentSessionId, []);
      for (const msg of msgs) {
        void sendMessage(msg, selectedAgentId);
      }
    }
  }, [sending]);

  // ── Empty state: no agents at all ──
  if (Object.keys(useAgentStore.getState().agents).length === 0) {
    return (
      <div className="flex flex-1 items-center justify-center">
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
      <div className="flex flex-1 items-center justify-center">
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
      <div className="flex flex-1 items-center justify-center">
        <div className="text-center">
          <Tooltip content={t("chatPanel.startAgent")} variant="plain">
            <button
              onClick={async () => {
                await startAgentAndSyncUI(selectedAgent.agent_id);
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

  // ── Initializing session ──
  // The window between MQTT pushing `running` → true and `startAgentAndSyncUI`
  // finishing its atomic initSessionForAgent chain (fetchLatestSession +
  // fetchSessions + openSession (ADR-038: was `activateSession`, now
  // `chatStore.openSession` which fires MQTT `open_session` + HTTP reload)
  // + loadSession + ensureLatestInCache).
  // During this brief window activeSessionId is still null, so without this
  // gate the chat view would mount with no session and surface the
  // "Start a conversation" placeholder for a few hundred ms — a misleading
  // "blank session" bug.  Showing a spinner here keeps the contract honest:
  // when the chat view finally mounts, the session is fully bootstrapped.
  if (!currentSessionId) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <div className="text-center">
          <div className="mx-auto h-8 w-8 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
          <p className="mt-3 text-xs text-zinc-400 dark:text-zinc-500">Loading session...</p>
        </div>
      </div>
    );
  }

  // ── Chat view ──
  // ADR-036: input gating must reflect BOTH the Gateway HTTP health AND
  // the MQTT realtime connection.  Before ADR-036 this only checked
  // gateway, leaving the textarea enabled while MQTT was silently
  // disconnected — the user could type and click send, but the message
  // would never reach the Runtime, producing a "ghost input" state.
  // `mqttConnected` is now driven by the Rust `mqtt-status` Tauri event
  // so this check reflects real broker liveness.
  const inputDisabled = gatewayStatus !== "connected" || !mqttConnected;

  return (
    <>
      <div
        className="flex flex-1 min-w-[288px] flex-col overflow-hidden"
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
            onScroll={() => {
                              lastScrollTopRef.current = (messagesContainerRef.current?.scrollTop ?? 0);
                              lastScrollHeightRef.current = (messagesContainerRef.current?.scrollHeight ?? 0);
                              const fvbId = vmlRef.current?.getFirstVisibleBlockId();
                              if (fvbId != null) lastFirstVisibleBlockIdRef.current = fvbId;
                              const fvbIdx = vmlRef.current?.getFirstVisibleBlockIndex();
                              if (fvbIdx != null) lastFirstVisibleBlockIdxRef.current = fvbIdx;
                              lastMessageOffsetRef.current = adapter.messageOffset;
                              scrollController.handleScroll();
                            }}
            className="relative h-full overflow-y-auto px-4 py-3 select-text cursor-text"
            role="log"
            aria-label={t("chatPanel.ariaLabelChatMessages")}
          >
            {/* VirtualMessageList — owns useVirtualizer and handles all virtual
                scrolling rendering. Scroll behavior is owned by useScrollController.
                key={currentScrollKey} forces React to unmount/remount the entire
                component on session/agent switch.  This creates a fresh Virtualizer
                instance with scrollOffset=0, eliminating the white-screen bug where
                the old instance's scrollOffset (e.g. 5000px from a long session)
                exceeds the new session's totalSize (e.g. 800px), causing
                getVirtualItems() to return an empty array. */}
            <VirtualMessageList
              key={currentScrollKey ?? "__no_session__"}
              ref={vmlRef}
              onRetryLoadSession={handleRetryLoadSession}
              messageBlocks={messageBlocks}
              sending={sending}
              pendingApproval={pendingApproval}
              currentSessionId={currentSessionId}
              selectedAgentId={selectedAgentId}
              agentDisplayName={agentDisplayName}
              selectedAgent={selectedAgent}
              userDisplayName={userDisplayName}
              userAvatarUrl={userAvatarUrl}
              userBuiltinAvatarId={userBuiltinAvatarId}
              onApprove={handleToolApprove}
              onCancelTool={handleToolCancel}
              toolProgress={toolProgressByToolCallId}
              isThinking={isThinking}
              thinkingContent={thinkingContent}
              thinkingStartTime={thinkingStartTime}
              t={t}
              adapter={adapter}
              scrollContainerRef={messagesContainerRef}
              isLoadingSession={isLoadingSession}
              loadError={loadError}
              messages={messages}
            />
            {/* ADR-050 C5: Working indicator removed.  The v2 adapter folds
                live streaming content into blocks (isLive: true); the trailing
                live block renders via ExploreBlock / StreamingSourceBlock which
                provides more information than a static "working..." label. */}
            {/* Debug paused banner — shown when the agent is in dev_mode and
                the debugger is currently in Stepping/Paused state. Provides
                F5 (resume) and F10 (step) actions directly from the chat.
                The banner renders its own bannerSlot wrapper and returns
                null when not visible, so no empty wrapper sits in the DOM
                when hidden — that empty wrapper would otherwise leave
                a mt-1.5 of dead space below the scroll viewport and cause
                a phantom scrollbar on empty sessions. */}
            <DebugPausedBanner />
            {/* 429 Retry wait banner — countdown + Skip Wait button, shown
                when LLM provider returns 429 with Retry-After > 10s. Same
                wrapper ownership as DebugPausedBanner. */}
            <RetryWaitBanner />
            {/* ADR-049: Phase indicators driven by getProcessingPhase().
                Each phase maps to a dot color + i18n label. paused is not
                handled here: DebugPausedBanner / RetryWaitBanner /
                iterationLimitPaused / loopDetectedPaused cover all paths. */}
            {phase === "thinking" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-purple-500 animate-pulse" />
                <span className="thinking-shimmer text-zinc-500 dark:text-zinc-400" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseThinking", { defaultValue: "Thinking…" })}
                </span>
              </div>
            )}
            {phase === "waiting" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-zinc-400 dark:bg-zinc-500 animate-pulse" />
                <span className="thinking-shimmer text-zinc-500 dark:text-zinc-400" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseWaiting", { defaultValue: "Waiting for model…" })}
                </span>
              </div>
            )}
            {phase === "streaming" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                <span className="thinking-shimmer text-zinc-500 dark:text-zinc-400" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseStreaming", { defaultValue: "Generating reply…" })}
                </span>
              </div>
            )}
            {phase === "tool_executing" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-blue-500 animate-pulse" />
                <span className="thinking-shimmer text-zinc-500 dark:text-zinc-400" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseToolExecuting", { defaultValue: "Running tool…" })}
                </span>
              </div>
            )}
            {phase === "waiting_approval" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-yellow-500 animate-pulse" />
                <span className="thinking-shimmer text-zinc-500 dark:text-zinc-400" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseWaitingApproval", { defaultValue: "Waiting for tool approval…" })}
                </span>
              </div>
            )}
{/* Iteration limit pause — hint + Continue button */}
            {iterationLimitPaused && (
              <div className="mt-1.5 flex justify-center px-6">
                <div className="inline-flex flex-wrap items-center gap-x-2 gap-y-2 rounded-md border border-[var(--color-accent)]/30 bg-[var(--color-accent)]/10 px-4 py-2 text-[var(--color-accent)] select-none dark:border-[var(--color-accent)]/40 dark:bg-[var(--color-accent)]/15 dark:text-[var(--color-accent)]">
                  <span
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)" }}
                  >
                    {iterationLimitPaused.message}
                  </span>
                  <button
                    onClick={handleContinue}
                    className="ml-auto flex w-fit max-w-full items-center gap-1 rounded bg-[var(--color-accent)] px-2 py-0.5 text-[11px] font-medium text-white transition-colors hover:brightness-90"
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}
                  >
                    <Play className="h-3 w-3" />
                    <span>
                      Continue ({iterationLimitPaused.iteration}/{iterationLimitPaused.maxIterations})
                    </span>
                  </button>
                </div>
              </div>
            )}
            {loopDetectedPaused && (
              <div className="mt-1.5 flex justify-center px-6">
                <div className="inline-flex flex-wrap items-center gap-x-2 gap-y-2 rounded-md border border-[var(--color-accent)]/30 bg-[var(--color-accent)]/10 px-4 py-2 text-[var(--color-accent)] select-none dark:border-[var(--color-accent)]/40 dark:bg-[var(--color-accent)]/15 dark:text-[var(--color-accent)]">
                  <span
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)" }}
                  >
                    {loopDetectedPaused.message}
                  </span>
                  <button
                    onClick={handleContinue}
                    className="ml-auto flex w-fit max-w-full items-center gap-1 rounded bg-[var(--color-accent)] px-2 py-0.5 text-[11px] font-medium text-white transition-colors hover:brightness-90"
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}
                  >
                    <Play className="h-3 w-3" />
                    <span>Continue</span>
                  </button>
                </div>
              </div>
            )}
            {/* Ask question cards — shown when LLM asks the user questions */}
            {serverError && (
              <div className="mt-1.5 flex justify-center px-6">
                <div className="inline-flex flex-wrap items-center gap-x-2 gap-y-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-4 py-2 text-amber-600 select-none dark:border-amber-500/40 dark:bg-amber-500/15 dark:text-amber-400">
                  <AlertTriangle className="h-4 w-4 shrink-0" />
                  <div className="min-w-0 flex-1">
                    <div className="whitespace-pre-wrap break-words" style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)" }}>
                      {serverError.content}
                    </div>
                    {serverError.errorDetail && (
                      <details className="mt-1">
                        <summary className="cursor-pointer text-xs text-amber-500/70 hover:text-amber-500 select-none">
                          Details
                        </summary>
                        <pre className="mt-1 max-h-40 overflow-auto rounded bg-black/5 dark:bg-white/5 p-2 text-xs text-amber-500/70 whitespace-pre-wrap break-all">
                          {serverError.errorDetail}
                        </pre>
                      </details>
                    )}
                  </div>
                  <button
                    onClick={() => {
                      if (selectedAgentId && currentSessionId) {
                        clearServerError(selectedAgentId, currentSessionId);
                      }
                    }}
                    className="ml-auto flex w-fit items-center gap-1 rounded bg-amber-500 px-2 py-0.5 text-[11px] font-medium text-white transition-colors hover:brightness-90"
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}
                  >
                    <X className="h-3 w-3" />
                    <span>Dismiss</span>
                  </button>
                </div>
              </div>
            )}
            {pendingQuestions.length > 0 && (
              <div className="space-y-1">
                {/* Progress indicator */}
                <div className="flex items-center gap-2 px-1">
                  <div className="flex items-center gap-1.5 text-xs text-zinc-500 dark:text-zinc-400">
                    <span className="font-medium text-zinc-700 dark:text-zinc-300">
                      {currentQuestionIndex + 1} / {pendingQuestions.length}
                    </span>
                    <span>{t("askQuestionCard.questions")}</span>
                  </div>
                  {pendingQuestions.length > 1 && (
                    <div className="flex items-center gap-1 ml-auto">
                      <button
                        onClick={() => setCurrentQuestionIndex((i) => Math.max(0, i - 1))}
                        disabled={currentQuestionIndex === 0}
                        className="rounded p-0.5 text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300 disabled:opacity-30 disabled:cursor-not-allowed transition-colors"
                        aria-label={t("askQuestionCard.previous")}
                      >
                        <ChevronLeft className="h-3.5 w-3.5" />
                      </button>
                      {/* Dots */}
                      <div className="flex items-center gap-1 mx-1">
                        {pendingQuestions.map((_, idx) => (
                          <button
                            key={idx}
                            onClick={() => setCurrentQuestionIndex(idx)}
                            className={`h-1.5 w-1.5 rounded-full transition-all ${
                              idx === currentQuestionIndex
                                ? "bg-zinc-500 dark:bg-zinc-300 w-3"
                                : "bg-zinc-300 dark:bg-zinc-600 hover:bg-zinc-400 dark:hover:bg-zinc-500"
                            }`}
                            aria-label={`${t("askQuestionCard.goTo")} ${idx + 1}`}
                          />
                        ))}
                      </div>
                      <button
                        onClick={() => setCurrentQuestionIndex((i) => Math.min(pendingQuestions.length - 1, i + 1))}
                        disabled={currentQuestionIndex === pendingQuestions.length - 1}
                        className="rounded p-0.5 text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-300 disabled:opacity-30 disabled:cursor-not-allowed transition-colors"
                        aria-label={t("askQuestionCard.next")}
                      >
                        <ChevronRight className="h-3.5 w-3.5" />
                      </button>
                    </div>
                  )}
                </div>
                {/* Current question card */}
                <AskQuestionCard
                  key={pendingQuestions[currentQuestionIndex].request_id}
                  event={pendingQuestions[currentQuestionIndex]}
                  agentId={selectedAgentId ?? ""}
                  sessionId={currentSessionId}
                  onAnswer={handleQuestionAnswer}
                />
              </div>
            )}
          </div>
          {/* Scroll-to-top button - visible when scrolled down > 1 screen.
              Default opacity is reduced so the button does not visually crowd
              the chat content; it goes fully opaque on hover/focus for clear
              affordance. The existing `transition-all` covers the opacity
              animation, and the `hover:bg-zinc-200` background change is
              independent of the opacity change so both compose cleanly. */}
          {scrollController.showScrollToTop && (
            <button
              onClick={scrollController.jumpToTop}
              className="absolute top-3 right-4 z-10 rounded-full bg-zinc-100 dark:bg-zinc-700 border border-zinc-200 dark:border-zinc-600 shadow-md p-1.5 opacity-40 hover:opacity-100 focus-visible:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600 transition-all animate-in fade-in zoom-in"
              aria-label="Scroll to top"
            >
              <ChevronsUp className="h-4 w-4 text-zinc-500 dark:text-zinc-400" />
            </button>
          )}
          {/* Scroll-to-bottom button — visible when scrolled up > 1 screen.
              Same default-reduced / hover-full-opacity treatment as the
              scroll-to-top button for visual consistency. */}
          {scrollController.showScrollToBottom && (
            <button
              onClick={scrollController.jumpToBottom}
              className="absolute bottom-3 right-4 z-10 rounded-full bg-zinc-100 dark:bg-zinc-700 border border-zinc-200 dark:border-zinc-600 shadow-md p-1.5 opacity-40 hover:opacity-100 focus-visible:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600 transition-all animate-in fade-in zoom-in"
              aria-label={t("chatPanel.ariaLabelScrollToBottom")}
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
              onClick={() => session.setTodosCollapsed(!session.todosCollapsed)}
            >
              {session.todosCollapsed ? (
                <ChevronRight className="h-3 w-3 mr-1 text-zinc-400 dark:text-zinc-500 shrink-0" />
              ) : (
                <ChevronDown className="h-3 w-3 mr-1 text-zinc-400 dark:text-zinc-500 shrink-0" />
              )}
              <span className="min-w-0 truncate text-[10px] font-medium text-zinc-400 dark:text-zinc-500 uppercase tracking-wider">
                {(() => {
                  const completed = todos.filter(t => t.status === "completed").length;
                  const total = todos.length;
                  const currentTodo = todos.find(t => t.status === "in_progress");
                  const isAllCompleted = completed === total && total > 0;
                  return (
                    <>
                      <span className="inline whitespace-nowrap">{t("chatPanel.taskList", { completed, total })}</span>
                      {!isAllCompleted && currentTodo && (
                        <>
                          <span className="inline-block w-8"/>
                          <span className="normal-case text-zinc-500 dark:text-zinc-400 truncate">
                            {t("chatPanel.currentTask", { current: currentTodo.content })}
                          </span>
                        </>
                      )}
                    </>
                  );
                })()}
              </span>
            </button>
            {!session.todosCollapsed && (
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
                        "flex-1 min-w-0 text-xs leading-relaxed truncate",
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
        <div className="mx-3 mb-3 rounded-md border border-zinc-200 dark:border-zinc-700 bg-chat-area">
          {/* Active skill badge */}
          {activeSkill && (
            <div className="flex items-center gap-1 px-3 pt-2">
              <span className="inline-flex items-center gap-1 rounded bg-[var(--color-accent)]/10 px-1.5 py-0.5 text-xs font-medium border border-[var(--color-accent)]/20" style={{ color: "var(--color-accent)" }}>
                /{activeSkill.name}
                <button
                  type="button"
                  onClick={clearActiveSkill}
                  className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-[var(--color-accent)]/15"
                  aria-label={t("chatPanel.ariaLabelClearActiveSkill")}
                >
                  <X size={12} />
                </button>
              </span>
            </div>
          )}

          {/* Pending attachment chips (ADR-046: unified entry-point).
              Documents use the existing `DocumentChip`; images get a thumbnail
              variant. Both render from `pendingAttachedItems`. */}
          {session.pendingAttachedItems.length > 0 && (
            <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
              {session.pendingAttachedItems.map((p) => {
                const item = p.item;
                // Workspace reference: already-resolved `attached_*` items.
                if (item && (item.type === "attached_file"
                  || item.type === "attached_selection"
                  || item.type === "attached_folder")) {
                  return (
                    <DocumentChip
                      key={p.tempId}
                      filename={item.name}
                      format="workspace"
                      status={p.status}
                      errorMessage={p.errorMessage}
                      onRemove={() => handleRemovePending(p.tempId)}
                    />
                  );
                }
                // Image upload (or pre-upload with only localUrl set).
                if (item && item.type === "image_upload") {
                  return (
                    <div
                      key={p.tempId}
                      className="group relative h-14 w-14 shrink-0 overflow-hidden rounded-md border border-zinc-200 dark:border-zinc-700"
                    >
                      <img
                        src={p.localUrl ?? `/api/agents/${selectedAgentId ?? ""}/files/${item.documentId}`}
                        alt={item.filename}
                        className="h-full w-full object-cover"
                      />
                      <button
                        type="button"
                        onClick={() => handleRemovePending(p.tempId)}
                        className="absolute -right-0.5 -top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-red-500 text-white opacity-0 transition-opacity group-hover:opacity-100"
                        aria-label={`Remove ${item.filename}`}
                      >
                        <X size={10} />
                      </button>
                    </div>
                  );
                }
                // Pre-upload preview for an image that has only a localUrl.
                if (!item && p.localUrl) {
                  return (
                    <div
                      key={p.tempId}
                      className="group relative h-14 w-14 shrink-0 overflow-hidden rounded-md border border-zinc-200 dark:border-zinc-700"
                    >
                      <img
                        src={p.localUrl}
                        alt=""
                        className="h-full w-full object-cover opacity-60"
                      />
                      <button
                        type="button"
                        onClick={() => handleRemovePending(p.tempId)}
                        className="absolute -right-0.5 -top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-red-500 text-white opacity-0 transition-opacity group-hover:opacity-100"
                        aria-label="Remove attachment"
                      >
                        <X size={10} />
                      </button>
                    </div>
                  );
                }
                // Document upload (or upload-in-flight without resolved item).
                const filename = item && item.type === "file_upload" ? item.filename : "";
                const format = item && item.type === "file_upload" ? item.format : "";
                const size = item && item.type === "file_upload" ? item.sizeBytes : undefined;
                return (
                  <DocumentChip
                    key={p.tempId}
                    filename={filename}
                    format={format}
                    size={size}
                    status={p.status}
                    errorMessage={p.errorMessage}
                    onRemove={() => handleRemovePending(p.tempId)}
                  />
                );
              })}
            </div>
          )}
          {/* Attached context chips (from right-click "Add to Chat") */}
          <AttachedContextChips />
          {/* Textarea area — borderless, transparent background */}
          <textarea
            value={session.inputValue}
            onChange={(e) => session.setInputValue(e.target.value)}
            placeholder={
              gatewayStatus !== "connected"
                ? t("chatPanel.inputGatewayDisconnected")
                : !mqttConnected
                  ? activeSkill
                    ? t("chatPanel.inputParamsConnecting")
                    : t("chatPanel.inputMessageConnecting")
                  : activeSkill
                    ? t("chatPanel.inputParams")
                    : t("chatPanel.inputMessage")
            }
            disabled={inputDisabled}
            className="w-full resize-none border-0 bg-transparent p-3 pb-2 outline-none placeholder:text-zinc-400 dark:placeholder:text-zinc-500 dark:text-zinc-100 disabled:cursor-not-allowed disabled:opacity-50 max-h-48 overflow-y-auto min-h-[4.5rem]"
            style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
            onKeyDown={(e) => {
              if (e.key !== "Enter" || e.shiftKey) return;
              // On macOS, compositionEnd fires before keydown(Enter) when the user
              // presses Enter to confirm an IME selection. isComposing is already
              // false by then, so we check a time window instead.
              const elapsed = Date.now() - lastCompositionEndRef.current;
              if (elapsed < 300) return; // IME confirmation — do not send
              e.preventDefault();
              // Enter is ALWAYS a send action — never a stop.
              // To stop, the user MUST click the Stop button (handleStop).
              // Rationale: spurious Enter events (macOS keyboard repeat, WKWebView
              // focus/keyboard quirks during window resize, accidental keypresses)
              // previously triggered urgent_stop and silently killed the stream.
              if (!sending) {
                handleSend();
                return;
              }
              // Sending: queue the input so it waits for the next loop iteration.
              // Empty input → no-op (do not enqueue an empty string).
              const content = session.inputValue.trim();
              if (content && selectedAgentId && currentSessionId) {
                useChatStore.getState().addQueuedMessage(
                  selectedAgentId,
                  currentSessionId,
                  content,
                );
                session.setInputValue("");
              }
            }}
            onCompositionEnd={() => {
              lastCompositionEndRef.current = Date.now();
            }}
          />

          {/* Bottom toolbar — @container for responsive button text collapse */}
          <div
            ref={(node) => {
              // Cleanup the previous install (if any) before handing
              // off to the ref callback, which re-installs on the new
              // node.  This handles re-mounts cleanly.
              const prev = (toolbarRef.current as unknown as { __toolbarTeardown__?: () => void } | null);
              prev?.__toolbarTeardown__?.();
              toolbarRefCallback(node);
            }}
            className="@container/tb flex items-center justify-between gap-2 px-3 pb-2 min-w-[264px]"
          >
            {/* Left: feature buttons */}
            <div className="flex items-center gap-1 min-w-0 overflow-visible">
              {/* Model switcher — only enabled when agent is running */}
              {availableModels.length > 1 && selectedAgent?.running && (
                <ModelMenu
                  wrapperRef={modelBtnRef}
                  textHidden={textHidden.model}
                  models={availableModels}
                  currentModel={currentModel}
                  currentProvider={currentProvider}
                  onSelect={(m, p) => selectedAgentId && setCurrentModel(m, p, selectedAgentId)}
                  btnId="model"
                />
              )}
              {/* Reasoning effort toggle — shown when session has a non-null reasoningEffort (null = provider doesn't support reasoning) */}
              {selectedAgent?.running && currentReasoningEffort != null && (
                <ReasoningEffortMenu
                  wrapperRef={effortBtnRef}
                  textHidden={textHidden.effort}
                  effort={currentReasoningEffort}
                  onChange={(e) => selectedAgentId && setReasoningEffort(e, selectedAgentId)}
                  btnId="effort"
                />
              )}
              {/* Workspace button */}
              <div ref={wsBtnRef} className="min-w-0">
                <WorkspaceSelector textHidden={textHidden.ws} />
              </div>
              {/* Skills dropdown */}
              <div ref={skBtnRef} className="min-w-0">
                <SkillsPanel textHidden={textHidden.sk} />
              </div>
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
                ? (session.inputValue.trim()
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
                        || (!session.inputValue.trim() && !session.pendingAttachedItems.some((p) => p.status === "success" && p.item !== undefined))
                        || session.pendingAttachedItems.some((p) => p.status === "uploading"))
                  }
                  aria-label={sending ? (session.inputValue.trim() ? t("chatPanel.addToQueue") : queuedMessages.length > 0 ? t("chatPanel.sendQueuedAndStop") : t("chatPanel.stop")) : t("chatPanel.sendMessage")}
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
        open={session.showImageUnsupportedDialog}
        models={session.imageCapableModels}
        onSelect={(model: string, provider: string) => {
          if (selectedAgentId) {
            setCurrentModel(model, provider, selectedAgentId);
            session.setShowImageUnsupportedDialog(false);
          }
        }}
        onClose={() => session.setShowImageUnsupportedDialog(false)}
      />
    </>
  );
}

/** Shell tools (bash, powershell, shell) need Terminal icon and command preview. */

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
    <div className="fixed inset-0 z-[60] flex items-center justify-center bg-modal-overlay" onClick={onClose}>
      <div
        className="w-[400px] overflow-hidden rounded-md bg-modal-surface shadow-xl flex flex-col"
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
  textHidden,
  wrapperRef: externalRef,
  btnId,
}: {
  models: { name: string; provider: string; tool_call?: boolean; reasoning?: boolean; input_modalities?: string[] }[];
  currentModel: string | null;
  currentProvider: string | null;
  onSelect: (model: string, provider: string) => void;
  textHidden?: boolean;
  /** Optional external ref merged with the internal click-outside ref */
  wrapperRef?: React.Ref<HTMLDivElement>;
  /** Toolbar button id used by ChatPanel's collapse observer */
  btnId?: string;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [showAddDialog, setShowAddDialog] = useState(false);
  const internalRef = useRef<HTMLDivElement>(null);
  const ref = useMergedRef(internalRef, externalRef);

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
      if (internalRef.current && !internalRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  const modelDisplayName = (() => {
    if (!currentModel || !currentModel.includes('/')) return currentModel ?? t("chatPanel.modelFallback");
    const parts = currentModel.split('/');
    const prefix = parts[0];
    const modelName = parts.slice(1).join('/');
    return modelName.length > prefix.length ? modelName : currentModel;
  })();

  return (
    <ToolbarDropdownTrigger
      icon={<Layers size={14} />}
      label={modelDisplayName}
      collapseClass="tb-model-text"
      tipClass="tb-model-tip"
      tooltip={t("chatPanel.selectModel")}
      open={open}
      onToggle={() => setOpen(!open)}
      wrapperRef={ref}
      textHidden={textHidden}
      btnId={btnId}
    >
      {/* Popup menu */}
      {open && (
        <div
          className={cn(
            "absolute bottom-full left-0 z-50 mb-1 overflow-hidden rounded-md border shadow-lg",
            "border-zinc-200 bg-modal-surface dark:border-zinc-700",
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
            {t("chatPanel.addModel")}
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
  textHidden,
  wrapperRef: externalRef,
  btnId,
}: {
  effort: string | null;
  onChange: (effort: string) => void;
  textHidden?: boolean;
  /** Optional external ref merged with the internal click-outside ref */
  wrapperRef?: React.Ref<HTMLDivElement>;
  /** Toolbar button id used by ChatPanel's collapse observer */
  btnId?: string;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const internalRef = useRef<HTMLDivElement>(null);
  const ref = useMergedRef(internalRef, externalRef);

  // Values are lowercase to match backend ReasoningEffort serde serialization.
  const OPTIONS: { value: string; label: string; color: string }[] = [
    { value: "auto", label: t("chatPanel.reasoningAuto"), color: "#22c55e" },
    { value: "off", label: t("chatPanel.reasoningOff"), color: "#9ca3af" },
    { value: "low", label: t("chatPanel.reasoningLow"), color: "#3b82f6" },
    { value: "medium", label: t("chatPanel.reasoningMedium"), color: "#8b5cf6" },
    { value: "high", label: t("chatPanel.reasoningHigh"), color: "#ef4444" },
  ];

  const currentOpt = OPTIONS.find((o) => o.value === effort);
  const effortLabel = currentOpt?.label ?? "Auto";
  const currentColor = currentOpt?.color ?? "#22c55e";

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (internalRef.current && !internalRef.current.contains(e.target as Node)) {
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
      collapseClass="tb-effort-text"
      tipClass="tb-effort-tip"
      tooltip={t("chatPanel.selectReasoningEffort") ?? "Reasoning effort"}
      open={open}
      onToggle={() => setOpen(!open)}
      wrapperRef={ref}
      textHidden={textHidden}
      btnId={btnId}
    >
      {open && (
        <div
          className={cn(
            "absolute bottom-full left-0 z-50 mb-1 overflow-hidden rounded-md border shadow-lg",
            "border-zinc-200 bg-modal-surface dark:border-zinc-700",
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

