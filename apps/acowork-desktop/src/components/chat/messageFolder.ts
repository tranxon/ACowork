/**
 * Pure function that folds raw `ChatMessage[]` into `MessageBlock[]`.
 *
 * Extracted from ChatPanel's `useMemo` (ADR-041 C1) so that the folding
 * logic is reusable and testable without a React component context.
 *
 * Key change from the old implementation: `blockId` is now **content-derived**
 * (`block-${items[0].id}`) instead of **index-derived** (`block-${i}`).
 * This means prepend/append operations do not change the blockId of any
 * existing block, which is the foundation for stable scroll anchoring.
 */

import type { ChatMessage } from "../../lib/types";

/**
 * Strict intermediate representation between the data layer (raw
 * `ChatMessage[]` in chatStore) and the rendering layer
 * (`VirtualMessageList`).
 *
 * Every property is data-defined, not visually estimated:
 *
 * - `type`           drives component choice (MessageBubble vs ExploreBlock).
 * - `items`          raw messages the block contains; for non-group blocks
 *                    `items.length === 1`.
 * - `rawCount`       `items.length` cached for cheap arithmetic.
 * - `anchorToLatest` true iff this block contains the **last raw entry** in
 *                    the current messages array.
 * - `hasFollowUpReply` true iff an `assistant` (or other non-explore) message
 *                    follows this explore block in display order.
 *                    Only meaningful for `type === "explore_group"`.
 */
export interface MessageBlock {
  /** Content-derived stable ID: `block-${items[0].id}`. Survives prepend/append. */
  blockId: string;
  type: ChatMessage["type"] | "explore_group";
  items: ChatMessage[];
  rawCount: number;
  anchorToLatest: boolean;
  hasFollowUpReply: boolean;
}

/**
 * Fold raw `ChatMessage[]` into `MessageBlock[]`.
 *
 * Rules:
 *  - Consecutive `tool_call` / `tool_result` / `thought` messages are grouped
 *    into a single `explore_group` block.
 *  - All other message types become individual blocks.
 *  - `blockId` is derived from the first item's `id` (content-derived, stable).
 *  - `anchorToLatest` is set on the block containing the last raw entry.
 *  - `hasFollowUpReply` is computed in a second pass for explore_group blocks.
 */
export function foldMessages(messages: ChatMessage[]): MessageBlock[] {
  const blocks: MessageBlock[] = [];
  const lastIdx = messages.length - 1;
  let exploreStartMsgIdx = -1; // index into messages[] where the explore group starts
  let exploreBuffer: ChatMessage[] = [];

  const flushExplore = () => {
    if (exploreBuffer.length === 0) return;
    const items = exploreBuffer;
    // Content-derived blockId: use the first item's id.
    const exploreBlockId = `block-${items[0].id}`;
    blocks.push({
      blockId: exploreBlockId,
      type: "explore_group",
      items,
      rawCount: items.length,
      anchorToLatest:
        exploreStartMsgIdx + items.length - 1 === lastIdx,
      // Backfilled in the second pass below.
      hasFollowUpReply: false,
    });
    exploreBuffer = [];
    exploreStartMsgIdx = -1;
  };

  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i];

    if (
      msg.type === "tool_call" ||
      msg.type === "tool_result" ||
      msg.type === "thought"
    ) {
      if (exploreStartMsgIdx < 0) exploreStartMsgIdx = i;
      exploreBuffer.push(msg);
    } else {
      flushExplore();
      const blockId = `block-${msg.id}`;
      blocks.push({
        blockId,
        type: msg.type,
        items: [msg],
        rawCount: 1,
        anchorToLatest: i === lastIdx,
        hasFollowUpReply: false,
      });
    }
  }
  flushExplore();

  // Second pass: an explore_group has a follow-up reply iff the next block
  // in display order is NOT itself an explore_group.
  for (let i = 0; i < blocks.length - 1; i++) {
    const cur = blocks[i];
    if (
      cur.type === "explore_group" &&
      blocks[i + 1].type !== "explore_group"
    ) {
      blocks[i] = { ...cur, hasFollowUpReply: true };
    }
  }

  return blocks;
}
