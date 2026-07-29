import React from "react";
import { StreamingSourceBlock } from "./StreamingSourceBlock";

export interface ThinkBlockProps {
  content: string;
  isStreaming?: boolean;
  hasReplyStarted?: boolean;
  startTime?: number;
  /** Fixed end time (set by done event); if absent, duration keeps ticking in streaming mode */
  endTime?: number;
  /** Whether to default to expanded state (e.g. when this is the last message) */
  defaultExpanded?: boolean;
}

/**
 * ThinkBlock — thin wrapper around `StreamingSourceBlock` for the
 * `variant="thought"` case.  Kept as a separate component for two
 * reasons:
 *
 * 1. Backward compatibility: the rest of the codebase (MessageBubble,
 *    ExploreBlock, VirtualMessageList) imports ThinkBlock directly.
 *    Keeping the wrapper avoids touching every call site.
 *
 * 2. Domain semantics: ThinkBlock is the user-facing concept of "the
 *    agent's reasoning trace" — it carries MessageBubble-specific
 *    props like `startTime`/`endTime`/`defaultExpanded` that map
 *    cleanly onto StreamingSourceBlock.  Future variants (e.g. plan
 *    traces) can be added by extending StreamingSourceBlock without
 *    changing ThinkBlock.
 */
export const ThinkBlock = React.memo(function ThinkBlock({
  content,
  isStreaming,
  startTime,
  endTime,
  defaultExpanded,
}: ThinkBlockProps) {
  return (
    <StreamingSourceBlock
      content={content}
      isStreaming={!!isStreaming}
      startTime={startTime}
      endTime={endTime}
      defaultExpanded={defaultExpanded}
      variant="thought"
    />
  );
});