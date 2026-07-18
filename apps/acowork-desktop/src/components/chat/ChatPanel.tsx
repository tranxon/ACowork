import React, { useEffect, useLayoutEffect, useRef, useState, useCallback, useMemo } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSkillStore } from "../../stores/skillStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { ToolApprovalNeededEvent } from "../../lib/types";
import { cn } from "../../lib/utils";
import { fetchProviderModels } from "../../lib/gateway-api";
import { startAgentAndSyncUI } from "../../lib/agent-start";
import { toolbarButton } from "../../lib/ui-styles";
import { AddProviderFlow } from "../harness/AddProviderFlow";
import { Bot, Play, Send, ChevronDown, ChevronRight, ChevronsDown, Wrench, AlertTriangle, X, Square, Plus, Layers, Loader, Pencil, Paperclip, Image, Brain, Circle, CircleDot } from "lucide-react";
import type { ChatMessage, VaultKeyEntry, ModelEntry } from "../../lib/types";
import { ContextUsageIcon } from "./ContextUsageIcon";
import { useSessionScope } from "./useSessionScope";
import { VirtualMessageList, type VirtualMessageListHandle } from "./VirtualMessageList";

/**
 * `MessageBlock` — strict intermediate representation between the data layer
 * (raw `ChatMessage[]` in the chatStore) and the rendering layer
 * (`VirtualMessageList`).
 *
 * Why this exists:
 *  - The chatStore stores raw messages in chronological order. A single
 *    "display group" (think + tool_call + tool_result folded into one chip)
 *    can span multiple raw messages.
 *  - The rendering layer needs a stable unit keyed by a single `blockId` so
 *    React's diff and TanStack's virtualizer can identify it cleanly.
 *  - Several display decisions that used to live in the rendering layer as
 *    *estimated* visual state (sticky-bottom, scroll restoration on prepend)
 *    are derivable purely from properties carried on the block itself.
 *
 * Every property on this interface is data-defined, not visually estimated:
 *
 * - `type`           drives the component choice (MessageBubble vs ExploreBlock).
 * - `items`          the raw messages the block contains; for non-group blocks
 *                    `items.length === 1`. This is the single source of truth
 *                    for "what raw entries does this block render".
 * - `rawCount`       `items.length` cached for cheap arithmetic in pagination.
 * - `anchorToLatest` true iff this block contains the **last raw entry** in
 *                    the current messages array. Pure data semantics — the
 *                    rendering layer derives "user is at the bottom" from
 *                    "the viewport contains an anchorToLatest block".
 * - `anchorToUser`   transient: true on exactly one block immediately after
 *                    a "load older" trigger, so the rendering layer knows
 *                    which block to keep visually stable when older messages
 *                    are prepended. Cleared after the renderer scrolls to it.
 */
export interface MessageBlock {
  blockId: string;
  type: ChatMessage["type"] | "explore_group";
  items: ChatMessage[];
  rawCount: number;
  anchorToLatest: boolean;
  anchorToUser: boolean;
  /**
   * True iff an `assistant` (or other non-explore) message follows this
   * explore block in display order.  Drives ExploreBlock's auto-collapse
   * and the virtualizer's exact-height estimator (collapsed = ~32px header
   * only, expanded = header + content up to 240px cap).  Pre-computed here
   * so the rendering layer doesn't have to walk the array a second time.
   * Only meaningful for `type === "explore_group"`; false for all others.
   */
  hasFollowUpReply: boolean;
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
import { AgentAvatar } from "../common/AgentAvatar";
import { DocumentChip } from "./DocumentChip";
import { AttachedContextChips } from "./AttachedContextChips";
import { ToolbarDropdownTrigger } from "../common/ToolbarDropdown";
import { Tooltip } from "../common/Tooltip";

// Generous threshold used ONLY for scroll snapshot on unmount (nav-back).
// Real-time pinned-to-bottom detection in handleScroll uses a strict 5px
// to let the user escape auto-scroll with a tiny upward flick.
const CHAT_BOTTOM_THRESHOLD_PX = 120;

// Stable empty array reference for the `messages` Zustand selector.
// Returning `[]` literals from a selector creates a new reference on every
// `getSnapshot` call, which trips useSyncExternalStore's "The result of
// getSnapshot should be cached" check and produces a "Maximum update depth
// exceeded" infinite re-render loop during transient states (mount, agent
// switch, session switch) where the agent's session entry does not yet
// exist. The same pattern is already used in ResultsPanel.tsx.
const EMPTY_MESSAGES: ChatMessage[] = [];

interface ChatScrollSnapshot {
  scrollOffset: number;
  pinnedToBottom: boolean;
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

function getDistanceFromBottom(container: HTMLElement): number {
  return container.scrollHeight - container.scrollTop - container.clientHeight;
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
  // Cache full / icon widths keyed by the set of present button IDs.
  // Measuring requires temporary DOM style writes that force a
  // synchronous layout — we do it only when the button-set changes,
  // NOT on every resize, to avoid layout side-effects on siblings
  // (e.g. the agent-list subtly changing width).
  const cachedWidthsRef = useRef<{ key: string; full: number[]; icon: number[] } | null>(null);
  // Deferred state update: store the latest desired textHidden here and
  // flush it in a rAF to avoid synchronous React re-renders during
  // ResizeObserver callbacks (which can cause sibling layout shifts).
  const pendingHiddenRef = useRef<Record<string, boolean> | null>(null);
  const toolbarRafRef = useRef<number>(0);
  useLayoutEffect(() => {
    const container = toolbarRef.current;
    if (!container) return;

    // Invalidate cache when the effect re-runs (agent changed).
    cachedWidthsRef.current = null;

    // Importance ranking — higher number = kept visible longer.
    // Fold starts from leftmost (model, lowest importance) → rightmost (sk, highest).
    const BUTTON_IMPORTANCE: Record<string, number> = {
      model: 1,
      effort: 2,
      ws: 3,
      sk: 4,
    };

    const measure = () => {
      const present: { id: string; el: HTMLElement }[] = [];
      const refs: Record<string, React.RefObject<HTMLDivElement | null>> = {
        model: modelBtnRef,
        effort: effortBtnRef,
        ws: wsBtnRef,
        sk: skBtnRef,
      };
      for (const [id, ref] of Object.entries(refs)) {
        if (ref.current) present.push({ id, el: ref.current });
      }
      console.log("[toolbar]", JSON.stringify({
        containerW: container.offsetWidth,
        present: present.map((p) => p.id),
      }));
      if (present.length === 0) return;

      const findText = (el: HTMLElement): HTMLElement | null =>
        el.querySelector('[data-toolbar-text]') as HTMLElement | null;
      const findChevron = (el: HTMLElement): HTMLElement | null =>
        el.querySelector('[data-toolbar-chevron]') as HTMLElement | null;

      const cacheKey = present.map((p) => p.id).join(",");

      let fullWidths: number[];
      let iconWidths: number[];

      if (cachedWidthsRef.current?.key === cacheKey) {
        // Use cached measurements — no DOM writes, no forced layout.
        fullWidths = cachedWidthsRef.current.full;
        iconWidths = cachedWidthsRef.current.icon;
      } else {
        // First time (or button-set changed): measure true full widths
        // via scrollWidth with all text visible, then icon-only widths.
        present.forEach((b) => {
          const t = findText(b.el); if (t) t.style.display = "";
          const c = findChevron(b.el); if (c) c.style.display = "";
        });
        fullWidths = present.map((b) => b.el.scrollWidth);

        present.forEach((b) => {
          const t = findText(b.el); if (t) t.style.display = "none";
          const c = findChevron(b.el); if (c) c.style.display = "none";
        });
        iconWidths = present.map((b) => b.el.offsetWidth);

        // Restore all text visible (default before first fold).
        present.forEach((b) => {
          const t = findText(b.el); if (t) t.style.display = "";
          const c = findChevron(b.el); if (c) c.style.display = "";
        });

        cachedWidthsRef.current = { key: cacheKey, full: fullWidths, icon: iconWidths };
      }

      // Compute available width for the left group.
      // Container has px-3 (24px total), gap-2 (8px) between left/right groups,
      // and a right cluster (ContextUsageIcon + send button ≈ 60px).
      // The two icon-only upload buttons (file + image) sit inside the left
      // group but are NOT in `present[]`, so we subtract their footprint
      // (~30 px each + their gap-1 separators) from the available width.
      const PAD_X = 24;
      const FLEX_GAP = 8; // gap-2 between left and right flex children
      const RIGHT_CLUSTER = 60;
      const UPLOAD_BUTTONS = 68; // gap + fileBtn(~30) + gap + imageBtn(~30), not in present[]
      const GAP = 4; // gap-1 between buttons in the left group
      const available = container.offsetWidth - PAD_X - FLEX_GAP - RIGHT_CLUSTER - UPLOAD_BUTTONS;

      // Greedy fold: start with all labels visible, then fold the lowest-importance
      // button while the row would overflow. Folded buttons free up
      // (fullWidth - iconWidth) pixels.
      const totalFull =
        fullWidths.reduce((a, b) => a + b, 0) + (present.length - 1) * GAP;
      let currentTotal = totalFull;
      const nextHidden: Record<string, boolean> = {};
      present.forEach((b) => (nextHidden[b.id] = false));

      if (currentTotal > available) {
        const order = present
          .map((b, i) => ({ id: b.id, fullW: fullWidths[i], iconW: iconWidths[i] }))
          .sort((a, b) => BUTTON_IMPORTANCE[a.id] - BUTTON_IMPORTANCE[b.id]); // asc: fold low-importance first
        for (const item of order) {
          if (currentTotal <= available) break;
          nextHidden[item.id] = true;
          currentTotal -= item.fullW - item.iconW;
        }
      }

      console.log("[toolbar:calc]", JSON.stringify({
        fullWidths, iconWidths, available, totalFull, nextHidden,
      }));

      // Defer state update to next animation frame so React re-renders
      // happen *after* the current layout is painted. This prevents
      // synchronous layout interference with sibling panels (e.g. the
      // agent-list subtly changing width during resize).
      pendingHiddenRef.current = nextHidden;
      if (toolbarRafRef.current === 0) {
        toolbarRafRef.current = requestAnimationFrame(() => {
          toolbarRafRef.current = 0;
          const latest = pendingHiddenRef.current;
          if (!latest) return;
          setTextHidden((prev) => {
            let changed = false;
            for (const k of Object.keys(latest)) {
              if (prev[k] !== latest[k]) { changed = true; break; }
            }
            return changed ? latest : prev;
          });
        });
      }
    };

    const ro = new ResizeObserver(measure);
    ro.observe(container);
    measure();
    return () => {
      ro.disconnect();
      if (toolbarRafRef.current) {
        cancelAnimationFrame(toolbarRafRef.current);
        toolbarRafRef.current = 0;
      }
    };
  }, [selectedAgent?.running]);

  // Per-agent + per-session state selectors.
  // messages and sessionStatus are split into granular selectors because
  // they change at different frequencies: messages updates every ~500ms
  // poll cycle during streaming, while sessionStatus only changes on
  // state transitions (idle→streaming→idle).  Keeping them separate
  // prevents sessionStatus-derived values (sending, etc.) from
  // re-evaluating on every poll tick.
  const messages = useChatStore((s) => {
    if (!selectedAgentId) return EMPTY_MESSAGES;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return EMPTY_MESSAGES;
    return agent.sessionStates[agent.activeSessionId]?.messages ?? EMPTY_MESSAGES;
  });
  const sessionStatus = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.sessionStatus ?? null;
  });
  // Remaining session fields — change infrequently, single selector is fine.
  const sessionState = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId] ?? null;
  });
  const iterationLimitPaused = sessionState?.iterationLimitPaused ?? null;
  const pendingApproval = sessionState?.pendingApproval ?? {};
  const pendingQuestion = sessionState?.pendingQuestion ?? null;
  const isLoadingSession = sessionState?.isLoadingSession ?? false;
  const loadError = sessionState?.loadError ?? null;
  const todos = sessionState?.todos ?? [];
  /** Per-session queued messages — persisted in chatStore across agent switches */
  const queuedMessages = sessionState?.queuedMessages ?? [];
  /** True while an assistant stream has accumulated more than the line
   *  threshold (see chatStore.ASSISTANT_REPLYING_LINE_THRESHOLD) and is
   *  still streaming.  Cleared by the record_complete handler.  Drives
   *  the trailing "replying" indicator virtual item rendered INSIDE
   *  VirtualMessageList (see `extraItems` math below), so the indicator
   *  physically anchors to the last message bubble — the same slot the
   *  final reply will occupy once it lands. */
  const isAssistantReplying = sessionState?.isAssistantReplying ?? false;

  // ADR-021: "sending" is derived purely from sessionStatus (backend source of truth).
  // No optimistic flags — the backend pushes session_state_changed within ~50ms.
  const sending = sessionStatus
    ? (sessionStatus.status === "streaming"
      || sessionStatus.status === "waiting_approval"
      || sessionStatus.status === "paused")
    : false;
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
  const isLoadingMore = useChatStore((s) => s.isLoadingMore);
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
  } = useChatStore.getState();
  const currentSessionId = useChatStore((s) => selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null);
  const currentScrollKey = selectedAgentId && currentSessionId ? `${selectedAgentId}:${currentSessionId}` : null;
  const gatewayStatus = useGatewayStore((s) => s.status);
  const { activeSkill, clearActiveSkill } = useSkillStore();

  // ── Per-session scope ──────────────────────────────────────────────
  // All mutable state that is scoped to a single session lives in this
  // hook.  On session change the entire scope is atomically reset to
  // defaults, eliminating the class of bugs where per-session refs/state
  // leak across session switches.
  const session = useSessionScope(currentSessionId);

  const [hasLlmConfig, setHasLlmConfig] = useState<boolean | null>(null); // null = checking

  // Auto-collapse todo list when all tasks are completed
  useEffect(() => {
    if (todos.length === 0) return;
    if (todos.every(t => t.status === "completed")) {
      session.setTodosCollapsed(true);
    }
  }, [todos, session.setTodosCollapsed]);

  const messagesContainerRef = useRef<HTMLDivElement>(null);
  /** Timestamp of the last compositionEnd event. On macOS WKWebView, compositionEnd
   *  fires BEFORE the keydown(Enter) that confirmed the IME selection, so
   *  isComposing is already false when keydown runs. We use a time-window
   *  check instead: if compositionEnd happened within the last 300ms, the
   *  Enter was almost certainly an IME confirmation, not a send intent. */
  const lastCompositionEndRef = useRef(0);
  /**
   * "Pinned to bottom" UI preference — owned HERE (not in session.scope)
   * because it is per-USER-INTENT, not per-session.  Owned by ChatPanel so
   * it survives useSessionScope's session-change reset.  Shared with
   * VirtualMessageList via prop, which writes to it on mount and reads it
   * from the sticky-bottom effect.  handleScroll and scrollToBottom also
   * write to it from user-initiated scroll/button actions.
   */
  const pinnedToBottomRef = useRef(false);

  /**
   * Imperative handle to VirtualMessageList. Exposes data-derived queries
   * about the rendered MessageBlock layout. Used here by handleScroll to pick
   * the "anchorToUser" block (the first visible block at the moment the
   * user scrolls near the top) BEFORE triggering loadMoreOlderMessages.
   * No pixel math, no estimateSize-based heuristic — a pure data lookup.
   */
  const vmlRef = useRef<VirtualMessageListHandle | null>(null);

  const agentDisplayName = useAgentStore((s) => selectedAgentId ? s.agents[selectedAgentId]?.profile?.displayName : undefined) ?? selectedAgent?.display_name ?? selectedAgent?.name;

  // Read saved scroll snapshot for nav-back restoration.
  // VirtualMessageList uses this as initialOffset so the Virtualizer renders
  // the correct items from the first frame, preventing a top→position flash.
  const scrollSnapshot = currentScrollKey
    ? chatScrollSnapshots.get(currentScrollKey)
    : undefined;
  // While streaming the message list grows continuously, so any saved
  // scrollOffset is stale by definition — the same numeric offset points to
  // different content (or no content at all) after even a few seconds of
  // streaming.  Force a "Fresh session" scroll-to-bottom on remount whenever
  // the session is actively streaming, regardless of what the snapshot says.
  // This also defends against the snapshot having been written with
  // pinnedToBottom=false (e.g. by a stale handleScroll read) which would
  // otherwise land the user near the top of the conversation.
  const initialScrollOffset = !sending && scrollSnapshot &&
    !scrollSnapshot.pinnedToBottom &&
    scrollSnapshot.scrollOffset > 0
    ? scrollSnapshot.scrollOffset
    : undefined;
  // DIAGNOSTIC — log only when the (key, sending, snapshot) signature actually
  // changes, not on every render. ChatPanel re-renders frequently for many
  // unrelated reasons (other state updates in parent stores), and a
  // per-render log here flooded the console with hundreds of identical lines
  // during the reload-resume investigation.  Snapshot is an object reference
  // that may be recreated without changing values, so we compare via a
  // JSON-encoded signature of the few fields that actually matter.
  const snapshotSig = `${currentScrollKey}|${sending}|${scrollSnapshot?.pinnedToBottom ? 1 : 0}|${scrollSnapshot?.scrollOffset ?? ""}|${initialScrollOffset ?? ""}`;
  const lastSnapshotSigRef = useRef("");
  if (snapshotSig !== lastSnapshotSigRef.current) {
    lastSnapshotSigRef.current = snapshotSig;
    console.log("[CP:snapshot-read]", {
      currentScrollKey,
      scrollSnapshot,
      sending,
      initialScrollOffset,
    });
  }

  // ── Strict intermediate layer: raw `messages` → `MessageBlock[]` ──────────
  //
  // This is the ONLY place that knows about the relationship between raw
  // entries (chatStore.messages, one ChatMessage per JSONL line) and the
  // visual blocks the renderer draws. After this useMemo runs, every
  // downstream consumer (virtualCount, sticky-bottom logic, scroll
  // restoration, Agent-header detection, hasFollowUpReply) reads from
  // `messageBlocks` and never from `messages` directly.
  //
  // Two derived properties on every block are critical and must not be
  // guessed at by the rendering layer:
  //
  //  - `anchorToLatest` — set iff the block contains the **last raw entry**
  //    in `messages`. Pure data: derived from the loop's cursor index
  //    versus `lastIdx`. The rendering layer derives "user is at the
  //    bottom" by asking "is any visible block anchorToLatest?", with no
  //    scrollTop or estimateSize involvement.
  //
  //  - `anchorToUser` — transient: set on exactly one block per "load
  //    older" cycle, read from `scope.current.anchorToUserBlockId` which
  //    ChatPanel.handleScroll writes right before triggering
  //    loadMoreOlderMessages. The rendering layer scrolls to the
  //    anchorToUser block after the prepend lands, then clears the flag
  //    (also via the scope ref) so it doesn't fire twice.
  const messageBlocks = useMemo<MessageBlock[]>(() => {
    const blocks: MessageBlock[] = [];
    const lastIdx = messages.length - 1;
    const anchorToUserBlockId = session.scope.current.anchorToUserBlockId;
    let exploreStart = -1;
    let exploreBuffer: ChatMessage[] = [];

    const flushExplore = () => {
      if (exploreBuffer.length === 0) return;
      const startCursor = exploreStart;
      const items = exploreBuffer;
      const exploreBlockId = `block-${startCursor}`;
      blocks.push({
        blockId: exploreBlockId,
        type: "explore_group",
        items,
        rawCount: items.length,
        anchorToLatest: startCursor + items.length - 1 === lastIdx,
        anchorToUser: anchorToUserBlockId === exploreBlockId,
        // Backfilled in the second pass below (needs lookahead at the next
        // block to decide).  Default false here so the type checker is happy.
        hasFollowUpReply: false,
      });
      exploreBuffer = [];
      exploreStart = -1;
    };

    for (let i = 0; i < messages.length; i++) {
      const msg = messages[i];
      const blockId = `block-${i}`;

      if (msg.type === "tool_call" || msg.type === "tool_result" || msg.type === "thought") {
        if (exploreStart < 0) exploreStart = i;
        exploreBuffer.push(msg);
      } else {
        flushExplore();
        blocks.push({
          blockId,
          type: msg.type,
          items: [msg],
          rawCount: 1,
          anchorToLatest: i === lastIdx,
          anchorToUser: anchorToUserBlockId === blockId,
          // Only meaningful for explore_group; false for everyone else.
          hasFollowUpReply: false,
        });
      }
    }
    flushExplore();

    // Second pass: an explore_group has a follow-up reply iff the next block
    // in display order is NOT itself an explore_group (the original rule,
    // now hoisted into the block shape so the height estimator and the
    // renderer agree without re-walking the array).
    for (let i = 0; i < blocks.length - 1; i++) {
      const cur = blocks[i];
      if (cur.type === "explore_group" && blocks[i + 1].type !== "explore_group") {
        blocks[i] = { ...cur, hasFollowUpReply: true };
      }
    }

    return blocks;
    // session.scope is a stable ref object — its `.current` is read for the
    // latest anchorToUserBlockId without re-creating the memo. The parent
    // re-renders after the scope ref is mutated, picking up the new flag on
    // the next paint.
  }, [messages, session.scope]);

  // Show compacting indicator below messages when compaction is in progress
  const isCompacting = sessionState?.isCompacting ?? false;
  const showCompactingItem = isCompacting;

  // Working indicator — shown ONLY when the agent has not yet produced any
  // visible reply (streaming delta OR frozen record_complete) to the most
  // recent user turn.  Once the agent has produced any visible content
  // (placeholder isStreaming=true OR frozen isStreaming=false), the working
  // indicator must NOT reappear.
  //
  // ADR-035 D5/Race: the previous gate `sending && !hasStreamingPlaceholder`
  // flickered between `record_complete` (assistant.isStreaming: true→false)
  // and the follow-up `session_state_changed(idle)` (sending: true→false),
  // because the backend emits them in order on the same chunk channel
  // (`poll_stop()` → `transition_status(Idle)` at loop_.rs:912-914).  In
  // that ~tens-of-ms window the working indicator reappeared above the
  // just-rendered final assistant reply.
  //
  // Anchoring on "the last visible message is a user message" makes the
  // gate independent of that race.  This also naturally suppresses the
  // working indicator during inter-tool iterations (after tool_result) and
  // during continuation thinking — both of which legitimately have
  // `sending=true` without needing a working indicator.
  // Working indicator has two trigger conditions, but renders through ONE
  // shared visual block (AgentAvatar header gated separately, then the
  // single "正在处理..." line). Both cases share the same chrome
  // (`flex items-center gap-1.5 ml-12 py-1.5` + pulse + `thinking-shimmer`)
  // so the transition between after-user and inter-step phases is
  // visually seamless.
  //
  //   - showWorkingItemAfterUser: classic case — last visible raw entry is a
  //     user message, agent hasn't produced any visible reply yet.
  //   - showInterStepProcessing: inter-tool / inter-thought gap — agent
  //     already has explore_block visible (last raw entry is an agent-side
  //     message), `sending=true`, but no new thought/assistant stream_delta
  //     has landed yet (e.g. just after tool_result, before the next LLM
  //     response starts). Without this branch the user sees a static
  //     "finished" explore_block during the LLM round-trip window.
  //
  // Both branches are mutually exclusive with `showReplyingItem` (assistant
  // streaming has crossed the threshold) and `showCompactingItem` (session
  // compacting), so the four indicators cannot overlap.
  const canShowWorkingItemAfterUser = (() => {
    for (let i = messages.length - 1; i >= 0; i--) {
      const msg = messages[i];
      if (msg.type === "user") return true;
      // Skip non-message items that may be interleaved between the user
      // message and the working indicator (e.g., a compaction event loaded
      // via poll after the user message was optimistically added, or a
      // document_upload that arrived concurrently with the user send).
      if (
        msg.type === "compaction" ||
        msg.type === "system" ||
        msg.type === "document_upload"
      ) {
        continue;
      }
      // assistant / thought / tool_call / tool_result / error — agent has
      // produced a visible reply; working indicator is forbidden here, but
      // the inter-step branch (see showInterStepProcessing) may still apply
      // if sending remains true and no fresh streaming has started.
      return false;
    }
    return false;
  })();
  const showWorkingItemAfterUser = sending && canShowWorkingItemAfterUser;
  const showWorkingItemHeader = showWorkingItemAfterUser;

  // Virtual scrolling: only render visible items (messages + trailing extra
  // items).  Both extras sit AFTER messageBlocks so the sticky-bottom effect
  // can keep the user's reading position stable as they flip on and off:
  //
  //   - showReplyingItem: trailing "replying" indicator on long assistant
  //     streams (line count > chatStore.ASSISTANT_REPLYING_LINE_THRESHOLD).
  //     Lives at `index === messageBlocks.length`.  When record_complete
  //     freezes the message and isAssistantReplying clears, this slot
  //     collapses onto the just-rendered bubble content — the visual frame
  //     the user was waiting on becomes the frame the reply fills.
  //     See blockHeightEstimator.ts and VirtualMessageList.tsx for the
  //     matching slot rendering and height math.
  //
  //   - showCompactingItem: trailing "compacting" indicator during a
  //     session-wide compaction.  Always at the LAST slot (after replying)
  //     if both are present.
  //
  // (The Working indicator renders OUTSIDE the virtual list on purpose —
  // see below — so it does NOT contribute to virtualCount.)
  const showReplyingItem = isAssistantReplying;
  // Inter-step processing indicator — fires when the agent already has an
  // explore_block visible (last raw entry is agent-side, so
  // canShowWorkingItemAfterUser=false), `sending=true` is still on, but no
  // fresh thought/assistant stream_delta has landed yet (e.g. just after
  // tool_result, before the next LLM round-trip). Mutually exclusive with
  // `showReplyingItem` (assistant crossed threshold) and
  // `showCompactingItem` (session compacting). Both branches of
  // `showWorkingItem` share the same visual chrome; only the AgentAvatar
  // header is gated to the after-user branch (see `showWorkingItemHeader`).
  const showInterStepProcessing =
    sending
    && !canShowWorkingItemAfterUser
    && !showReplyingItem
    && !showCompactingItem;
  const showWorkingItem = showWorkingItemAfterUser || showInterStepProcessing;
  let extraItems = 0;
  if (showReplyingItem) extraItems++;
  if (showCompactingItem) extraItems++;
  const virtualCount = messageBlocks.length + extraItems;

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
      const container = messagesContainerRef.current;
      if (!key || !container) return;

      const distFromBottom = getDistanceFromBottom(container);
      console.log("[CP:snapshot-write]", { key, scrollOffset: container.scrollTop, sending, pinnedToBottomRef: pinnedToBottomRef.current, distFromBottom });
      setScrollSnapshot(key, {
        scrollOffset: container.scrollTop,
        // Use a generous threshold (120px) for snapshot: if the user was only
        // slightly scrolled up, treat it as pinned so nav-back re-pins to bottom.
        // The real-time handleScroll below uses a strict 5px threshold, which is
        // too tight for snapshot — a 1-frame layout shift could set it to 6px.
        //
        // During streaming we ALWAYS record pinnedToBottom=true, because the
        // message list is growing on every poll and the saved scrollOffset
        // would point to stale (or prepended) content after nav-back.  This
        // also prevents the "nav-back lands on the top of the conversation"
        // bug where a transient pinnedToBottom=false read would otherwise
        // restore a small scrollOffset.
        pinnedToBottom: sending || pinnedToBottomRef.current || distFromBottom <= CHAT_BOTTOM_THRESHOLD_PX,
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
      console.log("[ChatPanel:mount] no active session, deferring...");
      return;
    }

    // Close session panel on agent switch (UX nicety)
    if (prevAgentIdRef.current !== null && prevAgentIdRef.current !== selectedAgentId) {
      useAgentStore.getState().reset();
    }
    prevAgentIdRef.current = selectedAgentId;

    const chatStore = useChatStore.getState();
    const existingMessages = chatStore.agentStates[selectedAgentId]?.sessionStates[currentSessId]?.messages;
    const hasMessages = !!(existingMessages && existingMessages.length > 0);

    console.log("[ChatPanel:mount] atomized restore start", {
      agentId: selectedAgentId,
      sessionId: currentSessId,
      hasMessages,
    });

    // ADR-033: connectStream removed — MQTT connection is managed by Rust backend.
    if (!hasMessages) {
      // 2a. No messages in store — load from backend (first mount or new session).
      // Use ensureLatestInCache to fetch the LAST MESSAGE_CACHE_WINDOW raw
      // entries in one request.  The rendering layer's ensureRenderable
      // effect then decides if more prepended data is needed to fill the
      // viewport (the loop fires loadMoreOlderMessages one page at a time
      // until virtualizer.getTotalSize() >= clientHeight, or we hit the
      // top of the conversation).
      session.scope.current.isInitialLoad = currentSessId;
      chatStore.ensureLatestInCache(selectedAgentId, currentSessId)
        .then(() => chatStore.fetchSessionState(selectedAgentId, currentSessId))
        .finally(() => {
          session.scope.current.isInitialLoad = null;
          console.log("[ChatPanel:mount] atomized restore done (full)", {
            agentId: selectedAgentId,
            sessionId: currentSessId,
            messageCount: useChatStore.getState().agentStates[selectedAgentId]?.sessionStates[currentSessId]?.messages?.length ?? 0,
          });
        });
    } else {
      // 2b. Messages already in store (nav-back: same agent, same session).
      //     No reload needed — messages survive in zustand across unmount.
      chatStore.fetchSessionState(selectedAgentId, currentSessId);
      console.log("[ChatPanel:mount] atomized restore done (incremental)", {
        agentId: selectedAgentId,
        sessionId: currentSessId,
        messageCount: existingMessages.length,
      });
    }
  }, [selectedAgentId, selectedAgent?.running, selectedAgent?.ready]);

  // ── Session switch effect ─────────────────────────────────────────
  // When the user picks a different session from the session panel,
  // ChatPanel stays mounted — only activeSessionId changes in chatStore.
  // Load messages for the newly-active session.
  useEffect(() => {
    if (!selectedAgentId || !currentSessionId) return;

    // Guard: mount effect (above) already handles the initial session load.
    // If it set isInitialLoad, it means a load is in progress for this session.
    if (session.scope.current.isInitialLoad === currentSessionId) {
      console.log("[ChatPanel:session-switch] skipped (mount effect loading this session)");
      return;
    }

    const chatStore = useChatStore.getState();
    const existingMessages = chatStore.agentStates[selectedAgentId]?.sessionStates[currentSessionId]?.messages;
    if (existingMessages && existingMessages.length > 0) {
      // Messages already cached — just refresh session state.
      chatStore.fetchSessionState(selectedAgentId, currentSessionId);
      return;
    }

    console.log("[ChatPanel:session-switch] loading messages", {
      agentId: selectedAgentId,
      sessionId: currentSessionId,
    });

    session.scope.current.isInitialLoad = currentSessionId;
    chatStore.ensureLatestInCache(selectedAgentId, currentSessionId)
      .then(() => chatStore.fetchSessionState(selectedAgentId, currentSessionId))
      .finally(() => {
        session.scope.current.isInitialLoad = null;
      });
  }, [currentSessionId, selectedAgentId]);

  // ── Scroll restoration ───────────────────────────────────────────
  // ChatPanel is conditionally rendered — navigating to Settings/Harness
  // unmounts the entire chat subtree.  On remount the VirtualMessageList
  // is brand-new; its internal scroll-to-bottom (useLayoutEffect + [])
  // fires before the virtualizer has measured all items.
  //
  // We handle restoration explicitly here in a useLayoutEffect so scroll
  // is set before the first paint.  This eliminates the "flash of top
  // position" that would occur with a useEffect-based approach.
  const restorePinnedToBottom = scrollSnapshot?.pinnedToBottom ?? false;
  useLayoutEffect(() => {
    if (!currentScrollKey || virtualCount === 0) return;
    const snapshot = chatScrollSnapshots.get(currentScrollKey);
    if (!snapshot) return;

    const container = messagesContainerRef.current;
    if (!container) return;

    if (snapshot.pinnedToBottom) {
      pinnedToBottomRef.current = true;
      container.scrollTop = container.scrollHeight;
    } else if (snapshot.scrollOffset > 0) {
      pinnedToBottomRef.current = false;
      container.scrollTop = snapshot.scrollOffset;
    }
  }, [currentScrollKey, virtualCount, restorePinnedToBottom]);

  // ── Retry session load ──────────────────────────────────────────
  // Called from VirtualMessageList when user clicks retry on load error.
  const handleRetryLoadSession = useCallback(() => {
    if (!selectedAgentId || !currentSessionId) return;
    useChatStore.getState().ensureLatestInCache(selectedAgentId, currentSessionId);
  }, [selectedAgentId, currentSessionId]);

  // ── Page-cursor derived predicates ──
  // "hasOlder" is the rendering layer's gate for "is there more data the user
  // can scroll back to?".  Used by VirtualMessageList's ensureRenderable
  // effect to decide whether to invoke onNeedMore.  Derived from the same
  // raw-entry cursor as the pagination logic (loadMoreOlderMessages), so the
  // two paths can't drift out of sync.
  //
  // MUST use three independent primitive selectors (NOT a single selector
  // returning a fresh `{ ... }` object).  Returning a new object literal
  // each call defeats zustand's default Object.is shallow comparison and
  // causes ChatPanel to re-render on every store update — combined with
  // effects that mutate the store, this triggers the
  // "Maximum update depth exceeded" infinite loop.
  const messageOffset = useChatStore((s) => {
    if (!selectedAgentId || !currentSessionId) return 0;
    return s.agentStates[selectedAgentId]?.sessionStates[currentSessionId]?.messageOffset ?? 0;
  });
  const messageLimit = useChatStore((s) => {
    if (!selectedAgentId || !currentSessionId) return 0;
    return s.agentStates[selectedAgentId]?.sessionStates[currentSessionId]?.messageLimit ?? 0;
  });
  const messageTotal = useChatStore((s) => {
    if (!selectedAgentId || !currentSessionId) return 0;
    return s.agentStates[selectedAgentId]?.sessionStates[currentSessionId]?.messageTotal ?? 0;
  });
  const hasOlder =
    messageOffset + messageLimit < messageTotal && messageLimit > 0;

  // ── onNeedMore: load one older page when ensureRenderable asks for it ──
  // Single-page load; the VirtualMessageList effect keeps firing until the
  // viewport is full (or we hit the top).  Internally guarded by
  // `isLoadingMore` so this is safe to call repeatedly.
  const handleNeedMore = useCallback(() => {
    if (!selectedAgentId || !currentSessionId) return;
    void useChatStore.getState().loadMoreOlderMessages(selectedAgentId, currentSessionId);
  }, [selectedAgentId, currentSessionId]);

  // ── Scroll-to-bottom ─────────────────────────────────────────
  // Data-driven two-phase jump:
  //   Phase 1 — await ensureLatestInCache: fetches the LAST
  //             MESSAGE_CACHE_WINDOW raw entries.  MUST complete
  //             before phase 2, otherwise scrollToIndex fires with
  //             stale virtualCount and the position ends up in the
  //             middle after the cache refresh changes totalSize.
  //   Phase 2 — vmlRef.scrollToBottom(): delegates to virtualizer.scrollToIndex
  //             using measurementsCache.  Since phase 1 just refreshed the
  //             cache with the most recent entries, those items are guaranteed
  //             to be in the measurement cache (they're the items the user
  //             was just looking at OR are about to look at), so scrollToIndex
  //             lands on the exact real bottom — no estimation, no force-overscan
  //             hack, no rAF retry needed.
  const scrollToBottom = useCallback(async () => {
    if (!selectedAgentId || !currentSessionId) return;
    pinnedToBottomRef.current = true;
    await useChatStore.getState().ensureLatestInCache(selectedAgentId, currentSessionId);
    vmlRef.current?.scrollToBottom();
  }, [selectedAgentId, currentSessionId]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Scroll handler ────────────────────────────────────────────────
  const handleScroll = useCallback(() => {
    const container = messagesContainerRef.current;
    if (!container || !selectedAgentId) return;

    // Scroll-to-bottom button visibility
    const distFromBottom = getDistanceFromBottom(container);
    session.setShowScrollToBottom(distFromBottom > container.clientHeight);

    // Update pinned-to-bottom state
    // Strict threshold (5px): within 5px of bottom → sticky, otherwise → release.
    // No hysteresis zone — the user only needs to scroll up ~5px to escape
    // auto-scroll during streaming. Content growth never fires scroll events,
    // so this state is updated purely by user-initiated scrolls.
    //
    // NOTE: This is intentionally narrower than the snapshot threshold (120px,
    // CHAT_BOTTOM_THRESHOLD_PX).  The snapshot needs tolerance for layout shift
    // across navigation; the live handler should be sensitive to user intent.
    pinnedToBottomRef.current = distFromBottom <= 5;

    // ── Pagination trigger ──
    // One-way detection: near top → load older.  Loading newer is NOT
    // triggered here — under the unified data-window model:
    //   - Initial mount / session switch calls ensureLatestInCache (cache
    //     lands at offset=0 immediately).
    //   - The scroll-to-bottom button calls ensureLatestInCache before
    //     scrolling (so the cache is guaranteed at the tail).
    //   - VirtualMessageList's ensureRenderable effect handles "cache
    //     doesn't yet fill the viewport" by looping loadMoreOlderMessages
    //     one page at a time until getTotalSize() >= clientHeight.
    //
    // The only direction the user can pull data by scrolling is OLDER
    // (scrolling up).  Scrolling DOWN through the latest content is just
    // a forward browse — no fetch needed because the cache already holds
    // the most recent MESSAGE_CACHE_WINDOW entries.
    const { isLoadingMore } = useChatStore.getState();
    if (isLoadingMore) return;
    const agent = useChatStore.getState().agentStates[selectedAgentId];
    const activeSessId = agent?.activeSessionId;
    const sessState = activeSessId ? agent?.sessionStates[activeSessId] : undefined;
    if (!sessState) return;
    const messageOffset = sessState.messageOffset ?? 0;
    const messageLimit = sessState.messageLimit ?? 0;
    const messageTotal = sessState.messageTotal ?? 0;
    const currentSessionId = selectedAgentId ? useChatStore.getState().getActiveSessionId(selectedAgentId) : null;
    if (!currentSessionId) return;

    const hasOlder = messageOffset + messageLimit < messageTotal && messageLimit > 0;
    if (hasOlder && container.scrollTop < 50) {
      // Record the "anchorToUser" blockId BEFORE the load fires.  After older
      // messages are prepended upstream, the messageBlocks useMemo will mark
      // exactly one block with anchorToUser=true (the one whose blockId
      // matches); the load-older effect inside VirtualMessageList will then
      // scrollToIndex to that block, putting the user back at the same
      // visual position they were reading at.
      //
      // Data-driven: the anchor blockId is read directly from the strict
      // intermediate layer (messageBlocks[firstVisibleIdx].blockId). No
      // pixel math, no scrollHeight-delta estimation.
      const firstVisibleIdx = vmlRef.current?.getFirstVisibleBlockIndex() ?? null;
      if (firstVisibleIdx !== null) {
        const anchorBlock = messageBlocks[firstVisibleIdx];
        if (anchorBlock) {
          session.scope.current.anchorToUserBlockId = anchorBlock.blockId;
        }
      }
      void useChatStore
        .getState()
        .loadMoreOlderMessages(selectedAgentId, currentSessionId);
    }
  }, [selectedAgentId, session]);

  const handleSend = () => {
    const content = session.inputValue.trim();
    const hasSuccessfulFiles = session.pendingFiles.some((f) => f.status === "success");
    const hasUploadingFiles = session.pendingFiles.some((f) => f.status === "uploading");
    const hasImages = session.pendingImages.length > 0;

    // Block send: no content AND no files AND no images, or files still uploading
    if ((!content && !hasSuccessfulFiles && !hasImages) || sending || !selectedAgentId || hasUploadingFiles) return;

    // Collect successfully uploaded document IDs and metadata for optimistic bubbles
    const documentIds = session.pendingFiles
      .filter((f) => f.status === "success" && f.documentId)
      .map((f) => f.documentId!);
    const documents = session.pendingFiles
      .filter((f) => f.status === "success" && f.documentId)
      .map((f) => ({
        id: f.documentId!,
        filename: f.filename,
        format: f.format,
        size: f.size,
      }));

    // Build image parts from pending images (for multimodal content_parts)
    const imageParts = session.pendingImages.map((img) => ({
      url: img.base64Url,
      width: img.width,
      height: img.height,
    }));

    // sendMessage is async but we fire-and-forget here —
    // the store handles all state updates internally
    session.scope.current.userJustSent = true;
    void sendMessage(content, selectedAgentId, activeSkill?.name, documentIds.length > 0 ? documentIds : undefined, documents.length > 0 ? documents : undefined, imageParts.length > 0 ? imageParts : undefined).then(() => {
      clearActiveSkill();
    });
    session.setInputValue("");
    // Clear pending files and images after send
    session.setPendingFiles([]);
    session.setPendingImages([]);
  };

  // Stop button dual-action:
  //   input has content → send to queue (no stop, message waits for next loop)
  //   input empty       → stop current loop
  const handleStop = () => {
    const content = session.inputValue.trim();
    if (content && selectedAgentId && currentSessionId) {
      // Add to queue — message waits in the queue box above the input area.
      useChatStore.getState().addQueuedMessage(selectedAgentId, currentSessionId, content);
      session.setInputValue("");
    } else if (queuedMessages.length > 0 && selectedAgentId && currentSessionId) {
      // Click with queued messages: send all queued + stop current loop.
      session.scope.current.userJustSent = true;
      const msgs = [...queuedMessages];
      useChatStore.getState().setQueuedMessages(selectedAgentId, currentSessionId, []);
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
      session.setPendingFiles(prev => [...prev, {
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
      session.setPendingFiles(prev => [...prev, {
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
    session.setPendingFiles(prev => [...prev, {
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
      session.setPendingFiles(prev => prev.map((f) =>
        f.tempId === tempId
          ? { ...f, status: "success", documentId: result.document_id, size: result.size_bytes }
          : f
      ));
    } catch (err) {
      const msg = err instanceof Error ? err.message : typeof err === "string" ? err : "Upload failed";
      console.error("[ChatPanel] Document upload failed:", err);
      // Update chip to error
      session.setPendingFiles(prev => prev.map((f) =>
        f.tempId === tempId ? { ...f, status: "error", errorMessage: msg } : f
      ));
    }
  };

  // Remove a pending file chip
  const handleRemoveFile = (tempId: string) => {
    session.setPendingFiles(prev => prev.filter((f) => f.tempId !== tempId));
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
      session.setImageCapableModels(imageModels);
      session.setShowImageUnsupportedDialog(true);
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
      session.setPendingImages(prev => [...prev, {
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
    session.setPendingImages(prev => prev.filter((img) => img.tempId !== tempId));
  };

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
      console.error("[ChatPanel] Failed to send approval:", err);
    }
    // Clear the specific approval from the pending map by tool_call_id
    if (selectedAgentId && approval.tool_call_id) {
      resolveApprovalByToolCallId(selectedAgentId, approval.tool_call_id);
    } else {
      resolveApproval(selectedAgentId ?? "");
    }
  };

  // Ask question answer: send answer via MQTT, then clear pendingQuestion
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
      console.error("[ChatPanel] Failed to send question answer:", err);
    }
    // Clear pending question state regardless of result
    useChatStore.getState().resolveQuestion(agentId);
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
  // + fetchSessionState + ensureLatestInCache).
  // During this brief window activeSessionId is still null, so without this
  // gate the chat view would mount with no session and surface the
  // "Start a conversation" placeholder for a few hundred ms — a misleading
  // "blank session" bug.  Showing a spinner here keeps the contract honest:
  // when the chat view finally mounts, the session is fully bootstrapped.
  if (!currentSessionId) {
    return (
      <div className="flex flex-1 items-center justify-center bg-zinc-50 dark:bg-zinc-900">
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
        className="flex flex-1 min-w-[288px] flex-col bg-[#FAFAFA] dark:bg-zinc-900 rounded-xl overflow-hidden"
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
            aria-label={t("chatPanel.ariaLabelChatMessages")}
          >
            {/* VirtualMessageList — owns useVirtualizer and handles all virtual
                scrolling, loading states, and scroll-to-bottom.
                key={currentScrollKey} forces React to unmount/remount the entire
                component on session/agent switch.  This creates a fresh Virtualizer
                instance with scrollOffset=0, eliminating the white-screen bug where
                the old instance's scrollOffset (e.g. 5000px from a long session)
                exceeds the new session's totalSize (e.g. 800px), causing
                getVirtualItems() to return an empty array. */}
            <VirtualMessageList
              key={currentScrollKey ?? "__no_session__"}
              ref={vmlRef}
              initialScrollOffset={initialScrollOffset}
              onRetryLoadSession={handleRetryLoadSession}
              messageBlocks={messageBlocks}
              virtualCount={virtualCount}
              showCompactingItem={showCompactingItem}
              showReplyingItem={showReplyingItem}
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
              t={t}
              scrollContainerRef={messagesContainerRef}
              scope={session.scope}
              pinnedToBottomRef={pinnedToBottomRef}
              isLoadingMore={isLoadingMore}
              hasOlder={hasOlder}
              onNeedMore={handleNeedMore}
              isLoadingSession={isLoadingSession}
              loadError={loadError}
              messages={messages}
            />
            {/* Working indicator — shown OUTSIDE the virtual list so it doesn't
                affect virtualCount and cause other messages to disappear.
                Shows while the session is "streaming" but no streaming placeholder
                message exists yet (gap between session_status→streaming and the
                first new_data_available poll response, ~500-2000ms).
                Includes the agent header (avatar + name + role) above the status
                line so the user sees WHO is preparing to respond before the
                streaming content arrives; otherwise the working status appears
                alone and the agent header suddenly pops in when the first
                streaming event arrives, which feels jarring.
                Header markup is identical to the one rendered before
                explore_group (see "Agent header" comment above), so the
                transition working → streaming explore_block is seamless. */}
            {showWorkingItem && (
              <div className="select-none">
                {showWorkingItemHeader && (
                  <div className="flex items-center gap-2 mb-2 mt-1">
                    <AgentAvatar
                      agentId={selectedAgentId ?? ""}
                      displayName={agentDisplayName}
                      avatarUrl={selectedAgent?.avatar}
                      version={selectedAgent?.version}
                      builtinAvatarId={selectedAgent?.builtin_avatar ?? null}
                      size={40}
                      className="shrink-0"
                    />
                    <div className="flex flex-col">
                      <span className="text-xs font-medium text-zinc-500 dark:text-zinc-400">
                        {agentDisplayName}
                      </span>
                      {selectedAgent?.role && (
                        <span className="text-[10px] leading-tight text-zinc-400 dark:text-zinc-500">
                          {selectedAgent.role}
                        </span>
                      )}
                    </div>
                  </div>
                )}
                <div className="flex items-center gap-1.5 ml-12 py-1.5">
                  <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                  <span className="thinking-shimmer" style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>{t("chatPanel.working")}</span>
                </div>
              </div>
            )}
            {/* Replying indicator is rendered INSIDE VirtualMessageList as
                a trailing extra virtual item (see showReplyingItem prop
                above), so it physically anchors to the last message bubble
                — same conversation slot the final reply will occupy.
                When record_complete freezes the message and
                isAssistantReplying clears, the virtual slot collapses onto
                the just-rendered bubble content without a jump. */}
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
                      session.scope.current.userJustSent = true;
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
          </div>
          {/* Scroll-to-bottom button — visible when scrolled up > 1 screen */}
          {session.showScrollToBottom && (
            <button
              onClick={scrollToBottom}
              className="absolute bottom-3 right-4 z-10 rounded-full bg-zinc-100 dark:bg-zinc-700 border border-zinc-200 dark:border-zinc-600 shadow-md p-1.5 hover:bg-zinc-200 dark:hover:bg-zinc-600 transition-all animate-in fade-in zoom-in"
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
              <span className="text-[10px] font-medium text-zinc-400 dark:text-zinc-500 uppercase tracking-wider">
                {(() => {
                  const completed = todos.filter(t => t.status === "completed").length;
                  const total = todos.length;
                  const currentTodo = todos.find(t => t.status === "in_progress");
                  const isAllCompleted = completed === total && total > 0;
                  return (
                    <>
                      {t("chatPanel.taskList", { completed, total })}
                      {!isAllCompleted && currentTodo && (
                        <>
                          <span className="inline-block w-8"/>
                          <span className="normal-case text-zinc-500 dark:text-zinc-400">
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
                  aria-label={t("chatPanel.ariaLabelClearActiveSkill")}
                >
                  <X size={12} />
                </button>
              </span>
            </div>
          )}

          {/* Pending file chips */}
          {session.pendingFiles.length > 0 && (
            <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
              {session.pendingFiles.map((file) => (
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
          {session.pendingImages.length > 0 && (
            <div className="flex flex-wrap items-center gap-2 px-3 pt-2">
              {session.pendingImages.map((img) => (
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
            ref={toolbarRef}
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
                />
              )}
              {/* Reasoning effort toggle — shown when session has a non-null reasoningEffort (null = provider doesn't support reasoning) */}
              {selectedAgent?.running && currentReasoningEffort != null && (
                <ReasoningEffortMenu
                  wrapperRef={effortBtnRef}
                  textHidden={textHidden.effort}
                  effort={currentReasoningEffort}
                  onChange={(e) => selectedAgentId && setReasoningEffort(e, selectedAgentId)}
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
                        || (!session.inputValue.trim() && !session.pendingFiles.some(f => f.status === "success") && session.pendingImages.length === 0)
                        || session.pendingFiles.some(f => f.status === "uploading"))
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
  textHidden,
  wrapperRef: externalRef,
}: {
  models: { name: string; provider: string; tool_call?: boolean; reasoning?: boolean; input_modalities?: string[] }[];
  currentModel: string | null;
  currentProvider: string | null;
  onSelect: (model: string, provider: string) => void;
  textHidden?: boolean;
  /** Optional external ref merged with the internal click-outside ref */
  wrapperRef?: React.Ref<HTMLDivElement>;
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
}: {
  effort: string | null;
  onChange: (effort: string) => void;
  textHidden?: boolean;
  /** Optional external ref merged with the internal click-outside ref */
  wrapperRef?: React.Ref<HTMLDivElement>;
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

