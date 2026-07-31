/**
 * Tests for the avatar-anchor rule and the messageFolder fold algorithm.
 *
 * The bug we're guarding against: when a user message carries two
 * attachments, the fold algorithm consolidates them into a single
 * `user_with_attachments` block.  The original avatar-anchor rule only
 * recognised `user` blocks, so the agent avatar header disappeared for
 * every reply that followed such a block.  These tests pin both halves
 * of the fix: the fold produces the right block type, and the anchor
 * rule accepts it as a user-anchored block.
 */

import { describe, expect, it } from "vitest";
import type { ChatMessage } from "../../lib/types";
import { foldMessages } from "./messageFolder";
import { isAgentBlock, shouldShowAgentAvatar } from "./avatarAnchor";

// ── helpers ──────────────────────────────────────────────────────────────

let counter = 0;
function id(): string {
  counter += 1;
  return `m-${counter}`;
}

function userMsg(timestamp: number, content = "hi", idOverride?: string): ChatMessage {
  return { id: idOverride ?? id(), type: "user", content, timestamp };
}

function assistantMsg(timestamp: number, content = "ok"): ChatMessage {
  return { id: id(), type: "assistant", content, timestamp };
}

function thoughtMsg(timestamp: number, content = "thinking"): ChatMessage {
  return { id: id(), type: "thought", content, timestamp };
}

function toolCallMsg(timestamp: number): ChatMessage {
  return { id: id(), type: "tool_call", content: "{}", timestamp };
}

function attachedFileMsg(
  timestamp: number,
  fileName: string,
  absPath?: string,
): ChatMessage {
  return {
    id: id(),
    type: "system",
    content: `Attached file: ${fileName}`,
    timestamp,
    metadata: {
      type: "attached_file",
      name: fileName,
      abs_path: absPath ?? `/tmp/${fileName}`,
    },
  };
}

function attachedSelectionMsg(timestamp: number): ChatMessage {
  return {
    id: id(),
    type: "system",
    content: "Attached selection",
    timestamp,
    metadata: { type: "attached_selection" },
  };
}

function compactionMsg(timestamp: number): ChatMessage {
  return { id: id(), type: "compaction", content: "...", timestamp };
}

function systemMsg(timestamp: number, content = "sys"): ChatMessage {
  return { id: id(), type: "system", content, timestamp };
}

// ── foldMessages: user + multiple attachments ─────────────────────────────

describe("foldMessages: user + multiple attachments", () => {
  it("folds user + two attached_file system entries into a single user_with_attachments block", () => {
    // Mirrors the conversation file the bug was reported from.
    const messages: ChatMessage[] = [
      userMsg(1785482337194, "两个问题：…", "msg-8758d4c0"),
      attachedFileMsg(
        1785482337197,
        "useScrollController.ts",
        "D:/projects/tranxon/ACoworkDev/apps/acowork-desktop/src/components/chat/useScrollController.ts",
      ),
      attachedFileMsg(
        1785482337197,
        "chatListAdapter.ts",
        "D:/projects/tranxon/ACoworkDev/apps/acowork-desktop/src/components/chat/chatListAdapter.ts",
      ),
      thoughtMsg(1785482345289),
      toolCallMsg(1785482345294),
    ];

    const blocks = foldMessages(messages);

    expect(blocks).toHaveLength(2);
    expect(blocks[0].type).toBe("user_with_attachments");
    expect(blocks[0].rawCount).toBe(3);
    expect(blocks[0].items.map((m) => m.id)).toEqual([
      "msg-8758d4c0",
      messages[1].id,
      messages[2].id,
    ]);
    expect(blocks[1].type).toBe("explore_group");
    expect(blocks[1].rawCount).toBe(2);
  });

  it("folds user + three different attachment types", () => {
    const messages: ChatMessage[] = [
      userMsg(1000, "with mix"),
      attachedFileMsg(1001, "a.ts"),
      attachedSelectionMsg(1002),
      toolCallMsg(1500),
    ];

    const blocks = foldMessages(messages);
    expect(blocks).toHaveLength(2);
    expect(blocks[0].type).toBe("user_with_attachments");
    expect(blocks[0].rawCount).toBe(3);
  });

  it("does NOT fold a system entry that is not an attachment type", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      // A non-attachment system message must remain a separate block.
      systemMsg(1001, "session notice"),
      assistantMsg(2000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks).toHaveLength(3);
    expect(blocks[0].type).toBe("user");
    expect(blocks[1].type).toBe("system");
    expect(blocks[2].type).toBe("assistant");
  });

  it("does NOT fold an attachment that arrived more than the fold window after the user message", () => {
    // ATTACHMENT_FOLD_WINDOW_MS is 100; place the attachment >100ms later.
    const messages: ChatMessage[] = [
      userMsg(1000),
      attachedFileMsg(1200, "late.ts"),
      assistantMsg(2000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks).toHaveLength(3);
    expect(blocks[0].type).toBe("user");
    expect(blocks[1].type).toBe("system");
    expect(blocks[2].type).toBe("assistant");
  });

  it("preserves the content-derived blockId for the user_with_attachments block", () => {
    const userId = "msg-abc";
    const messages: ChatMessage[] = [
      userMsg(1000, "x", userId),
      attachedFileMsg(1001, "a.ts"),
      attachedFileMsg(1001, "b.ts"),
    ];
    const blocks = foldMessages(messages);
    expect(blocks[0].blockId).toBe(`block-${userId}`);
  });
});

// ── isAgentBlock ─────────────────────────────────────────────────────────

describe("isAgentBlock", () => {
  function block(type: ChatMessage["type"] | "explore_group" | "user_with_attachments") {
    return {
      blockId: "x",
      type,
      items: [] as ChatMessage[],
      rawCount: 0,
      anchorToLatest: false,
      hasFollowUpReply: false,
      isLive: false,
    };
  }

  it("treats explore_group as an agent block", () => {
    expect(isAgentBlock(block("explore_group"))).toBe(true);
  });

  it("treats assistant / thought / tool_call as agent blocks", () => {
    expect(isAgentBlock(block("assistant"))).toBe(true);
    expect(isAgentBlock(block("thought"))).toBe(true);
    expect(isAgentBlock(block("tool_call"))).toBe(true);
    expect(isAgentBlock(block("tool_result"))).toBe(true);
    expect(isAgentBlock(block("error"))).toBe(true);
  });

  it("treats user / user_with_attachments / system / compaction as non-agent", () => {
    expect(isAgentBlock(block("user"))).toBe(false);
    expect(isAgentBlock(block("user_with_attachments"))).toBe(false);
    expect(isAgentBlock(block("system"))).toBe(false);
    expect(isAgentBlock(block("compaction"))).toBe(false);
  });
});

// ── shouldShowAgentAvatar: the regression ─────────────────────────────────

describe("shouldShowAgentAvatar — user_with_attachments regression", () => {
  it("shows the avatar when an agent block follows a user_with_attachments block (the reported bug)", () => {
    // The exact sequence from the reported conversation:
    //   [user_with_attachments (user + 2 attached_file), explore_group, assistant]
    const messages: ChatMessage[] = [
      userMsg(1785482337194, "q"),
      attachedFileMsg(1785482337197, "useScrollController.ts"),
      attachedFileMsg(1785482337197, "chatListAdapter.ts"),
      thoughtMsg(1785482345289),
      toolCallMsg(1785482345294),
      assistantMsg(1785482400000, "answer"),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user_with_attachments",
      "explore_group",
      "assistant",
    ]);

    // The avatar must appear above the explore_group (index 1) AND above
    // the assistant (index 2) — both are agent replies anchored to the
    // user_with_attachments block at index 0.
    expect(shouldShowAgentAvatar(blocks, 1)).toBe(true);
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(true);
  });

  it("shows the avatar even when a late attachment system entry sits between the user block and the agent reply", () => {
    // User message arrives at t=100, attachment >100ms later (outside the
    // fold window) becomes its own `system` block, then the agent replies.
    const messages: ChatMessage[] = [
      userMsg(1000),
      attachedFileMsg(1200, "late.ts"),
      thoughtMsg(2000),
      assistantMsg(3000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "system",
      "explore_group",
      "assistant",
    ]);

    expect(shouldShowAgentAvatar(blocks, 2)).toBe(true);
    expect(shouldShowAgentAvatar(blocks, 3)).toBe(true);
  });

  it("shows the avatar after a plain user block (legacy behaviour preserved)", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      assistantMsg(2000),
      assistantMsg(3000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual(["user", "assistant", "assistant"]);

    expect(shouldShowAgentAvatar(blocks, 1)).toBe(true);
    // The second assistant is NOT the first reply after the user — its
    // immediate predecessor is another agent block, so the avatar
    // belongs only to the first one.
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(false);
  });

  it("does not show the avatar above the very first block", () => {
    const messages: ChatMessage[] = [userMsg(1000), assistantMsg(2000)];
    const blocks = foldMessages(messages);
    expect(shouldShowAgentAvatar(blocks, 0)).toBe(false);
  });

  it("does not show the avatar when the conversation starts with an agent message", () => {
    const messages: ChatMessage[] = [assistantMsg(1000)];
    const blocks = foldMessages(messages);
    expect(shouldShowAgentAvatar(blocks, 0)).toBe(false);
  });

  it("skips compaction markers between the user and the agent reply", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      compactionMsg(1500),
      assistantMsg(2000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "compaction",
      "assistant",
    ]);
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(true);
  });

  it("does not show the avatar when two consecutive agent blocks precede a new user message", () => {
    const messages: ChatMessage[] = [
      userMsg(1000),
      assistantMsg(2000),
      assistantMsg(3000),
      userMsg(4000),
      assistantMsg(5000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user",
      "assistant",
      "assistant",
      "user",
      "assistant",
    ]);
    // First user→agent transition anchors the avatar on the first agent only.
    expect(shouldShowAgentAvatar(blocks, 1)).toBe(true);
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(false);
    // Second user→agent transition anchors the avatar on the new agent only.
    expect(shouldShowAgentAvatar(blocks, 4)).toBe(true);
  });

  it("treats back-to-back user_with_attachments blocks correctly: only the most recent anchors", () => {
    const messages: ChatMessage[] = [
      userMsg(1000, "first", "u1"),
      attachedFileMsg(1001, "a.ts"),
      userMsg(3000, "second", "u2"),
      attachedFileMsg(3001, "b.ts"),
      attachedFileMsg(3001, "c.ts"),
      assistantMsg(5000),
    ];
    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual([
      "user_with_attachments",
      "user_with_attachments",
      "assistant",
    ]);
    // The assistant at index 2 should anchor to the second user_with_attachments.
    expect(shouldShowAgentAvatar(blocks, 2)).toBe(true);
  });
});
