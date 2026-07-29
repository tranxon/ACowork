/**
 * ADR-046 §2.5: User message bubble with attached items.
 *
 * Renders a user text message with its associated attachment chips
 * (file_upload, image_upload, attached_file, attached_selection,
 * attached_folder) in a single right-aligned block with the user avatar.
 *
 * This is the rendering counterpart of the `user_with_attachments`
 * block type produced by `foldMessages`.
 */
import { useCallback } from "react";
import { UserAvatar } from "../common/UserAvatar";
import { AttachmentChipRow } from "./AttachmentChipRow";
import type { AttachedItem, ChatMessage } from "../../lib/types";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { openAttachedRef } from "../../lib/openWorkspaceRef";

export interface UserWithAttachmentsBubbleProps {
  /** The user text message (items[0] of the block). */
  userMessage: ChatMessage;
  /** Attachment system entries (items[1..] of the block). */
  attachments: ChatMessage[];
  /** User display name. */
  liveUserName: string | null | undefined;
  /** User avatar URL. */
  liveUserAvatarUrl: string | null | undefined;
  /** User builtin avatar ID. */
  liveUserBuiltinAvatarId: string | null | undefined;
  /** Current session ID (for remove handler). */
  currentSessionId: string;
  /** Agent ID (for image blob fetch). */
  agentId: string | null | undefined;
}

/** Reconstruct an `AttachedItem` from a system message's metadata. */
function metadataToAttachedItem(msg: ChatMessage): AttachedItem | null {
  const meta = msg.metadata;
  if (!meta) return null;
  const metaType = meta.type as string;

  switch (metaType) {
    case "file_upload":
      return {
        type: "file_upload",
        documentId: (meta.document_id as string) ?? (meta.documentId as string) ?? "",
        filename: (meta.filename as string) ?? "",
        format: (meta.format as string) ?? "",
        sizeBytes: (meta.size_bytes as number) ?? (meta.sizeBytes as number) ?? 0,
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    case "image_upload":
      return {
        type: "image_upload",
        documentId: (meta.document_id as string) ?? (meta.documentId as string) ?? "",
        filename: (meta.filename as string) ?? "",
        format: (meta.format as string) ?? "",
        sizeBytes: (meta.size_bytes as number) ?? (meta.sizeBytes as number) ?? 0,
        width: typeof meta.width === "number" ? meta.width : undefined,
        height: typeof meta.height === "number" ? meta.height : undefined,
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    case "attached_file":
      return {
        type: "attached_file",
        absPath: (meta.abs_path as string) ?? (meta.absPath as string) ?? "",
        name: (meta.name as string) ?? "",
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    case "attached_selection":
      return {
        type: "attached_selection",
        absPath: (meta.abs_path as string) ?? (meta.absPath as string) ?? "",
        name: (meta.name as string) ?? "",
        startLine: (meta.start_line as number) ?? (meta.startLine as number) ?? 1,
        endLine: (meta.end_line as number) ?? (meta.endLine as number) ?? 1,
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    case "attached_folder":
      return {
        type: "attached_folder",
        absPath: (meta.abs_path as string) ?? (meta.absPath as string) ?? "",
        name: (meta.name as string) ?? "",
        clientId: (meta.client_id as string) ?? (meta.clientId as string) ?? undefined,
      };
    default:
      return null;
  }
}

export function UserWithAttachmentsBubble({
  userMessage,
  attachments,
  liveUserName,
  liveUserAvatarUrl,
  liveUserBuiltinAvatarId,
  currentSessionId,
  agentId,
}: UserWithAttachmentsBubbleProps) {
  const removeMessageAttachment = useChatStore((s) => s.removeMessageAttachment);

  const handleChipRemove = useCallback(
    (_item: AttachedItem) => {
      removeMessageAttachment(agentId ?? "", currentSessionId, userMessage.id);
    },
    [agentId, currentSessionId, userMessage.id, removeMessageAttachment],
  );

  const handleChipClick = useCallback(
    (item: AttachedItem) => {
      // Open workspace refs (attached_file / attached_selection) in the
      // in-app fileTab. Matches the handler in MessageBubble.tsx.
      const agentIdFromStore = useAgentStore.getState().selectedAgentId;
      if (!agentIdFromStore) return;
      const workspaceId = useWorkspaceStore
        .getState()
        .getSessionWorkspaceId(currentSessionId);
      void openAttachedRef({
        item,
        agentId: agentIdFromStore,
        currentWorkspaceId: workspaceId,
      });
    },
    [currentSessionId],
  );

  const fontSizeStyle: React.CSSProperties | undefined = undefined;

  return (
    <div className="flex items-start justify-end gap-2">
      <div className="min-w-0 flex-1 flex flex-col items-end">
        {liveUserName && (
          <span className="mt-[2px] text-xs text-zinc-400 dark:text-zinc-500">
            {liveUserName}
          </span>
        )}

        {/* Attachment chips — rendered above the text bubble */}
        {attachments.length > 0 && (
          <div className="mt-2 flex flex-col items-end gap-1">
            {attachments.map((attMsg) => {
              const item = metadataToAttachedItem(attMsg);
              if (!item) return null;
              return (
                <AttachmentChipRow
                  key={attMsg.id}
                  item={item}
                  agentId={agentId}
                  compact
                  onChipClick={handleChipClick}
                  onRemove={handleChipRemove}
                  pending={!!attMsg._isOptimistic}
                />
              );
            })}
          </div>
        )}

        {/* User text bubble */}
        {userMessage.content && (
          <div
            className="mt-[6px] max-w-[85%] rounded-md rounded-br-sm bg-chat-user px-4 py-2.5 text-chat-user-text select-text whitespace-pre-wrap break-words max-h-48 overflow-y-auto"
            style={fontSizeStyle}
          >
            {userMessage.content}
          </div>
        )}
      </div>

      <UserAvatar
        displayName={liveUserName ?? undefined}
        avatarUrl={liveUserAvatarUrl ?? null}
        builtinAvatarId={liveUserBuiltinAvatarId ?? null}
        size={40}
        className="shrink-0 mt-1"
      />
    </div>
  );
}
