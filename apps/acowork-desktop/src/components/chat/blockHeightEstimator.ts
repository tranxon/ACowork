/**
 * Data-driven precise-height estimator for MessageBlocks.
 *
 * Every constant comes from `blockLayout.ts` — the single source of
 * truth shared with the rendered components (MessageBubble, ExploreBlock,
 * CompactionCard, DocumentChip, VirtualMessageList).  When you change a
 * Tailwind class in any of those components, update the matching constant
 * in `blockLayout.ts`; this file will pick it up automatically.
 *
 * Estimation strategy — render-faithful:
 *   The estimator mirrors the actual DOM layout of each block type.  For
 *   user messages that's max(leftColumn, rightColumn) where rightColumn
 *   is name + bubble + docs and leftColumn is avatar+mt-1.  For assistant
 *   / thought messages it sums bubble chrome + text-line height + a prose
 *   markdown bonus for paragraph/list/heading spacing.
 *
 *   Exact prediction of `prose` markdown height is impossible from data
 *   alone (it depends on actual font metrics + heading/list rendering),
 *   so we add a per-line markdown bonus tuned empirically to bias slightly
 *   high (overshoot).  Overshoot is harmless: it just means scrollTop
 *   sits a few px past the true bottom, which is visually correct.  The
 *   tanstack-virtual `measureElement` ref callback will replace our
 *   estimate with the real height the moment the item is in view, so any
 *   drift self-corrects on the next scroll.
 *
 * Why we still need a precise estimator despite measureElement:
 *   `measureElement` only writes the cache for items currently in the DOM.
 *   `virtualizer.scrollToIndex(end)` runs immediately when called and
 *   depends on `measurementsCache` for every item along the way.  Until
 *   each item has been mounted at least once, scrollToIndex lands on the
 *   estimateSize-derived position.  With the estimator below, that
 *   position is within ±10% of the true bottom for typical content, so
 *   users see "scroll lands close to the bottom" rather than "scrolls to
 *   the middle of the conversation".
 *
 * The gap between items (`useVirtualizer({ gap: 4 })`) is added by the
 * virtualizer itself, NOT by this function.  estimateSize must return
 * the body height of a single item; adding gap here double-counts.
 */

import type { MessageBlock } from "./ChatPanel";
import {
  BUBBLE_PADDING_Y,
  LINE_HEIGHT_PX,
  AVG_CHAR_WIDTH_PCT,
  PROSE_PARAGRAPH_GAP,
  PROSE_PARAGRAPH_MARGIN_TOP_PX,
  PROSE_HEADING_LINE_BONUS_PX,
  PROSE_LIST_ITEM_BONUS_PX,
  USER_NAME_LINE_HEIGHT_PX,
  USER_NAME_TOP_GAP_PX,
  USER_NAME_TO_BUBBLE_GAP_PX,
  USER_AVATAR_SIZE_PX,
  USER_AVATAR_TOP_OFFSET_PX,
  USER_BUBBLE_MAX_HEIGHT_PX,
  USER_DOCS_TOP_GAP_PX,
  DOCUMENT_CHIP_HEIGHT,
  EXPLORE_HEADER_HEIGHT,
  EXPLORE_CONTENT_MAX_HEIGHT,
  EXPLORE_CONTENT_PADDING_Y,
  EXPLORE_ITEM_ROW_HEIGHT,
  COMPACTION_CARD_HEIGHT,
  SYSTEM_BUBBLE_HEIGHT,
  COMPACTING_INDICATOR_HEIGHT,
  REPLYING_INDICATOR_HEIGHT,
  SAFE_FALLBACK_HEIGHT,
  getContentMaxWidthPct,
  getFontSizePx,
} from "./blockLayout";

// ── Text-bubble content height ──────────────────────────────────────────

/**
 * Estimate the rendered height of a plain text bubble (user / assistant /
 * thought).  Accounts for explicit \n wrap and character wrap at
 * `containerWidth × contentMaxWidthPct`.
 *
 * Returns the bubble's own height INCLUDING `py-2.5` padding.
 */
function estimateTextBubbleHeight(
  content: string,
  containerWidth: number,
): number {
  if (!content) {
    // Empty bubble (e.g. streaming placeholder) — base padding only,
    // plus one line for the cursor/dot.
    return BUBBLE_PADDING_Y + LINE_HEIGHT_PX;
  }
  if (containerWidth <= 0) return SAFE_FALLBACK_HEIGHT;

  const usableWidth = containerWidth * getContentMaxWidthPct();
  const fontSize = getFontSizePx();
  const charWidth = fontSize * AVG_CHAR_WIDTH_PCT;
  const charsPerLine = Math.max(1, Math.floor(usableWidth / charWidth));

  // Split into logical lines by \n first, then wrap each logical line.
  const logicalLines = content.split("\n");
  let totalLines = 0;
  for (const line of logicalLines) {
    if (line.length === 0) {
      totalLines += 1;
    } else {
      totalLines += Math.ceil(line.length / charsPerLine);
    }
  }

  return BUBBLE_PADDING_Y + totalLines * LINE_HEIGHT_PX;
}

// ── Public API ──────────────────────────────────────────────────────────

/**
 * Compute the height (in px) of one virtual item at `index`.
 *
 * Two extra virtual items can sit past the message blocks; their slot
 * indices are fixed so the renderer can dispatch by `index`:
 *   - replying indicator (if shown) is at `index === messageBlocks.length`
 *   - compacting indicator (if shown) is at the LAST slot, i.e.
 *     `index === messageBlocks.length + extraCount - 1`
 *
 * Both can technically co-exist on paper, but in practice compacting is a
 * session-wide system operation that runs while the user is idle, and
 * replying only fires mid-stream after the user has already received at
 * least the line threshold of content.  The layout supports both anyway
 * so callers don't have to coordinate them.
 *
 * @param index                Virtual item index (0 .. virtualCount-1).
 * @param messageBlocks        The strict intermediate MessageBlock array.
 * @param containerWidth       Current rendered width of the messages scroll
 *                             container in px.  Used for text-bubble line
 *                             wrap calculations; pass 0 if not yet measured
 *                             (the function falls back to a safe constant).
 * @param showCompactingItem   True when a virtual item is reserved for the
 *                             compacting indicator.
 * @param showReplyingItem     True when a virtual item is reserved for the
 *                             replying indicator (assistant long-stream).
 */
export function estimateBlockHeight(
  index: number,
  messageBlocks: MessageBlock[],
  containerWidth: number,
  showCompactingItem: boolean,
  showReplyingItem: boolean,
): number {
  // Trailing extra slot indices.  Replying (if active) sits IMMEDIATELY
  // after messageBlocks; compacting (if active) always sits LAST so the
  // sticky-bottom effect never has to reason about reordering them.
  const extraCount =
    (showReplyingItem ? 1 : 0) + (showCompactingItem ? 1 : 0);
  const replyingIdx = showReplyingItem ? messageBlocks.length : -1;
  const compactingIdx = showCompactingItem
    ? messageBlocks.length + extraCount - 1
    : -1;

  if (index === replyingIdx) {
    return REPLYING_INDICATOR_HEIGHT;
  }
  if (index === compactingIdx) {
    return COMPACTING_INDICATOR_HEIGHT;
  }
  // Past-the-end indices are treated like the nearest trailing indicator so
  // we never accidentally return the safe-fallback for slots the caller
  // forgot to account for.
  if (index >= messageBlocks.length && index < messageBlocks.length + extraCount) {
    return REPLYING_INDICATOR_HEIGHT;
  }

  const block = messageBlocks[index];
  if (!block) return SAFE_FALLBACK_HEIGHT;

  switch (block.type) {
    case "user": {
      const msg = block.items[0];
      const content = msg?.content ?? "";
      const hasName = !!msg?.senderDisplayName;
      const docCount = msg?.documents?.length ?? 0;
      const hasDocs = docCount > 0;
      // Right column (text side): liveUserName (mt-[2px] + line) →
      // mt-[6px] → bubble (capped at max-h-48) → mt-[6px] → doc row.
      const bubbleHeight = Math.min(
        USER_BUBBLE_MAX_HEIGHT_PX,
        estimateTextBubbleHeight(content, containerWidth),
      );
      const rightColumn =
        (hasName ? USER_NAME_TOP_GAP_PX + USER_NAME_LINE_HEIGHT_PX : 0)
        + USER_NAME_TO_BUBBLE_GAP_PX
        + bubbleHeight
        + (hasDocs ? USER_DOCS_TOP_GAP_PX + DOCUMENT_CHIP_HEIGHT : 0);
      // Left column (avatar side): mt-1 + 40px avatar.
      const leftColumn = USER_AVATAR_TOP_OFFSET_PX + USER_AVATAR_SIZE_PX;
      // The flex container is `items-start`, so total height = max of the
      // two columns.  This mirrors the actual rendered DOM.
      return Math.max(rightColumn, leftColumn);
    }
    case "assistant":
    case "thought": {
      const msg = block.items[0];
      const content = msg?.content ?? "";
      // While `isStreaming`, MessageBubble renders a single-line "thinking"
      // placeholder instead of the full text.  Default to the streaming
      // bubble height when content is empty (the streaming path also reads
      // `displayContent` from useStreamingContent, which is empty until
      // record_complete freezes the message).
      if (!content && msg?.isStreaming) {
        return BUBBLE_PADDING_Y + LINE_HEIGHT_PX;
      }
      const textHeight = estimateTextBubbleHeight(content, containerWidth);
      // Markdown prose bonus — head/lists/paragraphs inflate real height
      // beyond what plain-text line counts predict.  We estimate by
      // scanning the content for structural markers and adding per-line
      // bonuses.  Biases slightly high (overshoot) for safety.
      const logicalLines = content.split("\n");
      const nonEmpty = logicalLines.filter((l) => l.length > 0);
      let markdownBonus = 0;
      for (const line of nonEmpty) {
        // Markdown headings (`# `, `## `, `### `, `#### `) get an extra
        // ~8px line bonus for the larger font + line-height.
        if (/^#{1,6}\s/.test(line)) {
          markdownBonus += PROSE_HEADING_LINE_BONUS_PX;
        }
        // List markers (`- `, `* `, `1. `) get a per-item bonus.
        if (/^\s*([-*]|\d+\.)\s/.test(line)) {
          markdownBonus += PROSE_LIST_ITEM_BONUS_PX;
        }
        // Code fences inflate by a full line (``` is a 22px block).
        if (/^```/.test(line)) {
          markdownBonus += LINE_HEIGHT_PX;
        }
      }
      // Paragraph gap: every consecutive non-empty line after the first
      // gets a prose paragraph margin-top (16px) in addition to line-height.
      // Conservatively add one prose-margin per non-empty line for an
      // extra layer above plain text.
      const paragraphOverhead = Math.max(0, nonEmpty.length - 1) * PROSE_PARAGRAPH_GAP;
      // Additional prose-block margin for multi-paragraph responses.
      const proseBlockOverhead = nonEmpty.length > 1
        ? PROSE_PARAGRAPH_MARGIN_TOP_PX
        : 0;
      return textHeight + markdownBonus + paragraphOverhead + proseBlockOverhead;
    }
    case "explore_group": {
      // hasFollowUpReply is set by ChatPanel when it builds messageBlocks.
      // Collapsed (has follow-up reply): just the header.
      // Expanded: header + content area up to the 240px cap, scaled by item count.
      if (block.hasFollowUpReply) return EXPLORE_HEADER_HEIGHT;
      const itemCount = Math.max(1, block.items.length);
      const contentHeight = Math.min(
        EXPLORE_CONTENT_MAX_HEIGHT,
        itemCount * EXPLORE_ITEM_ROW_HEIGHT + EXPLORE_CONTENT_PADDING_Y,
      );
      return EXPLORE_HEADER_HEIGHT + contentHeight;
    }
    case "system":
      return SYSTEM_BUBBLE_HEIGHT;
    case "compaction":
      return COMPACTION_CARD_HEIGHT;
    case "document_upload":
      return DOCUMENT_CHIP_HEIGHT;
    // tool_call / tool_result outside an explore_group (orphaned) are
    // rendered as their own MessageBubble with fixed-height toggles.
    case "tool_call":
    case "tool_result":
      // Single-line collapsed toggle.  Expanded form is rare and a small
      // undershoot is harmless (measureElement corrects it the moment the
      // user scrolls past).
      return 28;
    default:
      return SAFE_FALLBACK_HEIGHT;
  }
}
